use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::HeaderMap;
use axum::response::Response;
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

pub struct MaybeConnectInfo(pub Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for MaybeConnectInfo {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0)))
    }
}

struct FailWindow {
    inner: Mutex<HashMap<String, Vec<Instant>>>,
}

impl FailWindow {
    fn new() -> Self {
        Self { inner: Mutex::new(HashMap::new()) }
    }
    fn blocked(&self, ip: &str) -> bool {
        let mut m = self.inner.lock().unwrap();
        let cut = Instant::now() - Duration::from_secs(600);
        // retain recent
        if let Some(v) = m.get(ip) {
            let keep: Vec<Instant> = v.iter().cloned().filter(|t| *t > cut).collect();
            if keep.is_empty() {
                m.remove(ip);
            } else {
                m.insert(ip.to_string(), keep);
            }
        }
        m.get(ip).map(|v| v.len() >= 10).unwrap_or(false)
    }
    fn fail(&self, ip: &str) {
        let mut m = self.inner.lock().unwrap();
        if m.len() > 10_000 {
            let cut = Instant::now() - Duration::from_secs(600);
            m.retain(|_, v| v.last().map(|t| *t > cut).unwrap_or(false));
            if m.len() > 10_000 {
                m.clear();
            }
        }
        m.entry(ip.to_string()).or_default().push(Instant::now());
    }
    fn reset(&self, ip: &str) {
        self.inner.lock().unwrap().remove(ip);
    }
}

static LOGIN_WINDOW: OnceLock<FailWindow> = OnceLock::new();
fn login_window() -> &'static FailWindow {
    LOGIN_WINDOW.get_or_init(FailWindow::new)
}

pub fn check_auth_rate_limit(ip: &str) -> bool {
    login_window().blocked(ip)
}

pub fn record_auth_failure(ip: &str) {
    login_window().fail(ip);
}

pub fn reset_auth_failure(ip: &str) {
    login_window().reset(ip);
}

pub fn is_https(app: &crate::server::App, headers: &HeaderMap) -> bool {
    if app.cfg.base_url.starts_with("https://") {
        return true;
    }
    if app.cfg.trust_proxy {
        if let Some(proto) = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) {
            if proto.split(',').next().is_some_and(|p| p.trim().eq_ignore_ascii_case("https")) {
                return true;
            }
        }
    }
    false
}

pub fn clear_cookie(name: &str, secure: bool) -> String {
    format!(
        "{name}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{}",
        if secure { "; Secure" } else { "" }
    )
}

pub fn client_ip(app: &crate::server::App, headers: &HeaderMap, remote: Option<SocketAddr>) -> String {
    let mut ip = String::new();
    if app.cfg.trust_proxy {
        if let Some(v) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
            ip = v.trim().to_string();
        }
        if ip.is_empty() {
            if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
                if let Some(last) = xff.split(',').next_back() {
                    ip = last.trim().to_string();
                }
            }
        }
    }
    if ip.is_empty() {
        ip = remote.map(|a| a.ip().to_string()).unwrap_or_else(|| "unknown".to_string());
    }
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(v6)) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                v4.to_string()
            } else {
                v6.to_string()
            }
        }
        Ok(addr) => addr.to_string(),
        Err(_) => "unknown".to_string(),
    }
}

pub async fn handle_favicon() -> Response {
    Response::builder()
        .status(200)
        .header("content-type", "image/svg+xml")
        .header("cache-control", "public, max-age=86400")
        .body(axum::body::Body::from(crate::server::FAVICON_SVG))
        .unwrap()
}

fn clean_next_path(p: &str) -> String {
    match p.trim_end_matches('/') {
        "/admin/activity" => "/admin/activity".to_string(),
        "/admin/security" => "/admin/security".to_string(),
        _ => "/admin/keys".to_string(),
    }
}

