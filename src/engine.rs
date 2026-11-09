use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;

pub use librqbit_core::hash_id::Id20;
pub type ManagedTorrentHandle = Arc<librqbit::ManagedTorrent>;

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
        Self {
            readers: 0,
            adding: false,
            last_access: now,
            label: String::new(),
            size: 0,
            rate: 0.0,
            prev_bytes: 0,
            prev_at: now,
            warned_no_peers: false,
        }
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

pub fn hex_to_id20(ih: &str) -> Option<Id20> {
    if ih.len() != 40 {
        return None;
    }
    let v = hex::decode(ih).ok()?;
    if v.len() != 20 {
        return None;
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&v);
    Some(Id20::new(out))
}

#[derive(Clone)]
pub struct Engine {
    session: Arc<librqbit::Session>,
    media_dir: PathBuf,
    trackers: Vec<String>,
    store: Arc<crate::store::Store>,
    max_cache_bytes: Arc<std::sync::atomic::AtomicI64>,
    reserve_bytes: i64,
    idle_timeout: Arc<std::sync::atomic::AtomicI64>, // nanos
    grace: Duration,
    retain: Duration,
    max_torrents: usize,
    info_timeout: Duration,
    act: Arc<RwLock<HashMap<String, Activity>>>,
    add_locks: Arc<tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    stats: Arc<RwLock<crate::cache::CacheStats>>,
    evicted_bytes: Arc<std::sync::atomic::AtomicI64>,
    evicted_count: Arc<std::sync::atomic::AtomicI64>,
    closed: Arc<std::sync::atomic::AtomicBool>,
}

