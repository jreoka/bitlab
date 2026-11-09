mod addon;
mod admin;
mod anilist;
mod cache;
mod config;
mod engine;
mod indexers;
mod netguard;
mod platform;
mod relevance;
mod security;
mod server;
mod store;
mod util;

use axum::extract::State;
use axum::response::Response;
use axum::routing::{delete, get, post};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "bitlab=info,info".into()))
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && (args[1] == "-version" || args[1] == "--version" || args[1] == "version") {
        println!("bitlab {}", config::VERSION);
        return;
    }

    let mut cfg = config::load_config();

    if let Err(e) = std::fs::create_dir_all(&cfg.data_dir) {
        eprintln!("data dir: {e}");
        std::process::exit(1);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&cfg.data_dir, std::fs::Permissions::from_mode(0o700));
    }

    if !cfg.admin_password.is_empty() {
        let _ = std::fs::remove_file(format!("{}/ADMIN_PASSWORD", cfg.data_dir));
    }
    if cfg.admin_password.is_empty() {
        cfg.admin_password = bootstrap_admin_password(&cfg.data_dir);
    }

    let db_path = std::path::Path::new(&cfg.data_dir).join("bitlab.db");
    let store = match store::Store::open(&db_path) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("store: {e}");
            std::process::exit(1);
        }
    };

    // DB settings override env defaults
    if let Ok(v) = store.get_setting("cache_gb", "").parse::<i64>() {
        if v >= 0 {
            cfg.cache_max_bytes = v.saturating_mul(1_000_000_000);
        }
    }
    if !store.get_setting("idle_minutes", "").is_empty() {
        if let Ok(n) = store.get_setting("idle_minutes", "").parse::<i64>() {
            if n > 0 {
                cfg.idle_timeout = Duration::from_secs((n as u64) * 60);
            }
        }
    }
    if !store.get_setting("readahead_mb", "").is_empty() {
        if let Ok(n) = store.get_setting("readahead_mb", "").parse::<i64>() {
            if n > 0 {
                cfg.readahead_bytes = n << 20;
            }
        }
    }

    let eng = match engine::Engine::new(&cfg, store.clone()).await {
        Ok(e) => Arc::new(e),
        Err(e) => {
            eprintln!("engine: {e}");
            std::process::exit(1);
        }
    };

    let mut secret = store.get_setting("session_secret", "");
    if secret.is_empty() {
        secret = util::rand_hex(32);
        let _ = store.set_setting("session_secret", &secret);
    }

    let app = server::App::new(cfg.clone(), store.clone(), eng.clone(), secret.as_bytes().to_vec());

    // background: purge expired keys + prune sessions
    {
        let store = store.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_secs(60));
            loop {
                t.tick().await;
                let n = store.delete_expired_keys();
                if n > 0 {
                    tracing::info!("purged {n} expired manifest key(s)");
                }
                store.prune_sessions(50);
            }
        });
    }
    // background: stats log
    {
        let eng = eng.clone();
        tokio::spawn(async move {
            let log_stats = || {
                let eng = eng.clone();
                async move {
                    let st = eng.cache_stats().await;
                    let free = if st.free_bytes >= 0 { util::human_bytes(st.free_bytes) } else { "unknown".to_string() };
                    tracing::info!(
                        "stats: torrents={} cache={} / {} free={free} entries={} partial={} reclaimed={}",
                        eng.torrent_count(),
                        util::human_bytes(st.used_bytes),
                        util::human_bytes(st.budget_bytes),
                        st.entries,
                        util::human_bytes(st.partial_bytes),
                        util::human_bytes(st.evicted_bytes)
                    );
                }
            };
            tokio::time::sleep(Duration::from_secs(15)).await;
            log_stats().await;
            let mut t = tokio::time::interval(Duration::from_secs(600));
            loop {
                t.tick().await;
                log_stats().await;
            }
        });
    }

    let router = build_router(app.clone());

    let addr = cfg.addr.clone();
    // normalize addr: ":8090" -> "0.0.0.0:8090"
    let listen_addr: SocketAddr = if addr.starts_with(':') {
        format!("0.0.0.0{addr}").parse().unwrap_or_else(|_| "0.0.0.0:8090".parse().unwrap())
    } else {
        addr.parse().unwrap_or_else(|_| "0.0.0.0:8090".parse().unwrap())
    };

    if !cfg.base_url.is_empty() {
        if let Ok(b) = url::Url::parse(&cfg.base_url) {
            if !b.host_str().unwrap_or("").is_empty() {
                tracing::info!("host gate: only {} may access this app; every other host gets a 403", b.host_str().unwrap());
            }
        }
    }

    tracing::info!("bitlab {} listening on {} (data={}, cache={})", config::VERSION, listen_addr, cfg.data_dir, util::human_bytes(cfg.cache_max_bytes));

    let listener = match tokio::net::TcpListener::bind(listen_addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("listen: {e}");
            std::process::exit(1);
        }
    };

    // graceful shutdown (ctrl_c + SIGTERM on unix)
    let srv = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>());
    let shutdown_signal = async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = signal(SignalKind::terminate()).expect("sigterm");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = term.recv() => {},
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        tracing::info!("shutting down");
    };
    srv.with_graceful_shutdown(shutdown_signal).await.unwrap();
}