pub async fn handle_admin_page(State(app): State<crate::server::App>, headers: HeaderMap, req: axum::extract::Request) -> Response {
    let path = req.uri().path().to_string();
    // path check
    let p = path.as_str();
    let ok = matches!(p.trim_end_matches('/'), "/admin" | "/admin/keys" | "/admin/activity" | "/admin/security");
    if !ok {
        return crate::server::http_error(404, "not found");
    }
    // session check
    if current_session(&app, &headers).await.is_none() {
        // set next cookie if not keys
        let next = clean_next_path(p);
        let mut b = Response::builder().status(302).header("location", "/login");
        if next != "/admin/keys" {
            let secure = is_https(&app, &headers);
            b = b.header(
                "set-cookie",
                format!("bitlab_next={next}; Path=/; Max-Age=600; HttpOnly; SameSite=Lax{}", if secure { "; Secure" } else { "" }),
            );
        }
        return b.body(axum::body::Body::empty()).unwrap();
    }
    serve_admin_shell()
}

pub async fn handle_login_page(State(app): State<crate::server::App>, headers: HeaderMap) -> Response {
    if current_session(&app, &headers).await.is_some() {
        return Response::builder().status(302).header("location", "/admin/keys").body(axum::body::Body::empty()).unwrap();
    }
    serve_admin_shell()
}

fn serve_admin_shell() -> Response {
    Response::builder()
        .status(200)
        .header("content-type", "text/html; charset=utf-8")
        .header("cache-control", "no-store")
        .header(
            "content-security-policy",
            "default-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        )
        .body(axum::body::Body::from(crate::server::ADMIN_HTML))
        .unwrap()
}

pub async fn handle_login(
    State(app): State<crate::server::App>,
    headers: HeaderMap,
    MaybeConnectInfo(remote): MaybeConnectInfo,
    body: axum::body::Bytes,
) -> Response {
    let ip = client_ip(&app, &headers, remote);
    if check_auth_rate_limit(&ip) {
        return crate::server::http_error(429, "too many attempts, try again later");
    }
    #[derive(serde::Deserialize)]
    struct B {
        #[serde(default)]
        password: String,
        #[serde(default)]
        code: String,
    }
    if body.len() > 64 * 1024 {
        return crate::server::http_error(400, "bad json");
    }
    let b: B = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return crate::server::http_error(400, "bad json"),
    };
    if !crate::security::check_password(&app, &b.password) {
        record_auth_failure(&ip);
        tracing::info!("admin login failed from {ip}");
        return crate::server::http_error(401, "wrong password");
    }
    if crate::security::two_factor_enabled(&app) {
        if b.code.trim().is_empty() {
            return crate::server::json_response(401, serde_json::json!({"error": "two-factor code required", "twofa": true}));
        }
        if !crate::security::check_second_factor(&app, &b.code).await {
            record_auth_failure(&ip);
            tracing::info!("two-factor failure from {ip}");
            return crate::server::http_error(401, "invalid two-factor code");
        }
    }
    reset_auth_failure(&ip);
    let ua = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let sess = match app.store.create_session(&crate::security::session_name(&ua), &ip, &ua, 7 * 24 * 3600) {
        Ok(s) => s,
        Err(e) => {
            tracing::info!("create session failed: {e}");
            return crate::server::http_error(500, "store unavailable");
        }
    };
    app.store.prune_sessions(50);
    let cookie = crate::security::issue_cookie_value(&app, &sess);
    tracing::info!("admin login from {ip} ({})", sess.name);
    let next = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|c| parse_cookie(c, "bitlab_next"))
        .map(|v| clean_next_path(&v))
        .unwrap_or_else(|| "/admin/keys".to_string());
    let mut resp = crate::server::json_response(200, serde_json::json!({"ok": true, "session": sess.id, "next": next}));
    // set cookies: session + clear next
    let secure = is_https(&app, &headers);
    let set1 = format!("bitlab_session={cookie}; Path=/; HttpOnly; SameSite=Lax; Expires={}{}", http_date(sess.expires_at), if secure { "; Secure" } else { "" });
    resp.headers_mut().append("set-cookie", set1.parse().unwrap());
    resp.headers_mut().append("set-cookie", clear_cookie("bitlab_next", secure).parse().unwrap());
    resp
}

