use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;

use crate::bt::{BtConfig, BtSession};
pub use crate::bt::{Id20, TorrentHandle};

pub type ManagedTorrentHandle = Arc<TorrentHandle>;
type AddLocksMap = Arc<tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>;

const MAX_TORRENT_DOWNLOAD_BYTES: usize = 4 * 1024 * 1024;
const TORRENT_FETCH_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Debug, Clone)]
struct Activity {
    active_readers: HashMap<u64, SystemTime>,
    adding: bool,
    last_access: SystemTime,
    label: String,
    warned_no_peers: bool,
}

impl Default for Activity {
    fn default() -> Self {
        let now = SystemTime::now();
        Self {
            active_readers: HashMap::new(),
            adding: false,
            last_access: now,
            label: String::new(),
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
    Id20::from_hex(ih).ok()
}

#[derive(Clone)]
pub struct Engine {
    session: Arc<BtSession>,
    media_dir: PathBuf,
    trackers: Vec<String>,
    store: Arc<crate::store::Store>,
    http: reqwest::Client,
    allow_private: bool,
    max_cache_bytes: Arc<AtomicI64>,
    reserve_bytes: i64,
    idle_timeout: Arc<AtomicI64>, // nanos
    max_torrents: usize,
    info_timeout: Duration,
    act: Arc<RwLock<HashMap<String, Activity>>>,
    add_locks: AddLocksMap,
    sweep_mu: Arc<tokio::sync::Mutex<()>>,
    next_reader_id: Arc<AtomicU64>,
    stats: Arc<RwLock<crate::cache::CacheStats>>,
    last_free_bytes: Arc<AtomicI64>,
    evicted_bytes: Arc<AtomicI64>,
    evicted_count: Arc<AtomicI64>,
}

impl Engine {
    pub async fn new(
        cfg: &crate::config::Config,
        store: Arc<crate::store::Store>,
    ) -> anyhow::Result<Self> {
        let media_dir = Path::new(&cfg.data_dir).join("media");
        tokio::fs::create_dir_all(&media_dir).await?;
        if let Err(e) = migrate_cache_root(Path::new(&cfg.data_dir), &media_dir).await {
            tracing::warn!("migrate cache: {e}");
        }

        let allow_private =
            std::env::var("BITLAB_ALLOW_PRIVATE_INDEXERS").as_deref() == Ok("1");
        let http = crate::netguard::build_client(allow_private);
        let readahead = Arc::new(AtomicI64::new(cfg.readahead_bytes));

        let session = BtSession::start(BtConfig {
            media_dir: media_dir.clone(),
            listen_port: cfg.torrent_port,
            enable_dht: true,
            enable_upnp: cfg.torrent_port != 0,
            enable_utp: true,
            bind_v4_only: false,
            allow_private,
            http: http.clone(),
            readahead,
        })
        .await?;

        let mut trackers = cfg.trackers.clone();
        if trackers.is_empty() {
            trackers = crate::config::default_trackers();
        }

        // Load previously cached media directories that already have `.bitlab_meta` strictly paused (0 network traffic).
        let now = SystemTime::now();
        let mut initial_act = HashMap::new();
        if let Ok(mut rd) = tokio::fs::read_dir(&media_dir).await {
            while let Ok(Some(ent)) = rd.next_entry().await {
                let ih = ent.file_name().to_string_lossy().to_string();
                if hex_to_id20(&ih).is_none() {
                    continue;
                }
                let tdir = ent.path();
                if !tdir.join(".bitlab_meta").exists() {
                    continue;
                }
                if let Ok(handle) = session
                    .add_magnet_or_hash(&ih, &trackers, tdir, true)
                    .await
                {
                    let last_access = store
                        .media_last_used(&ih)
                        .filter(|&ts| ts > 0)
                        .map(|ts| std::time::UNIX_EPOCH + Duration::from_secs(ts as u64))
                        .unwrap_or(now);
                    let st = handle.stats();
                    initial_act.insert(
                        ih,
                        Activity {
                            last_access,
                            label: st.name,
                            ..Default::default()
                        },
                    );
                }
            }
        }

        let init_dir = media_dir.clone();
        let init_free = tokio::task::spawn_blocking(move || {
            crate::platform::free_disk_space(&init_dir).unwrap_or(-1)
        })
        .await
        .unwrap_or(-1);

        let e = Self {
            session,
            media_dir,
            trackers,
            store,
            http,
            allow_private,
            max_cache_bytes: Arc::new(AtomicI64::new(cfg.cache_max_bytes)),
            reserve_bytes: cfg.cache_reserve_bytes,
            idle_timeout: Arc::new(AtomicI64::new(cfg.idle_timeout.as_nanos() as i64)),
            max_torrents: cfg.max_torrents,
            info_timeout: cfg.info_timeout,
            act: Arc::new(RwLock::new(initial_act)),
            add_locks: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            sweep_mu: Arc::new(tokio::sync::Mutex::new(())),
            next_reader_id: Arc::new(AtomicU64::new(1)),
            stats: Arc::new(RwLock::new(crate::cache::CacheStats::default())),
            last_free_bytes: Arc::new(AtomicI64::new(init_free)),
            evicted_bytes: Arc::new(AtomicI64::new(0)),
            evicted_count: Arc::new(AtomicI64::new(0)),
        };
        e.spawn_janitor();
        Ok(e)
    }

    pub fn readahead_handle(&self) -> Arc<AtomicI64> {
        self.session.readahead_handle()
    }

    pub fn set_max_cache(&self, v: i64) {
        self.max_cache_bytes.store(v, Ordering::Relaxed);
    }

    pub fn set_idle_timeout(&self, d: Duration) {
        self.idle_timeout
            .store(d.as_nanos() as i64, Ordering::Relaxed);
    }

    pub fn idle_timeout_dur(&self) -> Duration {
        let n = self.idle_timeout.load(Ordering::Relaxed);
        if n <= 0 {
            Duration::from_secs(20 * 60)
        } else {
            Duration::from_nanos(n as u64)
        }
    }

    fn spawn_janitor(&self) {
        let eng = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            let mut sweep_counter = 0;
            loop {
                tick.tick().await;
                let now = SystemTime::now();
                reap_idle(&eng.session, &eng.act, &eng.add_locks, &eng.store, &eng.idle_timeout, now)
                    .await;
                let dir = eng.media_dir.clone();
                if let Ok(Ok(free)) =
                    tokio::task::spawn_blocking(move || crate::platform::free_disk_space(&dir)).await
                {
                    eng.last_free_bytes.store(free, Ordering::Relaxed);
                }
                sweep_counter += 1;
                if sweep_counter >= 3 {
                    sweep_counter = 0;
                    eng.sweep_cache().await;
                }
            }
        });
    }

