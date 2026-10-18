package main

import (
	"database/sql"
	"encoding/json"
	"errors"
	"time"

	_ "modernc.org/sqlite"
)

var (
	ErrKeyNotFound = errors.New("manifest key not recognised")
	ErrKeyRevoked  = errors.New("manifest key revoked")
	ErrKeyExpired  = errors.New("manifest key expired")
	ErrNoKeys      = errors.New("no manifest keys exist")
)

// Key is a revocable per-user manifest credential.
type Key struct {
	ID        string `json:"id"`
	Label     string `json:"label"`
	Note      string `json:"note"`
	Token     string `json:"token"`
	CreatedAt int64  `json:"created_at"`
	LastUsed  int64  `json:"last_used_at"`
	Requests  int64  `json:"requests"`
	Bytes     int64  `json:"bytes"`
	Revoked   bool   `json:"revoked"`
	ExpiresAt int64  `json:"expires_at"`
}

// Indexer is one configured search source.
type Indexer struct {
	ID       string `json:"id"`
	Kind     string `json:"kind"` // torznab | apibay
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
	db, err := sql.Open("sqlite", "file:"+path+"?_pragma=busy_timeout(5000)&_pragma=journal_mode(WAL)&_pragma=synchronous(NORMAL)")
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
  revoked INTEGER NOT NULL DEFAULT 0,
  expires_at INTEGER NOT NULL DEFAULT 0
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
	return err
}

func (s *Store) CreateKey(label, note string, ttl time.Duration) (Key, error) {
	if label == "" {
		label = "unnamed"
	}
	id := randHex(6)
	token := "mln_" + randToken(20)
	var exp int64
	if ttl > 0 {
		exp = time.Now().Add(ttl).Unix()
	}
	now := time.Now().Unix()
	_, err := s.db.Exec(`INSERT INTO keys(id,token,label,note,created_at,expires_at) VALUES(?,?,?,?,?,?)`,
		id, token, label, note, now, exp)
	if err != nil {
		return Key{}, err
	}
	return Key{ID: id, Token: token, Label: label, Note: note, CreatedAt: now, ExpiresAt: exp}, nil
}

func (s *Store) ListKeys() ([]Key, error) {
	rows, err := s.db.Query(`SELECT id,token,label,note,created_at,last_used_at,requests,bytes,revoked,expires_at
		FROM keys ORDER BY created_at DESC`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	out := []Key{}
	for rows.Next() {
		var k Key
		if err := rows.Scan(&k.ID, &k.Token, &k.Label, &k.Note, &k.CreatedAt, &k.LastUsed,
			&k.Requests, &k.Bytes, &k.Revoked, &k.ExpiresAt); err != nil {
			return nil, err
		}
		out = append(out, k)
	}
	return out, rows.Err()
}

func (s *Store) KeyByToken(token string) (Key, error) {
	var k Key
	err := s.db.QueryRow(`SELECT id,token,label,note,created_at,last_used_at,requests,bytes,revoked,expires_at
		FROM keys WHERE token=?`, token).Scan(&k.ID, &k.Token, &k.Label, &k.Note, &k.CreatedAt,
		&k.LastUsed, &k.Requests, &k.Bytes, &k.Revoked, &k.ExpiresAt)
	if errors.Is(err, sql.ErrNoRows) {
		return Key{}, ErrKeyNotFound
	}
	if err != nil {
		return Key{}, err
	}
	if k.Revoked {
		return k, ErrKeyRevoked
	}
	if k.ExpiresAt > 0 && time.Now().Unix() > k.ExpiresAt {
		return k, ErrKeyExpired
	}
	return k, nil
}

func (s *Store) SetKeyRevoked(id string, revoked bool) error {
	res, err := s.db.Exec(`UPDATE keys SET revoked=? WHERE id=?`, boolInt(revoked), id)
	if err != nil {
		return err
	}
	n, _ := res.RowsAffected()
	if n == 0 {
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
	s.db.QueryRow(`SELECT count(*) FROM keys WHERE revoked=0 AND (expires_at=0 OR expires_at>?)`,
		time.Now().Unix()).Scan(&n)
	return n
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
