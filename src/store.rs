use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const KEY_CACHE_TTL: Duration = Duration::from_secs(15);
const WATCH_SESSION_WINDOW_SECS: i64 = 7200;

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct WatchRecordInput {
    pub key_id: String,
    pub imdb_id: String,
    pub media_type: String,
    pub title: String,
    pub year: i32,
    pub season: i32,
    pub episode: i32,
    pub infohash: String,
    pub release_name: String,
    pub quality: String,
}

enum WriteJob {
    TouchKey { id: String, now: i64 },
    AddKeyBytes { id: String, n: i64 },
    TouchMedia { ih: String, now: i64 },
    SaveMedia { ih: String, name: String, size: i64, files_json: String, now: i64 },
    RecordWatchStart { input: WatchRecordInput, now: i64 },
    AddWatchBytes { key_id: String, infohash: String, season: i32, episode: i32, bytes: i64, now: i64 },
    Flush(mpsc::SyncSender<()>),
    Shutdown,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Key {
    pub id: String,
    pub label: String,
    pub note: String,
    pub token: String,
    pub status: String,
    pub created_at: i64,
    pub last_used_at: i64,
    pub requests: i64,
    pub bytes: i64,
    pub expires_at: i64,
}

impl Key {
    pub fn paused(&self) -> bool {
        self.status == "paused"
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub name: String,
    pub ip: String,
    pub ua: String,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub expires_at: i64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CachedFile {
    pub idx: usize,
    pub path: String,
    pub size: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyLookup {
    NotFound,
    Paused,
    Expired,
}

pub struct Store {
    conn: Mutex<Connection>,
    key_cache: Mutex<HashMap<String, (Key, Instant)>>,
    writer_tx: Option<mpsc::Sender<WriteJob>>,
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA busy_timeout=5000; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        let writer_tx = if path.to_str() != Some(":memory:") {
            let mut wconn = Connection::open(path)?;
            wconn.execute_batch("PRAGMA busy_timeout=5000; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
            let (tx, rx) = mpsc::channel();
            std::thread::Builder::new()
                .name("store-writer".into())
                .spawn(move || store_writer_loop(&mut wconn, rx))?;
            Some(tx)
        } else {
            None
        };
        let s = Self {
            conn: Mutex::new(conn),
            key_cache: Mutex::new(HashMap::new()),
            writer_tx,
        };
        s.migrate()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(s)
    }

    pub fn flush_writes(&self) {
        if let Some(tx) = &self.writer_tx {
            let (done_tx, done_rx) = mpsc::sync_channel(1);
            if tx.send(WriteJob::Flush(done_tx)).is_ok() {
                let _ = done_rx.recv_timeout(Duration::from_secs(2));
            }
        }
    }

    fn migrate(&self) -> anyhow::Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute_batch(
            r#"
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
CREATE TABLE IF NOT EXISTS watch_history (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  key_id TEXT NOT NULL,
  imdb_id TEXT NOT NULL DEFAULT '',
  media_type TEXT NOT NULL DEFAULT 'movie',
  title TEXT NOT NULL DEFAULT '',
  year INTEGER NOT NULL DEFAULT 0,
  season INTEGER NOT NULL DEFAULT 0,
  episode INTEGER NOT NULL DEFAULT 0,
  infohash TEXT NOT NULL,
  release_name TEXT NOT NULL DEFAULT '',
  quality TEXT NOT NULL DEFAULT '',
  bytes_served INTEGER NOT NULL DEFAULT 0,
  started_at INTEGER NOT NULL,
  last_watched_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_watch_key_last ON watch_history(key_id, last_watched_at DESC);
CREATE INDEX IF NOT EXISTS idx_watch_last ON watch_history(last_watched_at DESC);
CREATE INDEX IF NOT EXISTS idx_watch_key_ih_ep ON watch_history(key_id, infohash, season, episode, last_watched_at DESC);
"#,
        )?;
        // status column
        let has_status: bool = c
            .prepare("PRAGMA table_info(keys)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(|r| r.ok())
            .any(|n| n == "status");
        if !has_status {
            c.execute("ALTER TABLE keys ADD COLUMN status TEXT NOT NULL DEFAULT 'active'", [])?;
        }
        // drop legacy tables if present
        for t in ["auto_picks", "auto_sessions", "stalls", "track_probes", "probe_candidates", "probes", "model_weights"] {
            let _ = c.execute(&format!("DROP TABLE IF EXISTS {t}"), []);
        }
        // delete revoked legacy rows (column may not exist; ignore errors)
        let _ = c.execute("DELETE FROM keys WHERE revoked=1", []);
        drop(c);
        Ok(())
    }

    fn scan_key_row(row: &rusqlite::Row) -> rusqlite::Result<Key> {
        Ok(Key {
            id: row.get(0)?,
            token: row.get(1)?,
            label: row.get(2)?,
            note: row.get(3)?,
            status: row.get(4)?,
            created_at: row.get(5)?,
            last_used_at: row.get(6)?,
            requests: row.get(7)?,
            bytes: row.get(8)?,
            expires_at: row.get(9)?,
        })
    }

    pub fn create_key(&self, label: &str, note: &str, ttl_secs: i64) -> anyhow::Result<Key> {
        let label = if label.trim().is_empty() { "unnamed".to_string() } else { label.trim().to_string() };
        let id = crate::util::rand_hex(6);
        let token = format!("bit_{}", crate::util::rand_token(20));
        let now = now_unix();
        let exp = if ttl_secs > 0 { now + ttl_secs } else { 0 };
        self.conn.lock().unwrap().execute(
            "INSERT INTO keys(id,token,label,note,status,created_at,expires_at) VALUES(?,?,?,?,?,?,?)",
            params![id, token, label, note, "active", now, exp],
        )?;
        Ok(Key { id, token, label, note: note.to_string(), status: "active".to_string(), created_at: now, last_used_at: 0, requests: 0, bytes: 0, expires_at: exp })
    }

    pub fn list_keys(&self) -> anyhow::Result<Vec<Key>> {
        self.flush_writes();
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare("SELECT id,token,label,note,status,created_at,last_used_at,requests,bytes,expires_at FROM keys ORDER BY created_at DESC")?;
        let rows = stmt.query_map([], Self::scan_key_row)?;
        let mut out = vec![];
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn list_keys_page(&self, q: &str, status: &str, limit: i64, offset: i64) -> anyhow::Result<(Vec<Key>, i64)> {
        self.flush_writes();
        let mut conds: Vec<String> = vec![];
        let mut args: Vec<String> = vec![];
        let q = q.trim();
        if !q.is_empty() {
            conds.push("(label LIKE ? ESCAPE '\\' OR note LIKE ? ESCAPE '\\' OR id LIKE ? ESCAPE '\\')".to_string());
            let esc = q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            let like = format!("%{esc}%");
            args.push(like.clone());
            args.push(like.clone());
            args.push(like);
        }
        if status == "active" || status == "paused" {
            conds.push("status = ?".to_string());
            args.push(status.to_string());
        }
        let where_clause = if conds.is_empty() { String::new() } else { format!(" WHERE {}", conds.join(" AND ")) };
        let c = self.conn.lock().unwrap();
        let total: i64 = {
            let mut stmt = c.prepare(&format!("SELECT count(*) FROM keys{where_clause}"))?;
            let p: Vec<&dyn rusqlite::ToSql> = args.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            stmt.query_row(p.as_slice(), |r| r.get(0))?
        };
        let mut stmt = c.prepare(&format!("SELECT id,token,label,note,status,created_at,last_used_at,requests,bytes,expires_at FROM keys{where_clause} ORDER BY created_at DESC LIMIT ? OFFSET ?"))?;
        let mut p: Vec<Box<dyn rusqlite::ToSql>> = vec![];
        for a in &args {
            p.push(Box::new(a.clone()));
        }
        p.push(Box::new(limit));
        p.push(Box::new(offset));
        let pref: Vec<&dyn rusqlite::ToSql> = p.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(pref.as_slice(), Self::scan_key_row)?;
        let mut out = vec![];
        for r in rows {
            out.push(r?);
        }
        Ok((out, total))
    }

    pub fn key_by_token(&self, token: &str) -> Result<Key, KeyLookup> {
        let now = now_unix();
        if let Some((cached, fetched_at)) = self.key_cache.lock().unwrap().get(token).cloned() {
            if fetched_at.elapsed() < KEY_CACHE_TTL {
                if cached.expires_at > 0 && now > cached.expires_at {
                    let _ = self.delete_key(&cached.id);
                    return Err(KeyLookup::Expired);
                }
                if cached.paused() {
                    return Err(KeyLookup::Paused);
                }
                return Ok(cached);
            }
        }

        let row: Option<Key> = self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT id,token,label,note,status,created_at,last_used_at,requests,bytes,expires_at FROM keys WHERE token=?",
                params![token],
                Self::scan_key_row,
            )
            .optional()
            .map_err(|_| KeyLookup::NotFound)?;
        let k = row.ok_or(KeyLookup::NotFound)?;
        if k.expires_at > 0 && now > k.expires_at {
            let _ = self.delete_key(&k.id);
            return Err(KeyLookup::Expired);
        }
        self.key_cache
            .lock()
            .unwrap()
            .insert(token.to_string(), (k.clone(), Instant::now()));
        if k.paused() {
            return Err(KeyLookup::Paused);
        }
        Ok(k)
    }

    pub fn set_key_status(&self, id: &str, status: &str) -> anyhow::Result<bool> {
        let n = self.conn.lock().unwrap().execute("UPDATE keys SET status=? WHERE id=?", params![status, id])?;
        self.key_cache.lock().unwrap().clear();
        Ok(n > 0)
    }

    pub fn delete_key(&self, id: &str) -> anyhow::Result<bool> {
        self.flush_writes();
        let c = self.conn.lock().unwrap();
        let n = c.execute("DELETE FROM keys WHERE id=?", params![id])?;
        if n > 0 {
            let _ = c.execute("DELETE FROM watch_history WHERE key_id=?", params![id]);
        }
        self.key_cache.lock().unwrap().clear();
        Ok(n > 0)
    }

    pub fn reset_all_stats(&self) -> anyhow::Result<()> {
        self.flush_writes();
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM watch_history", [])?;
        c.execute("UPDATE keys SET requests=0, bytes=0, last_used_at=0", [])?;
        self.key_cache.lock().unwrap().clear();
        Ok(())
    }

    pub fn reset_key_stats(&self, id: &str) -> anyhow::Result<bool> {
        self.flush_writes();
        let c = self.conn.lock().unwrap();
        let n = c.execute("UPDATE keys SET requests=0, bytes=0, last_used_at=0 WHERE id=?", params![id])?;
        if n == 0 {
            return Ok(false);
        }
        c.execute("DELETE FROM watch_history WHERE key_id=?", params![id])?;
        self.key_cache.lock().unwrap().clear();
        Ok(true)
    }

    pub fn delete_expired_keys(&self) -> i64 {
        let c = self.conn.lock().unwrap();
        match c.execute("DELETE FROM keys WHERE expires_at>0 AND expires_at<?", params![now_unix()]) {
            Ok(n) => {
                if n > 0 {
                    let _ = c.execute("DELETE FROM watch_history WHERE key_id NOT IN (SELECT id FROM keys)", []);
                    self.key_cache.lock().unwrap().clear();
                }
                n as i64
            }
            Err(_) => 0,
        }
    }

    pub fn record_watch_start(&self, input: WatchRecordInput) {
        let now = now_unix();
        if let Some(tx) = &self.writer_tx {
            let _ = tx.send(WriteJob::RecordWatchStart { input, now });
            return;
        }
        let c = self.conn.lock().unwrap();
        record_watch_start_conn(&c, &input, now);
    }

    pub fn add_watch_bytes(&self, key_id: &str, infohash: &str, season: i32, episode: i32, bytes: i64) {
        if bytes <= 0 {
            return;
        }
        let now = now_unix();
        if let Some(tx) = &self.writer_tx {
            let _ = tx.send(WriteJob::AddWatchBytes {
                key_id: key_id.to_string(),
                infohash: infohash.to_string(),
                season,
                episode,
                bytes,
                now,
            });
            return;
        }
        let c = self.conn.lock().unwrap();
        add_watch_bytes_conn(&c, key_id, infohash, season, episode, bytes, now);
    }

    pub fn touch_key(&self, id: &str) {
        let now = now_unix();
        if let Some(tx) = &self.writer_tx {
            let _ = tx.send(WriteJob::TouchKey { id: id.to_string(), now });
            return;
        }
        let _ = self.conn.lock().unwrap().execute("UPDATE keys SET last_used_at=?, requests=requests+1 WHERE id=?", params![now, id]);
    }

    pub fn add_key_bytes(&self, id: &str, n: i64) {
        if n <= 0 {
            return;
        }
        if let Some(tx) = &self.writer_tx {
            let _ = tx.send(WriteJob::AddKeyBytes { id: id.to_string(), n });
            return;
        }
        let _ = self.conn.lock().unwrap().execute("UPDATE keys SET bytes=bytes+? WHERE id=?", params![n, id]);
    }

    pub fn active_key_count(&self) -> i64 {
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM keys WHERE status='active' AND (expires_at=0 OR expires_at>?)", params![now_unix()], |r| r.get(0))
            .unwrap_or(0)
    }

    // sessions
    pub fn create_session(&self, name: &str, ip: &str, ua: &str, ttl_secs: i64) -> anyhow::Result<SessionRecord> {
        let now = now_unix();
        let sess = SessionRecord {
            id: crate::util::rand_hex(16),
            name: name.to_string(),
            ip: ip.to_string(),
            ua: ua.to_string(),
            created_at: now,
            last_seen_at: now,
            expires_at: now + ttl_secs,
        };
        {
            let c = self.conn.lock().unwrap();
            c.execute(
                "INSERT INTO sessions(id,name,ip,ua,created_at,last_seen_at,expires_at) VALUES(?,?,?,?,?,?,?)",
                params![sess.id, sess.name, sess.ip, sess.ua, sess.created_at, sess.last_seen_at, sess.expires_at],
            )?;
            let _ = c.execute("DELETE FROM sessions WHERE expires_at<?", params![now]);
        }
        Ok(sess)
    }

    pub fn get_session(&self, id: &str) -> Option<SessionRecord> {
        let row: rusqlite::Result<SessionRecord> = self.conn.lock().unwrap().query_row(
            "SELECT id,name,ip,ua,created_at,last_seen_at,expires_at FROM sessions WHERE id=?",
            params![id],
            |r| {
                Ok(SessionRecord {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    ip: r.get(2)?,
                    ua: r.get(3)?,
                    created_at: r.get(4)?,
                    last_seen_at: r.get(5)?,
                    expires_at: r.get(6)?,
                })
            },
        );
        match row {
            Ok(s) => {
                if s.expires_at < now_unix() {
                    let _ = self.delete_session(&s.id);
                    return None;
                }
                Some(s)
            }
            Err(_) => None,
        }
    }

    pub fn touch_session(&self, id: &str) {
        let cutoff = now_unix() - 60;
        let _ = self.conn.lock().unwrap().execute("UPDATE sessions SET last_seen_at=? WHERE id=? AND last_seen_at<?", params![now_unix(), id, cutoff]);
    }

    pub fn delete_session(&self, id: &str) -> anyhow::Result<()> {
        self.conn.lock().unwrap().execute("DELETE FROM sessions WHERE id=?", params![id])?;
        Ok(())
    }

    pub fn delete_other_sessions(&self, keep: &str) {
        let _ = self.conn.lock().unwrap().execute("DELETE FROM sessions WHERE id<>?", params![keep]);
    }

    pub fn list_sessions(&self) -> anyhow::Result<Vec<SessionRecord>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare("SELECT id,name,ip,ua,created_at,last_seen_at,expires_at FROM sessions ORDER BY last_seen_at DESC")?;
        let rows = stmt.query_map([], |r| {
            Ok(SessionRecord {
                id: r.get(0)?,
                name: r.get(1)?,
                ip: r.get(2)?,
                ua: r.get(3)?,
                created_at: r.get(4)?,
                last_seen_at: r.get(5)?,
                expires_at: r.get(6)?,
            })
        })?;
        let mut out = vec![];
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn prune_sessions(&self, keep: usize) {
        let c = self.conn.lock().unwrap();
        let _ = c.execute("DELETE FROM sessions WHERE expires_at<?", params![now_unix()]);
        let n: i64 = c.query_row("SELECT count(*) FROM sessions", [], |r| r.get(0)).unwrap_or(0);
        if n > keep as i64 {
            let drop_n = n - keep as i64;
            let _ = c.execute("DELETE FROM sessions WHERE id IN (SELECT id FROM sessions ORDER BY last_seen_at ASC LIMIT ?)", params![drop_n]);
        }
    }

    // settings
    pub fn get_setting(&self, k: &str, def: &str) -> String {
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT v FROM settings WHERE k=?", params![k], |r| r.get::<_, String>(0))
            .unwrap_or_else(|_| def.to_string())
    }

    pub fn set_setting(&self, k: &str, v: &str) -> anyhow::Result<()> {
        self.conn.lock().unwrap().execute("INSERT INTO settings(k,v) VALUES(?,?) ON CONFLICT(k) DO UPDATE SET v=excluded.v", params![k, v])?;
        Ok(())
    }

    pub fn delete_setting(&self, k: &str) -> anyhow::Result<()> {
        self.conn.lock().unwrap().execute("DELETE FROM settings WHERE k=?", params![k])?;
        Ok(())
    }

    // caches
    pub fn cache_meta(&self, k: &str, v: &str, ttl_secs: i64) -> anyhow::Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO meta_cache(k,v,expires_at) VALUES(?,?,?) ON CONFLICT(k) DO UPDATE SET v=excluded.v, expires_at=excluded.expires_at",
            params![k, v, now_unix() + ttl_secs],
        )?;
        Ok(())
    }

    pub fn get_cached_meta(&self, k: &str) -> Option<String> {
        let (v, exp): (String, i64) = self
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT v,expires_at FROM meta_cache WHERE k=?", params![k], |r| Ok((r.get(0)?, r.get(1)?)))
            .ok()?;
        if exp < now_unix() {
            return None;
        }
        Some(v)
    }

    pub fn delete_expired_meta(&self) -> i64 {
        match self.conn.lock().unwrap().execute("DELETE FROM meta_cache WHERE expires_at<?", params![now_unix()]) {
            Ok(n) => n as i64,
            Err(_) => 0,
        }
    }

    pub fn save_media(&self, ih: &str, name: &str, size: i64, files: &[CachedFile]) -> anyhow::Result<()> {
        let b = serde_json::to_string(files).unwrap_or_else(|_| "[]".to_string());
        let now = now_unix();
        if let Some(tx) = &self.writer_tx {
            let _ = tx.send(WriteJob::SaveMedia {
                ih: ih.to_string(),
                name: name.to_string(),
                size,
                files_json: b,
                now,
            });
            return Ok(());
        }
        self.conn.lock().unwrap().execute(
            "INSERT INTO media_cache(infohash,name,size,files,first_seen,last_used) VALUES(?,?,?,?,?,?) ON CONFLICT(infohash) DO UPDATE SET name=excluded.name, size=excluded.size, files=excluded.files, last_used=excluded.last_used",
            params![ih, name, size, b, now, now],
        )?;
        Ok(())
    }

    pub fn media_name(&self, ih: &str) -> Option<String> {
        self.flush_writes();
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT name FROM media_cache WHERE infohash=?", params![ih], |r| r.get(0))
            .ok()
    }

    pub fn media_size(&self, ih: &str) -> Option<i64> {
        if ih.is_empty() {
            return None;
        }
        self.flush_writes();
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT size FROM media_cache WHERE infohash=?", params![ih], |r| r.get(0))
            .ok()
    }

    pub fn touch_media(&self, ih: &str) {
        if ih.is_empty() {
            return;
        }
        let now = now_unix();
        if let Some(tx) = &self.writer_tx {
            let _ = tx.send(WriteJob::TouchMedia { ih: ih.to_string(), now });
            return;
        }
        let _ = self.conn.lock().unwrap().execute(
            "UPDATE media_cache SET last_used=? WHERE infohash=?",
            params![now, ih],
        );
    }

    pub fn media_last_used(&self, ih: &str) -> Option<i64> {
        if ih.is_empty() {
            return None;
        }
        self.flush_writes();
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT last_used FROM media_cache WHERE infohash=?", params![ih], |r| r.get(0))
            .ok()
    }

    pub fn forget_media(&self, ih: &str, name: &str) {
        self.flush_writes();
        if !ih.is_empty() {
            let _ = self.conn.lock().unwrap().execute("DELETE FROM media_cache WHERE infohash=?", params![ih]);
        }
        let name = name.trim_end_matches(".part");
        if !name.is_empty() {
            let _ = self.conn.lock().unwrap().execute("DELETE FROM media_cache WHERE name=?", params![name]);
        }
    }

    pub fn global_stats(&self, range: &str, recent_page: i64, recent_per: i64) -> anyhow::Result<GlobalStats> {
        self.flush_writes();
        let now = now_unix();
        let (norm_range, since, bucket_secs, bucket_count) = parse_stats_range(range, now);
        let recent_page = recent_page.max(1);
        let recent_per = recent_per.clamp(1, 100);
        let c = self.conn.lock().unwrap();

        let (total_plays, total_watch_bytes, unique_titles, active_viewers): (i64, i64, i64, i64) = c.query_row(
            "SELECT count(*), COALESCE(SUM(bytes_served), 0), count(DISTINCT CASE WHEN imdb_id<>'' THEN imdb_id ELSE title END), count(DISTINCT key_id) FROM watch_history WHERE last_watched_at>=?",
            params![since],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;

        let (total_keys, active_keys, total_key_bytes, total_requests): (i64, i64, i64, i64) = c.query_row(
            "SELECT count(*), COALESCE(SUM(CASE WHEN status='active' AND (expires_at=0 OR expires_at>?) THEN 1 ELSE 0 END), 0), COALESCE(SUM(bytes), 0), COALESCE(SUM(requests), 0) FROM keys",
            params![now],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;

        let type_breakdown = query_type_breakdown(&c, None, since)?;
        let quality_breakdown = query_quality_breakdown(&c, None, since)?;
        let timeline = query_timeline(&c, None, since, now, bucket_secs, bucket_count)?;
        let top_titles = query_top_titles(&c, None, since)?;

        let key_leaderboard = {
            let mut stmt = c.prepare(
                "SELECT k.id, k.label, k.note, k.status, k.expires_at, k.created_at, k.last_used_at, k.requests, k.bytes,
                        COALESCE(w.plays, 0), COALESCE(w.wbytes, 0), COALESCE(w.utitles, 0),
                        COALESCE(w.mcount, 0), COALESCE(w.scount, 0), COALESCE(w.last_w, 0)
                 FROM keys k
                 LEFT JOIN (
                   SELECT key_id,
                          count(*) AS plays,
                          COALESCE(SUM(bytes_served), 0) AS wbytes,
                          count(DISTINCT CASE WHEN imdb_id<>'' THEN imdb_id ELSE title END) AS utitles,
                          count(DISTINCT CASE WHEN media_type='movie' THEN (CASE WHEN imdb_id<>'' THEN imdb_id ELSE title END) END) AS mcount,
                          count(DISTINCT CASE WHEN media_type='series' THEN (CASE WHEN imdb_id<>'' THEN imdb_id ELSE title END) END) AS scount,
                          MAX(last_watched_at) AS last_w
                   FROM watch_history
                   WHERE last_watched_at>=?
                   GROUP BY key_id
                 ) w ON w.key_id = k.id
                 ORDER BY COALESCE(w.plays, 0) DESC, COALESCE(w.wbytes, 0) DESC, k.bytes DESC, k.created_at DESC",
            )?;
            let rows = stmt.query_map(params![since], |r| {
                Ok(KeyStatSummary {
                    key_id: r.get(0)?,
                    label: r.get(1)?,
                    note: r.get(2)?,
                    status: r.get(3)?,
                    expires_at: r.get(4)?,
                    created_at: r.get(5)?,
                    last_used_at: r.get(6)?,
                    requests: r.get(7)?,
                    key_bytes: r.get(8)?,
                    plays: r.get(9)?,
                    watch_bytes: r.get(10)?,
                    unique_titles: r.get(11)?,
                    movies_count: r.get(12)?,
                    series_count: r.get(13)?,
                    last_watched_at: r.get(14)?,
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            out
        };

        let recent_offset = (recent_page - 1).saturating_mul(recent_per);
        let recent_total = count_watch_events(&c, None, since, "", "")?;
        let recent_watches = query_watch_events(&c, None, since, "", "", recent_per, recent_offset)?;

        Ok(GlobalStats {
            range: norm_range,
            since,
            total_plays,
            total_watch_bytes,
            total_key_bytes,
            total_requests,
            unique_titles,
            active_viewers,
            active_keys,
            total_keys,
            type_breakdown,
            quality_breakdown,
            timeline,
            top_titles,
            key_leaderboard,
            recent_watches,
            recent_total,
            recent_page,
            recent_per,
        })
    }

    pub fn key_stats(
        &self,
        key_id: &str,
        range: &str,
        media_type: &str,
        q: &str,
        page: i64,
        per: i64,
    ) -> anyhow::Result<Option<KeyStats>> {
        self.flush_writes();
        let now = now_unix();
        let (norm_range, since, bucket_secs, bucket_count) = parse_stats_range(range, now);
        let page = page.max(1);
        let per = per.clamp(1, 100);
        let c = self.conn.lock().unwrap();
        let key = match c
            .query_row(
                "SELECT id,token,label,note,status,created_at,last_used_at,requests,bytes,expires_at FROM keys WHERE id=?",
                params![key_id],
                Self::scan_key_row,
            )
            .optional()?
        {
            Some(k) => k,
            None => return Ok(None),
        };

        let (total_plays, total_watch_bytes, unique_titles, movies_count, series_count, last_watched_at): (
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
        ) = c.query_row(
            "SELECT count(*),
                    COALESCE(SUM(bytes_served), 0),
                    count(DISTINCT CASE WHEN imdb_id<>'' THEN imdb_id ELSE title END),
                    count(DISTINCT CASE WHEN media_type='movie' THEN (CASE WHEN imdb_id<>'' THEN imdb_id ELSE title END) END),
                    count(DISTINCT CASE WHEN media_type='series' THEN (CASE WHEN imdb_id<>'' THEN imdb_id ELSE title END) END),
                    COALESCE(MAX(last_watched_at), 0)
             FROM watch_history
             WHERE key_id=? AND last_watched_at>=?",
            params![key_id, since],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )?;

        let type_breakdown = query_type_breakdown(&c, Some(key_id), since)?;
        let quality_breakdown = query_quality_breakdown(&c, Some(key_id), since)?;
        let timeline = query_timeline(&c, Some(key_id), since, now, bucket_secs, bucket_count)?;
        let top_titles = query_top_titles(&c, Some(key_id), since)?;

        let offset = (page - 1).saturating_mul(per);
        let history_total = count_watch_events(&c, Some(key_id), since, media_type, q)?;
        let history = query_watch_events(&c, Some(key_id), since, media_type, q, per, offset)?;

        Ok(Some(KeyStats {
            key,
            range: norm_range,
            since,
            total_plays,
            total_watch_bytes,
            unique_titles,
            movies_count,
            series_count,
            last_watched_at,
            type_breakdown,
            quality_breakdown,
            timeline,
            top_titles,
            history,
            history_total,
            history_page: page,
            history_per: per,
        }))
    }

    pub fn media_stats(
        &self,
        media_type: &str,
        imdb_id: &str,
        title: &str,
        infohash: &str,
        range: &str,
        page: i64,
        per: i64,
    ) -> anyhow::Result<Option<MediaStats>> {
        self.flush_writes();
        let now = now_unix();
        let (norm_range, since, bucket_secs, bucket_count) = parse_stats_range(range, now);
        let page = page.max(1);
        let per = per.clamp(1, 100);
        let Some((base_where, base_args)) = build_media_filter(media_type, imdb_id, title, infohash, None) else {
            return Ok(None);
        };

        let c = self.conn.lock().unwrap();
        let meta_sql = format!(
            "SELECT MAX(CASE WHEN w.title<>'' THEN w.title ELSE w.release_name END),
                    MAX(w.imdb_id),
                    MAX(w.media_type),
                    MAX(w.year),
                    count(*)
             FROM watch_history w{base_where}"
        );
        let base_params: Vec<&dyn rusqlite::ToSql> = base_args.iter().map(|b| b.as_ref()).collect();
        let (resolved_title, resolved_imdb, resolved_type, resolved_year, all_count): (String, String, String, i32, i64) =
            c.query_row(&meta_sql, base_params.as_slice(), |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    r.get::<_, Option<i32>>(3)?.unwrap_or(0),
                    r.get(4)?,
                ))
            })?;
        if all_count == 0 {
            return Ok(None);
        }

        let (where_since, args_since) = build_media_filter(media_type, imdb_id, title, infohash, Some(since)).unwrap();
        let params_since: Vec<&dyn rusqlite::ToSql> = args_since.iter().map(|b| b.as_ref()).collect();

        let sum_sql = format!(
            "SELECT count(*),
                    COALESCE(SUM(w.bytes_served), 0),
                    count(DISTINCT w.key_id),
                    count(DISTINCT CASE WHEN w.media_type='series' AND w.season>0 AND w.episode>0 THEN (w.season * 10000 + w.episode) END),
                    COALESCE(MIN(w.started_at), 0),
                    COALESCE(MAX(w.last_watched_at), 0)
             FROM watch_history w{where_since}"
        );
        let (total_plays, total_watch_bytes, unique_viewers, episodes_watched, first_watched_at, last_watched_at): (
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
        ) = c.query_row(&sum_sql, params_since.as_slice(), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?;

        let timeline = query_media_timeline(&c, media_type, imdb_id, title, infohash, since, now, bucket_secs, bucket_count)?;
        let quality_breakdown = query_media_quality_breakdown(&c, &where_since, &args_since)?;
        let viewer_breakdown = query_media_viewers(&c, &where_since, &args_since)?;
        let episode_breakdown = if resolved_type == "series" {
            query_media_episodes(&c, &where_since, &args_since)?
        } else {
            Vec::new()
        };

        let offset = (page - 1).saturating_mul(per);
        let history_total: i64 = c.query_row(
            &format!("SELECT count(*) FROM watch_history w{where_since}"),
            params_since.as_slice(),
            |r| r.get(0),
        )?;
        let history = query_media_watch_events(&c, &where_since, &args_since, per, offset)?;

        Ok(Some(MediaStats {
            title: resolved_title,
            imdb_id: resolved_imdb,
            media_type: resolved_type,
            year: resolved_year,
            range: norm_range,
            since,
            total_plays,
            total_watch_bytes,
            unique_viewers,
            episodes_watched,
            first_watched_at,
            last_watched_at,
            quality_breakdown,
            timeline,
            viewer_breakdown,
            episode_breakdown,
            history,
            history_total,
            history_page: page,
            history_per: per,
        }))
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WatchEvent {
    pub id: i64,
    pub key_id: String,
    pub key_label: String,
    pub imdb_id: String,
    pub media_type: String,
    pub title: String,
    pub year: i32,
    pub season: i32,
    pub episode: i32,
    pub infohash: String,
    pub release_name: String,
    pub quality: String,
    pub bytes_served: i64,
    pub started_at: i64,
    pub last_watched_at: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TitleStat {
    pub title: String,
    pub imdb_id: String,
    pub media_type: String,
    pub year: i32,
    pub plays: i64,
    pub unique_viewers: i64,
    pub episodes_watched: i64,
    pub bytes_served: i64,
    pub last_watched_at: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct KeyStatSummary {
    pub key_id: String,
    pub label: String,
    pub note: String,
    pub status: String,
    pub expires_at: i64,
    pub created_at: i64,
    pub last_used_at: i64,
    pub requests: i64,
    pub key_bytes: i64,
    pub plays: i64,
    pub watch_bytes: i64,
    pub unique_titles: i64,
    pub movies_count: i64,
    pub series_count: i64,
    pub last_watched_at: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BucketStat {
    pub bucket_ts: i64,
    pub plays: i64,
    pub bytes_served: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct QualityStat {
    pub quality: String,
    pub plays: i64,
    pub bytes_served: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TypeStat {
    pub media_type: String,
    pub plays: i64,
    pub unique_titles: i64,
    pub bytes_served: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GlobalStats {
    pub range: String,
    pub since: i64,
    pub total_plays: i64,
    pub total_watch_bytes: i64,
    pub total_key_bytes: i64,
    pub total_requests: i64,
    pub unique_titles: i64,
    pub active_viewers: i64,
    pub active_keys: i64,
    pub total_keys: i64,
    pub type_breakdown: Vec<TypeStat>,
    pub quality_breakdown: Vec<QualityStat>,
    pub timeline: Vec<BucketStat>,
    pub top_titles: Vec<TitleStat>,
    pub key_leaderboard: Vec<KeyStatSummary>,
    pub recent_watches: Vec<WatchEvent>,
    pub recent_total: i64,
    pub recent_page: i64,
    pub recent_per: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct KeyStats {
    pub key: Key,
    pub range: String,
    pub since: i64,
    pub total_plays: i64,
    pub total_watch_bytes: i64,
    pub unique_titles: i64,
    pub movies_count: i64,
    pub series_count: i64,
    pub last_watched_at: i64,
    pub type_breakdown: Vec<TypeStat>,
    pub quality_breakdown: Vec<QualityStat>,
    pub timeline: Vec<BucketStat>,
    pub top_titles: Vec<TitleStat>,
    pub history: Vec<WatchEvent>,
    pub history_total: i64,
    pub history_page: i64,
    pub history_per: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MediaViewerStat {
    pub key_id: String,
    pub key_label: String,
    pub plays: i64,
    pub episodes_watched: i64,
    pub bytes_served: i64,
    pub last_watched_at: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MediaEpisodeStat {
    pub season: i32,
    pub episode: i32,
    pub plays: i64,
    pub unique_viewers: i64,
    pub bytes_served: i64,
    pub last_watched_at: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MediaStats {
    pub title: String,
    pub imdb_id: String,
    pub media_type: String,
    pub year: i32,
    pub range: String,
    pub since: i64,
    pub total_plays: i64,
    pub total_watch_bytes: i64,
    pub unique_viewers: i64,
    pub episodes_watched: i64,
    pub first_watched_at: i64,
    pub last_watched_at: i64,
    pub quality_breakdown: Vec<QualityStat>,
    pub timeline: Vec<BucketStat>,
    pub viewer_breakdown: Vec<MediaViewerStat>,
    pub episode_breakdown: Vec<MediaEpisodeStat>,
    pub history: Vec<WatchEvent>,
    pub history_total: i64,
    pub history_page: i64,
    pub history_per: i64,
}

fn parse_stats_range(range: &str, now: i64) -> (String, i64, i64, usize) {
    match range {
        "24h" => ("24h".to_string(), now - 24 * 3600, 3600, 24),
        "7d" => ("7d".to_string(), now - 7 * 86400, 86400, 7),
        "all" => ("all".to_string(), 0, 86400, 30),
        _ => ("30d".to_string(), now - 30 * 86400, 86400, 30),
    }
}

fn query_type_breakdown(c: &Connection, key_id: Option<&str>, since: i64) -> anyhow::Result<Vec<TypeStat>> {
    let sql = if key_id.is_some() {
        "SELECT media_type, count(*), count(DISTINCT CASE WHEN imdb_id<>'' THEN imdb_id ELSE title END), COALESCE(SUM(bytes_served), 0) FROM watch_history WHERE key_id=? AND last_watched_at>=? GROUP BY media_type ORDER BY count(*) DESC"
    } else {
        "SELECT media_type, count(*), count(DISTINCT CASE WHEN imdb_id<>'' THEN imdb_id ELSE title END), COALESCE(SUM(bytes_served), 0) FROM watch_history WHERE last_watched_at>=? GROUP BY media_type ORDER BY count(*) DESC"
    };
    let mut stmt = c.prepare(sql)?;
    let map_row = |r: &rusqlite::Row| -> rusqlite::Result<TypeStat> {
        Ok(TypeStat {
            media_type: r.get(0)?,
            plays: r.get(1)?,
            unique_titles: r.get(2)?,
            bytes_served: r.get(3)?,
        })
    };
    let rows = if let Some(kid) = key_id {
        stmt.query_map(params![kid, since], map_row)?
    } else {
        stmt.query_map(params![since], map_row)?
    };
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn query_quality_breakdown(c: &Connection, key_id: Option<&str>, since: i64) -> anyhow::Result<Vec<QualityStat>> {
    let sql = if key_id.is_some() {
        "SELECT CASE WHEN quality='' THEN 'Unknown' ELSE quality END AS q, count(*), COALESCE(SUM(bytes_served), 0) FROM watch_history WHERE key_id=? AND last_watched_at>=? GROUP BY q ORDER BY count(*) DESC"
    } else {
        "SELECT CASE WHEN quality='' THEN 'Unknown' ELSE quality END AS q, count(*), COALESCE(SUM(bytes_served), 0) FROM watch_history WHERE last_watched_at>=? GROUP BY q ORDER BY count(*) DESC"
    };
    let mut stmt = c.prepare(sql)?;
    let map_row = |r: &rusqlite::Row| -> rusqlite::Result<QualityStat> {
        Ok(QualityStat {
            quality: r.get(0)?,
            plays: r.get(1)?,
            bytes_served: r.get(2)?,
        })
    };
    let rows = if let Some(kid) = key_id {
        stmt.query_map(params![kid, since], map_row)?
    } else {
        stmt.query_map(params![since], map_row)?
    };
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn query_timeline(
    c: &Connection,
    key_id: Option<&str>,
    since: i64,
    now: i64,
    bucket_secs: i64,
    bucket_count: usize,
) -> anyhow::Result<Vec<BucketStat>> {
    let end_bucket = (now / bucket_secs) * bucket_secs;
    let start_bucket = end_bucket - (bucket_count as i64 - 1) * bucket_secs;
    let query_since = since.max(start_bucket);
    let sql = if key_id.is_some() {
        "SELECT (last_watched_at / ?) * ? AS b, count(*), COALESCE(SUM(bytes_served), 0) FROM watch_history WHERE key_id=? AND last_watched_at>=? GROUP BY b ORDER BY b ASC"
    } else {
        "SELECT (last_watched_at / ?) * ? AS b, count(*), COALESCE(SUM(bytes_served), 0) FROM watch_history WHERE last_watched_at>=? GROUP BY b ORDER BY b ASC"
    };
    let mut stmt = c.prepare(sql)?;
    let mut map: HashMap<i64, (i64, i64)> = HashMap::new();
    let map_row = |r: &rusqlite::Row| -> rusqlite::Result<(i64, i64, i64)> {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    };
    let rows = if let Some(kid) = key_id {
        stmt.query_map(params![bucket_secs, bucket_secs, kid, query_since], map_row)?
    } else {
        stmt.query_map(params![bucket_secs, bucket_secs, query_since], map_row)?
    };
    for r in rows {
        let (b, plays, bytes) = r?;
        map.insert(b, (plays, bytes));
    }
    let mut out = Vec::with_capacity(bucket_count);
    for i in 0..bucket_count {
        let b = start_bucket + (i as i64) * bucket_secs;
        let (plays, bytes_served) = map.get(&b).copied().unwrap_or((0, 0));
        out.push(BucketStat { bucket_ts: b, plays, bytes_served });
    }
    Ok(out)
}

fn query_top_titles(c: &Connection, key_id: Option<&str>, since: i64) -> anyhow::Result<Vec<TitleStat>> {
    let sql = if key_id.is_some() {
        "SELECT MAX(CASE WHEN title<>'' THEN title ELSE release_name END),
                MAX(imdb_id),
                media_type,
                MAX(year),
                count(*) AS plays,
                count(DISTINCT key_id) AS viewers,
                count(DISTINCT CASE WHEN media_type='series' AND season>0 AND episode>0 THEN (season * 10000 + episode) END) AS eps,
                COALESCE(SUM(bytes_served), 0) AS b,
                MAX(last_watched_at) AS lw
         FROM watch_history
         WHERE key_id=? AND last_watched_at>=?
         GROUP BY CASE WHEN imdb_id<>'' THEN imdb_id ELSE (CASE WHEN title<>'' THEN title ELSE infohash END) END, media_type
         ORDER BY plays DESC, b DESC, lw DESC"
    } else {
        "SELECT MAX(CASE WHEN title<>'' THEN title ELSE release_name END),
                MAX(imdb_id),
                media_type,
                MAX(year),
                count(*) AS plays,
                count(DISTINCT key_id) AS viewers,
                count(DISTINCT CASE WHEN media_type='series' AND season>0 AND episode>0 THEN (season * 10000 + episode) END) AS eps,
                COALESCE(SUM(bytes_served), 0) AS b,
                MAX(last_watched_at) AS lw
         FROM watch_history
         WHERE last_watched_at>=?
         GROUP BY CASE WHEN imdb_id<>'' THEN imdb_id ELSE (CASE WHEN title<>'' THEN title ELSE infohash END) END, media_type
         ORDER BY plays DESC, b DESC, lw DESC"
    };
    let mut stmt = c.prepare(sql)?;
    let map_row = |r: &rusqlite::Row| -> rusqlite::Result<TitleStat> {
        Ok(TitleStat {
            title: r.get(0)?,
            imdb_id: r.get(1)?,
            media_type: r.get(2)?,
            year: r.get(3)?,
            plays: r.get(4)?,
            unique_viewers: r.get(5)?,
            episodes_watched: r.get(6)?,
            bytes_served: r.get(7)?,
            last_watched_at: r.get(8)?,
        })
    };
    let rows = if let Some(kid) = key_id {
        stmt.query_map(params![kid, since], map_row)?
    } else {
        stmt.query_map(params![since], map_row)?
    };
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn build_watch_filter(
    key_id: Option<&str>,
    since: i64,
    media_type: &str,
    q: &str,
) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let mut conds = vec!["w.last_watched_at >= ?".to_string()];
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(since)];
    if let Some(kid) = key_id {
        conds.push("w.key_id = ?".to_string());
        args.push(Box::new(kid.to_string()));
    }
    if media_type == "movie" || media_type == "series" {
        conds.push("w.media_type = ?".to_string());
        args.push(Box::new(media_type.to_string()));
    }
    let q = q.trim();
    if !q.is_empty() {
        let esc = q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let like = format!("%{esc}%");
        conds.push("(w.title LIKE ? ESCAPE '\\' OR w.release_name LIKE ? ESCAPE '\\' OR w.imdb_id LIKE ? ESCAPE '\\')".to_string());
        args.push(Box::new(like.clone()));
        args.push(Box::new(like.clone()));
        args.push(Box::new(like));
    }
    (format!(" WHERE {}", conds.join(" AND ")), args)
}

fn count_watch_events(
    c: &Connection,
    key_id: Option<&str>,
    since: i64,
    media_type: &str,
    q: &str,
) -> anyhow::Result<i64> {
    let (where_clause, args) = build_watch_filter(key_id, since, media_type, q);
    let sql = format!("SELECT count(*) FROM watch_history w{where_clause}");
    let mut stmt = c.prepare(&sql)?;
    let p: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let total: i64 = stmt.query_row(p.as_slice(), |r| r.get(0))?;
    Ok(total)
}

fn query_watch_events(
    c: &Connection,
    key_id: Option<&str>,
    since: i64,
    media_type: &str,
    q: &str,
    limit: i64,
    offset: i64,
) -> anyhow::Result<Vec<WatchEvent>> {
    let (where_clause, mut args) = build_watch_filter(key_id, since, media_type, q);
    args.push(Box::new(limit));
    args.push(Box::new(offset));
    let sql = format!(
        "SELECT w.id, w.key_id, COALESCE(k.label, w.key_id), w.imdb_id, w.media_type, w.title, w.year, w.season, w.episode, w.infohash, w.release_name, w.quality, w.bytes_served, w.started_at, w.last_watched_at
         FROM watch_history w
         LEFT JOIN keys k ON k.id = w.key_id
         {where_clause}
         ORDER BY w.last_watched_at DESC, w.id DESC
         LIMIT ? OFFSET ?"
    );
    let mut stmt = c.prepare(&sql)?;
    let p: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(p.as_slice(), |r| {
        Ok(WatchEvent {
            id: r.get(0)?,
            key_id: r.get(1)?,
            key_label: r.get(2)?,
            imdb_id: r.get(3)?,
            media_type: r.get(4)?,
            title: r.get(5)?,
            year: r.get(6)?,
            season: r.get(7)?,
            episode: r.get(8)?,
            infohash: r.get(9)?,
            release_name: r.get(10)?,
            quality: r.get(11)?,
            bytes_served: r.get(12)?,
            started_at: r.get(13)?,
            last_watched_at: r.get(14)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn build_media_filter(
    media_type: &str,
    imdb_id: &str,
    title: &str,
    infohash: &str,
    since: Option<i64>,
) -> Option<(String, Vec<Box<dyn rusqlite::ToSql>>)> {
    let mut conds: Vec<String> = Vec::new();
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let imdb = imdb_id.trim();
    let title = title.trim();
    let ih = infohash.trim();
    if !imdb.is_empty() {
        conds.push("w.imdb_id = ?".to_string());
        args.push(Box::new(imdb.to_string()));
    } else if !title.is_empty() {
        conds.push("(w.title = ? OR (w.title='' AND w.release_name = ?))".to_string());
        args.push(Box::new(title.to_string()));
        args.push(Box::new(title.to_string()));
    } else if !ih.is_empty() {
        conds.push("w.infohash = ?".to_string());
        args.push(Box::new(ih.to_string()));
    } else {
        return None;
    }
    if media_type == "movie" || media_type == "series" {
        conds.push("w.media_type = ?".to_string());
        args.push(Box::new(media_type.to_string()));
    }
    if let Some(s) = since {
        conds.push("w.last_watched_at >= ?".to_string());
        args.push(Box::new(s));
    }
    Some((format!(" WHERE {}", conds.join(" AND ")), args))
}

fn query_media_timeline(
    c: &Connection,
    media_type: &str,
    imdb_id: &str,
    title: &str,
    infohash: &str,
    since: i64,
    now: i64,
    bucket_secs: i64,
    bucket_count: usize,
) -> anyhow::Result<Vec<BucketStat>> {
    let end_bucket = (now / bucket_secs) * bucket_secs;
    let start_bucket = end_bucket - (bucket_count as i64 - 1) * bucket_secs;
    let query_since = since.max(start_bucket);
    let (where_clause, media_args) = build_media_filter(media_type, imdb_id, title, infohash, Some(query_since)).unwrap();
    let sql = format!(
        "SELECT (w.last_watched_at / ?) * ? AS b, count(*), COALESCE(SUM(w.bytes_served), 0)
         FROM watch_history w{where_clause}
         GROUP BY b ORDER BY b ASC"
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(bucket_secs), Box::new(bucket_secs)];
    args.extend(media_args);
    let p: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = c.prepare(&sql)?;
    let mut map: HashMap<i64, (i64, i64)> = HashMap::new();
    let rows = stmt.query_map(p.as_slice(), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?;
    for r in rows {
        let (b, plays, bytes) = r?;
        map.insert(b, (plays, bytes));
    }
    let mut out = Vec::with_capacity(bucket_count);
    for i in 0..bucket_count {
        let b = start_bucket + (i as i64) * bucket_secs;
        let (plays, bytes_served) = map.get(&b).copied().unwrap_or((0, 0));
        out.push(BucketStat { bucket_ts: b, plays, bytes_served });
    }
    Ok(out)
}

fn query_media_quality_breakdown(
    c: &Connection,
    where_clause: &str,
    args: &[Box<dyn rusqlite::ToSql>],
) -> anyhow::Result<Vec<QualityStat>> {
    let sql = format!(
        "SELECT CASE WHEN w.quality='' THEN 'Unknown' ELSE w.quality END AS q, count(*), COALESCE(SUM(w.bytes_served), 0)
         FROM watch_history w{where_clause}
         GROUP BY q ORDER BY count(*) DESC"
    );
    let mut stmt = c.prepare(&sql)?;
    let p: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(p.as_slice(), |r| {
        Ok(QualityStat {
            quality: r.get(0)?,
            plays: r.get(1)?,
            bytes_served: r.get(2)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn query_media_viewers(
    c: &Connection,
    where_clause: &str,
    args: &[Box<dyn rusqlite::ToSql>],
) -> anyhow::Result<Vec<MediaViewerStat>> {
    let sql = format!(
        "SELECT w.key_id,
                COALESCE(k.label, w.key_id),
                count(*) AS plays,
                count(DISTINCT CASE WHEN w.media_type='series' AND w.season>0 AND w.episode>0 THEN (w.season * 10000 + w.episode) END) AS eps,
                COALESCE(SUM(w.bytes_served), 0) AS b,
                MAX(w.last_watched_at) AS lw
         FROM watch_history w
         LEFT JOIN keys k ON k.id = w.key_id
         {where_clause}
         GROUP BY w.key_id
         ORDER BY plays DESC, b DESC, lw DESC"
    );
    let mut stmt = c.prepare(&sql)?;
    let p: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(p.as_slice(), |r| {
        Ok(MediaViewerStat {
            key_id: r.get(0)?,
            key_label: r.get(1)?,
            plays: r.get(2)?,
            episodes_watched: r.get(3)?,
            bytes_served: r.get(4)?,
            last_watched_at: r.get(5)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn query_media_episodes(
    c: &Connection,
    where_clause: &str,
    args: &[Box<dyn rusqlite::ToSql>],
) -> anyhow::Result<Vec<MediaEpisodeStat>> {
    let sql = format!(
        "SELECT w.season,
                w.episode,
                count(*) AS plays,
                count(DISTINCT w.key_id) AS viewers,
                COALESCE(SUM(w.bytes_served), 0) AS b,
                MAX(w.last_watched_at) AS lw
         FROM watch_history w
         {where_clause} AND w.season > 0 AND w.episode > 0
         GROUP BY w.season, w.episode
         ORDER BY w.season ASC, w.episode ASC"
    );
    let mut stmt = c.prepare(&sql)?;
    let p: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(p.as_slice(), |r| {
        Ok(MediaEpisodeStat {
            season: r.get(0)?,
            episode: r.get(1)?,
            plays: r.get(2)?,
            unique_viewers: r.get(3)?,
            bytes_served: r.get(4)?,
            last_watched_at: r.get(5)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn query_media_watch_events(
    c: &Connection,
    where_clause: &str,
    args: &[Box<dyn rusqlite::ToSql>],
    limit: i64,
    offset: i64,
) -> anyhow::Result<Vec<WatchEvent>> {
    let sql = format!(
        "SELECT w.id, w.key_id, COALESCE(k.label, w.key_id), w.imdb_id, w.media_type, w.title, w.year, w.season, w.episode, w.infohash, w.release_name, w.quality, w.bytes_served, w.started_at, w.last_watched_at
         FROM watch_history w
         LEFT JOIN keys k ON k.id = w.key_id
         {where_clause}
         ORDER BY w.last_watched_at DESC, w.id DESC
         LIMIT ? OFFSET ?"
    );
    let mut stmt = c.prepare(&sql)?;
    let mut p: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    p.push(&limit);
    p.push(&offset);
    let rows = stmt.query_map(p.as_slice(), |r| {
        Ok(WatchEvent {
            id: r.get(0)?,
            key_id: r.get(1)?,
            key_label: r.get(2)?,
            imdb_id: r.get(3)?,
            media_type: r.get(4)?,
            title: r.get(5)?,
            year: r.get(6)?,
            season: r.get(7)?,
            episode: r.get(8)?,
            infohash: r.get(9)?,
            release_name: r.get(10)?,
            quality: r.get(11)?,
            bytes_served: r.get(12)?,
            started_at: r.get(13)?,
            last_watched_at: r.get(14)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn record_watch_start_conn(c: &Connection, input: &WatchRecordInput, now: i64) {
    let cutoff = now - WATCH_SESSION_WINDOW_SECS;
    let existing: Option<i64> = c
        .query_row(
            "SELECT id FROM watch_history WHERE key_id=? AND infohash=? AND season=? AND episode=? AND last_watched_at>=? ORDER BY last_watched_at DESC LIMIT 1",
            params![input.key_id, input.infohash, input.season, input.episode, cutoff],
            |r| r.get(0),
        )
        .optional()
        .unwrap_or(None);
    if let Some(row_id) = existing {
        let _ = c.execute(
            "UPDATE watch_history
             SET last_watched_at = MAX(last_watched_at, ?),
                 imdb_id = CASE WHEN imdb_id='' THEN ? ELSE imdb_id END,
                 title = CASE WHEN title='' THEN ? ELSE title END,
                 year = CASE WHEN year=0 THEN ? ELSE year END,
                 quality = CASE WHEN quality='' THEN ? ELSE quality END,
                 release_name = CASE WHEN release_name='' THEN ? ELSE release_name END
             WHERE id = ?",
            params![now, input.imdb_id, input.title, input.year, input.quality, input.release_name, row_id],
        );
    } else {
        let _ = c.execute(
            "INSERT INTO watch_history(key_id,imdb_id,media_type,title,year,season,episode,infohash,release_name,quality,bytes_served,started_at,last_watched_at) VALUES(?,?,?,?,?,?,?,?,?,?,0,?,?)",
            params![
                input.key_id,
                input.imdb_id,
                input.media_type,
                input.title,
                input.year,
                input.season,
                input.episode,
                input.infohash,
                input.release_name,
                input.quality,
                now,
                now
            ],
        );
    }
}

fn add_watch_bytes_conn(c: &Connection, key_id: &str, infohash: &str, season: i32, episode: i32, bytes: i64, now: i64) {
    let cutoff = now - WATCH_SESSION_WINDOW_SECS;
    let existing: Option<i64> = c
        .query_row(
            "SELECT id FROM watch_history WHERE key_id=? AND infohash=? AND season=? AND episode=? AND last_watched_at>=? ORDER BY last_watched_at DESC LIMIT 1",
            params![key_id, infohash, season, episode, cutoff],
            |r| r.get(0),
        )
        .optional()
        .unwrap_or(None);
    if let Some(row_id) = existing {
        let _ = c.execute(
            "UPDATE watch_history SET bytes_served=bytes_served+?, last_watched_at=MAX(last_watched_at, ?) WHERE id=?",
            params![bytes, now, row_id],
        );
    } else {
        let _ = c.execute(
            "INSERT INTO watch_history(key_id,imdb_id,media_type,title,year,season,episode,infohash,release_name,quality,bytes_served,started_at,last_watched_at) VALUES(?,'',?,'',0,?,?,?,'','',?,?,?)",
            params![
                key_id,
                if season > 0 && episode > 0 { "series" } else { "movie" },
                season,
                episode,
                infohash,
                bytes,
                now,
                now
            ],
        );
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        if let Some(tx) = self.writer_tx.take() {
            let (done_tx, done_rx) = mpsc::sync_channel(1);
            if tx.send(WriteJob::Flush(done_tx)).is_ok() {
                let _ = done_rx.recv_timeout(Duration::from_secs(2));
            }
            let _ = tx.send(WriteJob::Shutdown);
        }
    }
}

fn store_writer_loop(conn: &mut Connection, rx: mpsc::Receiver<WriteJob>) {
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while let Ok(more) = rx.try_recv() {
            let is_ctrl = matches!(more, WriteJob::Flush(_) | WriteJob::Shutdown);
            batch.push(more);
            if is_ctrl || batch.len() >= 64 {
                break;
            }
        }

        let mut shut = false;
        let mut acks = Vec::new();
        if let Ok(tx) = conn.unchecked_transaction() {
            for job in batch {
                match job {
                    WriteJob::TouchKey { id, now } => {
                        let _ = tx.execute(
                            "UPDATE keys SET last_used_at=?, requests=requests+1 WHERE id=?",
                            params![now, id],
                        );
                    }
                    WriteJob::AddKeyBytes { id, n } => {
                        let _ = tx.execute("UPDATE keys SET bytes=bytes+? WHERE id=?", params![n, id]);
                    }
                    WriteJob::TouchMedia { ih, now } => {
                        let _ = tx.execute(
                            "UPDATE media_cache SET last_used=? WHERE infohash=?",
                            params![now, ih],
                        );
                    }
                    WriteJob::SaveMedia { ih, name, size, files_json, now } => {
                        let _ = tx.execute(
                            "INSERT INTO media_cache(infohash,name,size,files,first_seen,last_used) VALUES(?,?,?,?,?,?) ON CONFLICT(infohash) DO UPDATE SET name=excluded.name, size=excluded.size, files=excluded.files, last_used=excluded.last_used",
                            params![ih, name, size, files_json, now, now],
                        );
                    }
                    WriteJob::RecordWatchStart { input, now } => {
                        record_watch_start_conn(&tx, &input, now);
                    }
                    WriteJob::AddWatchBytes { key_id, infohash, season, episode, bytes, now } => {
                        add_watch_bytes_conn(&tx, &key_id, &infohash, season, episode, bytes, now);
                    }
                    WriteJob::Flush(done) => acks.push(done),
                    WriteJob::Shutdown => shut = true,
                }
            }
            let _ = tx.commit();
        }
        for ack in acks {
            let _ = ack.send(());
        }
        if shut {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_media_stats_movie_and_series_breakdown() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let k1 = store.create_key("Alice", "", 0).unwrap();
        let k2 = store.create_key("Bob", "", 0).unwrap();

        store.record_watch_start(WatchRecordInput {
            key_id: k1.id.clone(),
            imdb_id: "tt0903747".into(),
            media_type: "series".into(),
            title: "Breaking Bad".into(),
            year: 2008,
            season: 1,
            episode: 1,
            infohash: "ih_bb_101".into(),
            release_name: "Breaking.Bad.S01E01.1080p".into(),
            quality: "1080p".into(),
        });
        store.add_watch_bytes(&k1.id, "ih_bb_101", 1, 1, 500_000);

        store.record_watch_start(WatchRecordInput {
            key_id: k2.id.clone(),
            imdb_id: "tt0903747".into(),
            media_type: "series".into(),
            title: "Breaking Bad".into(),
            year: 2008,
            season: 1,
            episode: 2,
            infohash: "ih_bb_102".into(),
            release_name: "Breaking.Bad.S01E02.2160p".into(),
            quality: "2160p".into(),
        });
        store.add_watch_bytes(&k2.id, "ih_bb_102", 1, 2, 1_500_000);

        let ms = store
            .media_stats("series", "tt0903747", "", "", "30d", 1, 25)
            .unwrap()
            .unwrap();
        assert_eq!(ms.title, "Breaking Bad");
        assert_eq!(ms.total_plays, 2);
        assert_eq!(ms.unique_viewers, 2);
        assert_eq!(ms.episodes_watched, 2);
        assert_eq!(ms.total_watch_bytes, 2_000_000);
        assert_eq!(ms.viewer_breakdown.len(), 2);
        assert_eq!(ms.episode_breakdown.len(), 2);
        assert_eq!(ms.episode_breakdown[0].season, 1);
        assert_eq!(ms.episode_breakdown[0].episode, 1);
        assert_eq!(ms.episode_breakdown[1].episode, 2);
        assert_eq!(ms.quality_breakdown.len(), 2);

        assert!(store.media_stats("movie", "tt9999999", "", "", "30d", 1, 25).unwrap().is_none());
    }
}
