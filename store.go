package main

import (
	"database/sql"
	"encoding/json"
	"errors"
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
	APIKey   string `json:"api_key"`
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
	db, err := sql.Open("sqlite", "file:"+path+"?_pragma=busy_timeout(5000)&_pragma=journal_mode(WAL)&_pragma=synchronous(FULL)")
	if err != nil {
		return nil, err
	}
	db.SetMaxOpenConns(1)
	s := &Store{db: db}
	if err := s.migrate(); err != nil {
		db.Close()
		return nil, err
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
	// Container probe results, stall memory and the last auto pick. A probe is
	// keyed by infohash alone: a torrent's track layout cannot change.
	if _, err := s.db.Exec(`
CREATE TABLE IF NOT EXISTS track_probes (
  infohash TEXT PRIMARY KEY,
  file_idx INTEGER NOT NULL DEFAULT -1,
  width INTEGER NOT NULL DEFAULT 0,
  height INTEGER NOT NULL DEFAULT 0,
  video INTEGER NOT NULL DEFAULT 0,
  audio INTEGER NOT NULL DEFAULT 0,
  subs INTEGER NOT NULL DEFAULT 0,
  langs TEXT NOT NULL DEFAULT '',
  codecs TEXT NOT NULL DEFAULT '',
  container TEXT NOT NULL DEFAULT '',
  mbps REAL NOT NULL DEFAULT 0,
  probed_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS stalls (
  infohash TEXT NOT NULL,
  media_id TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  PRIMARY KEY (infohash, media_id)
);
CREATE TABLE IF NOT EXISTS auto_picks (
  media_id TEXT PRIMARY KEY,
  infohash TEXT NOT NULL,
  label TEXT NOT NULL DEFAULT '',
  score REAL NOT NULL DEFAULT 0,
  picked_at INTEGER NOT NULL
);
`); err != nil {
		return err
	}
	// A saved pick remembers the bitrate it must sustain, so a probe that lands
	// after the decision can tell whether the pick is still viable.
	if !s.hasColumn("auto_picks", "need") {
		if _, err := s.db.Exec(`ALTER TABLE auto_picks ADD COLUMN need REAL NOT NULL DEFAULT 0`); err != nil {
			return err
		}
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

func (s *Store) TouchMedia(ih string) {
	s.db.Exec(`UPDATE media_cache SET last_used=? WHERE infohash=?`, time.Now().Unix(), ih)
}

func (s *Store) MediaCacheCount() int {
	var n int
	s.db.QueryRow(`SELECT count(*) FROM media_cache`).Scan(&n)
	return n
}

func boolInt(b bool) int {
	if b {
		return 1
	}
	return 0
}

// ---- probe cache, stall memory, auto history ----

// Probe is a cached container probe: the tracks a release really has, plus the
// throughput observed while reading its header.
type Probe struct {
	InfoHash string `json:"infohash"`
	FileIdx  int    `json:"file_idx"`
	TrackInfo
	ProbedAt int64 `json:"probed_at"`
}

func (s *Store) SaveProbe(ih string, fileIdx int, ti TrackInfo) {
	_, err := s.db.Exec(`INSERT INTO track_probes(infohash,file_idx,width,height,video,audio,subs,langs,codecs,container,mbps,probed_at)
		VALUES(?,?,?,?,?,?,?,?,?,?,?,?)
		ON CONFLICT(infohash) DO UPDATE SET file_idx=excluded.file_idx, width=excluded.width, height=excluded.height,
		video=excluded.video, audio=excluded.audio, subs=excluded.subs, langs=excluded.langs,
		codecs=excluded.codecs, container=excluded.container, mbps=excluded.mbps, probed_at=excluded.probed_at`,
		ih, fileIdx, ti.Width, ti.Height, ti.Video, ti.Audio, ti.Subs,
		strings.Join(ti.Langs, ","), strings.Join(ti.AudioCodecs, ","), ti.Container, ti.Mbps, time.Now().Unix())
	if err != nil {
		return
	}
}

// GetProbe returns a cached probe younger than a day. Track layout is stable,
// so the only thing that expires is how much we trust the throughput sample.
func (s *Store) GetProbe(ih string) (Probe, bool) {
	var p Probe
	var langs, codecs string
	err := s.db.QueryRow(`SELECT infohash,file_idx,width,height,video,audio,subs,langs,codecs,container,mbps,probed_at
		FROM track_probes WHERE infohash=? AND probed_at > ?`, ih, time.Now().Add(-24*time.Hour).Unix()).
		Scan(&p.InfoHash, &p.FileIdx, &p.Width, &p.Height, &p.Video, &p.Audio, &p.Subs,
			&langs, &codecs, &p.Container, &p.Mbps, &p.ProbedAt)
	if err != nil {
		return Probe{}, false
	}
	p.Langs = splitNonEmpty(langs)
	p.AudioCodecs = splitNonEmpty(codecs)
	return p, true
}

func splitNonEmpty(s string) []string {
	if s == "" {
		return nil
	}
	return strings.Split(s, ",")
}

// RecordStall marks a release as having failed to stream for this title, so the
// next auto attempt picks something else instead of retrying it.
func (s *Store) RecordStall(ih, mediaID string) {
	_, err := s.db.Exec(`INSERT INTO stalls(infohash,media_id,created_at) VALUES(?,?,?)
		ON CONFLICT(infohash,media_id) DO UPDATE SET created_at=excluded.created_at`,
		ih, mediaID, time.Now().Unix())
	if err != nil {
		return
	}
}

// StalledHashes is the set of releases demoted for this title within window.
func (s *Store) StalledHashes(mediaID string, window time.Duration) map[string]bool {
	out := map[string]bool{}
	cutoff := time.Now().Add(-window).Unix()
	rows, err := s.db.Query(`SELECT infohash FROM stalls WHERE media_id=? AND created_at > ?`, mediaID, cutoff)
	if err != nil {
		return out
	}
	defer rows.Close()
	for rows.Next() {
		var ih string
		if rows.Scan(&ih) == nil {
			out[ih] = true
		}
	}
	s.db.Exec(`DELETE FROM stalls WHERE created_at < ?`, time.Now().Add(-7*24*time.Hour).Unix())
	return out
}

func (s *Store) SaveAutoPick(mediaID, ih, label string, score, need float64) {
	s.db.Exec(`INSERT INTO auto_picks(media_id,infohash,label,score,need,picked_at) VALUES(?,?,?,?,?,?)
		ON CONFLICT(media_id) DO UPDATE SET infohash=excluded.infohash, label=excluded.label,
		score=excluded.score, need=excluded.need, picked_at=excluded.picked_at`,
		mediaID, ih, label, score, need, time.Now().Unix())
}

// GetAutoPickFull also returns the bitrate the pick was chosen to sustain.
func (s *Store) GetAutoPickFull(mediaID string) (ih, label string, at int64, need float64, ok bool) {
	err := s.db.QueryRow(`SELECT infohash,label,picked_at,need FROM auto_picks WHERE media_id=?`, mediaID).
		Scan(&ih, &label, &at, &need)
	return ih, label, at, need, err == nil
}

func (s *Store) GetAutoPick(mediaID string) (ih, label string, at int64, ok bool) {
	err := s.db.QueryRow(`SELECT infohash,label,picked_at FROM auto_picks WHERE media_id=?`, mediaID).
		Scan(&ih, &label, &at)
	return ih, label, at, err == nil
}

// PicksForHash returns the media ids whose auto pick is this release, with the
// bitrate each must sustain. A probe that lands after the decision uses this to
// notice that the pick cannot stream.
func (s *Store) PicksForHash(ih string) map[string]float64 {
	out := map[string]float64{}
	rows, err := s.db.Query(`SELECT media_id,need,picked_at FROM auto_picks WHERE infohash=?`, ih)
	if err != nil {
		return out
	}
	defer rows.Close()
	for rows.Next() {
		var media string
		var need float64
		var at int64
		if rows.Scan(&media, &need, &at) == nil && time.Since(time.Unix(at, 0)) < autoPickTTL {
			out[media] = need
		}
	}
	return out
}