fn parse_cookie(header: &str, name: &str) -> Option<String> {
    for part in header.split(';') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=') {
            if k.trim() == name {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn http_date(unix: i64) -> String {
    // Format as IMF-fixdate
    use chrono::{TimeZone, Utc};
    Utc.timestamp_opt(unix, 0).single().map(|d| d.format("%a, %d %b %Y %H:%M:%S GMT").to_string()).unwrap_or_default()
}

pub async fn handle_logout(State(app): State<crate::server::App>, headers: HeaderMap) -> Response {
    if let Some(s) = current_session(&app, &headers).await {
        let _ = app.store.delete_session(&s.id);
    }
    let secure = is_https(&app, &headers);
    let mut r = crate::server::json_response(200, serde_json::json!({"ok": true}));
    r.headers_mut().insert("set-cookie", clear_cookie("bitlab_session", secure).parse().unwrap());
    r
}

pub async fn current_session(app: &crate::server::App, headers: &HeaderMap) -> Option<crate::store::SessionRecord> {
    let c = headers.get("cookie")?.to_str().ok()?;
    let val = parse_cookie(c, "bitlab_session")?;
    let (id, sig) = val.split_once('.')?;
    if !hmac_equal(&app.session_mac(id), sig) {
        return None;
    }
    let s = app.store.get_session(id)?;
    app.store.touch_session(id);
    Some(s)
}

fn hmac_equal(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

pub async fn require_admin_state(app: &crate::server::App, headers: &HeaderMap) -> Option<Response> {
    if current_session(app, headers).await.is_none() {
        return Some(crate::server::http_error(401, "not signed in"));
    }
    None
}

pub async fn handle_overview(State(app): State<crate::server::App>, headers: HeaderMap) -> Response {
    if let Some(e) = require_admin_state(&app, &headers).await {
        return e;
    }
    let keys = match app.store.list_keys() {
        Ok(k) => k,
        Err(e) => {
            tracing::info!("overview: list keys: {e}");
            return crate::server::http_error(500, "store unavailable");
        }
    };
    let mut ix = app.indexers();
    for i in ix.iter_mut() {
        i.api_key.clear();
    }
    let idle_min = (app.engine.idle_timeout_dur().as_secs() / 60) as i64;
    let cache = app.engine.cache_stats().await;
    let uptime = SystemTime::now().duration_since(app.start).map(|d| d.as_secs() as i64).unwrap_or(0);
    crate::server::json_response(
        200,
        serde_json::json!({
            "keys": keys,
            "indexers": ix,
            "torrents": app.engine.views().await,
            "cache_bytes": cache.used_bytes,
            "cache_max_bytes": cache.budget_bytes,
            "cache": cache,
            "uptime_seconds": uptime,
            "version": crate::config::VERSION,
            "base_url": app.cfg.base_url,
            "active_keys": app.store.active_key_count(),
            "idle_minutes": idle_min,
            "readahead_mb": (app.readahead.load(std::sync::atomic::Ordering::Relaxed) >> 20),
        }),
    )
}

pub async fn handle_list_keys(State(app): State<crate::server::App>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Some(e) = require_admin_state(&app, &headers).await {
        return e;
    }
    let qq = q.get("q").cloned().unwrap_or_default();
    let status = q.get("status").cloned().unwrap_or_default();
    let mut page: i64 = q.get("page").and_then(|v| v.parse().ok()).unwrap_or(1);
    page = page.clamp(1, 1_000_000);
    let mut per: i64 = q.get("per").and_then(|v| v.parse().ok()).unwrap_or(25);
    per = per.clamp(5, 100);
    let offset = (page - 1).saturating_mul(per);
    match app.store.list_keys_page(&qq, &status, per, offset) {
        Ok((keys, total)) => crate::server::json_response(200, serde_json::json!({"keys": keys, "total": total, "page": page, "per": per, "query": qq, "status": status})),
        Err(e) => {
            tracing::info!("list keys: {e}");
            crate::server::http_error(500, "store unavailable")
        }
    }
}

pub async fn handle_create_key(State(app): State<crate::server::App>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if let Some(e) = require_admin_state(&app, &headers).await {
        return e;
    }
    if body.len() > 64 * 1024 {
        return crate::server::http_error(400, "bad json");
    }
    #[derive(serde::Deserialize)]
    struct B {
        #[serde(default)]
        label: String,
        #[serde(default)]
        note: String,
        #[serde(default)]
        expires: String,
        #[serde(default)]
        days: i64,
    }
    let b: B = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return crate::server::http_error(400, "bad json"),
    };
    let label = b.label.trim();
    let note = b.note.trim();
    if label.chars().count() > 200 || note.chars().count() > 1000 {
        return crate::server::http_error(400, "label or note too long");
    }
    if b.days < 0 {
        return crate::server::http_error(400, "days must be non-negative");
    }
    let mut ttl = match crate::security::parse_ttl(&b.expires) {
        Ok(v) => v,
        Err(e) => return crate::server::http_error(400, &e.to_string()),
    };
    if ttl == 0 && b.days > 0 {
        if b.days > 50 * 365 {
            return crate::server::http_error(400, "expiry too far in the future (max 50 years)");
        }
        ttl = b.days.saturating_mul(24 * 3600);
    }
    match app.store.create_key(label, note, ttl) {
        Ok(k) => {
            tracing::info!("key created id={} label={:?}", k.id, k.label);
            crate::server::json_response(200, serde_json::to_value(&k).unwrap())
        }
        Err(e) => {
            tracing::info!("create key failed: {e}");
            crate::server::http_error(500, "failed to create key")
        }
    }
}

pub async fn handle_pause_key(State(app): State<crate::server::App>, headers: HeaderMap, Path(id): Path<String>, body: axum::body::Bytes) -> Response {
    if let Some(e) = require_admin_state(&app, &headers).await {
        return e;
    }
    if body.len() > 64 * 1024 {
        return crate::server::http_error(400, "bad json");
    }
    #[derive(serde::Deserialize)]
    struct B {
        #[serde(default)]
        paused: bool,
    }
    let b: B = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return crate::server::http_error(400, "bad json"),
    };
    let status = if b.paused { "paused" } else { "active" };
    match app.store.set_key_status(&id, status) {
        Ok(true) => {
            tracing::info!("key {id} status={status}");
            crate::server::json_response(200, serde_json::json!({"ok": true, "status": status}))
        }
        Ok(false) => crate::server::http_error(404, "not found"),
        Err(e) => {
            tracing::info!("set key status failed: {e}");
            crate::server::http_error(500, "store unavailable")
        }
    }
}

pub async fn handle_delete_key(State(app): State<crate::server::App>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Some(e) = require_admin_state(&app, &headers).await {
        return e;
    }
    match app.store.delete_key(&id) {
        Ok(true) => {
            tracing::info!("key {id} revoked (deleted)");
            crate::server::json_response(200, serde_json::json!({"ok": true}))
        }
        _ => crate::server::http_error(404, "not found"),
    }
}

pub async fn handle_search(State(app): State<crate::server::App>, headers: HeaderMap, Query(qm): Query<HashMap<String, String>>) -> Response {
    if let Some(e) = require_admin_state(&app, &headers).await {
        return e;
    }
    let mut q = qm.get("q").cloned().unwrap_or_default().trim().to_string();
    let mut typ = qm.get("type").cloned().unwrap_or_else(|| "movie".to_string());
    if typ.is_empty() {
        typ = "movie".to_string();
    }
    if typ != "movie" && typ != "series" {
        return crate::server::http_error(400, "type must be movie or series");
    }
    if q.is_empty() {
        return crate::server::http_error(400, "q required");
    }
    let (base_id, season, ep) = crate::addon::parse_media_id(&q);
    let mut imdb = String::new();
    if let Some(after_tt) = base_id.strip_prefix("tt") {
        if !after_tt.is_empty() && after_tt.bytes().all(|b| b.is_ascii_digit()) {
            if !crate::addon::is_valid_imdb(&base_id) {
                return crate::server::http_error(400, "invalid imdb id");
            }
            imdb = base_id.clone();
            match app.resolve_meta(&typ, &base_id).await {
                Ok((title, year)) => {
                    if typ == "series" && season > 0 && ep > 0 {
                        q = format!("{title} S{season:02}E{ep:02}");
                    } else if typ == "series" && season > 0 {
                        q = format!("{title} S{season:02}");
                    } else if year > 0 {
                        q = format!("{title} {year}");
                    } else {
                        q = title;
                    }
                }
                Err(e) => return crate::server::http_error(502, &format!("meta: {e}")),
            }
        }
    }
    let (res, errs) = tokio::time::timeout(Duration::from_secs(25), app.search_all(vec![q.clone()], &typ, season, ep, &imdb))
        .await
        .unwrap_or((vec![], vec!["timeout".to_string()]));
    crate::server::json_response(200, serde_json::json!({"query": q, "count": res.len(), "results": res, "errors": errs}))
}

pub async fn handle_settings(State(app): State<crate::server::App>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if let Some(e) = require_admin_state(&app, &headers).await {
        return e;
    }
    if body.len() > 64 * 1024 {
        return crate::server::http_error(400, "bad json");
    }
    #[derive(serde::Deserialize)]
    struct B {
        cache_gb: Option<i64>,
        idle_minutes: Option<i64>,
        readahead_mb: Option<i64>,
    }
    let b: B = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return crate::server::http_error(400, "bad json"),
    };
    if let Some(v) = b.cache_gb {
        if !(0..=1_000_000).contains(&v) {
            return crate::server::http_error(400, "cache_gb must be between 0 and 1000000");
        }
    }
    if let Some(v) = b.idle_minutes {
        if !(1..=7 * 24 * 60).contains(&v) {
            return crate::server::http_error(400, "idle_minutes must be between 1 and 10080");
        }
    }
    if let Some(v) = b.readahead_mb {
        if !(1..=1024).contains(&v) {
            return crate::server::http_error(400, "readahead_mb must be between 1 and 1024");
        }
    }
    if let Some(v) = b.cache_gb {
        app.engine.set_max_cache(v.saturating_mul(1_000_000_000));
        if let Err(e) = app.store.set_setting("cache_gb", &v.to_string()) {
            tracing::info!("save cache_gb failed: {e}");
            return crate::server::http_error(500, "store unavailable");
        }
    }
    if let Some(v) = b.idle_minutes {
        app.engine.set_idle_timeout(Duration::from_secs((v as u64) * 60));
        if let Err(e) = app.store.set_setting("idle_minutes", &v.to_string()) {
            tracing::info!("save idle_minutes failed: {e}");
            return crate::server::http_error(500, "store unavailable");
        }
    }
    if let Some(v) = b.readahead_mb {
        app.readahead.store(v << 20, std::sync::atomic::Ordering::Relaxed);
        if let Err(e) = app.store.set_setting("readahead_mb", &v.to_string()) {
            tracing::info!("save readahead_mb failed: {e}");
            return crate::server::http_error(500, "store unavailable");
        }
    }
    handle_overview(State(app), headers).await
}

pub async fn handle_purge_cache(State(app): State<crate::server::App>, headers: HeaderMap) -> Response {
    if let Some(e) = require_admin_state(&app, &headers).await {
        return e;
    }
    let (freed_bytes, freed_count) = app.engine.purge_inactive_cache().await;
    tracing::info!("cache: admin purge reclaimed {} across {freed_count} entries", crate::util::human_bytes(freed_bytes));
    crate::server::json_response(200, serde_json::json!({
        "ok": true,
        "freed_bytes": freed_bytes,
        "freed_count": freed_count,
        "cache": app.engine.cache_stats().await,
    }))
}