impl Engine {
    pub async fn new(cfg: &crate::config::Config, store: Arc<crate::store::Store>) -> anyhow::Result<Self> {

        let media_dir = Path::new(&cfg.data_dir).join("media");
        tokio::fs::create_dir_all(&media_dir).await?;
        if let Err(e) = migrate_cache_root(Path::new(&cfg.data_dir), &media_dir).await {
            tracing::warn!("migrate cache: {e}");
        }

        // DHT with persistent routing table: reuses the previous port and known
        // nodes so peer discovery on click is warm instead of cold.
        let mut dht_config = librqbit::DhtSessionConfig::default();
        if cfg.torrent_port != 0 {
            dht_config.port = Some(cfg.torrent_port);
        }
        dht_config.persistence = Some(librqbit::dht::DhtPersistenceConfig {
            dump_interval: Some(Duration::from_secs(60)),
            config_filename: Some(Path::new(&cfg.data_dir).join("dht.json")),
        });
        // Session-level trackers: every torrent (including resumed ones) gets
        // the full tracker list even if its magnet omitted some.
        let mut session_trackers = std::collections::HashSet::new();
        for t in &cfg.trackers {
            if let Ok(u) = url::Url::parse(t) {
                session_trackers.insert(u);
            }
        }
        // Aggressive-but-sane peer tuning for fast start:
        // - 5s connect timeout culls slow peers quickly (default 10s)
        // - 32 request window reacts to seeks/new streams faster (default 128)
        let peer_opts = librqbit::PeerConnectionOptions {
            connect_timeout: Some(Duration::from_secs(5)),
            read_write_timeout: Some(Duration::from_secs(10)),
            keep_alive_interval: Some(Duration::from_secs(30)),
            max_request_window: Some(32),
        };
        let mut session_opts = librqbit::SessionOptions {
            dht: Some(dht_config),
            fastresume: true,
            trackers: session_trackers,
            peer_limit: Some(150),
            concurrent_init_limit: Some(10),
            connect: Some(librqbit::ConnectionOptions {
                enable_tcp: true,
                peer_opts: Some(peer_opts),
                ..Default::default()
            }),
            ..Default::default()
        };
        // Always listen (even with port 0 = random port) so we get incoming
        // connections. Previously port 0 meant no listener at all, which
        // roughly halves the reachable peer set and hurts first-byte time.
        // UPnP is enabled so a fixed BITLAB_TORRENT_PORT gets forwarded
        // automatically on home routers.
        {
            let mut listen_opts = librqbit::ListenerOptions::default();
            if cfg.torrent_port != 0 {
                listen_opts.listen_addr = std::net::SocketAddr::new(
                    std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                    cfg.torrent_port,
                );
                listen_opts.announce_port = Some(cfg.torrent_port);
            }
            listen_opts.enable_upnp_port_forwarding = true;
            session_opts.listen = Some(listen_opts);
        }

        let session = librqbit::Session::new_with_opts(media_dir.clone(), session_opts).await
            .map_err(|e| anyhow::anyhow!("librqbit session: {e}"))?;

        let e = Self {
            session,
            media_dir,
            trackers: cfg.trackers.clone(),
            store,
            max_cache_bytes: Arc::new(std::sync::atomic::AtomicI64::new(cfg.cache_max_bytes)),
            reserve_bytes: cfg.cache_reserve_bytes,
            idle_timeout: Arc::new(std::sync::atomic::AtomicI64::new(cfg.idle_timeout.as_nanos() as i64)),
            grace: cfg.cache_grace,
            retain: cfg.cache_retain,
            max_torrents: cfg.max_torrents,
            info_timeout: cfg.info_timeout,
            act: Arc::new(RwLock::new(HashMap::new())),
            add_locks: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            stats: Arc::new(RwLock::new(crate::cache::CacheStats::default())),
            evicted_bytes: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            evicted_count: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        e.spawn_janitor();
        Ok(e)
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
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                if closed.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let now = SystemTime::now();
                reap_idle(&session, &store, &act, &idle, now).await;
                sweep_cache_once(&session, &media_dir, &store, &act, &stats, &ev_b, &ev_c, &max_cache, reserve, now, grace, retain).await;
            }
        });
    }

    pub async fn sweep_cache(&self) {
        let now = SystemTime::now();
        sweep_cache_once(
            &self.session,
            &self.media_dir,
            &self.store,
            &self.act,
            &self.stats,
            &self.evicted_bytes,
            &self.evicted_count,
            &self.max_cache_bytes,
            self.reserve_bytes,
            now,
            self.grace,
            self.retain,
        ).await;
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

    /// Play path: starts UNPAUSED so DHT/tracker announces and peer
    /// connections begin during metadata resolution instead of after it.
    pub async fn ensure_play(&self, ih: &str, name: &str, extra: &[String]) -> anyhow::Result<ManagedTorrentHandle> {
        let free = self.disk_free().await;
        let used = self.stats.read().await.used_bytes;
        let budget = self.max_cache_bytes.load(std::sync::atomic::Ordering::Relaxed);
        let effective_reserve = if self.reserve_bytes > 0 { self.reserve_bytes } else { crate::config::HEALTH_FREE_FLOOR };
        if (budget > 0 && used > budget) || (free >= 0 && free < effective_reserve) {
            self.sweep_cache().await;
        }
        self.ensure_inner(ih, name, extra).await
    }

    async fn ensure_inner(&self, ih: &str, name: &str, extra: &[String]) -> anyhow::Result<ManagedTorrentHandle> {
        if !crate::util::valid_infohash_strict_lower(&ih.to_lowercase()) && !crate::util::valid_infohash(ih) {
            anyhow::bail!("invalid infohash");
        }
        let ih = ih.to_lowercase();
        self.touch(&ih, name).await;
        let id20 = hex_to_id20(&ih).ok_or_else(|| anyhow::anyhow!("invalid infohash"))?;

        if let Some(handle) = self.session.get(librqbit::api::TorrentIdOrHash::Hash(id20)) {
            return Ok(handle);
        }

        let lk = self.lock_for(&ih).await;
        let _g = lk.lock().await;

        if let Some(handle) = self.session.get(librqbit::api::TorrentIdOrHash::Hash(id20)) {
            return Ok(handle);
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

        let current_count = self.session.with_torrents(|iter| iter.count());
        if self.max_torrents > 0 && current_count >= self.max_torrents && !self.evict_oldest().await {
            let mut m = self.act.write().await;
            if let Some(a) = m.get_mut(&ih) {
                a.adding = false;
            }
            anyhow::bail!("too many active torrents; try again shortly");
        }

        let magnet = crate::indexers::magnet_uri(&ih, name, &trs);
        let add = librqbit::AddTorrent::from_url(magnet);
        let dir = self.media_dir.join(&ih);
        let _ = tokio::fs::create_dir_all(&dir).await;

        let opts = librqbit::AddTorrentOptions {
            overwrite: true,
            paused: false,
            output_folder: Some(dir.to_string_lossy().to_string()),
            ..Default::default()
        };

        let timeout_dur = if self.info_timeout.is_zero() {
            Duration::from_secs(45)
        } else {
            self.info_timeout
        };
        let res = match tokio::time::timeout(timeout_dur, self.session.add_torrent(add, Some(opts))).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!("timeout waiting for torrent metadata")),
        };
        {
            let mut m = self.act.write().await;
            if let Some(a) = m.get_mut(&ih) {
                a.adding = false;
            }
        }

        let handle = res.map_err(|e| anyhow::anyhow!("{e}"))?
            .into_handle()
            .ok_or_else(|| anyhow::anyhow!("failed to get managed torrent handle"))?;

        Ok(handle)
    }

    #[cfg(test)]
    pub async fn add_torrent_bytes(&self, bytes: bytes::Bytes) -> anyhow::Result<ManagedTorrentHandle> {
        let add = librqbit::AddTorrent::TorrentFileBytes(bytes);
        let opts = librqbit::AddTorrentOptions {
            overwrite: true,
            paused: false,
            ..Default::default()
        };
        let res = self.session.add_torrent(add, Some(opts)).await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .into_handle()
            .ok_or_else(|| anyhow::anyhow!("failed to get managed torrent handle"))?;
        let ih = res.info_hash().as_string();
        self.touch(&ih, &res.name().unwrap_or_default()).await;
        Ok(res)
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
            if let Some(id20) = hex_to_id20(&oldest) {
                let _ = self.session.delete(librqbit::api::TorrentIdOrHash::Hash(id20), false).await;
                return true;
            }
        }
    }

    pub async fn record_media(&self, handle: &ManagedTorrentHandle) {
        let (name, total, files_out) = match handle.with_metadata(|meta| {
            let name = meta.info.name().map(|n| n.to_string()).unwrap_or_default();
            let total = meta.lengths().total_length() as i64;
            let mut files_out = vec![];
            for (i, fi) in meta.file_infos.iter().enumerate() {
                let rel = fi.relative_filename.to_string_lossy().to_string();
                if !crate::relevance::is_video_path(&rel) {
                    continue;
                }
                files_out.push(crate::store::CachedFile {
                    idx: i,
                    path: rel,
                    size: fi.len as i64,
                });
            }
            (name, total, files_out)
        }) {
            Ok(v) => v,
            Err(_) => return,
        };
        let ih = handle.info_hash().as_string();
        let _ = self.store.save_media(&ih, &name, total, &files_out);
    }

    pub async fn views(&self) -> Vec<TorrentView> {
        let mut out = vec![];
        let handles = self.session.with_torrents(|iter| {
            iter.map(|(_id, handle)| handle.clone()).collect::<Vec<_>>()
        });
        let now = SystemTime::now();
        for handle in handles {
            let ih = handle.info_hash().as_string();
            let (readers, idle_s, label) = {
                let m = self.act.read().await;
                if let Some(a) = m.get(&ih) {
                    (
                        a.readers,
                        now.duration_since(a.last_access).map(|d| d.as_secs() as i64).unwrap_or(0),
                        a.label.clone(),
                    )
                } else {
                    (0, 0, String::new())
                }
            };
            let stats = handle.stats();
            let name = handle.name().unwrap_or_else(|| if label.is_empty() { ih.clone() } else { label });
            let size = stats.total_bytes as i64;
            let completed = stats.progress_bytes as i64;
            let progress = if size > 0 { (completed as f64) / (size as f64) } else { 0.0 };
            let (peers, rate) = if let Some(live) = &stats.live {
                (live.snapshot.peer_stats.live as usize, live.download_speed.as_bytes() as f64)
            } else {
                (0, 0.0)
            };
            out.push(TorrentView {
                infohash: ih,
                name,
                size,
                completed,
                progress,
                peers,
                seeders: peers,
                rate,
                readers,
                idle_seconds: idle_s,
            });
        }
        out.sort_by_key(|a| a.idle_seconds);
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
        st.entries = entries.len();
        st.partial_bytes = partial;
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
        self.session.with_torrents(|iter| iter.count())
    }

    pub async fn select_only_file(&self, handle: &ManagedTorrentHandle, file_idx: usize) -> anyhow::Result<()> {
        // Play path already waited for metadata; skip the second wait when
        // metadata is present so peer/piece work isn't stalled an extra RTT.
        if handle.with_metadata(|_| true).is_err() {
            let _ = handle.wait_until_initialized().await;
        }
        let mut set = std::collections::HashSet::new();
        set.insert(file_idx);
        let _ = self.session.update_only_files(handle, &set).await;
        if handle.is_paused() {
            self.session.unpause(handle).await?;
        }
        Ok(())
    }

    pub async fn pause_others(&self, current_ih: &str) {
        let act = self.act.read().await;
        let idle_hashes: Vec<String> = act.iter()
            .filter(|(ih, a)| ih.as_str() != current_ih && a.readers == 0)
            .map(|(ih, _)| ih.clone())
            .collect();
        drop(act);

        for ih in idle_hashes {
            if let Some(id20) = hex_to_id20(&ih) {
                if let Some(handle) = self.session.get(librqbit::api::TorrentIdOrHash::Hash(id20)) {
                    if !handle.is_paused() {
                        tracing::info!("pausing idle torrent {} (stream-first)", &ih[..8.min(ih.len())]);
                        let _ = self.session.pause(&handle).await;
                    }
                }
            }
        }
    }

    pub fn schedule_pause_if_idle(&self, ih: String, delay: Duration) {
        let act = self.act.clone();
        let session = self.session.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let readers = {
                let m = act.read().await;
                m.get(&ih).map(|a| a.readers).unwrap_or(0)
            };
            if readers == 0 {
                if let Some(id20) = hex_to_id20(&ih) {
                    if let Some(handle) = session.get(librqbit::api::TorrentIdOrHash::Hash(id20)) {
                        if !handle.is_paused() {
                            tracing::info!("halted torrent {} after disconnect (0 readers)", &ih[..8.min(ih.len())]);
                            let _ = session.pause(&handle).await;
                        }
                    }
                }
            }
        });
    }

    pub async fn inc_reader(&self, ih: &str, size: i64) {
        self.store.touch_media(ih);
        let mut m = self.act.write().await;
        let a = m.entry(ih.to_string()).or_insert_with(Activity::default);
        a.readers += 1;
        a.last_access = SystemTime::now();
        a.size = size;
    }

    pub async fn dec_reader(&self, ih: &str) {
        self.store.touch_media(ih);
        let (zero_readers, ih_owned) = {
            let mut m = self.act.write().await;
            if let Some(a) = m.get_mut(ih) {
                a.readers = (a.readers - 1).max(0);
                a.last_access = SystemTime::now();
                (a.readers == 0, ih.to_string())
            } else {
                (false, String::new())
            }
        };
        if zero_readers {
            self.schedule_pause_if_idle(ih_owned, Duration::from_secs(5));
        }
    }

    pub async fn touch_now(&self, ih: &str) {
        self.store.touch_media(ih);
        let mut m = self.act.write().await;
        if let Some(a) = m.get_mut(ih) {
            a.last_access = SystemTime::now();
        }
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

async fn reap_idle(
    session: &librqbit::Session,
    store: &Arc<crate::store::Store>,
    act: &Arc<RwLock<HashMap<String, Activity>>>,
    idle: &Arc<std::sync::atomic::AtomicI64>,
    now: SystemTime,
) {
    let handles = session.with_torrents(|iter| {
        iter.map(|(_id, handle)| handle.clone()).collect::<Vec<_>>()
    });
    for handle in handles {
        let ih = handle.info_hash().as_string();
        let stats = handle.stats();
        let size = stats.total_bytes;
        let completed = stats.progress_bytes;
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
        if readers > 0 {
            let peers = stats.live.as_ref().map(|l| l.snapshot.peer_stats.live as usize).unwrap_or(0);
            if size > 0 && peers == 0 && now.duration_since(last).unwrap_or_default() > Duration::from_secs(15) && !warned {
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
            store.touch_media(&ih);
            let _ = session.delete(librqbit::api::TorrentIdOrHash::Hash(handle.info_hash()), false).await;
            let mut m = act.write().await;
            m.remove(&ih);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn sweep_cache_once(
    session: &librqbit::Session,
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
    let (owned, pending) = session.with_torrents(|iter| {
        let mut owned = std::collections::HashSet::new();
        let mut pending = std::collections::HashSet::new();
        for (_id, handle) in iter {
            let ih = handle.info_hash().as_string();
            if handle.with_metadata(|_| true).is_ok() {
                owned.insert(ih);
            } else if store.media_name(&ih).is_some() {
                pending.insert(ih);
            }
        }
        (owned, pending)
    });

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
        let is_ih = en.name.len() == 40 && en.name.chars().all(|c| c.is_ascii_hexdigit());
        let mut u = crate::cache::CacheUnit {
            entry: en.clone(),
            infohash: if is_ih { en.name.clone() } else { String::new() },
            loaded: false,
            readers: 0,
            last_use: en.modified,
        };
        if is_ih {
            if let Some(lu_unix) = store.media_last_used(&en.name) {
                if lu_unix > 0 {
                    let lu = std::time::UNIX_EPOCH + Duration::from_secs(lu_unix as u64);
                    if lu > u.last_use {
                        u.last_use = lu;
                    }
                }
            }
        }
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
            if let Some(id20) = hex_to_id20(&u.infohash) {
                let _ = session.delete(librqbit::api::TorrentIdOrHash::Hash(id20), false).await;
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
        store.forget_media(&u.infohash, &u.entry.name);
        freed += u.entry.bytes;
        freed_count += 1;
        if reason != "retention" {
            tracing::info!("cache: evicted {} ({}, {reason})", u.entry.name, crate::util::human_bytes(u.entry.bytes));
        } else {
            tracing::info!("cache: retention sweep evicted {} ({})", u.entry.name, crate::util::human_bytes(u.entry.bytes));
        }
    }
    let free_after = crate::platform::free_disk_space(data_dir).unwrap_or(if free >= 0 { free + freed } else { -1 });
    let mut st = crate::cache::CacheStats {
        used_bytes: used - freed,
        budget_bytes: max_cache.load(std::sync::atomic::Ordering::Relaxed),
        reserve_bytes: reserve,
        free_bytes: free_after,
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
            crate::util::human_bytes(freed), crate::util::human_bytes(st.used_bytes), crate::util::human_bytes(max_cache.load(std::sync::atomic::Ordering::Relaxed)), crate::util::human_bytes(free_after));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_engine_init() {
        let temp_dir = std::env::temp_dir().join(format!("bitlab_test_{}", rand::random::<u64>()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let cfg = crate::config::Config {
            addr: "127.0.0.1:0".to_string(),
            data_dir: temp_dir.to_string_lossy().to_string(),
            base_url: "".to_string(),
            trackers: vec![],
            cache_max_bytes: 10 * 1024 * 1024,
            cache_reserve_bytes: 1024 * 1024,
            cache_retain: Duration::from_secs(60),
            cache_grace: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(60),
            info_timeout: Duration::from_secs(10),
            readahead_bytes: 1024 * 1024,
            torrent_port: 0,
            max_torrents: 5,
            admin_password: "".to_string(),
            trust_proxy: false,
        };
        let store = Arc::new(crate::store::Store::open(&temp_dir.join("store.db")).unwrap());
        let engine = Engine::new(&cfg, store).await.unwrap();
        assert_eq!(engine.torrent_count(), 0);
        let views = engine.views().await;
        assert!(views.is_empty());
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_engine_ensure_and_views() {
        let temp_dir = std::env::temp_dir().join(format!("bitlab_test_{}", rand::random::<u64>()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let cfg = crate::config::Config {
            addr: "127.0.0.1:0".to_string(),
            data_dir: temp_dir.to_string_lossy().to_string(),
            base_url: "".to_string(),
            trackers: vec![],
            cache_max_bytes: 10 * 1024 * 1024,
            cache_reserve_bytes: 1024 * 1024,
            cache_retain: Duration::from_secs(60),
            cache_grace: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(60),
            info_timeout: Duration::from_millis(50),
            readahead_bytes: 1024 * 1024,
            torrent_port: 0,
            max_torrents: 5,
            admin_password: "".to_string(),
            trust_proxy: false,
        };
        let store = Arc::new(crate::store::Store::open(&temp_dir.join("store.db")).unwrap());
        let engine = Engine::new(&cfg, store).await.unwrap();
        
        // Ensure dead magnet times out cleanly
        let ih = "0123456789abcdef0123456789abcdef01234567";
        let res = engine.ensure_play(ih, "Test Torrent", &[]).await;
        assert!(res.is_err(), "expected timeout or error for nonexistent magnet");

        // Add dummy torrent bytes
        let dummy = bytes::Bytes::from_static(b"d4:infod6:lengthi10e4:name8:test.bin12:piece lengthi16384e6:pieces20:12345678901234567890ee");
        let handle = engine.add_torrent_bytes(dummy).await.unwrap();
        let actual_ih = handle.info_hash().as_string();
        assert_eq!(engine.torrent_count(), 1);

        let views = engine.views().await;
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].infohash, actual_ih);

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }
}
