use axum::response::Response;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub const ADMIN_HTML: &str = include_str!("../web/admin.html");
pub const FAVICON_SVG: &str = include_str!("../web/favicon.svg");

#[derive(Clone)]
pub struct App {
    pub cfg: Arc<crate::config::Config>,
    pub store: Arc<crate::store::Store>,
    pub engine: Arc<crate::engine::Engine>,
    pub http: reqwest::Client,
    pub start: SystemTime,
    pub session_key: Vec<u8>,
    pub readahead: Arc<std::sync::atomic::AtomicI64>,
    pub hints: Arc<dashmap_like::DashMap>,
    pub hint_n: Arc<std::sync::atomic::AtomicI64>,
    pub totp_mu: Arc<tokio::sync::Mutex<()>>,
    pub ix_cache: Arc<dashmap_like::IxCache>,
    pub ep_cache: Arc<dashmap_like::EpCache>,
    pub ep_n: Arc<std::sync::atomic::AtomicI64>,
}

// Minimal DashMap replacements to avoid extra dep (use std RwLock<HashMap>)
pub mod dashmap_like {
use std::collections::HashMap;
    use std::sync::RwLock;
    pub struct DashMap {
        inner: RwLock<HashMap<String, Vec<String>>>,
    }
    impl DashMap {
        pub fn new() -> Self {
            Self { inner: RwLock::new(HashMap::new()) }
        }
        pub fn insert(&self, k: String, v: Vec<String>) {
            self.inner.write().unwrap().insert(k, v);
        }
        pub fn get(&self, k: &str) -> Option<Vec<String>> {
            self.inner.read().unwrap().get(k).cloned()
        }
        pub fn clear(&self) {
            self.inner.write().unwrap().clear();
        }
    }
    pub struct IxCache {
        inner: RwLock<HashMap<String, (Vec<crate::indexers::ResultRow>, SystemTime)>>,
    }
    use std::time::SystemTime;
    impl IxCache {
        pub fn new() -> Self {
            Self { inner: RwLock::new(HashMap::new()) }
        }
        pub fn get(&self, k: &str) -> Option<(Vec<crate::indexers::ResultRow>, SystemTime)> {
            self.inner.read().unwrap().get(k).cloned()
        }
        pub fn insert(&self, k: String, v: (Vec<crate::indexers::ResultRow>, SystemTime)) {
            // bound size: clear if too big
            let mut w = self.inner.write().unwrap();
            if w.len() > 4096 {
                w.clear();
            }
            w.insert(k, v);
        }
        pub fn remove(&self, k: &str) {
            self.inner.write().unwrap().remove(k);
        }
    }
    pub struct EpCache {
        inner: RwLock<HashMap<String, Vec<(i32, i32)>>>,
    }
    impl EpCache {
        pub fn new() -> Self {
            Self { inner: RwLock::new(HashMap::new()) }
        }
        pub fn get(&self, k: &str) -> Option<Vec<(i32, i32)>> {
            self.inner.read().unwrap().get(k).cloned()
        }
        pub fn insert(&self, k: String, v: Vec<(i32, i32)>) {
            let mut w = self.inner.write().unwrap();
            if w.len() > 4096 {
                w.clear();
            }
            w.insert(k, v);
        }
        pub fn clear(&self) {
            self.inner.write().unwrap().clear();
        }
    }
}

impl App {
    pub fn new(cfg: crate::config::Config, store: Arc<crate::store::Store>, engine: Arc<crate::engine::Engine>, session_key: Vec<u8>) -> Self {
        let allow_private = std::env::var("BITLAB_ALLOW_PRIVATE_INDEXERS").as_deref() == Ok("1");
        let http = crate::netguard::build_client(allow_private);
        let ra = cfg.readahead_bytes;
        Self {
            cfg: Arc::new(cfg),
            store,
            engine,
            http,
            start: SystemTime::now(),
            session_key,
            readahead: Arc::new(std::sync::atomic::AtomicI64::new(ra)),
            hints: Arc::new(dashmap_like::DashMap::new()),
            hint_n: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            totp_mu: Arc::new(tokio::sync::Mutex::new(())),
            ix_cache: Arc::new(dashmap_like::IxCache::new()),
            ep_cache: Arc::new(dashmap_like::EpCache::new()),
            ep_n: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            // cfg stored with original values; readahead atomic holds live value
        }
    }