fn bootstrap_admin_password(data_dir: &str) -> String {
    let p = format!("{data_dir}/ADMIN_PASSWORD");
    if let Ok(b) = std::fs::read_to_string(&p) {
        let s = b.trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    let pw = util::rand_token(12);
    if let Err(e) = std::fs::write(&p, format!("{pw}\n")) {
        eprintln!("cannot write admin password: {e}");
        std::process::exit(1);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    }
    println!("generated admin password, saved to {p}");
    pw
}

fn build_router(app: server::App) -> axum::Router {
    use axum::middleware;
    axum::Router::new()
        .route("/k/{token}/manifest.json", get(addon::handle_manifest))
        .route("/k/{token}/stream/{type}/{id}", get(addon::handle_stream))
        .route("/k/{token}/play/{infohash}", get(addon::handle_play))
        .route("/k/{token}/play/{infohash}/{fname}", get(addon::handle_play_file))
        .route("/healthz", get(handle_healthz))
        .route("/readyz", get(handle_readyz))
        .route("/manifest.json", get(handle_manifest_root))
        .route("/", get(handle_root))
        .route("/favicon.svg", get(admin::handle_favicon))
        .route("/favicon.ico", get(admin::handle_favicon))
        .route("/login", get(admin::handle_login_page))
        .route("/admin", get(admin::handle_admin_page))
        .route("/admin/", get(admin::handle_admin_page))
        .route("/admin/keys", get(admin::handle_admin_page))
        .route("/admin/activity", get(admin::handle_admin_page))
        .route("/admin/security", get(admin::handle_admin_page))
        .route("/admin/api/login", post(admin::handle_login))
        .route("/admin/api/logout", post(admin::handle_logout))
        .route("/admin/api/overview", get(admin::handle_overview))
        .route("/admin/api/keys", get(admin::handle_list_keys).post(admin::handle_create_key))
        .route("/admin/api/keys/{id}/pause", post(admin::handle_pause_key))
        .route("/admin/api/keys/{id}", delete(admin::handle_delete_key))
        .route("/admin/api/search", get(admin::handle_search))
        .route("/admin/api/settings", post(admin::handle_settings))
        .route("/admin/api/security", get(security::handle_security))
        .route("/admin/api/2fa/begin", post(security::handle_2fa_begin))
        .route("/admin/api/2fa/enable", post(security::handle_2fa_enable))
        .route("/admin/api/2fa/disable", post(security::handle_2fa_disable))
        .route("/admin/api/sessions/{id}", delete(security::handle_delete_session))
        .route("/admin/api/sessions/revoke-others", post(security::handle_revoke_others))
        .layer(middleware::from_fn_with_state(app.clone(), middleware_fn))
        .with_state(app)
}

async fn handle_healthz(State(app): State<server::App>) -> Response {
    let free = app.engine.disk_free().await;
    let degraded = (0..config::HEALTH_FREE_FLOOR).contains(&free);
    server::json_response(200, serde_json::json!({"ok": true, "degraded": degraded}))
}

async fn handle_readyz(State(app): State<server::App>) -> Response {
    let free = app.engine.disk_free().await;
    let degraded = (0..config::HEALTH_FREE_FLOOR).contains(&free);
    let status = if degraded { 503 } else { 200 };
    server::json_response(status, serde_json::json!({"ready": !degraded}))
}

async fn handle_manifest_root() -> Response {
    server::http_error(401, "use your personal manifest URL: /k/<key>/manifest.json")
}

async fn handle_root() -> Response {
    Response::builder().status(302).header("location", "/admin").body(axum::body::Body::empty()).unwrap()
}

async fn middleware_fn(
    State(app): State<server::App>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = req.uri().path().to_string();
    let host = req.headers().get("host").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    // host gate
    if !host_allowed(&app, &path, &host) {
        return server::http_error(403, "this host is not served here");
    }
    // Run next first to get response, then add headers? We need to add headers to response.
    // Instead, process request checks before calling next.
    let method = req.method().clone();
    // CORS preflight + headers decision needs path
    let is_addon = path.starts_with("/k/");
    // CSRF checks for mutating methods
    match method {
        axum::http::Method::POST | axum::http::Method::PUT | axum::http::Method::PATCH | axum::http::Method::DELETE => {
            if let Some(site) = req.headers().get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
                if site != "same-origin" && site != "none" {
                    return server::http_error(403, "cross-site request refused");
                }
            }
            if let Some(origin) = req.headers().get("origin").and_then(|v| v.to_str().ok()) {
                if !same_origin(&app, &req, origin) {
                    return server::http_error(403, "origin does not match this site");
                }
            }
            if method == axum::http::Method::POST && path.starts_with("/admin/api/") {
                let ct = req.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("");
                if !ct.starts_with("application/json") {
                    return server::http_error(415, "application/json required");
                }
            }
        }
        _ => {}
    }
    if method == axum::http::Method::OPTIONS {
        let mut r = Response::builder().status(204).body(axum::body::Body::empty()).unwrap();
        apply_security_headers(&app, &mut r, is_addon);
        return r;
    }
    let mut res = next.run(req).await;
    apply_security_headers(&app, &mut res, is_addon);
    res
}