    pub async fn sweep_cache(&self) -> (i64, i64) {
        let now = SystemTime::now();
        sweep_cache_once(
            &self.sweep_mu,
            &self.session,
            &self.media_dir,
            &self.store,
            &self.act,
            &self.stats,
            &self.last_free_bytes,
            &self.evicted_bytes,
            &self.evicted_count,
            &self.max_cache_bytes,
            self.reserve_bytes,
            now,
            false,
        )
        .await
    }

    pub async fn purge_inactive_cache(&self) -> (i64, i64) {
        let now = SystemTime::now();
        sweep_cache_once(
            &self.sweep_mu,
            &self.session,
            &self.media_dir,
            &self.store,
            &self.act,
            &self.stats,
            &self.last_free_bytes,
            &self.evicted_bytes,
            &self.evicted_count,
            &self.max_cache_bytes,
            self.reserve_bytes,
            now,
            true,
        )
        .await
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
        m.entry(ih.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Click-triggered playback path (zero preloading before click):
    /// schedules cache cleanup in background when near budget, starts magnet peers+DHT+trackers
    /// unpaused immediately, and concurrently races `.torrent` HTTP URL fetches to bypass `ut_metadata` RTTs.
    pub async fn ensure_play(
        &self,
        ih: &str,
        name: &str,
        extra: &[String],
        torrent_urls: &[String],
    ) -> anyhow::Result<ManagedTorrentHandle> {
        if !crate::util::valid_infohash(ih) {
            anyhow::bail!("invalid infohash");
        }
        let ih = ih.to_lowercase();

        // Trigger async background cache sweep when close to budget/free floor — never block `/play`.
        let free = self.last_free_bytes.load(Ordering::Relaxed);
        let used = self.stats.read().await.used_bytes;
        let budget = self.max_cache_bytes.load(Ordering::Relaxed);
        let effective_reserve = if self.reserve_bytes > 0 {
            self.reserve_bytes
        } else {
            crate::config::HEALTH_FREE_FLOOR
        };
        if (budget > 0 && used > budget) || (free >= 0 && free < effective_reserve) {
            let eng = self.clone();
            tokio::spawn(async move {
                let _ = eng.sweep_cache().await;
            });
        }

        self.touch(&ih, name).await;
        let id20 = hex_to_id20(&ih).ok_or_else(|| anyhow::anyhow!("invalid infohash"))?;

        let timeout_dur = if self.info_timeout.is_zero() {
            Duration::from_secs(45)
        } else {
            self.info_timeout
        };

        if let Some(handle) = self.session.get(&id20) {
            handle.unpause();
            if handle.files().is_empty() {
                handle.wait_metadata(timeout_dur).await?;
            }
            return Ok(handle);
        }

        let lk = self.lock_for(&ih).await;
        let _g = lk.lock().await;

        if let Some(handle) = self.session.get(&id20) {
            handle.unpause();
            if handle.files().is_empty() {
                handle.wait_metadata(timeout_dur).await?;
            }
            return Ok(handle);
        }

        let current_count = self.act.read().await.len();
        if self.max_torrents > 0 && current_count >= self.max_torrents && !self.evict_oldest().await
        {
            anyhow::bail!("too many active torrents; try again shortly");
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

        let magnet = crate::indexers::magnet_uri(&ih, name, &trs);
        let dir = self.media_dir.join(&ih);
        let handle = match self
            .session
            .add_magnet_or_hash(&magnet, &trs, dir.clone(), false)
            .await
        {
            Ok(h) => h,
            Err(e) => {
                let mut m = self.act.write().await;
                if let Some(a) = m.get_mut(&ih) {
                    a.adding = false;
                }
                return Err(e);
            }
        };

        // Race direct `.torrent` URL downloads against `ut_metadata` strictly on user click.
        if handle.files().is_empty() && !torrent_urls.is_empty() {
            for raw_u in torrent_urls.iter().take(3) {
                let Ok(parsed_u) = url::Url::parse(raw_u) else {
                    continue;
                };
                if !crate::netguard::url_allowed(&parsed_u, self.allow_private) {
                    continue;
                }
                let http = self.http.clone();
                let session = self.session.clone();
                let trs_c = trs.clone();
                let dir_c = dir.clone();
                tokio::spawn(async move {
                    let Ok(resp) = http.get(parsed_u).timeout(TORRENT_FETCH_TIMEOUT).send().await
                    else {
                        return;
                    };
                    if !resp.status().is_success() {
                        return;
                    }
                    if let Ok(bytes) = crate::server::read_body_limited(resp, MAX_TORRENT_DOWNLOAD_BYTES).await {
                        let _ = session
                            .add_torrent_bytes(&bytes, Some(id20), &trs_c, dir_c, false)
                            .await;
                    }
                });
            }
        }

        let wait_res = handle.wait_metadata(timeout_dur).await;
        {
            let mut m = self.act.write().await;
            if let Some(a) = m.get_mut(&ih) {
                a.adding = false;
            }
        }

        if let Err(e) = wait_res {
            let no_readers = {
                let mut m = self.act.write().await;
                let empty = m
                    .get(&ih)
                    .map(|a| a.active_readers.is_empty())
                    .unwrap_or(true);
                if empty {
                    m.remove(&ih);
                }
                empty
            };
            if no_readers {
                self.session.delete(&id20);
            }
            return Err(anyhow::anyhow!("waiting for metadata: {e}"));
        }

        Ok(handle)
    }

    #[cfg(test)]
    pub async fn add_torrent_bytes(
        &self,
        bytes: bytes::Bytes,
    ) -> anyhow::Result<ManagedTorrentHandle> {
        let dir = self.media_dir.join(format!("test_{}", rand::random::<u32>()));
        let res = self
            .session
            .add_torrent_bytes(&bytes, None, &[], dir, false)
            .await?;
        let ih = res.info_hash().as_string();
        let label = res.stats().name;
        self.touch(&ih, &label).await;
        Ok(res)
    }

    async fn evict_oldest(&self) -> bool {
        let oldest = {
            let m = self.act.read().await;
            let mut oldest: Option<String> = None;
            let mut when: Option<SystemTime> = None;
            for (ih, a) in m.iter() {
                if !a.active_readers.is_empty() || a.adding {
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
        let Some(oldest) = oldest else {
            return false;
        };
        self.act.write().await.remove(&oldest);
        self.add_locks.lock().await.remove(&oldest);
        if let Some(id20) = hex_to_id20(&oldest) {
            if self.session.get(&id20).is_some() {
                self.session.delete(&id20);
                return true;
            }
        }
        false
    }

    pub async fn record_media(&self, handle: &ManagedTorrentHandle) {
        let files = handle.files();
        if files.is_empty() {
            return;
        }
        let st = handle.stats();
        let ih = handle.info_hash().as_string();
        let mut files_out = vec![];
        for f in &files {
            if f.is_padding || !crate::relevance::is_video_path(&f.name) {
                continue;
            }
            files_out.push(crate::store::CachedFile {
                idx: f.idx,
                path: f.name.clone(),
                size: f.len as i64,
            });
        }
        let _ = self
            .store
            .save_media(&ih, &st.name, st.total_bytes as i64, &files_out);
    }

    pub async fn views(&self) -> Vec<TorrentView> {
        let keys: Vec<String> = self.act.read().await.keys().cloned().collect();
        let now = SystemTime::now();
        let mut out = Vec::new();
        for ih in keys {
            let Some(id20) = hex_to_id20(&ih) else {
                continue;
            };
            let Some(handle) = self.session.get(&id20) else {
                continue;
            };
            let (readers, idle_s, label) = {
                let m = self.act.read().await;
                if let Some(a) = m.get(&ih) {
                    (
                        a.active_readers.len() as i32,
                        now.duration_since(a.last_access)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0),
                        a.label.clone(),
                    )
                } else {
                    (0, 0, String::new())
                }
            };
            let stats = handle.stats();
            let name = if stats.name.is_empty() || stats.name == ih {
                if label.is_empty() { ih.clone() } else { label }
            } else {
                stats.name
            };
            let size = stats.total_bytes as i64;
            let completed = stats.downloaded_bytes as i64;
            let progress = if size > 0 {
                (completed as f64 / size as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };
            out.push(TorrentView {
                infohash: ih,
                name,
                size,
                completed,
                progress,
                peers: stats.peers,
                seeders: stats.seeders,
                rate: stats.rate_bps as f64,
                readers,
                idle_seconds: idle_s,
            });
        }
        out.sort_by_key(|a| a.idle_seconds);
        out
    }

    pub async fn cache_stats(&self) -> crate::cache::CacheStats {
        let dir = self.media_dir.clone();
        let entries =
            tokio::task::spawn_blocking(move || crate::cache::scan_cache(&dir).unwrap_or_default())
                .await
                .unwrap_or_default();
        let mut used = 0i64;
        let mut partial = 0i64;
        for e in &entries {
            used += e.bytes;
            let mut is_part = e.partial;
            if !is_part {
                if let Some(sz) = self.store.media_size(&e.name) {
                    if sz > 0 && e.bytes < sz {
                        is_part = true;
                    }
                }
            }
            if is_part {
                partial += e.bytes;
            }
        }
        let mut st = self.stats.read().await.clone();
        st.used_bytes = used;
        st.entries = entries.len();
        st.partial_bytes = partial;
        st.budget_bytes = self.max_cache_bytes.load(Ordering::Relaxed);
        st.reserve_bytes = self.reserve_bytes;
        st.free_bytes = self.disk_free().await;
        st
    }

    pub async fn disk_free(&self) -> i64 {
        let dir = self.media_dir.clone();
        let free = tokio::task::spawn_blocking(move || {
            crate::platform::free_disk_space(&dir).unwrap_or(-1)
        })
        .await
        .unwrap_or(-1);
        if free >= 0 {
            self.last_free_bytes.store(free, Ordering::Relaxed);
        }
        free
    }

    pub fn torrent_count(&self) -> usize {
        self.act.try_read().map(|m| m.len()).unwrap_or(0)
    }

    pub async fn pause_others(&self, current_ih: &str) {
        let now = SystemTime::now();
        let mut stale_to_clear = Vec::new();
        let idle_hashes: Vec<String> = {
            let act = self.act.read().await;
            act.iter()
                .filter_map(|(ih, a)| {
                    if ih.as_str() == current_ih {
                        return None;
                    }
                    if a.active_readers.is_empty() {
                        return Some(ih.clone());
                    }
                    let inactive =
                        now.duration_since(a.last_access).unwrap_or_default() >= Duration::from_secs(8);
                    if inactive {
                        stale_to_clear.push(ih.clone());
                        Some(ih.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };

        if !stale_to_clear.is_empty() {
            let mut act = self.act.write().await;
            for ih in &stale_to_clear {
                if let Some(a) = act.get_mut(ih) {
                    tracing::info!(
                        "stream switch: clearing stale reader for other torrent {}",
                        &ih[..8.min(ih.len())]
                    );
                    a.active_readers.clear();
                }
            }
        }

        for ih in idle_hashes {
            if let Some(id20) = hex_to_id20(&ih) {
                if let Some(handle) = self.session.get(&id20) {
                    if !handle.stats().paused {
                        tracing::info!(
                            "pausing idle torrent {} (stream-first)",
                            &ih[..8.min(ih.len())]
                        );
                        handle.pause();
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
                m.get(&ih).map(|a| a.active_readers.len()).unwrap_or(0)
            };
            if readers == 0 {
                if let Some(id20) = hex_to_id20(&ih) {
                    if let Some(handle) = session.get(&id20) {
                        if !handle.stats().paused {
                            tracing::info!(
                                "halted torrent {} after disconnect (0 readers)",
                                &ih[..8.min(ih.len())]
                            );
                            handle.pause();
                        }
                    }
                }
            }
        });
    }

    pub async fn inc_reader(&self, ih: &str) -> u64 {
        self.store.touch_media(ih);
        let id = self.next_reader_id.fetch_add(1, Ordering::Relaxed);
        let now = SystemTime::now();
        let mut m = self.act.write().await;
        let a = m.entry(ih.to_string()).or_insert_with(Activity::default);
        a.active_readers.insert(id, now);
        a.last_access = now;
        id
    }

    pub async fn dec_reader(&self, ih: &str, reader_id: u64) {
        self.store.touch_media(ih);
        let (zero_readers, ih_owned) = {
            let mut m = self.act.write().await;
            if let Some(a) = m.get_mut(ih) {
                a.active_readers.remove(&reader_id);
                a.last_access = SystemTime::now();
                (a.active_readers.is_empty(), ih.to_string())
            } else {
                (false, String::new())
            }
        };
        if zero_readers {
            self.schedule_pause_if_idle(ih_owned, Duration::from_secs(5));
        }
    }

    pub async fn touch_reader(
        &self,
        ih: &str,
        reader_id: u64,
        closed: &std::sync::atomic::AtomicBool,
    ) {
        {
            let mut m = self.act.write().await;
            if closed.load(Ordering::SeqCst) {
                return;
            }
            let now = SystemTime::now();
            if let Some(a) = m.get_mut(ih) {
                a.last_access = now;
                a.active_readers.insert(reader_id, now);
            }
        }
        self.store.touch_media(ih);
        if let Some(id20) = hex_to_id20(ih) {
            self.session.unpause(&id20);
        }
    }
}

async fn migrate_cache_root(root: &Path, media: &Path) -> anyhow::Result<()> {
    let mut rd = tokio::fs::read_dir(root).await?;
    while let Some(ent) = rd.next_entry().await? {
        let name = ent.file_name().to_string_lossy().to_string();
        if name == "media"
            || name == "ADMIN_PASSWORD"
            || name == "dht.json"
            || name.starts_with("dht.json.")
            || crate::cache::cache_skip(&name)
        {
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
    session: &BtSession,
    act: &Arc<RwLock<HashMap<String, Activity>>>,
    add_locks: &AddLocksMap,
    store: &Arc<crate::store::Store>,
    idle: &Arc<AtomicI64>,
    now: SystemTime,
) {
    let keys: Vec<String> = act.read().await.keys().cloned().collect();
    for ih in keys {
        let Some(id20) = hex_to_id20(&ih) else {
            continue;
        };
        let Some(handle) = session.get(&id20) else {
            act.write().await.remove(&ih);
            continue;
        };
        let stats = handle.stats();
        let (mut readers, adding, last, warned) = {
            let m = act.read().await;
            m.get(&ih)
                .map(|a| (a.active_readers.len() as i32, a.adding, a.last_access, a.warned_no_peers))
        }
        .unwrap_or_else(|| {
            let last_access = store
                .media_last_used(&ih)
                .filter(|&ts| ts > 0)
                .map(|ts| std::time::UNIX_EPOCH + Duration::from_secs(ts as u64))
                .unwrap_or(now);
            (0, false, last_access, false)
        });

        let inactive_dur = now.duration_since(last).unwrap_or_default();
        if readers > 0 && inactive_dur >= Duration::from_secs(45) {
            tracing::info!(
                "clearing {readers} stale reader(s) for {} (inactive for {inactive_dur:?})",
                &ih[..8.min(ih.len())]
            );
            {
                let mut m = act.write().await;
                if let Some(a) = m.get_mut(&ih) {
                    a.active_readers.clear();
                }
            }
            readers = 0;
            handle.pause();
        }
        if readers > 0 && stats.total_bytes > 0 && stats.peers == 0 && inactive_dur > Duration::from_secs(15) && !warned {
            tracing::warn!(
                "no peers for {} after {:?}",
                &ih[..8.min(ih.len())],
                inactive_dur
            );
            let mut m = act.write().await;
            if let Some(a) = m.get_mut(&ih) {
                a.warned_no_peers = true;
            }
        }
        let idle_dur = Duration::from_nanos(idle.load(Ordering::Relaxed).max(0) as u64);
        if readers == 0 && !adding && inactive_dur > idle_dur {
            tracing::info!(
                "drop idle torrent {} ({:?} idle)",
                &ih[..8.min(ih.len())],
                inactive_dur
            );
            session.delete(&id20);
            act.write().await.remove(&ih);
        }
    }
    let m = act.read().await;
    let mut locks = add_locks.lock().await;
    locks.retain(|k, v| Arc::strong_count(v) > 1 || m.contains_key(k));
}

#[allow(clippy::too_many_arguments)]
async fn sweep_cache_once(
    sweep_mu: &Arc<tokio::sync::Mutex<()>>,
    session: &BtSession,
    data_dir: &Path,
    store: &Arc<crate::store::Store>,
    act: &Arc<RwLock<HashMap<String, Activity>>>,
    stats: &Arc<RwLock<crate::cache::CacheStats>>,
    last_free: &Arc<AtomicI64>,
    ev_b: &Arc<AtomicI64>,
    ev_c: &Arc<AtomicI64>,
    max_cache: &Arc<AtomicI64>,
    reserve: i64,
    now: SystemTime,
    force_purge_inactive: bool,
) -> (i64, i64) {
    let _sweep_guard = sweep_mu.lock().await;
    let scan_dir = data_dir.to_path_buf();
    let entries = match tokio::task::spawn_blocking(move || crate::cache::scan_cache(&scan_dir)).await
    {
        Ok(Ok(e)) => e,
        Ok(Err(e)) => {
            tracing::warn!("cache: scan {}: {e}", data_dir.display());
            return (0, 0);
        }
        Err(e) => {
            tracing::warn!("cache: scan join {}: {e}", data_dir.display());
            return (0, 0);
        }
    };

    let active_keys: Vec<String> = act.read().await.keys().cloned().collect();
    let mut owned = std::collections::HashSet::new();
    let mut pending = std::collections::HashSet::new();
    let mut stats_map = std::collections::HashMap::new();
    for ih in active_keys {
        if let Some(id20) = hex_to_id20(&ih) {
            if let Some(handle) = session.get(&id20) {
                if !handle.files().is_empty() {
                    let st = handle.stats();
                    let finished = st.total_bytes > 0 && st.downloaded_bytes >= st.total_bytes;
                    owned.insert(ih.clone());
                    stats_map.insert(ih, (finished, st.downloaded_bytes as i64, st.total_bytes as i64));
                } else if store.media_name(&ih).is_some() {
                    pending.insert(ih);
                }
            }
        }
    }

    let mut units: Vec<crate::cache::CacheUnit> = vec![];
    let mut used = 0i64;
    let mut partial_bytes = 0i64;
    let mut orphan_bytes = 0i64;
    let mut orphan_count = 0usize;
    let mut loaded = 0usize;
    for en in &entries {
        used += en.bytes;
        let is_ih = en.name.len() == 40 && en.name.chars().all(|c| c.is_ascii_hexdigit());
        let mut is_partial = en.partial;
        if is_ih {
            if let Some((finished, prog, total)) = stats_map.get(&en.name) {
                if !*finished || (*total > 0 && *prog < *total) {
                    is_partial = true;
                }
            } else if let Some(expected_size) = store.media_size(&en.name) {
                if expected_size > 0 && en.bytes < expected_size {
                    is_partial = true;
                }
            }
        }
        if is_partial {
            partial_bytes += en.bytes;
        }
        let mut u = crate::cache::CacheUnit {
            entry: crate::cache::CacheEntry {
                name: en.name.clone(),
                path: en.path.clone(),
                bytes: en.bytes,
                modified: en.modified,
                partial: is_partial,
            },
            infohash: if is_ih { en.name.clone() } else { String::new() },
            loaded: false,
            orphan: false,
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
                    Some(a) => (a.active_readers.len() as i32, a.last_access),
                    None => (0, en.modified),
                }
            };
            u.readers = readers;
            if last > u.last_use {
                u.last_use = last;
            }
        } else if pending.contains(&en.name) {
            continue;
        } else if !is_ih || store.media_name(&en.name).is_none() {
            u.orphan = true;
            orphan_count += 1;
            orphan_bytes += en.bytes;
        }
        units.push(u);
    }

    let free_dir = data_dir.to_path_buf();
    let free = tokio::task::spawn_blocking(move || {
        crate::platform::free_disk_space(&free_dir).unwrap_or(-1)
    })
    .await
    .unwrap_or(-1);
    if free >= 0 {
        last_free.store(free, Ordering::Relaxed);
    }
    let plan = crate::cache::plan_cache(
        &units,
        used,
        max_cache.load(Ordering::Relaxed),
        reserve,
        free,
        now,
        force_purge_inactive,
    );
    let mut freed = 0i64;
    let mut freed_count = 0i64;
    let mut rem_orphans = 0usize;
    let mut rem_orphan_bytes = 0i64;
    let mut rem_partial = 0i64;
    let mut rem_loaded = 0usize;
    for (idx, reason) in plan {
        let u = &units[idx];
        if u.loaded {
            if let Some(id20) = hex_to_id20(&u.infohash) {
                session.delete(&id20);
            }
        }
        if crate::platform::remove_all_async(&u.entry.path).await.is_err() {
            tracing::warn!("cache: remove {}", u.entry.name);
            continue;
        }
        if u.loaded {
            rem_loaded += 1;
        }
        if u.orphan {
            rem_orphans += 1;
            rem_orphan_bytes += u.entry.bytes;
        }
        if !u.infohash.is_empty() {
            act.write().await.remove(&u.infohash);
        }
        if u.entry.partial {
            rem_partial += u.entry.bytes;
        }
        store.forget_media(&u.infohash, &u.entry.name);
        freed += u.entry.bytes;
        freed_count += 1;
        if reason != "retention" && reason != "idle" {
            tracing::info!(
                "cache: evicted {} ({}, {reason})",
                u.entry.name,
                crate::util::human_bytes(u.entry.bytes)
            );
        } else {
            tracing::info!(
                "cache: auto-evicted idle {} ({})",
                u.entry.name,
                crate::util::human_bytes(u.entry.bytes)
            );
        }
    }
    let free_dir_after = data_dir.to_path_buf();
    let free_after =
        tokio::task::spawn_blocking(move || crate::platform::free_disk_space(&free_dir_after).ok())
            .await
            .ok()
            .flatten()
            .unwrap_or(if free >= 0 { free + freed } else { -1 });
    if free_after >= 0 {
        last_free.store(free_after, Ordering::Relaxed);
    }
    let mut st = crate::cache::CacheStats {
        used_bytes: (used - freed).max(0),
        budget_bytes: max_cache.load(Ordering::Relaxed),
        reserve_bytes: reserve,
        free_bytes: free_after,
        entries: entries.len().saturating_sub(freed_count as usize),
        partial_bytes: (partial_bytes - rem_partial).max(0),
        orphan_bytes: (orphan_bytes - rem_orphan_bytes).max(0),
        orphan_entries: orphan_count.saturating_sub(rem_orphans),
        loaded_entries: loaded.saturating_sub(rem_loaded),
        last_sweep_unix: SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        evicted_bytes: 0,
        evicted_entries: 0,
    };
    let nb = ev_b.fetch_add(freed, Ordering::Relaxed) + freed;
    let nc = ev_c.fetch_add(freed_count, Ordering::Relaxed) + freed_count;
    st.evicted_bytes = nb;
    st.evicted_entries = nc;
    *stats.write().await = st.clone();
    if freed_count > 0 {
        tracing::info!(
            "cache: reclaimed {} in {freed_count} entries (used {} / budget {}, free {})",
            crate::util::human_bytes(freed),
            crate::util::human_bytes(st.used_bytes),
            crate::util::human_bytes(max_cache.load(Ordering::Relaxed)),
            crate::util::human_bytes(free_after)
        );
    }
    (freed, freed_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_engine_init() {
        let temp_dir =
            std::env::temp_dir().join(format!("bitlab_test_{}", rand::random::<u64>()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let cfg = crate::config::Config {
            addr: "127.0.0.1:0".to_string(),
            data_dir: temp_dir.to_string_lossy().to_string(),
            base_url: "".to_string(),
            trackers: vec![],
            cache_max_bytes: 10 * 1024 * 1024,
            cache_reserve_bytes: 1024 * 1024,
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
        let temp_dir =
            std::env::temp_dir().join(format!("bitlab_test_{}", rand::random::<u64>()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let cfg = crate::config::Config {
            addr: "127.0.0.1:0".to_string(),
            data_dir: temp_dir.to_string_lossy().to_string(),
            base_url: "".to_string(),
            trackers: vec![],
            cache_max_bytes: 10 * 1024 * 1024,
            cache_reserve_bytes: 1024 * 1024,
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

        let ih = "0123456789abcdef0123456789abcdef01234567";
        let res = engine.ensure_play(ih, "Test Torrent", &[], &[]).await;
        assert!(res.is_err(), "expected timeout or error for nonexistent magnet");

        let dummy = bytes::Bytes::from_static(
            b"d4:infod6:lengthi10e4:name8:test.bin12:piece lengthi16384e6:pieces20:12345678901234567890ee",
        );
        let handle = engine.add_torrent_bytes(dummy).await.unwrap();
        let actual_ih = handle.info_hash().as_string();
        assert_eq!(engine.torrent_count(), 1);

        let views = engine.views().await;
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].infohash, actual_ih);

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_engine_ensure_play_http_torrent_race() {
        use sha1::Digest;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let temp_dir =
            std::env::temp_dir().join(format!("bitlab_test_race_{}", rand::random::<u64>()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let cfg = crate::config::Config {
            addr: "127.0.0.1:0".to_string(),
            data_dir: temp_dir.to_string_lossy().to_string(),
            base_url: "".to_string(),
            trackers: vec![],
            cache_max_bytes: 10 * 1024 * 1024,
            cache_reserve_bytes: 1024 * 1024,
            idle_timeout: Duration::from_secs(60),
            info_timeout: Duration::from_secs(3),
            readahead_bytes: 1024 * 1024,
            torrent_port: 0,
            max_torrents: 5,
            admin_password: "".to_string(),
            trust_proxy: false,
        };
        let store = Arc::new(crate::store::Store::open(&temp_dir.join("store.db")).unwrap());
        let mut engine = Engine::new(&cfg, store).await.unwrap();
        engine.allow_private = true;
        engine.http = crate::netguard::build_client(true);

        let info_dict = b"d6:lengthi10e4:name8:test.bin12:piece lengthi16384e6:pieces20:12345678901234567890e";
        let mut raw_torrent = Vec::from(&b"d4:info"[..]);
        raw_torrent.extend_from_slice(info_dict);
        raw_torrent.push(b'e');
        let ih_hex = hex::encode(sha1::Sha1::digest(info_dict));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = raw_torrent.clone();
        let srv = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut req_buf = [0u8; 1024];
                let _ = sock.read(&mut req_buf).await;
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/x-bittorrent\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(hdr.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.shutdown().await;
            }
        });

        let url = format!("http://{addr}/sample.torrent");
        let handle = engine
            .ensure_play(&ih_hex, "Test Race", &[], &[url])
            .await
            .expect("http .torrent race should resolve metadata without peers");
        assert_eq!(handle.info_hash().as_string(), ih_hex);
        let files = handle.files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "test.bin");
        assert_eq!(files[0].len, 10);

        srv.abort();
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }
}
