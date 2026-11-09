use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use std::collections::HashMap;
use futures::StreamExt;
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
    #[serde(rename = "behaviorHints", skip_serializing_if = "Option::is_none")]
    behavior_h: Option<serde_json::Value>,
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
        behavior_h: None,
    }
}

async fn auth_key(app: &crate::server::App, token: &str) -> Result<crate::store::Key, Response> {
    if token.is_empty() {
        return Err(crate::server::http_error(401, "missing manifest key"));
    }
    match app.store.key_by_token(token) {
        Ok(k) => {
            app.store.touch_key(&k.id);
            Ok(k)
        }
        Err(crate::store::KeyLookup::NotFound) => Err(crate::server::http_error(401, "unknown manifest key")),
        Err(crate::store::KeyLookup::Paused) => Err(crate::server::http_error(403, "this manifest key is paused")),
        Err(crate::store::KeyLookup::Expired) => Err(crate::server::http_error(410, "this manifest key expired")),
    }
}

pub async fn handle_manifest(
    State(app): State<crate::server::App>,
    Path(token): Path<String>,
) -> Response {
    let k = match auth_key(&app, &token).await {
        Ok(k) => k,
        Err(e) => return e,
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

fn release_label(title: &str) -> (String, String) {
    let q = if regex::Regex::new(r"(?i)\b(2160p|4k|uhd)\b").unwrap().is_match(title) {
        "2160p"
    } else if regex::Regex::new(r"(?i)\b1080p\b").unwrap().is_match(title) {
        "1080p"
    } else if regex::Regex::new(r"(?i)\b720p\b").unwrap().is_match(title) {
        "720p"
    } else {
        "SD"
    };
    let src = if regex::Regex::new(r"(?i)\bremux\b").unwrap().is_match(title) {
        "REMUX"
    } else if regex::Regex::new(r"(?i)\b(bluray|blu-ray|bdrip)\b").unwrap().is_match(title) {
        "BluRay"
    } else if regex::Regex::new(r"(?i)\b(web-dl|webrip|web)\b").unwrap().is_match(title) {
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
        Err(e) => return e,
    };
    let raw_id = id.trim_end_matches(".json").to_string();
    let (base_id, mut season, mut ep) = parse_media_id(&raw_id);

    if typ != "movie" && typ != "series" {
        return crate::server::json_response(200, serde_json::json!({"streams": []}));
    }
    if typ == "series" && season <= 0 {
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
    let tls = false;
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
                let n = app.hint_n.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if n > 8192 {
                    app.hints.clear();
                    app.hint_n.store(0, std::sync::atomic::Ordering::Relaxed);
                }
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
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    handle_play_inner(app, headers, token, ih, q).await
}

pub async fn handle_play_file(
    State(app): State<crate::server::App>,
    headers: HeaderMap,
    Path((token, ih, _fname)): Path<(String, String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    handle_play_inner(app, headers, token, ih, q).await
}

async fn handle_play_inner(
    app: crate::server::App,
    headers: HeaderMap,
    token: String,
    ih_raw: String,
    q: HashMap<String, String>,
) -> Response {
    let k = match auth_key(&app, &token).await {
        Ok(k) => k,
        Err(e) => return e,
    };
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
    // titles for movie
    let titles: Vec<String> = match q.get("t") {
        Some(v) => vec![v.clone()],
        None => vec![],
    };
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
    // range past end check
    if let Some(rh) = headers.get("range").and_then(|v| v.to_str().ok()) {
        if let Some(start) = crate::util::range_start(rh) {
            if start >= file_len {
                tracing::info!("play {}: range starts at {start} but this file is {} — the player is holding an offset from another release", &ih[..8.min(ih.len())], crate::util::human_bytes(file_len));
            }
        }
    }

    let fname = std::path::Path::new(&rel).file_name().and_then(|s| s.to_str()).unwrap_or("video");
    let mut ct = mime_guess::from_path(fname).first_raw().unwrap_or("video/mp4").to_string();
    let disp_name = fname.to_string();
    if !ct.starts_with("video/") {
        ct = "video/mp4".to_string();
    }

    // readers accounting
    app.engine.inc_reader(&ih).await;
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
            app.engine.dec_reader(&ih).await;
            return crate::server::http_error(500, &format!("stream: {e}"));
        }
    };

    // Range handling
    let range_hdr = headers.get("range").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let (start, end, status) = parse_range(&range_hdr, file_len);
    let content_len = end - start + 1;

    use tokio::io::AsyncSeekExt;
    if start > 0 {
        if let Err(e) = stream.seek(std::io::SeekFrom::Start(start as u64)).await {
            tracing::warn!("stream seek to {start}: {e}");
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
    let last_touch = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
    ));
    let last_touch2 = last_touch.clone();
    let s = reader_stream.map(move |chunk| {
        if let Ok(b) = &chunk {
            written2.fetch_add(b.len() as i64, std::sync::atomic::Ordering::Relaxed);
            let now_s = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            if now_s - last_touch2.load(std::sync::atomic::Ordering::Relaxed) >= 2 {
                last_touch2.store(now_s, std::sync::atomic::Ordering::Relaxed);
                let a = app3.clone();
                let ihc = ih3.clone();
                tokio::spawn(async move { a.engine.touch_now(&ihc).await; });
            }
        }
        chunk.map_err(std::io::Error::other)
    });

    // Decrement reader when stream ends: spawn watchdog that polls? Use Drop via extension?
    // We use a background task that waits until response is done is hard without body callbacks.
    // Instead, decrement after content_len bytes served OR after idle timeout. Simplest: decrement in a task after estimated time?
    // Correct approach: use `async_stream` with finally. We'll wrap with a custom stream that decrements on last poll.
    // For simplicity, spawn a task that decrements after (content_len / 1MB * 2s + 60s) — but that leaks readers.
    // Better: use Stream impl with Drop.
    struct GuardStream<S> {
        inner: S,
        app: crate::server::App,
        ih: String,
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
                        let app = this.app.clone();
                        let ih = this.ih.clone();
                        let key_id = this.key_id.clone();
                        let idx = this.idx;
                        let range = this.range.clone();
                        let written = this.written.load(std::sync::atomic::Ordering::Relaxed);
                        tokio::spawn(async move {
                            app.engine.dec_reader(&ih).await;
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
        key_id: k.id.clone(),
        idx,
        range: range_hdr.clone(),
        written,
        done: false,
    };
    // Also handle client disconnect via request body? Axum will drop stream on disconnect, but our Drop won't run unless polled to None.
    // Add Drop impl to decrement if not done.
    impl<S> Drop for GuardStream<S> {
        fn drop(&mut self) {
            if !self.done {
                let app = self.app.clone();
                let ih = self.ih.clone();
                let key_id = self.key_id.clone();
                let idx = self.idx;
                let range = self.range.clone();
                let written = self.written.load(std::sync::atomic::Ordering::Relaxed);
                tokio::spawn(async move {
                    app.engine.dec_reader(&ih).await;
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
        .header("content-disposition", format!("inline; filename=\"{}\"", disp_name.replace('"', "_")))
        .header("accept-ranges", "bytes")
        .header("cache-control", "no-store")
        .header("x-bitlab-file", idx.to_string());
    if status == 206 {
        builder = builder.status(206)
            .header("content-range", format!("bytes {start}-{end}/{file_len}"))
            .header("content-length", content_len.to_string());
    } else {
        builder = builder.status(200).header("content-length", file_len.to_string());
    }
    builder.body(body).unwrap()
}

fn parse_range(hdr: &str, size: i64) -> (i64, i64, u16) {
    let hdr = hdr.trim();
    if !hdr.starts_with("bytes=") {
        return (0, size - 1, 200);
    }
    let spec = hdr.trim_start_matches("bytes=").split(',').next().unwrap_or("").trim();
    if let Some(stripped) = spec.strip_prefix('-') {
        // suffix: last N bytes
        if let Ok(n) = stripped.parse::<i64>() {
            if n <= 0 {
                return (0, size - 1, 200);
            }
            let start = (size - n).max(0);
            return (start, size - 1, 206);
        }
        return (0, size - 1, 200);
    }
    let parts: Vec<&str> = spec.split('-').collect();
    if parts.len() != 2 {
        return (0, size - 1, 200);
    }
    let start: i64 = parts[0].trim().parse().unwrap_or(0);
    let end: i64 = if parts[1].trim().is_empty() {
        size - 1
    } else {
        parts[1].trim().parse().unwrap_or(size - 1)
    };
    if start >= size {
        return (start, size - 1, 206);
    }
    let end = end.min(size - 1).max(start);
    (start, end, 206)
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
}
