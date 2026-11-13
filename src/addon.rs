use axum::extract::{Path, RawQuery, State};
use axum::http::HeaderMap;
use axum::response::Response;
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

#[derive(Debug, serde::Serialize)]
pub(crate) struct Manifest {
    id: String,
    version: String,
    name: String,
    description: String,
    resources: Vec<String>,
    types: Vec<String>,
    #[serde(rename = "idPrefixes")]
    id_prefixes: Vec<String>,
    catalogs: Vec<serde_json::Value>,
}

#[derive(Debug, serde::Serialize)]
struct Stream {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    url: String,
    #[serde(rename = "behaviorHints", skip_serializing_if = "Option::is_none")]
    behavior_hints: Option<HashMap<String, serde_json::Value>>,
}

pub(crate) fn manifest_for(label: &str, key_id: &str) -> Manifest {
    Manifest {
        id: format!("org.bitlab.{key_id}"),
        version: crate::config::VERSION.to_string(),
        name: "Bitlab".to_string(),
        description: format!("Self-hosted streaming hub. Manifest: {label}"),
        resources: vec!["stream".to_string()],
        types: vec!["movie".to_string(), "series".to_string()],
        id_prefixes: vec!["tt".to_string()],
        catalogs: vec![],
    }
}

async fn auth_key(app: &crate::server::App, token: &str) -> Result<crate::store::Key, Box<Response>> {
    if token.is_empty() {
        return Err(Box::new(crate::server::http_error(401, "missing manifest key")));
    }
    match app.store.key_by_token(token) {
        Ok(k) => {
            app.store.touch_key(&k.id);
            Ok(k)
        }
        Err(crate::store::KeyLookup::NotFound) => Err(Box::new(crate::server::http_error(401, "unknown manifest key"))),
        Err(crate::store::KeyLookup::Paused) => Err(Box::new(crate::server::http_error(403, "this manifest key is paused"))),
        Err(crate::store::KeyLookup::Expired) => Err(Box::new(crate::server::http_error(410, "this manifest key expired"))),
    }
}

pub async fn handle_manifest(
    State(app): State<crate::server::App>,
    Path(token): Path<String>,
) -> Response {
    let k = match auth_key(&app, &token).await {
        Ok(k) => k,
        Err(e) => return *e,
    };
    let m = manifest_for(&k.label, &k.id);
    let body = serde_json::to_string(&m).unwrap();
    Response::builder()
        .status(200)
        .header("content-type", "application/json; charset=utf-8")
        .header("access-control-allow-origin", "*")
        .header("cache-control", "max-age=300")
        .body(axum::body::Body::from(body))
        .unwrap()
}

pub fn parse_media_id(id: &str) -> (String, i32, i32) {
    let id = id.trim_end_matches(".json");
    let parts: Vec<&str> = id.split(':').collect();
    let base = parts.first().map(|s| s.to_string()).unwrap_or_default();
    let mut season = 0;
    let mut ep = 0;
    if parts.len() >= 3 {
        season = parts[1].parse().unwrap_or(0);
        ep = parts[2].parse().unwrap_or(0);
    }
    (base, season, ep)
}

pub(crate) fn is_valid_imdb(s: &str) -> bool {
    s.starts_with("tt")
        && (7..=14).contains(&s.len())
        && s.as_bytes()[2..].iter().all(|b| b.is_ascii_digit())
}

fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '"' | '\\' | '/' | ';') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "video".to_string()
    } else {
        trimmed.to_string()
    }
}

