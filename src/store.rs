use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
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
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA busy_timeout=5000; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
        let s = Self { conn: Mutex::new(conn) };
        s.migrate()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(s)
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
        if k.expires_at > 0 && now_unix() > k.expires_at {
            let _ = self.delete_key(&k.id);
            return Err(KeyLookup::Expired);
        }
        if k.paused() {
            return Err(KeyLookup::Paused);
        }
        Ok(k)
    }

    pub fn set_key_status(&self, id: &str, status: &str) -> anyhow::Result<bool> {
        let n = self.conn.lock().unwrap().execute("UPDATE keys SET status=? WHERE id=?", params![status, id])?;
        Ok(n > 0)
    }

    pub fn delete_key(&self, id: &str) -> anyhow::Result<bool> {
        let n = self.conn.lock().unwrap().execute("DELETE FROM keys WHERE id=?", params![id])?;
        Ok(n > 0)
    }

    pub fn delete_expired_keys(&self) -> i64 {
        match self.conn.lock().unwrap().execute("DELETE FROM keys WHERE expires_at>0 AND expires_at<?", params![now_unix()]) {
            Ok(n) => n as i64,
            Err(_) => 0,
        }
    }

    pub fn touch_key(&self, id: &str) {
        let _ = self.conn.lock().unwrap().execute("UPDATE keys SET last_used_at=?, requests=requests+1 WHERE id=?", params![now_unix(), id]);
    }

    pub fn add_key_bytes(&self, id: &str, n: i64) {
        if n <= 0 {
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
            Ok(mut s) => {
                if s.expires_at < now_unix() {
                    let _ = self.delete_session(&s.id);
                    return None;
                }
                s.last_seen_at = s.last_seen_at; // keep
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

    pub fn save_media(&self, ih: &str, name: &str, size: i64, files: &[CachedFile]) -> anyhow::Result<()> {
        let b = serde_json::to_string(files).unwrap_or_else(|_| "[]".to_string());
        let now = now_unix();
        self.conn.lock().unwrap().execute(
            "INSERT INTO media_cache(infohash,name,size,files,first_seen,last_used) VALUES(?,?,?,?,?,?) ON CONFLICT(infohash) DO UPDATE SET name=excluded.name, size=excluded.size, files=excluded.files, last_used=excluded.last_used",
            params![ih, name, size, b, now, now],
        )?;
        Ok(())
    }

    pub fn media_cache_count(&self) -> i64 {
        self.conn.lock().unwrap().query_row("SELECT count(*) FROM media_cache", [], |r| r.get(0)).unwrap_or(0)
    }

    pub fn media_name(&self, ih: &str) -> Option<String> {
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT name FROM media_cache WHERE infohash=?", params![ih], |r| r.get(0))
            .ok()
    }

    pub fn forget_media(&self, ih: &str, name: &str) {
        if !ih.is_empty() {
            let _ = self.conn.lock().unwrap().execute("DELETE FROM media_cache WHERE infohash=?", params![ih]);
            return;
        }
        let name = name.trim_end_matches(".part");
        if name.is_empty() {
            return;
        }
        let _ = self.conn.lock().unwrap().execute("DELETE FROM media_cache WHERE name=?", params![name]);
    }
}
