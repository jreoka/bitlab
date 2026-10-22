package main

import (
	"database/sql"
	"encoding/json"
	"errors"
	"os"
	"strings"
	"time"

	_ "modernc.org/sqlite"
)

var (
	ErrKeyNotFound = errors.New("manifest key not recognised")
	ErrKeyPaused   = errors.New("manifest key paused")
	ErrKeyExpired  = errors.New("manifest key expired")
	ErrNoKeys      = errors.New("no manifest keys exist")
)

// Key is a per-user manifest credential. Revoking deletes it outright;
// pausing keeps the row (and its statistics) but stops it resolving.
type Key struct {
	ID        string `json:"id"`
	Label     string `json:"label"`
	Note      string `json:"note"`
	Token     string `json:"token"`
	Status    string `json:"status"` // active | paused
	CreatedAt int64  `json:"created_at"`
	LastUsed  int64  `json:"last_used_at"`
	Requests  int64  `json:"requests"`
	Bytes     int64  `json:"bytes"`
	ExpiresAt int64  `json:"expires_at"`
}

func (k Key) Paused() bool { return k.Status == "paused" }

// Session is one signed-in admin browser.
type Session struct {
	ID        string `json:"id"`
	Name      string `json:"name"`
	IP        string `json:"ip"`
	UA        string `json:"ua"`
	CreatedAt int64  `json:"created_at"`
	LastSeen  int64  `json:"last_seen_at"`
	ExpiresAt int64  `json:"expires_at"`
}

// Indexer is one configured search source.
type Indexer struct {
	ID       string `json:"id"`
	Kind     string `json:"kind"` // torznab | knaben | nyaa | apibay | eztv | yts | animetosho
	Name     string `json:"name"`
	URL      string `json:"url"`
	APIKey   string `json:"api_key,omitempty"`
	Enabled  bool   `json:"enabled"`
	Priority int    `json:"priority"`
}

// CachedFile is a file entry remembered from torrent metadata.
type CachedFile struct {
	Idx  int    `json:"idx"`
	Path string `json:"path"`
	Size int64  `json:"size"`
}

type Store struct {
	db *sql.DB
}

func OpenStore(path string) (*Store, error) {
	// A plain file name plus query parameters: the driver strips the query and
	// applies the PRAGMAs itself, so SQLite never parses the path as a URI.
	// That matters on Windows, where a URI would mangle "C:\..." and a name
	// containing '%' or '#' would be misread.
	dsn := path + "?_pragma=busy_timeout(5000)&_pragma=journal_mode(WAL)&_pragma=synchronous(FULL)"
	db, err := sql.Open("sqlite", dsn)
	if err != nil {
		return nil, err
	}
	db.SetMaxOpenConns(1)
	s := &Store{db: db}
	if err := s.migrate(); err != nil {
		db.Close()
		return nil, err
	}
	// The database holds manifest tokens, the session HMAC key and the TOTP
	// seed. mkdir 0700 already limits the directory, but a copied file or a
	// volume snapshot should not be world-readable either.
	for _, p := range []string{path, path + "-wal", path + "-shm"} {
		_ = os.Chmod(p, 0o600)
	}
	return s, nil
}

func (s *Store) Close() error { return s.db.Close() }