fn release_label(title: &str) -> (String, String) {
    static RE_4K: OnceLock<regex::Regex> = OnceLock::new();
    static RE_1080: OnceLock<regex::Regex> = OnceLock::new();
    static RE_720: OnceLock<regex::Regex> = OnceLock::new();
    static RE_REMUX: OnceLock<regex::Regex> = OnceLock::new();
    static RE_BLURAY: OnceLock<regex::Regex> = OnceLock::new();
    static RE_WEB: OnceLock<regex::Regex> = OnceLock::new();

    let re_4k = RE_4K.get_or_init(|| regex::Regex::new(r"(?i)\b(2160p|4k|uhd)\b").unwrap());
    let re_1080 = RE_1080.get_or_init(|| regex::Regex::new(r"(?i)\b1080p\b").unwrap());
    let re_720 = RE_720.get_or_init(|| regex::Regex::new(r"(?i)\b720p\b").unwrap());
    let re_remux = RE_REMUX.get_or_init(|| regex::Regex::new(r"(?i)\bremux\b").unwrap());
    let re_bluray = RE_BLURAY.get_or_init(|| regex::Regex::new(r"(?i)\b(bluray|blu-ray|bdrip)\b").unwrap());
    let re_web = RE_WEB.get_or_init(|| regex::Regex::new(r"(?i)\b(web-dl|webrip|web)\b").unwrap());

    let q = if re_4k.is_match(title) {
        "2160p"
    } else if re_1080.is_match(title) {
        "1080p"
    } else if re_720.is_match(title) {
        "720p"
    } else {
        "SD"
    };
    let src = if re_remux.is_match(title) {
        "REMUX"
    } else if re_bluray.is_match(title) {
        "BluRay"
    } else if re_web.is_match(title) {
        "WEB"
    } else {
        ""
    };
    let label = format!("{} {}", q, src).trim().to_string();
    (label, q.to_string())
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        s[..s.floor_char_boundary(n)].to_string()
    }
}

