use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
struct Activity {
    readers: i32,
    adding: bool,
    last_access: SystemTime,
    label: String,
    size: i64,
    rate: f64,
    prev_bytes: u64,
    prev_at: SystemTime,
    warned_no_peers: bool,
}

impl Default for Activity {
    fn default() -> Self {
        let now = SystemTime::now();
        Self { readers: 0, adding: false, last_access: now, label: String::new(), size: 0, rate: 0.0, prev_bytes: 0, prev_at: now, warned_no_peers: false }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TorrentView {
    pub infohash: String,
    pub name: String,
    pub size: i64,
    pub completed: i64,
    pub progress: f64,
    pub peers: usize,
    pub seeders: usize,
    pub rate: f64,
    pub readers: i32,
    pub idle_seconds: i64,
}

pub struct Engine {
    session: std::sync::Arc<torrent::session::Session>,
    data_dir: PathBuf,
    media_dir: PathBuf,
    trackers: Vec<String>,
    store: Arc<crate::store::Store>,
    max_cache_bytes: Arc<std::sync::atomic::AtomicI64>,
    reserve_bytes: i64,
    idle_timeout: Arc<std::sync::atomic::AtomicI64>, // nanos
    grace: Duration,
    retain: Duration,
    max_torrents: usize,
    act: Arc<RwLock<HashMap<String, Activity>>>,
    // per-infohash add locks to serialize ensures
    add_locks: Arc<tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    warm_sem: Arc<tokio::sync::Semaphore>,
    stats: Arc<RwLock<crate::cache::CacheStats>>,
    evicted_bytes: Arc<std::sync::atomic::AtomicI64>,
    evicted_count: Arc<std::sync::atomic::AtomicI64>,
    closed: Arc<std::sync::atomic::AtomicBool>,
    has_wg: bool,
}

fn hex_to_infohash(ih: &str) -> Option<[u8; 20]> {
    if ih.len() != 40 {
        return None;
    }
    let v = hex::decode(ih).ok()?;
    if v.len() != 20 {
        return None;
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&v);
    Some(out)
}

fn infohash_hex(h: &[u8; 20]) -> String {
    hex::encode(h)
}

impl Engine {
    pub async fn new(cfg: &crate::config::Config, store: Arc<crate::store::Store>) -> anyhow::Result<Self> {
        let has_wg = !cfg.wireguard_conf.is_empty();
        if has_wg {
            match crate::wireguard::start_wireguard(&cfg.wireguard_conf) {
                Ok(t) => tracing::info!("wireguard tunnel active (local IPs: {:?})", t.local_ips),
                Err(e) => anyhow::bail!("wireguard: {e}"),
            }
        }
        let mut scfg = torrent::session::SessionConfig::default();
        scfg.listen_port = if cfg.torrent_port == 0 { 6881 } else { cfg.torrent_port };
        scfg.max_active_torrents = cfg.max_torrents;
        scfg.tracker_timeout = Duration::from_secs(15);
        if has_wg {
            // Route through WG is not yet implemented in this port; DHT stays
            // enabled so magnets can still resolve over the host network.
            tracing::warn!("wireguard: BitTorrent traffic currently uses the host network (tunnel parsed, device not attached)");
        }
        let session = std::sync::Arc::new(torrent::session::Session::new(scfg).await.map_err(|e| anyhow::anyhow!("torrent client: {e}"))?);

        let media_dir = Path::new(&cfg.data_dir).join("media");
        tokio::fs::create_dir_all(&media_dir).await?;
        // migrate old cache root
        if let Err(e) = migrate_cache_root(Path::new(&cfg.data_dir), &media_dir).await {
            tracing::warn!("migrate cache: {e}");
        }

        let e = Self {
            session,
            data_dir: Path::new(&cfg.data_dir).to_path_buf(),
            media_dir,
            trackers: cfg.trackers.clone(),
            store,
            max_cache_bytes: Arc::new(std::sync::atomic::AtomicI64::new(cfg.cache_max_bytes)),
            reserve_bytes: cfg.cache_reserve_bytes,
            idle_timeout: Arc::new(std::sync::atomic::AtomicI64::new(cfg.idle_timeout.as_nanos() as i64)),
            grace: cfg.cache_grace,
            retain: cfg.cache_retain,
            max_torrents: cfg.max_torrents,
            act: Arc::new(RwLock::new(HashMap::new())),
            add_locks: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            warm_sem: Arc::new(tokio::sync::Semaphore::new(4)),
            stats: Arc::new(RwLock::new(crate::cache::CacheStats::default())),
            evicted_bytes: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            evicted_count: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            has_wg,
        };
        e.spawn_janitor();
        Ok(e)
    }

    pub fn has_wireguard(&self) -> bool {
        self.has_wg
    }

    pub fn set_max_cache(&self, v: i64) {
        self.max_cache_bytes.store(v, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn set_idle_timeout(&self, d: Duration) {
        self.idle_timeout.store(d.as_nanos() as i64, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn idle_timeout_dur(&self) -> Duration {
        let n = self.idle_timeout.load(std::sync::atomic::Ordering::Relaxed);
        if n <= 0 {
            Duration::from_secs(20 * 60)
        } else {
            Duration::from_nanos(n as u64)
        }
    }

    fn spawn_janitor(&self) {
        let act = self.act.clone();
        let session = self.session.clone();
        let media_dir = self.media_dir.clone();
        let store = self.store.clone();
        let stats = self.stats.clone();
        let ev_b = self.evicted_bytes.clone();
        let ev_c = self.evicted_count.clone();
        let max_cache = self.max_cache_bytes.clone();
        let reserve = self.reserve_bytes;
        let grace = self.grace;
        let retain = self.retain;
        let idle = self.idle_timeout.clone();
        let closed = self.closed.clone();
        let max_t = self.max_torrents;
        let _ = max_t;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            let mut sweeps = 0u64;
            loop {
                tick.tick().await;
                if closed.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let now = SystemTime::now();
                reap_idle(&session, &act, &idle, now).await;
                if sweeps == 0 || sweeps % 4 == 0 {
                    sweep_cache_once(&session, &media_dir, &store, &act, &stats, &ev_b, &ev_c, &max_cache, reserve, now, grace, retain).await;
                }
                sweeps += 1;
            }
        });
    }

    pub async fn touch(&self, ih: &str, label: &str) {
        let mut m = self.act.write().await;
        let a = m.entry(ih.to_string()).or_insert_with(Activity::default);
        if !label.is_empty() {
            a.label = label.to_string();
        }
        a.last_access = SystemTime::now();
    }

    async fn lock_for(&self, ih: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut m = self.add_locks.lock().await;
        m.entry(ih.to_string()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
    }

    /// Ensure a torrent handle exists, adding the magnet if needed.
    pub async fn ensure(&self, ih: &str, name: &str, extra: &[String]) -> anyhow::Result<[u8; 20]> {
        if !crate::util::valid_infohash_strict_lower(&ih.to_lowercase()) && !crate::util::valid_infohash(ih) {
            anyhow::bail!("invalid infohash");
        }
        let ih = ih.to_lowercase();
        self.touch(&ih, name).await;
        let hb = hex_to_infohash(&ih).ok_or_else(|| anyhow::anyhow!("invalid infohash"))?;
        // already present?
        if self.session.active_torrents().iter().any(|h| h == &hb) {
            return Ok(hb);
        }
        // also check metainfo presence (registered but not active)?
        // torrent crate keeps registered torrents even without swarm; active_torrents only lists running ones.
        // Try status query: if metainfo exists, treat as present.
        if self.session.metainfo(&hb).is_some() {
            return Ok(hb);
        }
        let lk = self.lock_for(&ih).await;
        let _g = lk.lock().await;
        // re-check
        if self.session.active_torrents().iter().any(|h| h == &hb) || self.session.metainfo(&hb).is_some() {
            return Ok(hb);
        }
        {
            let mut m = self.act.write().await;
            if let Some(a) = m.get_mut(&ih) {
                a.adding = true;
            }
        }
        struct Guard<'a> {
            act: &'a Arc<RwLock<HashMap<String, Activity>>>,
            ih: String,
        }
        impl Drop for Guard<'_> {
            fn drop(&mut self) {}
        }
        let mut trs = self.trackers.clone();
        for t in extra {
            if !t.is_empty() && !trs.contains(t) {
                trs.push(t.clone());
            }
        }
        if self.max_torrents > 0 && self.session.active_torrents().len() >= self.max_torrents {
            if !self.evict_oldest().await {
                let mut m = self.act.write().await;
                if let Some(a) = m.get_mut(&ih) {
                    a.adding = false;
                }
                anyhow::bail!("too many active torrents; try again shortly");
            }
        }
        let magnet = crate::indexers::magnet_uri(&ih, name, &trs);
        let res = self.session.add_magnet_str(&magnet);
        {
            let mut m = self.act.write().await;
            if let Some(a) = m.get_mut(&ih) {
                a.adding = false;
            }
        }
        let builder = res.map_err(|e| anyhow::anyhow!("{e}"))?;
        let dir = self.media_dir.join(&ih);
        // create dir eagerly so pending detection works
        let _ = tokio::fs::create_dir_all(&dir).await;
        builder.download_dir(dir).start().await.map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(hb)
    }

    async fn evict_oldest(&self) -> bool {
        loop {
            let oldest = {
                let m = self.act.read().await;
                let mut oldest: Option<String> = None;
                let mut when: Option<SystemTime> = None;
                for (ih, a) in m.iter() {
                    if a.readers > 0 || a.adding {
                        continue;
                    }
                    match when {
                        None => {
                            oldest = Some(ih.clone());
                            when = Some(a.last_access);
                        }
                        Some(w) if a.last_access < w => {
                            oldest = Some(ih.clone());
                            when = Some(a.last_access);
                        }
                        _ => {}
                    }
                }
                oldest
            };
            let oldest = match oldest {
                Some(o) => o,
                None => return false,
            };
            {
                let mut m = self.act.write().await;
                m.remove(&oldest);
            }
            {
                let mut l = self.add_locks.lock().await;
                l.remove(&oldest);
            }
            if let Some(hb) = hex_to_infohash(&oldest) {
                if self.session.metainfo(&hb).is_some() || self.session.active_torrents().contains(&hb) {
                    let _ = self.session.remove_torrent(&hb).await;
                    return true;
                }
                continue;
            } else {
                continue;
            }
        }
    }

    pub async fn stream_busy(&self) -> bool {
        let m = self.act.read().await;
        m.values().any(|a| a.readers > 0)
    }

    pub fn warm(&self, ih: String, name: String, extra: Vec<String>, mb: i64) {
        if !crate::util::valid_infohash(&ih.to_lowercase()) {
            return;
        }
        let this = self.clone_handle();
        tokio::spawn(async move {
            let permit = match this.warm_sem.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => return,
            };
            let _ = permit;
            if this.stream_busy().await {
                tracing::info!("warm {} skipped: a stream is active (stream-first)", &ih[..8.min(ih.len())]);
                return;
            }
            let hb = match this.ensure(&ih, &name, &extra).await {
                Ok(h) => h,
                Err(_) => return,
            };
            // wait metadata up to 60s
            let ok = wait_metainfo(&this.session, &hb, Duration::from_secs(60)).await;
            if !ok {
                return;
            }
            this.record_media(&hb).await;
            if this.stream_busy().await {
                return;
            }
            this.prebuffer(&hb, mb).await;
        });
    }

    async fn prebuffer(&self, hb: &[u8; 20], mb: i64) {
        if mb <= 0 {
            return;
        }
        let Some(meta) = self.session.metainfo(hb) else { return };
        let (paths, sizes) = crate::tmeta::torrent_files(&meta);
        let idx = match crate::relevance::choose_file(&paths, &sizes, -1) {
            Ok(i) => i,
            Err(_) => return,
        };
        // Prebuffer = wait until first `mb` bytes of the file exist on disk, polling.
        let ih = infohash_hex(hb);
        let dir = self.media_dir.join(&ih);
        // Resolve file path on disk: media/<ih>/<torrent_name>/<file_path...>
        // torrent crate FileStorageFactory layout: download_dir/<file_path>.
        // For single-file torrents, file is directly under dir.
        let rel = paths[idx].clone();
        let fp = dir.join(&rel);
        let alt = dir.join(crate::tmeta::torrent_name(&meta)).join(&rel);
        let target = (mb as u64) * 1024 * 1024;
        let start = SystemTime::now();
        let mut last_log = 0u64;
        loop {
            if self.stream_busy().await {
                break;
            }
            if start.elapsed().unwrap_or_default() > Duration::from_secs(20) {
                break;
            }
            let have = tokio::fs::metadata(&fp).await.map(|m| m.len()).unwrap_or(0).max(tokio::fs::metadata(&alt).await.map(|m| m.len()).unwrap_or(0));
            if have > 0 && have != last_log {
                last_log = have;
            }
            if have as u64 >= target {
                break;
            }
            // check if download completed or stalled
            if let Ok(st) = self.session.torrent_status(hb).await {
                if st.progress >= 1.0 {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            // if file appeared with some bytes, keep waiting until target/timeout
            if have > 0 && start.elapsed().unwrap_or_default() > Duration::from_secs(10) && have as u64 >= 1 * 1024 * 1024 {
                // log partial
            }
        }
        if last_log > 0 {
            tracing::info!("prebuffered {} of {:?} ({})", crate::util::human_bytes(last_log as i64), paths[idx], &ih[..8.min(ih.len())]);
        }
    }

    async fn record_media(&self, hb: &[u8; 20]) {
        let Some(meta) = self.session.metainfo(hb) else { return };
        let ih = infohash_hex(hb);
        let (paths2, sizes2) = crate::tmeta::torrent_files(&meta);
        let tname = crate::tmeta::torrent_name(&meta);
        let mut files_out = vec![];
        for (i, p) in paths2.iter().enumerate() {
            if !crate::relevance::is_video_path(p) { continue; }
            files_out.push(crate::store::CachedFile { idx: i, path: p.clone(), size: sizes2[i] });
        }
        let total = crate::tmeta::torrent_total(&meta);
        let _ = self.store.save_media(&ih, &tname, total, &files_out);
    }

    pub async fn views(&self) -> Vec<TorrentView> {
        let mut out = vec![];
        for hb in self.session.active_torrents() {
            let ih = infohash_hex(&hb);
            let (readers, rate, idle_s, label) = {
                let m = self.act.read().await;
                if let Some(a) = m.get(&ih) {
                    (a.readers, a.rate, SystemTime::now().duration_since(a.last_access).map(|d| d.as_secs() as i64).unwrap_or(0), a.label.clone())
                } else {
                    (0, 0.0, 0, String::new())
                }
            };
            let status = self.session.torrent_status(&hb).await.ok();
            let meta = self.session.metainfo(&hb);
            let name = meta.as_ref().map(crate::tmeta::torrent_name).unwrap_or(label);
            let (size, completed, progress, peers, seeders) = match status {
                Some(s) => (s.total_size as i64, s.total_downloaded as i64, s.progress, s.num_peers, s.num_seeds),
                None => (0, 0, 0.0, 0, 0),
            };
            out.push(TorrentView {
                infohash: ih,
                name,
                size,
                completed,
                progress,
                peers,
                seeders,
                rate,
                readers,
                idle_seconds: idle_s,
            });
        }
        out.sort_by(|a, b| a.idle_seconds.cmp(&b.idle_seconds));
        out
    }

    pub async fn cache_stats(&self) -> crate::cache::CacheStats {
        let entries = crate::cache::scan_cache(&self.media_dir).unwrap_or_default();
        let mut used = 0i64;
        let mut partial = 0i64;
        for e in &entries {
            used += e.bytes;
            if e.partial {
                partial += e.bytes;
            }
        }
        let mut st = self.stats.read().await.clone();
        st.used_bytes = used;
        if st.last_sweep_unix == 0 {
            st.entries = entries.len();
            st.partial_bytes = partial;
        }
        st.budget_bytes = self.max_cache_bytes.load(std::sync::atomic::Ordering::Relaxed);
        st.reserve_bytes = self.reserve_bytes;
        st.free_bytes = self.disk_free().await;
        st.retain_minutes = self.retain.as_secs() as i64 / 60;
        st.grace_minutes = self.grace.as_secs() as i64 / 60;
        st
    }

    pub async fn disk_free(&self) -> i64 {
        crate::platform::free_disk_space(&self.media_dir).unwrap_or(-1)
    }

    pub fn torrent_count(&self) -> usize {
        self.session.active_torrents().len()
    }

    pub fn media_dir(&self) -> &Path {
        &self.media_dir
    }

    pub fn session_ref(&self) -> &torrent::session::Session {
        &self.session
    }
    pub fn session_arc(&self) -> std::sync::Arc<torrent::session::Session> {
        self.session.clone()
    }

    pub async fn inc_reader(&self, ih: &str, size: i64) {
        let mut m = self.act.write().await;
        let a = m.entry(ih.to_string()).or_insert_with(Activity::default);
        a.readers += 1;
        a.last_access = SystemTime::now();
        a.size = size;
    }

    pub async fn dec_reader(&self, ih: &str) {
        let mut m = self.act.write().await;
        if let Some(a) = m.get_mut(ih) {
            a.readers = (a.readers - 1).max(0);
            a.last_access = SystemTime::now();
        }
    }

    pub async fn touch_now(&self, ih: &str) {
        let mut m = self.act.write().await;
        if let Some(a) = m.get_mut(ih) {
            a.last_access = SystemTime::now();
        }
    }

    pub async fn peek_activity(&self, ih: &str) -> (i32, SystemTime) {
        let m = self.act.read().await;
        if let Some(a) = m.get(ih) {
            (a.readers, a.last_access)
        } else {
            (0, SystemTime::UNIX_EPOCH)
        }
    }

    fn clone_handle(&self) -> EngineHandle {
        EngineHandle {
            session: self.session.clone(),
            media_dir: self.media_dir.clone(),
            trackers: self.trackers.clone(),
            store: self.store.clone(),
            max_cache_bytes: self.max_cache_bytes.clone(),
            reserve_bytes: self.reserve_bytes,
            idle_timeout: self.idle_timeout.clone(),
            grace: self.grace,
            retain: self.retain,
            max_torrents: self.max_torrents,
            act: self.act.clone(),
            add_locks: self.add_locks.clone(),
            warm_sem: self.warm_sem.clone(),
            stats: self.stats.clone(),
            evicted_bytes: self.evicted_bytes.clone(),
            evicted_count: self.evicted_count.clone(),
            closed: self.closed.clone(),
            has_wg: self.has_wg,
        }
    }
}

// A cheap cloneable handle that reuses the same Arcs (Engine itself is not Clone due to Session? Session is Clone).
#[derive(Clone)]
struct EngineHandle {
    session: std::sync::Arc<torrent::session::Session>,
    media_dir: PathBuf,
    trackers: Vec<String>,
    store: Arc<crate::store::Store>,
    max_cache_bytes: Arc<std::sync::atomic::AtomicI64>,
    reserve_bytes: i64,
    idle_timeout: Arc<std::sync::atomic::AtomicI64>,
    grace: Duration,
    retain: Duration,
    max_torrents: usize,
    act: Arc<RwLock<HashMap<String, Activity>>>,
    add_locks: Arc<tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    warm_sem: Arc<tokio::sync::Semaphore>,
    stats: Arc<RwLock<crate::cache::CacheStats>>,
    evicted_bytes: Arc<std::sync::atomic::AtomicI64>,
    evicted_count: Arc<std::sync::atomic::AtomicI64>,
    closed: Arc<std::sync::atomic::AtomicBool>,
    has_wg: bool,
}

impl std::ops::Deref for EngineHandle {
    type Target = Engine;
    fn deref(&self) -> &Engine {
        // SAFETY: layout-compatible hack is not possible; instead implement methods directly.
        // This Deref is never actually used for Engine methods that need &self with different fields;
        // we implement the needed methods on EngineHandle via inherent impl below by duplicating logic.
        // To keep the compiler happy, we panic if deref is used.
        panic!("deref not supported")
    }
}

// Implement the subset of Engine methods on EngineHandle by duplicating bodies.
impl EngineHandle {
    async fn stream_busy(&self) -> bool {
        let m = self.act.read().await;
        m.values().any(|a| a.readers > 0)
    }
    async fn touch(&self, ih: &str, label: &str) {
        let mut m = self.act.write().await;
        let a = m.entry(ih.to_string()).or_insert_with(Activity::default);
        if !label.is_empty() {
            a.label = label.to_string();
        }
        a.last_access = SystemTime::now();
    }
    async fn lock_for(&self, ih: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut m = self.add_locks.lock().await;
        m.entry(ih.to_string()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
    }
    async fn ensure(&self, ih: &str, name: &str, extra: &[String]) -> anyhow::Result<[u8; 20]> {
        if !crate::util::valid_infohash(ih) && !crate::util::valid_infohash_strict_lower(&ih.to_lowercase()) {
            anyhow::bail!("invalid infohash");
        }
        let ih = ih.to_lowercase();
        self.touch(&ih, name).await;
        let hb = hex_to_infohash(&ih).ok_or_else(|| anyhow::anyhow!("invalid infohash"))?;
        if self.session.active_torrents().iter().any(|h| h == &hb) || self.session.metainfo(&hb).is_some() {
            return Ok(hb);
        }
        let lk = self.lock_for(&ih).await;
        let _g = lk.lock().await;
        if self.session.active_torrents().iter().any(|h| h == &hb) || self.session.metainfo(&hb).is_some() {
            return Ok(hb);
        }
        {
            let mut m = self.act.write().await;
            if let Some(a) = m.get_mut(&ih) {
                a.adding = true;
            }
        }
        let mut trs = self.trackers.clone();
        for t in extra {
            if !t.is_empty() && !trs.contains(t) {
                trs.push(t.clone());
            }
        }
        if self.max_torrents > 0 && self.session.active_torrents().len() >= self.max_torrents {
            // evict oldest (duplicate logic)
            let oldest = {
                let m = self.act.read().await;
                let mut oldest: Option<String> = None;
                let mut when: Option<SystemTime> = None;
                for (k, a) in m.iter() {
                    if a.readers > 0 || a.adding {
                        continue;
                    }
                    match when {
                        None => {
                            oldest = Some(k.clone());
                            when = Some(a.last_access);
                        }
                        Some(w) if a.last_access < w => {
                            oldest = Some(k.clone());
                            when = Some(a.last_access);
                        }
                        _ => {}
                    }
                }
                oldest
            };
            let mut evicted = false;
            if let Some(o) = oldest {
                {
                    let mut m = self.act.write().await;
                    m.remove(&o);
                }
                if let Some(hbo) = hex_to_infohash(&o) {
                    if self.session.metainfo(&hbo).is_some() || self.session.active_torrents().contains(&hbo) {
                        let _ = self.session.remove_torrent(&hbo).await;
                        evicted = true;
                    }
                }
            }
            if !evicted {
                let mut m = self.act.write().await;
                if let Some(a) = m.get_mut(&ih) {
                    a.adding = false;
                }
                anyhow::bail!("too many active torrents; try again shortly");
            }
        }
        let magnet = crate::indexers::magnet_uri(&ih, name, &trs);
        let res = self.session.add_magnet_str(&magnet);
        {
            let mut m = self.act.write().await;
            if let Some(a) = m.get_mut(&ih) {
                a.adding = false;
            }
        }
        let builder = res.map_err(|e| anyhow::anyhow!("{e}"))?;
        let dir = self.media_dir.join(&ih);
        let _ = tokio::fs::create_dir_all(&dir).await;
        builder.download_dir(dir).start().await.map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(hb)
    }
    async fn record_media(&self, hb: &[u8; 20]) {
        let Some(meta) = self.session.metainfo(hb) else { return };
        let ih = infohash_hex(hb);
        let (paths2, sizes2) = crate::tmeta::torrent_files(&meta);
        let tname = crate::tmeta::torrent_name(&meta);
        let mut files_out = vec![];
        for (i, p) in paths2.iter().enumerate() {
            if !crate::relevance::is_video_path(p) { continue; }
            files_out.push(crate::store::CachedFile { idx: i, path: p.clone(), size: sizes2[i] });
        }
        let total = crate::tmeta::torrent_total(&meta);
        let _ = self.store.save_media(&ih, &tname, total, &files_out);
    }
    async fn prebuffer(&self, hb: &[u8; 20], mb: i64) {
        if mb <= 0 {
            return;
        }
        let Some(meta) = self.session.metainfo(hb) else { return };
        let (paths, sizes) = crate::tmeta::torrent_files(&meta);
        let idx = match crate::relevance::choose_file(&paths, &sizes, -1) {
            Ok(i) => i,
            Err(_) => return,
        };
        let ih = infohash_hex(hb);
        let dir = self.media_dir.join(&ih);
        let rel = paths[idx].clone();
        let fp = dir.join(&rel);
        let target = (mb as u64) * 1024 * 1024;
        let start = SystemTime::now();
        let mut last_log = 0u64;
        loop {
            if self.stream_busy().await {
                break;
            }
            if start.elapsed().unwrap_or_default() > Duration::from_secs(20) {
                break;
            }
            let have = tokio::fs::metadata(&fp).await.map(|m| m.len()).unwrap_or(0);
            if have > 0 {
                last_log = have;
            }
            if have as u64 >= target {
                break;
            }
            if let Ok(st) = self.session.torrent_status(hb).await {
                if st.progress >= 1.0 {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if last_log > 0 {
            tracing::info!("prebuffered {} of {:?} ({})", crate::util::human_bytes(last_log as i64), paths[idx], &ih[..8.min(ih.len())]);
        }
    }
}

pub async fn wait_metainfo(session: &torrent::session::Session, hb: &[u8; 20], timeout: Duration) -> bool {
    let start = SystemTime::now();
    loop {
        if session.metainfo(hb).is_some() {
            return true;
        }
        if start.elapsed().unwrap_or_default() > timeout {
            return session.metainfo(hb).is_some();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

async fn migrate_cache_root(root: &Path, media: &Path) -> anyhow::Result<()> {
    let mut rd = tokio::fs::read_dir(root).await?;
    while let Some(ent) = rd.next_entry().await? {
        let name = ent.file_name().to_string_lossy().to_string();
        if name == "media" || name == "ADMIN_PASSWORD" || crate::cache::cache_skip(&name) {
            continue;
        }
        let src = root.join(&name);
        let mut dst = media.join(&name);
        if tokio::fs::metadata(&dst).await.is_ok() {
            dst = media.join(format!("{name}-migrated"));
        }
        if let Err(e) = tokio::fs::rename(&src, &dst).await {
            tracing::warn!("migrate {name}: {e}");
        }
    }
    Ok(())
}

async fn reap_idle(session: &torrent::session::Session, act: &Arc<RwLock<HashMap<String, Activity>>>, idle: &Arc<std::sync::atomic::AtomicI64>, now: SystemTime) {
    for hb in session.active_torrents() {
        let ih = infohash_hex(&hb);
        let (size, completed) = match session.torrent_status(&hb).await {
            Ok(s) => (s.total_size, s.total_downloaded),
            Err(_) => (0, 0),
        };
        {
            let mut m = act.write().await;
            let a = m.entry(ih.clone()).or_insert_with(Activity::default);
            if size > 0 {
                a.size = size as i64;
            }
            let dt = now.duration_since(a.prev_at).map(|d| d.as_secs_f64()).unwrap_or(0.0);
            if dt > 0.0 {
                a.rate = (completed.saturating_sub(a.prev_bytes)) as f64 / dt;
                a.prev_bytes = completed;
            }
            a.prev_at = now;
        }
        let (readers, last, warned) = {
            let m = act.read().await;
            match m.get(&ih) {
                Some(a) => (a.readers, a.last_access, a.warned_no_peers),
                None => (0, now, false),
            }
        };
        // peer warning
        if readers > 0 {
            let peers = session.torrent_status(&hb).await.map(|s| s.num_peers).unwrap_or(0);
            if size as i64 > 0 && peers == 0 && now.duration_since(last).unwrap_or_default() > Duration::from_secs(15) && !warned {
                tracing::warn!("no peers for {} after {:?}", &ih[..8.min(ih.len())], now.duration_since(last).unwrap_or_default());
                let mut m = act.write().await;
                if let Some(a) = m.get_mut(&ih) {
                    a.warned_no_peers = true;
                }
            }
        }
        let idle_dur = Duration::from_nanos(idle.load(std::sync::atomic::Ordering::Relaxed) as u64);
        if readers == 0 && now.duration_since(last).unwrap_or_default() > idle_dur {
            tracing::info!("drop idle torrent {} ({:?} idle)", &ih[..8.min(ih.len())], now.duration_since(last).unwrap_or_default());
            let _ = session.remove_torrent(&hb).await;
            let mut m = act.write().await;
            m.remove(&ih);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn sweep_cache_once(
    session: &torrent::session::Session,
    data_dir: &Path,
    store: &Arc<crate::store::Store>,
    act: &Arc<RwLock<HashMap<String, Activity>>>,
    stats: &Arc<RwLock<crate::cache::CacheStats>>,
    ev_b: &Arc<std::sync::atomic::AtomicI64>,
    ev_c: &Arc<std::sync::atomic::AtomicI64>,
    max_cache: &Arc<std::sync::atomic::AtomicI64>,
    reserve: i64,
    now: SystemTime,
    grace: Duration,
    retain: Duration,
) {
    let entries = match crate::cache::scan_cache(data_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("cache: scan {}: {e}", data_dir.display());
            return;
        }
    };
    // owned/pending
    let mut owned: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut pending: std::collections::HashSet<String> = std::collections::HashSet::new();
    for hb in session.active_torrents() {
        let ih = infohash_hex(&hb);
        if session.metainfo(&hb).is_some() {
            owned.insert(ih);
        } else if store.media_name(&ih).is_some() {
            pending.insert(ih);
        }
    }
    // also include registered-but-not-active metainfo? session.metainfo covers them only if active? Include anyway by checking all act keys?
    let mut units: Vec<crate::cache::CacheUnit> = vec![];
    let mut used = 0i64;
    let mut partial_bytes = 0i64;
    let mut orphan_bytes = 0i64;
    let mut orphan_count = 0usize;
    let mut loaded = 0usize;
    for en in &entries {
        used += en.bytes;
        if en.partial {
            partial_bytes += en.bytes;
        }
        let mut u = crate::cache::CacheUnit { entry: en.clone(), infohash: String::new(), loaded: false, readers: 0, last_use: en.modified };
        if owned.contains(&en.name) {
            u.loaded = true;
            u.infohash = en.name.clone();
            loaded += 1;
            let (readers, last) = {
                let m = act.read().await;
                match m.get(&en.name) {
                    Some(a) => (a.readers, a.last_access),
                    None => (0, en.modified),
                }
            };
            u.readers = readers;
            if last > u.last_use {
                u.last_use = last;
            }
        } else if pending.contains(&en.name) {
            continue;
        } else {
            orphan_count += 1;
            orphan_bytes += en.bytes;
        }
        units.push(u);
    }
    let free = crate::platform::free_disk_space(data_dir).unwrap_or(-1);
    let plan = crate::cache::plan_cache(&units, used, max_cache.load(std::sync::atomic::Ordering::Relaxed), reserve, free, now, grace, retain);
    let mut freed = 0i64;
    let mut freed_count = 0i64;
    let mut rem_orphans = 0i64;
    let mut rem_orphan_bytes = 0i64;
    let mut rem_partial = 0i64;
    for (idx, reason) in plan {
        let u = &units[idx];
        if u.loaded {
            if let Some(hb) = hex_to_infohash(&u.infohash) {
                let _ = session.remove_torrent(&hb).await;
            }
        }
        if crate::platform::remove_all_async(&u.entry.path).await.is_err() {
            tracing::warn!("cache: remove {}", u.entry.name);
            continue;
        }
        if !u.infohash.is_empty() {
            let mut m = act.write().await;
            m.remove(&u.infohash);
        } else {
            rem_orphans += 1;
            rem_orphan_bytes += u.entry.bytes;
        }
        if u.entry.partial {
            rem_partial += u.entry.bytes;
        }
        store.forget_media(&u.infohash, "");
        freed += u.entry.bytes;
        freed_count += 1;
        if reason != "retention" {
            tracing::info!("cache: evicted {} ({}, {reason})", u.entry.name, crate::util::human_bytes(u.entry.bytes));
        }
    }
    let mut st = crate::cache::CacheStats {
        used_bytes: used - freed,
        budget_bytes: max_cache.load(std::sync::atomic::Ordering::Relaxed),
        reserve_bytes: reserve,
        free_bytes: free,
        entries: entries.len() - freed_count as usize,
        partial_bytes: partial_bytes - rem_partial,
        orphan_bytes: orphan_bytes - rem_orphan_bytes,
        orphan_entries: orphan_count - rem_orphans as usize,
        loaded_entries: loaded,
        last_sweep_unix: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
        retain_minutes: retain.as_secs() as i64 / 60,
        grace_minutes: grace.as_secs() as i64 / 60,
        evicted_bytes: 0,
        evicted_entries: 0,
    };
    let nb = ev_b.fetch_add(freed, std::sync::atomic::Ordering::Relaxed) + freed;
    let nc = ev_c.fetch_add(freed_count, std::sync::atomic::Ordering::Relaxed) + freed_count;
    st.evicted_bytes = nb;
    st.evicted_entries = nc;
    *stats.write().await = st.clone();
    if freed_count > 0 {
        tracing::info!("cache: reclaimed {} in {freed_count} entries (used {} / budget {}, free {})",
            crate::util::human_bytes(freed), crate::util::human_bytes(st.used_bytes), crate::util::human_bytes(max_cache.load(std::sync::atomic::Ordering::Relaxed)), crate::util::human_bytes(free));
    }
}