func (s *Store) migrate() error {
	_, err := s.db.Exec(`
CREATE TABLE IF NOT EXISTS keys (
  id TEXT PRIMARY KEY,
  token TEXT NOT NULL UNIQUE,
  label TEXT NOT NULL,
  note TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  last_used_at INTEGER NOT NULL DEFAULT 0,
  requests INTEGER NOT NULL DEFAULT 0,
  bytes INTEGER NOT NULL DEFAULT 0,
  expires_at INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS sessions (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL DEFAULT '',
  ip TEXT NOT NULL DEFAULT '',
  ua TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS indexers (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  name TEXT NOT NULL,
  url TEXT NOT NULL DEFAULT '',
  api_key TEXT NOT NULL DEFAULT '',
  enabled INTEGER NOT NULL DEFAULT 1,
  priority INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS settings (
  k TEXT PRIMARY KEY,
  v TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS media_cache (
  infohash TEXT PRIMARY KEY,
  name TEXT NOT NULL DEFAULT '',
  size INTEGER NOT NULL DEFAULT 0,
  files TEXT NOT NULL DEFAULT '[]',
  first_seen INTEGER NOT NULL,
  last_used INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS meta_cache (
  k TEXT PRIMARY KEY,
  v TEXT NOT NULL,
  expires_at INTEGER NOT NULL
);
`)
	if err != nil {
		return err
	}
	// `status` replaces the old boolean `revoked`; revoking now deletes the key,
	// so any key that was revoked under the old scheme goes away.
	if !s.hasColumn("keys", "status") {
		if _, err := s.db.Exec(`ALTER TABLE keys ADD COLUMN status TEXT NOT NULL DEFAULT 'active'`); err != nil {
			return err
		}
	}
	if s.hasColumn("keys", "revoked") {
		if _, err := s.db.Exec(`DELETE FROM keys WHERE revoked=1`); err != nil {
			return err
		}
	}
	// The one-row Auto picker was rolled back at v2.3.0 and its tables were left
	// behind with their rows. Nothing reads them; dropping keeps the file honest.
	for _, t := range []string{"auto_picks", "auto_sessions", "stalls", "track_probes"} {
		if _, err := s.db.Exec(`DROP TABLE IF EXISTS ` + t); err != nil {
			return err
		}
	}
	// The load-time predictor (v2.10.0) keeps its own three tables: the pool of
	// releases it may probe, the measurements themselves, and the weights.
	//
	// probe_candidates.seeders is the *indexer's* reported count at capture
	// time, not the live swarm: that is the number a row carries when it is
	// drawn, so it is the only seeder figure the model may train on.
	_, err = s.db.Exec(`
CREATE TABLE IF NOT EXISTS probe_candidates (
  infohash TEXT PRIMARY KEY,
  name TEXT NOT NULL DEFAULT '',
  indexer TEXT NOT NULL DEFAULT '',
  seeders INTEGER NOT NULL DEFAULT 0,
  size INTEGER NOT NULL DEFAULT 0,
  typ TEXT NOT NULL DEFAULT '',
  season INTEGER NOT NULL DEFAULT 0,
  first_seen INTEGER NOT NULL,
  last_seen INTEGER NOT NULL,
  probes INTEGER NOT NULL DEFAULT 0,
  last_probe INTEGER NOT NULL DEFAULT 0,
  last_ms INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS probe_candidates_pick ON probe_candidates(last_probe);
CREATE TABLE IF NOT EXISTS probes (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  infohash TEXT NOT NULL,
  at INTEGER NOT NULL,
  meta_ms INTEGER NOT NULL DEFAULT -1,
  first_ms INTEGER NOT NULL DEFAULT -1,
  total_ms INTEGER NOT NULL DEFAULT -1,
  mbps REAL NOT NULL DEFAULT 0,
  bytes INTEGER NOT NULL DEFAULT 0,
  cached INTEGER NOT NULL DEFAULT 0,
  cold INTEGER NOT NULL DEFAULT 0,
  seeders_report INTEGER NOT NULL DEFAULT 0,
  seeders_seen INTEGER NOT NULL DEFAULT -1,
  size INTEGER NOT NULL DEFAULT 0,
  indexer TEXT NOT NULL DEFAULT '',
  title TEXT NOT NULL DEFAULT '',
  ok INTEGER NOT NULL DEFAULT 0,
  err TEXT NOT NULL DEFAULT '',
  feat TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS probes_at ON probes(at);
CREATE INDEX IF NOT EXISTS probes_ih ON probes(infohash);
CREATE TABLE IF NOT EXISTS model_weights (
  k TEXT PRIMARY KEY,
  v BLOB NOT NULL,
  trained_at INTEGER NOT NULL,
  samples INTEGER NOT NULL DEFAULT 0,
  mae REAL NOT NULL DEFAULT 0,
  within2x REAL NOT NULL DEFAULT 0
);
`)
	if err != nil {
		return err
	}
	return nil
}

func (s *Store) hasColumn(table, col string) bool {
	rows, err := s.db.Query(`PRAGMA table_info(` + table + `)`)
	if err != nil {
		return false
	}
	defer rows.Close()
	for rows.Next() {
		var cid int
		var name, ctype string
		var notnull, pk int
		var dflt any
		if err := rows.Scan(&cid, &name, &ctype, &notnull, &dflt, &pk); err != nil {
			return false
		}
		if name == col {
			return true
		}
	}
	return false
}

// --- keys ---

const keyCols = `id,token,label,note,status,created_at,last_used_at,requests,bytes,expires_at`