pub async fn handle_stream(
    State(app): State<crate::server::App>,
    headers: HeaderMap,
    Path((token, typ, id)): Path<(String, String, String)>,
) -> Response {
    let k = match auth_key(&app, &token).await {
        Ok(k) => k,
        Err(e) => return *e,
    };
    let raw_id = id.trim_end_matches(".json").to_string();
    let (base_id, mut season, mut ep) = parse_media_id(&raw_id);

    if typ != "movie" && typ != "series" {
        return crate::server::json_response(200, serde_json::json!({"streams": []}));
    }
    if !is_valid_imdb(&base_id) {
        return crate::server::json_response(200, serde_json::json!({"streams": []}));
    }
    if typ == "series" && (season <= 0 || ep <= 0) {
        tracing::info!("stream {raw_id:?}: no season/episode to match, refusing to guess");
        return crate::server::json_response(200, serde_json::json!({"streams": []}));
    }

    let ctx = tokio::time::timeout(Duration::from_secs(30), async {
        let title_year = app.resolve_meta(&typ, &base_id).await;
        title_year
    })
    .await;

    let (title, year) = match ctx {
        Ok(Ok(v)) => v,
        _ => {
            tracing::info!("stream {raw_id:?}: meta failed");
            return crate::server::json_response(200, serde_json::json!({"streams": []}));
        }
    };

    let (s2, e2) = app.fix_episode_number(&base_id, season, ep);
    season = s2;
    ep = e2;

    let mut query = title.clone();
    if typ == "series" && season > 0 {
        query = format!("{title} S{season:02}E{ep:02}");
    }

    let mut queries = vec![query.clone()];
    let mut aliases: Vec<String> = vec![];
    let mut play_hints: Vec<String> = vec![];
    if typ == "series" && season > 0 {
        aliases = crate::anilist::anime_aliases(&app, &title).await;
        for (i, a) in aliases.iter().enumerate() {
            if i >= 2 {
                break;
            }
            queries.push(format!("{a} S{season:02}E{ep:02}"));
        }
        if !aliases.is_empty() {
            queries.push(aliases[0].clone());
        }
        queries.push(format!("{title} S{season:02}"));
        if !aliases.is_empty() {
            queries.push(format!("{} S{season:02}", aliases[0]));
        }
    } else if typ == "movie" {
        let forms = crate::relevance::title_forms(&title);
        let mut lookup = title.clone();
        if forms.len() > 1 {
            lookup = forms[1].clone();
            queries.push(crate::relevance::with_year(&forms[1], year));
        }
        let mut al = crate::anilist::anime_aliases(&app, &lookup).await;
        al.extend(forms.iter().skip(1).cloned());
        aliases = al;
        for (i, a) in aliases.iter().enumerate() {
            if i >= 2 {
                break;
            }
            queries.push(crate::relevance::with_year(a, year));
        }
        let clean = crate::relevance::clean_title(&title);
        if !clean.is_empty() && !clean.eq_ignore_ascii_case(&title) {
            queries.push(clean);
        }
        queries.push(crate::relevance::with_year(&title, year));
        let mut all_hints = vec![];
        for h in forms.iter().chain(aliases.iter()) {
            all_hints.extend(crate::relevance::title_forms(h));
        }
        play_hints = crate::relevance::dedupe_strings(all_hints);
    }
    queries = crate::relevance::dedupe_strings(queries);

    // search with 30s ctx
    let (results, errs) = tokio::time::timeout(Duration::from_secs(30), app.search_all(queries.clone(), &typ, season, ep, &base_id))
        .await
        .unwrap_or((vec![], vec!["search timeout".to_string()]));
    for e in &errs {
        tracing::info!("stream {raw_id:?}: {e}");
    }
    // relevance filter
    let mut results = results;
    {
        let triples: Vec<(String, i64, i32)> = results.iter().map(|r| (r.title.clone(), r.size, r.seeders)).collect();
        let (keep, dropped) = crate::relevance::filter_relevant_generic(&triples, &typ, &title, &aliases, year, season);
        if dropped > 0 {
            tracing::info!("stream {raw_id}: dropped {dropped} of {} candidates that do not match {title:?} ({year})", triples.len());
        }
        let mut filtered = vec![];
        for i in keep {
            filtered.push(results[i].clone());
        }
        results = filtered;
    }
    if results.is_empty() {
        tracing::info!("stream {raw_id:?}: no results for {query:?}");
        return crate::server::json_response(200, serde_json::json!({"streams": []}));
    }

    // base url
    let host = headers.get("host").and_then(|v| v.to_str().ok()).unwrap_or("localhost");
    let tls = crate::admin::is_https(&app, &headers);
    let base = app.base_url(host, tls);

    let mut streams = vec![];
    for res in &results {
        let ih = res.infohash.to_lowercase().trim().to_string();
        if !crate::util::valid_infohash(&ih) {
            continue;
        }
        if !res.trackers.is_empty() {
            let mut clean = vec![];
            for tr in &res.trackers {
                if clean.len() >= 20 {
                    break;
                }
                let t = tr.trim();
                // valid_tracker already allows http/https/udp and rejects
                // private/local hosts. Keep UDP trackers: they usually answer
                // faster than HTTP and materially speed up first-peer time.
                if crate::util::valid_tracker(t) {
                    clean.push(t.to_string());
                }
            }
            if !clean.is_empty() {
                app.hints.insert(ih.clone(), clean);
            }
        }
        let (label, q) = release_label(&res.title);
        let src = label.trim_start_matches(&q).trim().to_string();
        let display = if typ == "series" && season > 0 {
            format!("{title} S{season:02}E{ep:02}")
        } else if year > 0 {
            format!("{title} ({year})")
        } else {
            title.clone()
        };
        let (tk, exp) = app.signed_play_token(&k.id, &ih, season, ep);
        let mut link = format!("{base}/k/{}/play/{ih}?n={}&tk={tk}&exp={exp}", k.token, url::form_urlencoded::byte_serialize(truncate(&res.title, 120).as_bytes()).collect::<String>());
        if typ == "series" && season > 0 {
            link.push_str(&format!("&s={season}&e={ep}"));
        }
        if typ == "movie" {
            let hints = if play_hints.is_empty() { crate::relevance::title_forms(&title) } else { play_hints.clone() };
            for (j, h) in hints.iter().enumerate() {
                if j >= 4 {
                    break;
                }
                link.push_str(&format!("&t={}", url::form_urlencoded::byte_serialize(h.as_bytes()).collect::<String>()));
            }
        }
        let mut bits = vec![];
        if !src.is_empty() {
            bits.push(src);
        }
        bits.push(format!("💾 {}", crate::util::human_bytes(res.size)));
        bits.push(format!("👥 {}", res.seeders));
        let detail = bits.join(" · ");
        let mut bh = HashMap::new();
        bh.insert("bingeGroup".to_string(), serde_json::json!(format!("bitlab-{q}")));
        bh.insert("notWebReady".to_string(), serde_json::json!(false));
        streams.push(Stream {
            name: format!("{display} · {q}"),
            title: Some(detail),
            url: link,
            behavior_hints: Some(bh),
        });
    }
    crate::server::json_response(200, serde_json::json!({"streams": streams}))
}

