use std::time::Duration;

pub const VERSION: &str = "2.9.16";
pub const HEALTH_FREE_FLOOR: i64 = 200 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Config {
    pub addr: String,
    pub data_dir: String,
    pub base_url: String,
    pub trackers: Vec<String>,
    pub cache_max_bytes: i64,
    pub cache_reserve_bytes: i64,
    pub idle_timeout: Duration,
    pub info_timeout: Duration,
    pub readahead_bytes: i64,
    pub torrent_port: u16,
    pub max_torrents: usize,
    pub admin_password: String,
    pub trust_proxy: bool,
}

pub fn default_trackers() -> Vec<String> {
    vec![
        "http://nyaa.tracker.wf:7777/announce",
        "http://open.tracker.cl:1337/announce",
        "http://tracker.qu.ax:6969/announce",
        "http://tracker.dler.org:6969/announce",
        "http://tracker.renfei.net:8080/announce",
        "https://open.ftorrent.com:443/announce",
        "https://1.tracker.eu.org:443/announce",
        "https://tracker.foreverpirates.co:443/announce",
        "http://tracker.opentrackr.org:1337/announce",
        "udp://tracker.opentrackr.org:1337/announce",
        "udp://open.stealth.si:80/announce",
        "udp://tracker.torrent.eu.org:451/announce",
        "udp://exodus.desync.com:6969/announce",
        "udp://open.demonii.com:1337/announce",
        "udp://explodie.org:6969/announce",
        "udp://open.tracker.cl:1337/announce",
        "udp://tracker.qu.ax:6969/announce",
        "udp://tracker.corpscorp.online:80/announce",
        "udp://tracker.bittor.pw:1337/announce",
        "udp://tracker.ducks.party:1984/announce",
        "udp://tracker-udp.gbitt.info:80/announce",
        "udp://tracker.nyaa.vc:6969/announce",
        "udp://tracker.dler.org:6969/announce",
        "udp://tracker.torrents.observer:80/announce",
        "udp://open.ftorrent.com:443/announce",
        "udp://opentracker.lain.moscow:6969/announce",
        "udp://tracker.breizh.pm:6969/announce",
    ]
    .into_iter()
    .map(|s| s.to_string())
    .collect()
}

fn env_str(k: &str, def: &str) -> String {
    std::env::var(k).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| def.to_string())
}

fn env_int(k: &str, def: i64) -> i64 {
    std::env::var(k).ok().and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(def)
}

fn env_int_clamped(k: &str, def: i64, lo: i64, hi: i64) -> i64 {
    let n = env_int(k, def);
    n.clamp(lo, hi)
}

pub fn load_config() -> Config {
    let data_dir = env_str("BITLAB_DATA", "./data");
    let mut cfg = Config {
        addr: env_str("BITLAB_ADDR", ":8090"),
        data_dir: data_dir.clone(),
        base_url: env_str("BITLAB_BASE_URL", ""),
        trackers: vec![],
        cache_max_bytes: env_int_clamped("BITLAB_CACHE_GB", 40, 0, 1_000_000).saturating_mul(1_000_000_000),
        cache_reserve_bytes: env_int_clamped("BITLAB_CACHE_RESERVE_GB", 2, 0, 1_000_000).saturating_mul(1_000_000_000),
        idle_timeout: Duration::from_secs((env_int_clamped("BITLAB_IDLE_MINUTES", 20, 1, 7 * 24 * 60) as u64) * 60),
        info_timeout: Duration::from_secs(env_int_clamped("BITLAB_INFO_TIMEOUT", 45, 5, 600) as u64),
        readahead_bytes: env_int_clamped("BITLAB_READAHEAD_MB", 64, 1, 1024) << 20,
        torrent_port: env_int_clamped("BITLAB_TORRENT_PORT", 0, 0, 65535) as u16,
        max_torrents: env_int_clamped("BITLAB_MAX_TORRENTS", 40, 1, 500) as usize,
        admin_password: env_str("BITLAB_ADMIN_PASSWORD", ""),
        trust_proxy: std::env::var("BITLAB_TRUST_PROXY").ok().as_deref() == Some("1"),
    };
    if let Ok(t) = std::env::var("BITLAB_TRACKERS") {
        if !t.trim().is_empty() {
            for s in t.split(',') {
                let s = s.trim();
                if !s.is_empty() {
                    cfg.trackers.push(s.to_string());
                }
            }
        }
    }
    if cfg.trackers.is_empty() {
        cfg.trackers = default_trackers();
    }
    cfg
}