func scanKey(sc interface{ Scan(...any) error }) (Key, error) {
	var k Key
	err := sc.Scan(&k.ID, &k.Token, &k.Label, &k.Note, &k.Status, &k.CreatedAt, &k.LastUsed,
		&k.Requests, &k.Bytes, &k.ExpiresAt)
	return k, err
}

func (s *Store) CreateKey(label, note string, ttl time.Duration) (Key, error) {
	if label == "" {
		label = "unnamed"
	}
	id := randHex(6)
	token := "bit_" + randToken(20)
	var exp int64
	if ttl > 0 {
		exp = time.Now().Add(ttl).Unix()
	}
	now := time.Now().Unix()
	_, err := s.db.Exec(`INSERT INTO keys(id,token,label,note,status,created_at,expires_at) VALUES(?,?,?,?,?,?,?)`,
		id, token, label, note, "active", now, exp)
	if err != nil {
		return Key{}, err
	}
	return Key{ID: id, Token: token, Label: label, Note: note, Status: "active", CreatedAt: now, ExpiresAt: exp}, nil
}

func (s *Store) ListKeys() ([]Key, error) {
	rows, err := s.db.Query(`SELECT ` + keyCols + ` FROM keys ORDER BY created_at DESC`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	out := []Key{}
	for rows.Next() {
		k, err := scanKey(rows)
		if err != nil {
			return nil, err
		}
		out = append(out, k)
	}
	return out, rows.Err()
}

// ListKeysPage returns one page of keys, optionally filtered by a substring of
// label/note/id, plus the total matching count.
func (s *Store) ListKeysPage(q string, limit, offset int) ([]Key, int, error) {
	where := ""
	var args []any
	if q = strings.TrimSpace(q); q != "" {
		where = " WHERE (label LIKE ? OR note LIKE ? OR id LIKE ?)"
		like := "%" + q + "%"
		args = append(args, like, like, like)
	}
	var total int
	if err := s.db.QueryRow("SELECT count(*) FROM keys"+where, args...).Scan(&total); err != nil {
		return nil, 0, err
	}
	pageArgs := append(append([]any{}, args...), limit, offset)
	rows, err := s.db.Query("SELECT "+keyCols+" FROM keys"+where+" ORDER BY created_at DESC LIMIT ? OFFSET ?", pageArgs...)
	if err != nil {
		return nil, 0, err
	}
	defer rows.Close()
	out := []Key{}
	for rows.Next() {
		k, err := scanKey(rows)
		if err != nil {
			return nil, 0, err
		}
		out = append(out, k)
	}
	return out, total, rows.Err()
}

func (s *Store) KeyByToken(token string) (Key, error) {
	k, err := scanKey(s.db.QueryRow(`SELECT `+keyCols+` FROM keys WHERE token=?`, token))
	if errors.Is(err, sql.ErrNoRows) {
		return Key{}, ErrKeyNotFound
	}
	if err != nil {
		return Key{}, err
	}
	// Expiry means deletion, not just refusal: purge the row so nothing lingers.
	if k.ExpiresAt > 0 && time.Now().Unix() > k.ExpiresAt {
		s.DeleteKey(k.ID)
		return k, ErrKeyExpired
	}
	if k.Paused() {
		return k, ErrKeyPaused
	}
	return k, nil
}

func (s *Store) SetKeyStatus(id, status string) error {
	res, err := s.db.Exec(`UPDATE keys SET status=? WHERE id=?`, status, id)
	if err != nil {
		return err
	}
	if n, _ := res.RowsAffected(); n == 0 {
		return ErrKeyNotFound
	}
	return nil
}

func (s *Store) DeleteKey(id string) error {
	res, err := s.db.Exec(`DELETE FROM keys WHERE id=?`, id)
	if err != nil {
		return err
	}
	if n, _ := res.RowsAffected(); n == 0 {
		return ErrKeyNotFound
	}
	return nil
}

// DeleteExpiredKeys purges every key past its expiry and reports the count.
func (s *Store) DeleteExpiredKeys() int64 {
	res, err := s.db.Exec(`DELETE FROM keys WHERE expires_at>0 AND expires_at<?`, time.Now().Unix())
	if err != nil {
		return 0
	}
	n, _ := res.RowsAffected()
	return n
}

func (s *Store) TouchKey(id string) {
	s.db.Exec(`UPDATE keys SET last_used_at=?, requests=requests+1 WHERE id=?`, time.Now().Unix(), id)
}

func (s *Store) AddKeyBytes(id string, n int64) {
	if n <= 0 {
		return
	}
	s.db.Exec(`UPDATE keys SET bytes=bytes+? WHERE id=?`, n, id)
}

func (s *Store) ActiveKeyCount() int {
	var n int
	s.db.QueryRow(`SELECT count(*) FROM keys WHERE status='active' AND (expires_at=0 OR expires_at>?)`,
		time.Now().Unix()).Scan(&n)
	return n
}

// --- sessions ---

func (s *Store) CreateSession(name, ip, ua string, ttl time.Duration) (Session, error) {
	now := time.Now()
	sess := Session{
		ID:        randHex(16),
		Name:      name,
		IP:        ip,
		UA:        ua,
		CreatedAt: now.Unix(),
		LastSeen:  now.Unix(),
		ExpiresAt: now.Add(ttl).Unix(),
	}
	_, err := s.db.Exec(`INSERT INTO sessions(id,name,ip,ua,created_at,last_seen_at,expires_at) VALUES(?,?,?,?,?,?,?)`,
		sess.ID, sess.Name, sess.IP, sess.UA, sess.CreatedAt, sess.LastSeen, sess.ExpiresAt)
	if err != nil {
		return Session{}, err
	}
	s.db.Exec(`DELETE FROM sessions WHERE expires_at<?`, now.Unix())
	return sess, nil
}

func (s *Store) GetSession(id string) (Session, bool) {
	var sess Session
	err := s.db.QueryRow(`SELECT id,name,ip,ua,created_at,last_seen_at,expires_at FROM sessions WHERE id=?`, id).
		Scan(&sess.ID, &sess.Name, &sess.IP, &sess.UA, &sess.CreatedAt, &sess.LastSeen, &sess.ExpiresAt)
	if err != nil {
		return Session{}, false
	}
	if sess.ExpiresAt < time.Now().Unix() {
		s.DeleteSession(id)
		return Session{}, false
	}
	return sess, true
}

func (s *Store) TouchSession(id string) {
	s.db.Exec(`UPDATE sessions SET last_seen_at=? WHERE id=? AND last_seen_at<?`,
		time.Now().Unix(), id, time.Now().Add(-60*time.Second).Unix())
}

func (s *Store) DeleteSession(id string) error {
	_, err := s.db.Exec(`DELETE FROM sessions WHERE id=?`, id)
	return err
}

func (s *Store) DeleteOtherSessions(keepID string) {
	s.db.Exec(`DELETE FROM sessions WHERE id<>?`, keepID)
}

func (s *Store) ListSessions() ([]Session, error) {
	rows, err := s.db.Query(`SELECT id,name,ip,ua,created_at,last_seen_at,expires_at FROM sessions ORDER BY last_seen_at DESC`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	out := []Session{}
	for rows.Next() {
		var sess Session
		if err := rows.Scan(&sess.ID, &sess.Name, &sess.IP, &sess.UA, &sess.CreatedAt, &sess.LastSeen, &sess.ExpiresAt); err != nil {
			return nil, err
		}
		out = append(out, sess)
	}
	return out, rows.Err()
}

// PruneSessions drops expired rows and keeps only the newest n.
func (s *Store) PruneSessions(keep int) {
	s.db.Exec(`DELETE FROM sessions WHERE expires_at<?`, time.Now().Unix())
	var n int
	s.db.QueryRow(`SELECT count(*) FROM sessions`).Scan(&n)
	if n > keep {
		s.db.Exec(`DELETE FROM sessions WHERE id IN (SELECT id FROM sessions ORDER BY last_seen_at ASC LIMIT ?)`, n-keep)
	}
}

// --- indexers ---

func (s *Store) ListIndexers() ([]Indexer, error) {
	rows, err := s.db.Query(`SELECT id,kind,name,url,api_key,enabled,priority FROM indexers ORDER BY priority, name`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	out := []Indexer{}
	for rows.Next() {
		var i Indexer
		if err := rows.Scan(&i.ID, &i.Kind, &i.Name, &i.URL, &i.APIKey, &i.Enabled, &i.Priority); err != nil {
			return nil, err
		}
		out = append(out, i)
	}
	return out, rows.Err()
}

func (s *Store) SaveIndexer(i Indexer) (Indexer, error) {
	if i.Kind == "" {
		i.Kind = "torznab"
	}
	if i.ID == "" {
		i.ID = randHex(5)
		_, err := s.db.Exec(`INSERT INTO indexers(id,kind,name,url,api_key,enabled,priority) VALUES(?,?,?,?,?,?,?)`,
			i.ID, i.Kind, i.Name, i.URL, i.APIKey, boolInt(i.Enabled), i.Priority)
		return i, err
	}
	_, err := s.db.Exec(`UPDATE indexers SET kind=?,name=?,url=?,api_key=?,enabled=?,priority=? WHERE id=?`,
		i.Kind, i.Name, i.URL, i.APIKey, boolInt(i.Enabled), i.Priority, i.ID)
	return i, err
}

// UpsertIndexerByName inserts or updates an indexer identified by kind+name,
// which is how BITLAB_INDEXERS is applied on every boot without duplicating rows.
func (s *Store) UpsertIndexerByName(ix Indexer) error {
	var id string
	err := s.db.QueryRow(`SELECT id FROM indexers WHERE kind=? AND name=?`, ix.Kind, ix.Name).Scan(&id)
	if err == nil {
		ix.ID = id
		_, err = s.db.Exec(`UPDATE indexers SET url=?,api_key=?,enabled=?,priority=? WHERE id=?`,
			ix.URL, ix.APIKey, boolInt(ix.Enabled), ix.Priority, id)
		return err
	}
	if !errors.Is(err, sql.ErrNoRows) {
		return err
	}
	_, err = s.db.Exec(`INSERT INTO indexers(id,kind,name,url,api_key,enabled,priority) VALUES(?,?,?,?,?,?,?)`,
		randHex(5), ix.Kind, ix.Name, ix.URL, ix.APIKey, boolInt(ix.Enabled), ix.Priority)
	return err
}

func (s *Store) DeleteIndexer(id string) error {
	_, err := s.db.Exec(`DELETE FROM indexers WHERE id=?`, id)
	return err
}

// --- settings ---

func (s *Store) GetSetting(k, def string) string {
	var v string
	if err := s.db.QueryRow(`SELECT v FROM settings WHERE k=?`, k).Scan(&v); err != nil {
		return def
	}
	return v
}

func (s *Store) SetSetting(k, v string) error {
	_, err := s.db.Exec(`INSERT INTO settings(k,v) VALUES(?,?) ON CONFLICT(k) DO UPDATE SET v=excluded.v`, k, v)
	return err
}

func (s *Store) DeleteSetting(k string) error {
	_, err := s.db.Exec(`DELETE FROM settings WHERE k=?`, k)
	return err
}

// --- caches ---

func (s *Store) CacheMeta(key, val string, ttl time.Duration) error {
	_, err := s.db.Exec(`INSERT INTO meta_cache(k,v,expires_at) VALUES(?,?,?)
		ON CONFLICT(k) DO UPDATE SET v=excluded.v, expires_at=excluded.expires_at`,
		key, val, time.Now().Add(ttl).Unix())
	return err
}

func (s *Store) GetCachedMeta(key string) (string, bool) {
	var v string
	var exp int64
	if err := s.db.QueryRow(`SELECT v,expires_at FROM meta_cache WHERE k=?`, key).Scan(&v, &exp); err != nil {
		return "", false
	}
	if exp < time.Now().Unix() {
		return "", false
	}
	return v, true
}

func (s *Store) SaveMedia(ih, name string, size int64, files []CachedFile) error {
	b, _ := json.Marshal(files)
	now := time.Now().Unix()
	_, err := s.db.Exec(`INSERT INTO media_cache(infohash,name,size,files,first_seen,last_used) VALUES(?,?,?,?,?,?)
		ON CONFLICT(infohash) DO UPDATE SET name=excluded.name, size=excluded.size, files=excluded.files, last_used=excluded.last_used`,
		ih, name, size, string(b), now, now)
	return err
}

func (s *Store) GetMedia(ih string) (string, int64, []CachedFile, bool) {
	var name string
	var size int64
	var files string
	if err := s.db.QueryRow(`SELECT name,size,files FROM media_cache WHERE infohash=?`, ih).Scan(&name, &size, &files); err != nil {
		return "", 0, nil, false
	}
	var cf []CachedFile
	json.Unmarshal([]byte(files), &cf)
	s.db.Exec(`UPDATE media_cache SET last_used=? WHERE infohash=?`, time.Now().Unix(), ih)
	return name, size, cf, true
}

func (s *Store) MediaCacheCount() int {
	var n int
	s.db.QueryRow(`SELECT count(*) FROM media_cache`).Scan(&n)
	return n
}

// MediaName is a quiet lookup: it returns the name media_cache remembers for
// an infohash without touching last_used, so the cache sweeper can tell a
// torrent whose metadata is still in flight from an abandoned directory.
func (s *Store) MediaName(ih string) (string, bool) {
	var name string
	if err := s.db.QueryRow(`SELECT name FROM media_cache WHERE infohash=?`, ih).Scan(&name); err != nil {
		return "", false
	}
	return name, true
}

// ForgetMedia drops a media_cache row, by infohash when it is known and
// otherwise by the name the file was stored under (with or without .part).
func (s *Store) ForgetMedia(ih, name string) {
	if ih != "" {
		s.db.Exec(`DELETE FROM media_cache WHERE infohash=?`, ih)
		return
	}
	name = strings.TrimSuffix(name, ".part")
	if name == "" {
		return
	}
	s.db.Exec(`DELETE FROM media_cache WHERE name=?`, name)
}

func boolInt(b bool) int {
	if b {
		return 1
	}
	return 0
}

// ---- load-time predictor ----

// ObserveCandidates records the releases a search just returned. first_seen and
// the probe counters survive an update: a release seen twice is still one
// candidate, and re-observing it must not reset its cooldown.
func (s *Store) ObserveCandidates(cs []Candidate) error {
	if len(cs) == 0 {
		return nil
	}
	tx, err := s.db.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()
	now := time.Now().Unix()
	for _, c := range cs {
		if c.InfoHash == "" {
			continue
		}
		if _, err := tx.Exec(`INSERT INTO probe_candidates
			(infohash,name,indexer,seeders,size,typ,season,first_seen,last_seen)
			VALUES(?,?,?,?,?,?,?,?,?)
			ON CONFLICT(infohash) DO UPDATE SET
			  name=excluded.name, indexer=excluded.indexer, seeders=excluded.seeders,
			  size=excluded.size, typ=excluded.typ, season=excluded.season,
			  last_seen=excluded.last_seen`,
			c.InfoHash, c.Title, c.Indexer, c.Seeders, c.Size, c.Typ, c.Season, now, now); err != nil {
			return err
		}
	}
	return tx.Commit()
}

// ProbePool draws candidates that are due for a measurement. Random order, not
// recency: the point of the pool is an unbiased sample of what the indexers
// actually return.
func (s *Store) ProbePool(staleBefore int64, limit int) ([]Candidate, error) {
	rows, err := s.db.Query(`SELECT infohash,name,indexer,seeders,size,typ,season,first_seen,last_probe,last_ms
		FROM probe_candidates
		WHERE last_probe < ? AND seeders > 0 AND size > 0 AND probes < 24
		ORDER BY RANDOM() LIMIT ?`, staleBefore, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return scanCandidates(rows)
}

func scanCandidates(rows *sql.Rows) ([]Candidate, error) {
	var out []Candidate
	for rows.Next() {
		var c Candidate
		if err := rows.Scan(&c.InfoHash, &c.Title, &c.Indexer, &c.Seeders, &c.Size,
			&c.Typ, &c.Season, &c.FirstSeen, &c.LastProbe, &c.LastMs); err != nil {
			return nil, err
		}
		out = append(out, c)
	}
	return out, rows.Err()
}

// RecordProbe stores one measurement and moves the candidate's cooldown. A
// failed probe counts: the swarm is what it is, and retrying it in a loop
// would just spend the budget on a dead hash.
func (s *Store) RecordProbe(p ProbeRow) error {
	_, err := s.db.Exec(`INSERT INTO probes
		(infohash,at,meta_ms,first_ms,total_ms,mbps,bytes,cached,cold,seeders_report,
		 seeders_seen,size,indexer,title,ok,err,feat)
		VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`,
		p.InfoHash, p.At, p.MetaMs, p.FirstMs, p.TotalMs, p.Mbps, p.Bytes,
		boolInt(p.Cached), boolInt(p.Cold), p.SeedersReport, p.SeedersSeen,
		p.Size, p.Indexer, p.Title, boolInt(p.OK), p.Err, p.Feat)
	if err != nil {
		return err
	}
	_, err = s.db.Exec(`UPDATE probe_candidates SET probes=probes+1, last_probe=?, last_ms=?
		WHERE infohash=?`, p.At, p.TotalMs, p.InfoHash)
	return err
}

// ProbeTraining returns recent measurements that carry a usable label. Only
// probes that actually reached the swarm are useful: a read served from local
// disk measures the volume, and training on it would teach the model that
// everything recently probed is instant.
func (s *Store) ProbeTraining(limit int) ([]ProbeRow, error) {
	rows, err := s.db.Query(`SELECT infohash,at,meta_ms,first_ms,total_ms,mbps,bytes,cached,
		cold,seeders_report,seeders_seen,size,indexer,title,ok,err,feat
		FROM probes WHERE cached=0 AND total_ms>0 AND feat<>''
		ORDER BY at DESC LIMIT ?`, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []ProbeRow
	for rows.Next() {
		var p ProbeRow
		var cached, cold, ok int
		if err := rows.Scan(&p.InfoHash, &p.At, &p.MetaMs, &p.FirstMs, &p.TotalMs, &p.Mbps,
			&p.Bytes, &cached, &cold, &p.SeedersReport, &p.SeedersSeen, &p.Size,
			&p.Indexer, &p.Title, &ok, &p.Err, &p.Feat); err != nil {
			return nil, err
		}
		p.Cached, p.Cold, p.OK = cached != 0, cold != 0, ok != 0
		out = append(out, p)
	}
	return out, rows.Err()
}

// ProbeWindow is the budget accounting: how many probes and how many bytes the
// last hour actually spent.
func (s *Store) ProbeWindow(since int64) (n int, bytes int64) {
	s.db.QueryRow(`SELECT count(*), COALESCE(SUM(bytes),0) FROM probes WHERE at >= ?`, since).Scan(&n, &bytes)
	return
}

// ProbeTotals is the console's view of the loop.
func (s *Store) ProbeTotals() (n, hashes, okN int) {
	s.db.QueryRow(`SELECT count(*), COUNT(DISTINCT infohash), COALESCE(SUM(ok),0) FROM probes`).Scan(&n, &hashes, &okN)
	return
}

// FactsFor is what the model knows about a hash before it measures anything:
// when the pool first saw it, and the result of the newest probe. It is one
// query because it runs once per stream row.
func (s *Store) FactsFor(ih string) (firstSeen, lastProbe, lastMs int64, ok bool) {
	err := s.db.QueryRow(`SELECT c.first_seen,
		COALESCE((SELECT at FROM probes p WHERE p.infohash=c.infohash ORDER BY at DESC LIMIT 1),0),
		COALESCE((SELECT total_ms FROM probes p WHERE p.infohash=c.infohash ORDER BY at DESC LIMIT 1),0)
		FROM probe_candidates c WHERE c.infohash=?`, ih).
		Scan(&firstSeen, &lastProbe, &lastMs)
	if err != nil {
		return 0, 0, 0, false
	}
	return firstSeen, lastProbe, lastMs, true
}

func (s *Store) CandidateCount() int {
	var n int
	s.db.QueryRow(`SELECT count(*) FROM probe_candidates`).Scan(&n)
	return n
}

// PruneProbes keeps the table bounded. The model only trains on recent rows
// anyway, so old measurements are dead weight in a 20 GiB volume.
func (s *Store) PruneProbes(keep int) {
	s.db.Exec(`DELETE FROM probes WHERE id NOT IN
		(SELECT id FROM probes ORDER BY id DESC LIMIT ?)`, keep)
}

func (s *Store) SaveModel(blob []byte, samples int, mae, within2x float64) error {
	_, err := s.db.Exec(`INSERT INTO model_weights(k,v,trained_at,samples,mae,within2x)
		VALUES('load',?,?,?,?,?)
		ON CONFLICT(k) DO UPDATE SET v=excluded.v, trained_at=excluded.trained_at,
		  samples=excluded.samples, mae=excluded.mae, within2x=excluded.within2x`,
		blob, time.Now().Unix(), samples, mae, within2x)
	return err
}

func (s *Store) LoadModel() ([]byte, int, float64, float64, int64, bool) {
	var blob []byte
	var samples int
	var mae, within2x float64
	var at int64
	err := s.db.QueryRow(`SELECT v,trained_at,samples,mae,within2x FROM model_weights WHERE k='load'`).
		Scan(&blob, &at, &samples, &mae, &within2x)
	if err != nil {
		return nil, 0, 0, 0, 0, false
	}
	return blob, samples, mae, within2x, at, true
}