// ---- play ----

pub async fn handle_play(
    State(app): State<crate::server::App>,
    headers: HeaderMap,
    Path((token, ih)): Path<(String, String)>,
    RawQuery(raw_q): RawQuery,
) -> Response {
    handle_play_inner(app, headers, token, ih, raw_q).await
}

pub async fn handle_play_file(
    State(app): State<crate::server::App>,
    headers: HeaderMap,
    Path((token, ih, _fname)): Path<(String, String, String)>,
    RawQuery(raw_q): RawQuery,
) -> Response {
    handle_play_inner(app, headers, token, ih, raw_q).await
}

async fn handle_play_inner(
    app: crate::server::App,
    headers: HeaderMap,
    token: String,
    ih_raw: String,
    raw_q: Option<String>,
) -> Response {
    let k = match auth_key(&app, &token).await {
        Ok(k) => k,
        Err(e) => return *e,
    };
    let mut q: HashMap<String, String> = HashMap::new();
    let mut titles: Vec<String> = Vec::new();
    for (key, val) in url::form_urlencoded::parse(raw_q.as_deref().unwrap_or("").as_bytes()) {
        if key == "t" {
            let v = val.into_owned();
            if !v.is_empty() && titles.len() < 8 {
                titles.push(v);
            }
        } else {
            q.insert(key.into_owned(), val.into_owned());
        }
    }
    let ih = ih_raw.to_lowercase();
    if !crate::util::valid_infohash(&ih) {
        return crate::server::http_error(400, "bad infohash");
    }
    let hint = q.get("n").cloned().unwrap_or_default();
    let mut want_idx: i32 = -1;
    if let Some(f) = q.get("f") {
        if let Ok(n) = f.parse::<i32>() {
            want_idx = n;
        }
    }
    let season_wanted: i32 = q.get("s").and_then(|v| v.parse().ok()).unwrap_or(0);
    let ep_wanted: i32 = q.get("e").and_then(|v| v.parse().ok()).unwrap_or(0);
    let tk = q.get("tk").cloned().unwrap_or_default();
    let exp = q.get("exp").cloned().unwrap_or_default();
    if !app.valid_play_token(&tk, &exp, &k.id, &ih, season_wanted, ep_wanted) {
        return crate::server::http_error(403, "play link expired or not issued for this key — reopen the stream list");
    }
    let extra: Vec<String> = app.hints.get(&ih).unwrap_or_default();
    // Unpaused add: announces + dials peers while metadata resolves.
    let handle = match app.engine.ensure_play(&ih, &hint, &extra).await {
        Ok(h) => h,
        Err(e) => return crate::server::http_error(500, &format!("torrent: {e}")),
    };
    // wait metadata
    let info_timeout = app.cfg.info_timeout;
    let wait_res = tokio::time::timeout(info_timeout, handle.wait_until_initialized()).await;
    if wait_res.is_err() || wait_res.unwrap().is_err() {
        return crate::server::http_error(504, "waiting for metadata: timeout");
    }
    // file list
    let (paths, sizes) = match handle.with_metadata(|meta| {
        let paths: Vec<String> = meta.file_infos.iter().map(|f| f.relative_filename.to_string_lossy().to_string()).collect();
        let sizes: Vec<i64> = meta.file_infos.iter().map(|f| f.len as i64).collect();
        (paths, sizes)
    }) {
        Ok(m) => m,
        Err(_) => return crate::server::http_error(504, "waiting for metadata: unavailable"),
    };
    app.engine.record_media(&handle).await;
    // titles for movie already extracted cleanly from raw query parameters
    let idx_res = if !titles.is_empty() && ep_wanted == 0 {
        crate::relevance::choose_file_for_title(&paths, &sizes, want_idx, &titles)
    } else {
        crate::relevance::choose_file_for_episode(&paths, &sizes, want_idx, season_wanted, ep_wanted)
    };
    let idx = match idx_res {
        Ok(i) => i,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("no file for this episode") {
                tracing::info!("play {} S{season_wanted:02}E{ep_wanted:02}: {e}", &ih[..8.min(ih.len())]);
            }
            return crate::server::http_error(404, &msg);
        }
    };
    let rel = paths[idx].clone();
    let file_len = sizes[idx];

    // Range handling BEFORE incrementing active readers or waking piece streaming
    let range_hdr = headers.get("range").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let (start, end, status, content_len) = match parse_range(&range_hdr, file_len) {
        RangeSpec::Unsatisfiable => {
            if let Some(start) = crate::util::range_start(&range_hdr) {
                if start >= file_len {
                    tracing::info!(
                        "play {}: range starts at {start} but this file is {} — the player is holding an offset from another release",
                        &ih[..8.min(ih.len())],
                        crate::util::human_bytes(file_len)
                    );
                }
            }
            return Response::builder()
                .status(416)
                .header("content-range", format!("bytes */{}", file_len.max(0)))
                .header("accept-ranges", "bytes")
                .header("cache-control", "no-store")
                .body(axum::body::Body::empty())
                .unwrap();
        }
        RangeSpec::Partial(s, e) => (s, e, 206u16, e - s + 1),
        RangeSpec::Full => (0, (file_len - 1).max(0), 200u16, file_len.max(0)),
    };

    let fname = std::path::Path::new(&rel).file_name().and_then(|s| s.to_str()).unwrap_or("video");
    let mut ct = mime_guess::from_path(fname).first_raw().unwrap_or("video/mp4").to_string();
    let disp_name = sanitize_filename(fname);
    if !ct.starts_with("video/") {
        ct = "video/mp4".to_string();
    }

    // readers accounting
    let reader_id = app.engine.inc_reader(&ih).await;
    let closed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let app2 = app.clone();
    let ih2 = ih.clone();

    // Stream-first: immediately pause any background/idle torrents
    app.engine.pause_others(&ih).await;

    // Select only this file to prevent downloading other files/episodes in the swarm
    if let Err(e) = app.engine.select_only_file(&handle, idx).await {
        tracing::warn!("select_only_file({idx}): {e}");
    }

    // Stream directly via librqbit Piece Prioritizer
    let mut stream = match handle.stream(idx).await {
        Ok(s) => s,
        Err(e) => {
            closed.store(true, std::sync::atomic::Ordering::SeqCst);
            app.engine.dec_reader(&ih, reader_id).await;
            return crate::server::http_error(500, &format!("stream: {e}"));
        }
    };

    use tokio::io::AsyncSeekExt;
    if start > 0 {
        if let Err(e) = stream.seek(std::io::SeekFrom::Start(start as u64)).await {
            closed.store(true, std::sync::atomic::Ordering::SeqCst);
            app.engine.dec_reader(&ih, reader_id).await;
            tracing::warn!("stream seek to {start}: {e}");
            return crate::server::http_error(500, &format!("stream seek: {e}"));
        }
    }
    let limited = tokio::io::AsyncReadExt::take(stream, content_len as u64);

    // counting + touch stream
    let reader_stream = tokio_util::io::ReaderStream::new(limited);
    // Wrap to count bytes and touch activity
    let written = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    let written2 = written.clone();
    let app3 = app.clone();
    let ih3 = ih.clone();
    let closed2 = closed.clone();
    let last_touch = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
    ));
    let last_touch2 = last_touch.clone();
    let s = reader_stream.map(move |chunk| {
        if let Ok(b) = &chunk {
            written2.fetch_add(b.len() as i64, std::sync::atomic::Ordering::Relaxed);
            let now_s = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            if !closed2.load(std::sync::atomic::Ordering::SeqCst)
                && now_s - last_touch2.load(std::sync::atomic::Ordering::Relaxed) >= 2
            {
                last_touch2.store(now_s, std::sync::atomic::Ordering::Relaxed);
                let a = app3.clone();
                let ihc = ih3.clone();
                let cl = closed2.clone();
                tokio::spawn(async move { a.engine.touch_reader(&ihc, reader_id, &cl).await; });
            }
        }
        chunk.map_err(std::io::Error::other)
    });

    struct GuardStream<S> {
        inner: S,
        app: crate::server::App,
        ih: String,
        reader_id: u64,
        closed: std::sync::Arc<std::sync::atomic::AtomicBool>,
        key_id: String,
        idx: usize,
        range: String,
        written: std::sync::Arc<std::sync::atomic::AtomicI64>,
        done: bool,
    }
    impl<S> GuardStream<S>
    where
        S: futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin,
    {
        fn poll_next_inner(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<S::Item>> {
            let this = &mut *self;
            let mut inner = std::pin::Pin::new(&mut this.inner);
            match futures::Stream::poll_next(inner.as_mut(), cx) {
                std::task::Poll::Ready(None) => {
                    if !this.done {
                        this.done = true;
                        this.closed.store(true, std::sync::atomic::Ordering::SeqCst);
                        let app = this.app.clone();
                        let ih = this.ih.clone();
                        let reader_id = this.reader_id;
                        let key_id = this.key_id.clone();
                        let idx = this.idx;
                        let range = this.range.clone();
                        let written = this.written.load(std::sync::atomic::Ordering::Relaxed);
                        tokio::spawn(async move {
                            app.engine.dec_reader(&ih, reader_id).await;
                            app.store.add_key_bytes(&key_id, written);
                            tracing::info!("play {} key={key_id} file={idx} served={} range={range:?}", &ih[..8.min(ih.len())], crate::util::human_bytes(written));
                        });
                    }
                    std::task::Poll::Ready(None)
                }
                other => other,
            }
        }
    }
    impl<S> futures::Stream for GuardStream<S>
    where
        S: futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin,
    {
        type Item = Result<bytes::Bytes, std::io::Error>;
        fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
            Self::poll_next_inner(self.as_mut(), cx)
        }
    }
    let guarded = GuardStream {
        inner: Box::pin(s),
        app: app2,
        ih: ih2,
        reader_id,
        closed,
        key_id: k.id.clone(),
        idx,
        range: range_hdr.clone(),
        written,
        done: false,
    };
    impl<S> Drop for GuardStream<S> {
        fn drop(&mut self) {
            if !self.done {
                self.done = true;
                self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
                let app = self.app.clone();
                let ih = self.ih.clone();
                let reader_id = self.reader_id;
                let key_id = self.key_id.clone();
                let idx = self.idx;
                let range = self.range.clone();
                let written = self.written.load(std::sync::atomic::Ordering::Relaxed);
                tokio::spawn(async move {
                    app.engine.dec_reader(&ih, reader_id).await;
                    if written > 0 {
                        app.store.add_key_bytes(&key_id, written);
                    }
                    tracing::info!("play {} key={key_id} file={idx} served={} range={range:?} (dropped)", &ih[..8.min(ih.len())], crate::util::human_bytes(written));
                });
            }
        }
    }

    let body = axum::body::Body::from_stream(guarded);
    let mut builder = Response::builder()
        .header("content-type", ct)
        .header("content-disposition", format!("inline; filename=\"{disp_name}\""))
        .header("accept-ranges", "bytes")
        .header("cache-control", "no-store")
        .header("x-bitlab-file", idx.to_string());
    if status == 206 {
        builder = builder.status(206)
            .header("content-range", format!("bytes {start}-{end}/{file_len}"))
            .header("content-length", content_len.to_string());
    } else {
        builder = builder.status(200).header("content-length", file_len.max(0).to_string());
    }
    builder.body(body).unwrap()
}