    pub fn base_url(&self, host: &str, tls: bool) -> String {
        if !self.cfg.base_url.is_empty() {
            return self.cfg.base_url.trim_end_matches('/').to_string();
        }
        let scheme = if tls { "https" } else { "http" };
        format!("{scheme}://{host}")
    }

    pub fn play_token(&self, key_id: &str, ih: &str, season: i32, ep: i32, exp: i64) -> String {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.session_key).expect("hmac");
        mac.update(format!("play|{key_id}|{ih}|{season}|{ep}|{exp}").as_bytes());
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, mac.finalize().into_bytes())
    }

    pub fn signed_play_token(&self, key_id: &str, ih: &str, season: i32, ep: i32) -> (String, i64) {
        let exp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) + 24 * 3600;
        (self.play_token(key_id, ih, season, ep, exp), exp)
    }

    pub fn valid_play_token(&self, tk: &str, exp_str: &str, key_id: &str, ih: &str, season: i32, ep: i32) -> bool {
        if tk.is_empty() {
            return false;
        }
        let exp: i64 = match exp_str.parse() {
            Ok(n) => n,
            Err(_) => return false,
        };
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        if exp < now || exp > now + 48 * 3600 {
            return false;
        }
        let want = self.play_token(key_id, ih, season, ep, exp);
        use subtle::ConstantTimeEq;
        want.as_bytes().ct_eq(tk.as_bytes()).into()
    }

    pub fn session_mac(&self, id: &str) -> String {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.session_key).expect("hmac");
        mac.update(id.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    pub fn indexers(&self) -> Vec<crate::indexers::Indexer> {
        crate::indexers::default_indexers()
    }

    pub fn store_episode_list(&self, imdb: &str, eps: Vec<(i32, i32)>) {
        if self.ep_cache.get(imdb).is_none() && self.ep_n.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1 > 4096 {
            self.ep_cache.clear();
            self.ep_n.store(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.ep_cache.insert(imdb.to_string(), eps);
    }

    pub fn fix_episode_number(&self, imdb: &str, season: i32, ep: i32) -> (i32, i32) {
        if ep <= 0 || season <= 0 {
            return (season, ep);
        }
        if let Some(eps) = self.ep_cache.get(imdb) {
            return crate::relevance::remap_episode(&eps, season, ep);
        }
        (season, ep)
    }

    /// Fan-out search across indexers with soft deadline.
    pub async fn search_all(&self, queries: Vec<String>, typ: &str, season: i32, ep: i32, imdb: &str) -> (Vec<crate::indexers::ResultRow>, Vec<String>) {
use std::collections::HashMap;
        let mut queries = queries;
        if queries.len() > 6 {
            queries.truncate(6);
        }
        if queries.is_empty() {
            return (vec![], vec!["empty query".to_string()]);
        }
        let enabled: Vec<_> = self.indexers().into_iter().filter(|ix| ix.enabled).collect();
        if enabled.is_empty() {
            return (vec![], vec!["no indexers configured".to_string()]);
        }
        // launch
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, anyhow::Result<Vec<crate::indexers::ResultRow>>)>();
        let mut launched: HashMap<String, usize> = HashMap::new();
        for ix in &enabled {
            let qs = crate::indexers::queries_for(&ix.kind, &queries);
            for q in qs {
                *launched.entry(ix.name.clone()).or_insert(0) += 1;
                let ix = ix.clone();
                let q = q.clone();
                let typ = typ.to_string();
                let imdb = imdb.to_string();
                let tx = tx.clone();
                let app = self.clone();
                tokio::spawn(async move {
                    let r = app.search_indexer_cached(&ix, &q, &typ, season, ep, &imdb).await;
                    let _ = tx.send((ix.name.clone(), r));
                });
            }
        }
        drop(tx);
        // soft deadline 5s
        let deadline = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(deadline);
        let mut all: Vec<crate::indexers::ResultRow> = vec![];
        let mut errs: Vec<String> = vec![];
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut received: HashMap<String, usize> = HashMap::new();
        let mut done = false;
        while !done {
            tokio::select! {
                _ = &mut deadline => {
                    // drain whatever is already queued without waiting
                    while let Ok((name, r)) = rx.try_recv() {
                        *received.entry(name.clone()).or_insert(0) += 1;
                        match r {
                            Ok(mut v) => {
                                for row in v.drain(..) {
                                    if row.infohash.is_empty() { continue; }
                                    if let Some(&i) = seen.get(&row.infohash) {
                                        if row.seeders > all[i].seeders {
                                            all[i] = row;
                                        }
                                        continue;
                                    }
                                    seen.insert(row.infohash.clone(), all.len());
                                    all.push(row);
                                }
                            }
                            Err(e) => errs.push(format!("{name}: {e}")),
                        }
                    }
                    done = true;
                }
                res = rx.recv() => {
                    match res {
                        None => done = true,
                        Some((name, r)) => {
                            *received.entry(name.clone()).or_insert(0) += 1;
                            match r {
                                Ok(mut v) => {
                                    for row in v.drain(..) {
                                        if row.infohash.is_empty() { continue; }
                                        if let Some(&i) = seen.get(&row.infohash) {
                                            if row.seeders > all[i].seeders {
                                                all[i] = row;
                                            }
                                            continue;
                                        }
                                        seen.insert(row.infohash.clone(), all.len());
                                        all.push(row);
                                    }
                                }
                                Err(e) => errs.push(format!("{name}: {e}")),
                            }
                            // check if all launched received
                            let total_launched: usize = launched.values().sum();
                            let total_received: usize = received.values().sum();
                            if total_received >= total_launched {
                                done = true;
                            }
                        }
                    }
                }
            }
        }
        let mut slow = vec![];
        for (name, n) in &launched {
            if received.get(name).copied().unwrap_or(0) < *n {
                slow.push(name.clone());
            }
        }
        if !slow.is_empty() {
            slow.sort();
            tracing::info!("search: answered without {} (still pending at 5s)", slow.join(", "));
        }
        for r in all.iter_mut() {
            r.score = crate::relevance::score_candidate(
                &crate::relevance::ScoredCandidate { title: r.title.clone(), size: r.size, seeders: r.seeders },
                typ, season, ep,
            );
        }
        if typ == "series" && season > 0 {
            all.retain(|r| crate::relevance::episode_signal(&r.title, season, ep) >= 0);
        }
        all.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        // dropDead
        if all.iter().any(|r| r.seeders > 0) {
            all.retain(|r| r.seeders > 0);
        }
        if all.len() > 40 {
            all.truncate(40);
        }
        (all, errs)
    }

    async fn search_indexer_cached(&self, ix: &crate::indexers::Indexer, q: &str, typ: &str, season: i32, ep: i32, imdb: &str) -> anyhow::Result<Vec<crate::indexers::ResultRow>> {
        let ck = format!("{}|{}|{}|{}|{}|{}|{}", ix.kind, ix.url, typ, season, ep, q.to_lowercase(), imdb);
        if let Some((res, exp)) = self.ix_cache.get(&ck) {
            if SystemTime::now() < exp {
                return Ok(res);
            }
            self.ix_cache.remove(&ck);
        }
        let res = self.search_indexer_uncached(ix, q, typ, season, ep, imdb).await;
        // redact api key from errors
        let res = res.map_err(|e| {
            if ix.api_key.is_empty() {
                e
            } else {
                let mut msg = e.to_string().replace(&ix.api_key, "[redacted]");
                // url-encoded form
                let enc: String = url::form_urlencoded::byte_serialize(ix.api_key.as_bytes()).collect();
                msg = msg.replace(&enc, "[redacted]");
                anyhow::anyhow!(msg)
            }
        })?;
        if !res.is_empty() {
            self.ix_cache.insert(ck, (res.clone(), SystemTime::now() + Duration::from_secs(180)));
        }
        Ok(res)
    }

    async fn search_indexer_uncached(&self, ix: &crate::indexers::Indexer, q: &str, typ: &str, season: i32, ep: i32, imdb: &str) -> anyhow::Result<Vec<crate::indexers::ResultRow>> {
        let timeout = crate::indexers::indexer_timeout(&ix.kind);
        let fut = async {
            match ix.kind.as_str() {
                "nyaa" => crate::indexers::nyaa_search(&self.http, ix, q).await,
                "knaben" => crate::indexers::knaben_search(&self.http, ix, q).await,
                "apibay" => crate::indexers::apibay_search(&self.http, ix, q).await,
                "eztv" => crate::indexers::eztv_search(&self.http, ix, typ, season, ep, imdb).await,
                "yts" => crate::indexers::yts_search(&self.http, ix, typ, imdb, q).await,
                "animetosho" => crate::indexers::animetosho_search(&self.http, ix, q).await,
                "torrentio" => crate::indexers::torrentio_search(&self.http, ix, typ, season, ep, imdb).await,
                "torznab" | "" => crate::indexers::torznab_search(&self.http, ix, q, typ, season, ep, imdb).await,
                k => anyhow::bail!("unknown indexer kind {k:?}"),
            }
        };
        tokio::time::timeout(timeout, fut).await.map_err(|_| anyhow::anyhow!("timeout"))?
    }

    pub async fn resolve_meta(&self, typ: &str, imdb: &str) -> anyhow::Result<(String, i32)> {
        let ck = format!("meta2:{typ}:{imdb}");
        if let Some(v) = self.store.get_cached_meta(&ck) {
            if let Ok(m) = serde_json::from_str::<serde_json::Value>(&v) {
                if let (Some(name), Some(year)) = (m.get("name").and_then(|v| v.as_str()), m.get("year").and_then(|v| v.as_i64())) {
                    if !name.is_empty() {
                        let eps: Vec<(i32, i32)> = m.get("eps").and_then(|e| serde_json::from_value(e.clone()).ok()).unwrap_or_default();
                        self.store_episode_list(imdb, eps);
                        return Ok((name.to_string(), year as i32));
                    }
                }
            }
        }
        let u = format!("https://v3-cinemeta.strem.io/meta/{}/{}.json", typ, url::form_urlencoded::byte_serialize(imdb.as_bytes()).collect::<String>());
        let resp = self.http.get(&u).header("User-Agent", "bitlab/1.0").send().await?;
        if resp.status() != 200 {
            anyhow::bail!("cinemeta http {}", resp.status());
        }
        let body = resp.bytes().await?;
        if body.len() > 16 * 1024 * 1024 {
            anyhow::bail!("meta too large");
        }
        #[derive(Deserialize)]
        struct Payload {
            #[serde(default)]
            meta: Meta,
        }
        #[derive(Deserialize, Default)]
        struct Meta {
            #[serde(default)]
            name: String,
            #[serde(default)]
            year: String,
            #[serde(default, rename = "releaseInfo")]
            release_info: String,
            #[serde(default)]
            videos: Vec<Video>,
        }
        #[derive(Deserialize)]
        struct Video {
            #[serde(default)]
            season: i32,
            #[serde(default)]
            episode: i32,
        }
        let p: Payload = serde_json::from_slice(&body).map_err(|e| anyhow::anyhow!("{e}"))?;
        let name = p.meta.name.trim().to_string();
        if name.is_empty() {
            anyhow::bail!("no metadata for {imdb}");
        }
        let mut ys = p.meta.year.clone();
        if ys.is_empty() {
            ys = p.meta.release_info.clone();
        }
        if ys.len() > 4 {
            ys = ys[..4].to_string();
        }
        let year: i32 = ys.parse().unwrap_or(0);
        let mut eps: Vec<(i32, i32)> = p.meta.videos.into_iter().filter(|v| v.season > 0 && v.episode > 0).map(|v| (v.season, v.episode)).collect();
        eps.sort();
        self.store_episode_list(imdb, eps.clone());
        let b = serde_json::json!({"name": name, "year": year, "eps": eps}).to_string();
        let _ = self.store.cache_meta(&ck, &b, 30 * 24 * 3600);
        Ok((name, year))
    }
}

// http helpers
pub fn json_response(code: u16, v: serde_json::Value) -> Response {
    let body = serde_json::to_string(&v).unwrap_or_else(|_| "{}".to_string());
    Response::builder()
        .status(code)
        .header("content-type", "application/json; charset=utf-8")
        .header("cache-control", "no-store")
        .body(axum::body::Body::from(body))
        .unwrap()
}

pub fn http_error(code: u16, msg: &str) -> Response {
    json_response(code, serde_json::json!({"error": msg}))
}
