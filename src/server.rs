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
    pub allow_private: bool,
    pub start: SystemTime,
    pub session_key: Vec<u8>,
    pub readahead: Arc<std::sync::atomic::AtomicI64>,
    pub hints: Arc<dashmap_like::DashMap>,
    pub torrent_hints: Arc<dashmap_like::DashMap>,
    pub totp_mu: Arc<tokio::sync::Mutex<()>>,
    pub ix_cache: Arc<dashmap_like::IxCache>,
    pub ep_cache: Arc<dashmap_like::EpCache>,
}

pub const MAX_HTTP_BODY: usize = 16 * 1024 * 1024;

pub fn de_null_default<'de, D, T>(de: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(de)?.unwrap_or_default())
}

pub fn de_string_or_num<'de, D>(de: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<serde_json::Value>::deserialize(de)? {
        Some(serde_json::Value::String(s)) => Ok(s),
        Some(serde_json::Value::Number(n)) => Ok(n.to_string()),
        _ => Ok(String::new()),
    }
}

pub async fn read_body_limited(mut resp: reqwest::Response, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
    if let Some(len) = resp.content_length() {
        if len > max_bytes as u64 {
            anyhow::bail!("response too large ({len} bytes)");
        }
    }
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if out.len().saturating_add(chunk.len()) > max_bytes {
            anyhow::bail!("response exceeds {max_bytes} bytes");
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

// Minimal DashMap replacements to avoid extra dep (use std RwLock<HashMap>)
pub mod dashmap_like {
    use std::collections::HashMap;
    use std::sync::RwLock;
    use std::time::SystemTime;

    pub struct DashMap {
        inner: RwLock<HashMap<String, Vec<String>>>,
    }
    impl DashMap {
        pub fn new() -> Self {
            Self { inner: RwLock::new(HashMap::new()) }
        }
        pub fn insert(&self, k: String, v: Vec<String>) {
            let mut w = self.inner.write().unwrap();
            if w.len() >= 8192 && !w.contains_key(&k) {
                w.clear();
            }
            w.insert(k, v);
        }
        pub fn get(&self, k: &str) -> Option<Vec<String>> {
            self.inner.read().unwrap().get(k).cloned()
        }
    }
    pub struct IxCache {
        inner: RwLock<HashMap<String, (Vec<crate::indexers::ResultRow>, SystemTime)>>,
    }
    impl IxCache {
        pub fn new() -> Self {
            Self { inner: RwLock::new(HashMap::new()) }
        }
        pub fn get(&self, k: &str) -> Option<(Vec<crate::indexers::ResultRow>, SystemTime)> {
            self.inner.read().unwrap().get(k).cloned()
        }
        pub fn insert(&self, k: String, v: (Vec<crate::indexers::ResultRow>, SystemTime)) {
            // bound size: drop expired entries first, then clear if still oversized
            let mut w = self.inner.write().unwrap();
            if w.len() >= 4096 && !w.contains_key(&k) {
                let now = SystemTime::now();
                w.retain(|_, (_, exp)| now < *exp);
                if w.len() >= 4096 {
                    w.clear();
                }
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
            if w.len() >= 4096 && !w.contains_key(&k) {
                w.clear();
            }
            w.insert(k, v);
        }
    }
}

impl App {
    pub fn new(cfg: crate::config::Config, store: Arc<crate::store::Store>, engine: Arc<crate::engine::Engine>, session_key: Vec<u8>) -> Self {
        let allow_private = std::env::var("BITLAB_ALLOW_PRIVATE_INDEXERS").as_deref() == Ok("1");
        let http = crate::netguard::build_client(allow_private);
        let readahead = engine.readahead_handle();
        Self {
            cfg: Arc::new(cfg),
            store,
            engine,
            http,
            allow_private,
            start: SystemTime::now(),
            session_key,
            readahead,
            hints: Arc::new(dashmap_like::DashMap::new()),
            torrent_hints: Arc::new(dashmap_like::DashMap::new()),
            totp_mu: Arc::new(tokio::sync::Mutex::new(())),
            ix_cache: Arc::new(dashmap_like::IxCache::new()),
            ep_cache: Arc::new(dashmap_like::EpCache::new()),
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
                        merge_search_batch(&name, r, &mut seen, &mut all, &mut errs);
                    }
                    done = true;
                }
                res = rx.recv() => {
                    match res {
                        None => done = true,
                        Some((name, r)) => {
                            *received.entry(name.clone()).or_insert(0) += 1;
                            merge_search_batch(&name, r, &mut seen, &mut all, &mut errs);
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
        if !ix.url.trim().is_empty() {
            let parsed = url::Url::parse(ix.url.trim()).map_err(|_| anyhow::anyhow!("invalid indexer url"))?;
            if !crate::netguard::url_allowed(&parsed, self.allow_private) {
                anyhow::bail!("indexer host is not allowed");
            }
        }
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
        if typ != "movie" && typ != "series" {
            anyhow::bail!("invalid media type");
        }
        if !crate::addon::is_valid_imdb(imdb) {
            anyhow::bail!("invalid imdb id");
        }
        let ck = format!("meta2:{typ}:{imdb}");
        if let Some(v) = self.store.get_cached_meta(&ck) {
            if let Ok(m) = serde_json::from_str::<serde_json::Value>(&v) {
                if let (Some(name), Some(year)) = (m.get("name").and_then(|v| v.as_str()), m.get("year").and_then(|v| v.as_i64())) {
                    if !name.is_empty() {
                        let eps: Vec<(i32, i32)> = m.get("eps").and_then(|e| serde_json::from_value(e.clone()).ok()).unwrap_or_default();
                        if !eps.is_empty() {
                            self.store_episode_list(imdb, eps);
                        }
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
        let body = read_body_limited(resp, MAX_HTTP_BODY).await?;
        #[derive(Deserialize)]
        struct Payload {
            #[serde(default, deserialize_with = "de_null_default")]
            meta: Meta,
        }
        #[derive(Deserialize, Default)]
        struct Meta {
            #[serde(default, deserialize_with = "de_null_default")]
            name: String,
            #[serde(default, deserialize_with = "de_string_or_num")]
            year: String,
            #[serde(default, rename = "releaseInfo", deserialize_with = "de_string_or_num")]
            release_info: String,
            #[serde(default, deserialize_with = "de_null_default")]
            videos: Vec<Video>,
        }
        #[derive(Deserialize)]
        struct Video {
            #[serde(default, deserialize_with = "de_null_default")]
            season: i32,
            #[serde(default, deserialize_with = "de_null_default")]
            episode: i32,
        }
        let p: Payload = serde_json::from_slice(&body).map_err(|e| anyhow::anyhow!("{e}"))?;
        let name = p.meta.name.trim().to_string();
        if name.is_empty() {
            anyhow::bail!("no metadata for {imdb}");
        }
        let ys = if !p.meta.year.trim().is_empty() {
            p.meta.year.as_str()
        } else {
            p.meta.release_info.as_str()
        };
        let year = extract_year(ys);
        let mut eps: Vec<(i32, i32)> = p.meta.videos.into_iter().filter(|v| v.season > 0 && v.episode > 0).map(|v| (v.season, v.episode)).collect();
        eps.sort();
        if !eps.is_empty() {
            self.store_episode_list(imdb, eps.clone());
        }
        let b = serde_json::json!({"name": name, "year": year, "eps": eps}).to_string();
        let _ = self.store.cache_meta(&ck, &b, 30 * 24 * 3600);
        Ok((name, year))
    }
}

fn merge_search_batch(
    name: &str,
    r: anyhow::Result<Vec<crate::indexers::ResultRow>>,
    seen: &mut std::collections::HashMap<String, usize>,
    all: &mut Vec<crate::indexers::ResultRow>,
    errs: &mut Vec<String>,
) {
    match r {
        Ok(mut v) => {
            for mut row in v.drain(..) {
                let ih = row.infohash.trim().to_lowercase();
                if !crate::util::valid_infohash(&ih) {
                    continue;
                }
                row.infohash = ih.clone();
                if let Some(&i) = seen.get(&ih) {
                    if row.seeders > all[i].seeders {
                        let mut merged_trackers = row.trackers;
                        for tr in all[i].trackers.drain(..) {
                            if !merged_trackers.contains(&tr) && merged_trackers.len() < 32 {
                                merged_trackers.push(tr);
                            }
                        }
                        let fallback_size = all[i].size;
                        let fallback_torrent_url = std::mem::take(&mut all[i].torrent_url);
                        all[i] = crate::indexers::ResultRow {
                            trackers: merged_trackers,
                            size: if row.size > 0 { row.size } else { fallback_size },
                            torrent_url: if !row.torrent_url.is_empty() {
                                row.torrent_url
                            } else {
                                fallback_torrent_url
                            },
                            ..row
                        };
                    } else {
                        for tr in row.trackers {
                            if !all[i].trackers.contains(&tr) && all[i].trackers.len() < 32 {
                                all[i].trackers.push(tr);
                            }
                        }
                        if all[i].size <= 0 && row.size > 0 {
                            all[i].size = row.size;
                        }
                        if all[i].torrent_url.is_empty() && !row.torrent_url.is_empty() {
                            all[i].torrent_url = row.torrent_url;
                        }
                    }
                    continue;
                }
                seen.insert(ih, all.len());
                all.push(row);
            }
        }
        Err(e) => errs.push(format!("{name}: {e}")),
    }
}

fn extract_year(s: &str) -> i32 {
    for w in s.as_bytes().windows(4) {
        if w.iter().all(|b| b.is_ascii_digit()) {
            if let Ok(text) = std::str::from_utf8(w) {
                if let Ok(y) = text.parse::<i32>() {
                    if (1880..=2100).contains(&y) {
                        return y;
                    }
                }
            }
        }
    }
    0
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_year_utf8_safe() {
        assert_eq!(extract_year("2014–2020"), 2014);
        assert_eq!(extract_year("日本語2019年"), 2019);
        assert_eq!(extract_year(""), 0);
        assert_eq!(extract_year("999"), 0);
    }

    #[test]
    fn test_merge_search_batch_combines_trackers_and_size() {
        let mut seen = std::collections::HashMap::new();
        let mut all = Vec::new();
        let mut errs = Vec::new();
        let ih = "0123456789abcdef0123456789abcdef01234567";
        let r1 = crate::indexers::ResultRow {
            title: "Movie 1080p".into(),
            infohash: ih.to_uppercase(),
            size: 1_500_000_000,
            seeders: 5,
            indexer: "ix1".into(),
            trackers: vec!["udp://tracker.opentrackr.org:1337/announce".into()],
            torrent_url: "https://example.org/movie.torrent".into(),
            ..Default::default()
        };
        let r2 = crate::indexers::ResultRow {
            title: "Movie 1080p".into(),
            infohash: ih.into(),
            size: 0,
            seeders: 20,
            indexer: "ix2".into(),
            trackers: vec!["udp://open.stealth.si:80/announce".into()],
            ..Default::default()
        };
        merge_search_batch("ix1", Ok(vec![r1]), &mut seen, &mut all, &mut errs);
        merge_search_batch("ix2", Ok(vec![r2]), &mut seen, &mut all, &mut errs);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].infohash, ih);
        assert_eq!(all[0].seeders, 20);
        assert_eq!(all[0].size, 1_500_000_000);
        assert_eq!(all[0].trackers.len(), 2);
        assert_eq!(all[0].torrent_url, "https://example.org/movie.torrent");
    }
}