fn apply_security_headers(app: &server::App, res: &mut Response, is_addon: bool) {
    let h = res.headers_mut();
    if is_addon {
        h.insert("access-control-allow-origin", "*".parse().unwrap());
        h.insert("access-control-allow-headers", "Content-Type, Range".parse().unwrap());
        h.insert("access-control-allow-methods", "GET, HEAD, OPTIONS".parse().unwrap());
        h.insert("access-control-expose-headers", "Content-Length, Content-Range, Accept-Ranges".parse().unwrap());
    }
    h.insert("x-content-type-options", "nosniff".parse().unwrap());
    h.insert("referrer-policy", "no-referrer".parse().unwrap());
    h.insert("x-frame-options", "DENY".parse().unwrap());
    h.insert("cross-origin-opener-policy", "same-origin".parse().unwrap());
    h.insert("permissions-policy", "camera=(), microphone=(), geolocation=()".parse().unwrap());
    if app.cfg.base_url.starts_with("https://") {
        h.insert("strict-transport-security", "max-age=31536000; includeSubDomains".parse().unwrap());
    }
}

fn host_allowed(app: &server::App, path: &str, host: &str) -> bool {
    if path == "/healthz" || path == "/readyz" {
        return true;
    }
    if app.cfg.base_url.is_empty() {
        return true;
    }
    let b = match url::Url::parse(&app.cfg.base_url) {
        Ok(u) => u,
        Err(_) => return true,
    };
    let want = match b.host_str() {
        Some(h) => h.to_string(),
        None => return true,
    };
    // include port if present
    let want_full = if let Some(p) = b.port() { format!("{want}:{p}") } else { want.clone() };
    util::hosts_equal(host, &want_full, b.scheme()) || util::hosts_equal(host, &want, b.scheme())
}

fn same_origin(app: &server::App, req: &axum::extract::Request, origin: &str) -> bool {
    let u = match url::Url::parse(origin) {
        Ok(u) => u,
        Err(_) => return false,
    };
    let Some(ohost) = u.host_str() else { return false };
    let want_host = if !app.cfg.base_url.is_empty() {
        match url::Url::parse(&app.cfg.base_url) {
            Ok(b) => b.host_str().unwrap_or("").to_string(),
            Err(_) => req.headers().get("host").and_then(|v| v.to_str().ok()).unwrap_or("").to_string(),
        }
    } else {
        req.headers().get("host").and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
    };
    let want_scheme = if !app.cfg.base_url.is_empty() {
        url::Url::parse(&app.cfg.base_url).map(|u| u.scheme().to_string()).unwrap_or_else(|_| "http".to_string())
    } else {
        "http".to_string()
    };
    // host header may include port; use hosts_equal
    let req_host = req.headers().get("host").and_then(|v| v.to_str().ok()).unwrap_or("");
    // origin host vs want host
    let origin_full = if let Some(p) = u.port() { format!("{ohost}:{p}") } else { ohost.to_string() };
    util::hosts_equal(&origin_full, &want_host, &want_scheme) || {
        // fallback: compare against request host when no base_url
        if app.cfg.base_url.is_empty() {
            util::hosts_equal(&origin_full, req_host, u.scheme())
        } else {
            false
        }
    }
}