#[derive(Debug, PartialEq, Eq)]
enum RangeSpec {
    Full,
    Partial(i64, i64),
    Unsatisfiable,
}

fn parse_range(hdr: &str, size: i64) -> RangeSpec {
    let hdr = hdr.trim();
    if hdr.is_empty() || hdr.len() < 6 || !hdr[..6].eq_ignore_ascii_case("bytes=") {
        return RangeSpec::Full;
    }
    let spec = hdr[6..].split(',').next().unwrap_or("").trim();
    let Some((start_s, end_s)) = spec.split_once('-') else {
        return RangeSpec::Full;
    };
    let start_s = start_s.trim();
    let end_s = end_s.trim();

    if start_s.is_empty() {
        if end_s.is_empty() || !end_s.bytes().all(|b| b.is_ascii_digit()) {
            return RangeSpec::Full;
        }
        let n = end_s.parse::<u64>().unwrap_or(u64::MAX).min(i64::MAX as u64) as i64;
        if n <= 0 || size <= 0 {
            return RangeSpec::Unsatisfiable;
        }
        let start = size.saturating_sub(n).max(0);
        return RangeSpec::Partial(start, size - 1);
    }

    if !start_s.bytes().all(|b| b.is_ascii_digit()) {
        return RangeSpec::Full;
    }
    let start = match start_s.parse::<i64>() {
        Ok(v) => v,
        Err(_) => return RangeSpec::Unsatisfiable,
    };

    if end_s.is_empty() {
        if size <= 0 || start >= size {
            return RangeSpec::Unsatisfiable;
        }
        return RangeSpec::Partial(start, size - 1);
    }

    if !end_s.bytes().all(|b| b.is_ascii_digit()) {
        return RangeSpec::Full;
    }
    let raw_end = end_s.parse::<u64>().unwrap_or(u64::MAX).min(i64::MAX as u64) as i64;
    if start > raw_end {
        return RangeSpec::Full;
    }
    if size <= 0 || start >= size {
        return RangeSpec::Unsatisfiable;
    }
    let end = raw_end.min(size - 1);
    RangeSpec::Partial(start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_utf8() {
        let title = "The 100 Girlfriends Who Really, Really, Really, Really, REALLY Love You - 君のことが大大大大大好きな100人の彼女";
        let res = truncate(title, 120);
        assert!(res.len() <= 120);
        // Ensure valid UTF-8 slice and doesn't panic
        assert_eq!(res, &title[..title.floor_char_boundary(120)]);
    }

    #[test]
    fn test_parse_range_cases() {
        assert_eq!(parse_range("", 1000), RangeSpec::Full);
        assert_eq!(parse_range("items=0-5", 1000), RangeSpec::Full);
        assert_eq!(parse_range("BYTES=0-99", 1000), RangeSpec::Partial(0, 99));
        assert_eq!(parse_range("bytes=500-", 1000), RangeSpec::Partial(500, 999));
        assert_eq!(parse_range("bytes=500-5000", 1000), RangeSpec::Partial(500, 999));
        assert_eq!(parse_range("bytes=-100", 1000), RangeSpec::Partial(900, 999));
        assert_eq!(parse_range("bytes=-5000", 1000), RangeSpec::Partial(0, 999));
        assert_eq!(parse_range("bytes=1000-", 1000), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=1500-2000", 1000), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 1000), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-", 0), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("", 0), RangeSpec::Full);
        assert_eq!(parse_range("bytes=+0-10", 1000), RangeSpec::Full);
        assert_eq!(parse_range("bytes=500-100", 1000), RangeSpec::Full);
        assert_eq!(parse_range("bytes=99999999999999999999999-", 1000), RangeSpec::Unsatisfiable);
    }

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("movie.mkv"), "movie.mkv");
        assert_eq!(sanitize_filename("bad\"\r\n;\\/name.mkv"), "bad______name.mkv");
        assert_eq!(sanitize_filename("   \0 "), "_");
        assert_eq!(sanitize_filename(""), "video");
    }

    #[test]
    fn test_is_valid_imdb() {
        assert!(is_valid_imdb("tt0133093"));
        assert!(is_valid_imdb("tt12345"));
        assert!(!is_valid_imdb("tt1234"));
        assert!(!is_valid_imdb("nm0133093"));
        assert!(!is_valid_imdb("tt013309a"));
    }
}
