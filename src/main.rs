mod cinemeta;
mod scraper;
mod stremio;

use axum::{
    Router,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Json, Response},
    routing::get,
};
use std::net::SocketAddr;
use stremio::{Manifest, Stream, StreamResponse};
use tower_http::cors::{Any, CorsLayer};

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    meta_cache: Arc<RwLock<HashMap<String, (String, Option<String>)>>>,
    stream_cache: Arc<RwLock<HashMap<String, (Vec<Stream>, std::time::Instant)>>>,
    torrent_files_cache: Arc<RwLock<HashMap<String, Vec<scraper::TorrentFile>>>>,
}

#[tokio::main]
async fn main() {
    // Set up tracing / logging
    println!("Starting Bitlab Stremio Addon...");

    let mut headers = axum::http::HeaderMap::new();
    headers.insert("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36".parse().unwrap());
    headers.insert("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/json".parse().unwrap());
    headers.insert("Accept-Language", "en-US,en;q=0.9".parse().unwrap());
    headers.insert("Cache-Control", "no-cache".parse().unwrap());

    let state = AppState {
        client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .default_headers(headers)
            .build()
            .unwrap(),
        meta_cache: Arc::new(RwLock::new(HashMap::new())),
        stream_cache: Arc::new(RwLock::new(HashMap::new())),
        torrent_files_cache: Arc::new(RwLock::new(HashMap::new())),
    };

    // Spawn background cache pruner task to prevent memory leaks/indefinite growth
    let state_clone = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await; // run hourly

            // 1. Prune expired stream cache entries (older than 1 hour)
            {
                let mut cache = state_clone.stream_cache.write().await;
                cache.retain(|_, (_, timestamp)| {
                    timestamp.elapsed().as_secs() < scraper::STREAM_CACHE_TTL_SECS
                });
            }

            // 2. Clear meta cache if it grows too large (keep it under 5000 items)
            {
                let mut cache = state_clone.meta_cache.write().await;
                if cache.len() > 5000 {
                    cache.clear();
                }
            }

            // 3. Clear torrent files cache if it grows too large (keep it under 5000 items)
            {
                let mut cache = state_clone.torrent_files_cache.write().await;
                if cache.len() > 5000 {
                    cache.clear();
                }
            }
        }
    });

    // Configure CORS
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_headers(Any)
        .allow_methods(Any);

    // Set up routes
    let app = Router::new()
        .route("/", get(landing_handler))
        .route("/manifest.json", get(manifest_handler))
        .route("/stream/:type/:id", get(stream_handler))
        .route("/favicon.ico", get(favicon_handler))
        .route("/favicon.svg", get(favicon_handler))
        .layer(middleware::from_fn(host_validation_middleware))
        .layer(cors)
        .with_state(state);

    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("Listening on http://{}", addr);
    println!(
        "Addon manifest is available at http://{}/manifest.json",
        addr
    );

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn favicon_handler() -> impl IntoResponse {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><circle cx="16" cy="16" r="14" fill="black" stroke="white" stroke-width="2"/></svg>"#;
    (
        [
            (axum::http::header::CONTENT_TYPE, "image/svg+xml"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        svg,
    )
}

async fn manifest_handler() -> impl IntoResponse {
    let manifest = Manifest {
        id: "org.bitlab.stremio".to_string(),
        version: "1.0.0".to_string(),
        name: "Bitlab".to_string(),
        description: Some("A high-performance Stremio scraper addon by Bitlab.".to_string()),
        resources: vec!["stream".to_string()],
        types: vec!["movie".to_string(), "series".to_string()],
        catalogs: vec![],
        id_prefixes: vec!["tt".to_string(), "kitsu".to_string()],
    };

    Json(manifest)
}

async fn stream_handler(
    Path((r#type, id)): Path<(String, String)>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let clean_id = id.strip_suffix(".json").unwrap_or(&id);
    println!("Stream requested: type={}, id={}", r#type, clean_id);

    let streams = match r#type.as_str() {
        "movie" => {
            if is_valid_imdb_id(clean_id) {
                scraper::get_movie_streams(
                    &state.client,
                    &state.meta_cache,
                    &state.stream_cache,
                    clean_id,
                )
                .await
            } else {
                Vec::new()
            }
        }
        "series" => {
            // Series IDs are formatted as imdb_id:season:episode
            // But kitsu IDs are formatted as kitsu:id:episode
            let parts: Vec<&str> = clean_id.split(':').collect();
            if parts.len() == 3 {
                if parts[0] == "kitsu" {
                    match (parts[1].parse::<u64>(), parts[2].parse::<u32>()) {
                        (Ok(kitsu_id), Ok(episode)) if kitsu_id > 0 && episode > 0 => {
                            let kitsu_id = format!("kitsu:{}", kitsu_id);
                            // Kitsu uses absolute episode numbering and has no
                            // season component in its Stremio video ID.
                            scraper::get_series_streams(
                                &state.client,
                                &state.meta_cache,
                                &state.stream_cache,
                                &state.torrent_files_cache,
                                &kitsu_id,
                                1,
                                episode,
                            )
                            .await
                        }
                        _ => Vec::new(),
                    }
                } else {
                    let imdb_id = parts[0];
                    match (parts[1].parse::<u32>(), parts[2].parse::<u32>()) {
                        (Ok(season), Ok(episode)) if is_valid_imdb_id(imdb_id) && episode > 0 => {
                            scraper::get_series_streams(
                                &state.client,
                                &state.meta_cache,
                                &state.stream_cache,
                                &state.torrent_files_cache,
                                imdb_id,
                                season,
                                episode,
                            )
                            .await
                        }
                        _ => Vec::new(),
                    }
                }
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        "Cache-Control",
        "max-age=0, no-cache, no-store, must-revalidate"
            .parse()
            .unwrap(),
    );

    (headers, Json(StreamResponse { streams })).into_response()
}

fn is_valid_imdb_id(id: &str) -> bool {
    let Some(digits) = id.strip_prefix("tt") else {
        return false;
    };
    digits.len() >= 7 && digits.chars().all(|c| c.is_ascii_digit())
}

async fn landing_handler(headers: HeaderMap) -> impl IntoResponse {
    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:3000");

    let manifest_url = if let Ok(allowed) = std::env::var("ALLOWED_URL") {
        format!("{}/manifest.json", allowed.trim_end_matches('/'))
    } else {
        let proto = headers
            .get("x-forwarded-proto")
            .and_then(|p| p.to_str().ok())
            .unwrap_or("http");
        format!("{}://{}/manifest.json", proto, host)
    };

    let html_content = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Bitlab</title>
    <link rel="icon" type="image/svg+xml" href="/favicon.svg">
    <link href="https://fonts.googleapis.com/css2?family=Outfit:wght@400;600;800&display=swap" rel="stylesheet">
    <style>
        body {{
            font-family: 'Outfit', sans-serif;
            background-color: #000000;
            color: #ffffff;
            box-sizing: border-box;
            min-height: 100dvh;
            display: flex;
            align-items: center;
            justify-content: center;
            padding: 24px;
            margin: 0;
        }}
        .container {{
            max-width: 480px;
            width: 100%;
            display: flex;
            flex-direction: column;
            gap: 4px;
        }}
        h1 {{
            font-size: 2.2rem;
            font-weight: 800;
            letter-spacing: -0.03em;
            margin: 0;
        }}
        .url-box {{
            background: #090909;
            border: 1px solid #1a1a1a;
            border-radius: 12px;
            padding: 8px 8px 8px 16px;
            display: flex;
            align-items: center;
            justify-content: space-between;
            gap: 12px;
        }}
        .url-text {{
            font-family: monospace;
            font-size: 0.85rem;
            color: #a1a1aa;
            white-space: nowrap;
            overflow: hidden;
            text-overflow: ellipsis;
            user-select: all;
        }}
        .btn-copy {{
            background: #ffffff;
            color: #000000;
            border: none;
            padding: 10px 18px;
            border-radius: 8px;
            font-size: 0.85rem;
            font-weight: 600;
            cursor: pointer;
            transition: all 0.2s ease;
            white-space: nowrap;
        }}
        .btn-copy:hover {{
            opacity: 0.9;
        }}
        .copied {{
            background: #00ff66 !important;
            color: #000000 !important;
        }}
    </style>
</head>
<body>
    <div class="container">
        <h1>Bitlab</h1>
        <div class="url-box">
            <span class="url-text" id="manifest-url">{manifest_url}</span>
            <button class="btn-copy" onclick="copyManifestUrl()" id="copy-btn">Copy URL</button>
        </div>
    </div>

    <script>
        function copyManifestUrl() {{
            const urlText = document.getElementById('manifest-url').innerText;
            navigator.clipboard.writeText(urlText).then(() => {{
                const copyBtn = document.getElementById('copy-btn');
                copyBtn.innerText = 'Copied';
                copyBtn.classList.add('copied');
                setTimeout(() => {{
                    copyBtn.innerText = 'Copy URL';
                    copyBtn.classList.remove('copied');
                }}, 2000);
            }});
        }}
    </script>
</body>
</html>"#
    );

    Html(html_content)
}

async fn host_validation_middleware(req: Request, next: Next) -> Result<Response, StatusCode> {
    if let Ok(allowed_url) = std::env::var("ALLOWED_URL") {
        // Extract host header (prefer X-Forwarded-Host if behind a proxy)
        let host = req
            .headers()
            .get("x-forwarded-host")
            .or_else(|| req.headers().get("host"))
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");

        // Extract protocol
        let proto = req
            .headers()
            .get("x-forwarded-proto")
            .and_then(|p| p.to_str().ok())
            .unwrap_or("http");

        let request_url = format!("{}://{}", proto, host);

        if let (Some(req_parsed), Some(allow_parsed)) = (
            parse_scheme_host_port(&request_url),
            parse_scheme_host_port(&allowed_url),
        ) {
            if req_parsed != allow_parsed {
                println!(
                    "Forbidden access: Request URL scheme/host/port ({:?}) does not match ALLOWED_URL ({:?})",
                    req_parsed, allow_parsed
                );
                return Err(StatusCode::FORBIDDEN);
            }
        } else {
            println!(
                "Forbidden access: Failed to parse request URL ({}) or ALLOWED_URL ({})",
                request_url, allowed_url
            );
            return Err(StatusCode::FORBIDDEN);
        }
    }

    Ok(next.run(req).await)
}

fn parse_scheme_host_port(url_str: &str) -> Option<(String, String, Option<u16>)> {
    let parts: Vec<&str> = url_str.splitn(2, "://").collect();
    if parts.len() != 2 {
        return None;
    }
    let scheme = parts[0].to_lowercase();

    // Split by '/' to remove path, e.g. "bitlab.dill.moe/manifest.json" -> "bitlab.dill.moe"
    let host_port_path: Vec<&str> = parts[1].splitn(2, '/').collect();
    let host_port = host_port_path[0];

    let host_port_parts: Vec<&str> = host_port.splitn(2, ':').collect();
    let host = host_port_parts[0].to_lowercase();
    let mut port = None;
    if host_port_parts.len() == 2 {
        if let Ok(p) = host_port_parts[1].parse::<u16>() {
            port = Some(p);
        }
    }

    let normalized_port = match (scheme.as_str(), port) {
        ("http", Some(80)) => None,
        ("https", Some(443)) => None,
        _ => port,
    };

    Some((scheme, host, normalized_port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_scheme_host_port() {
        assert_eq!(
            parse_scheme_host_port("https://bitlab.dill.moe"),
            Some(("https".to_string(), "bitlab.dill.moe".to_string(), None))
        );
        assert_eq!(
            parse_scheme_host_port("https://bitlab.dill.moe/"),
            Some(("https".to_string(), "bitlab.dill.moe".to_string(), None))
        );
        assert_eq!(
            parse_scheme_host_port("https://bitlab.dill.moe/manifest.json"),
            Some(("https".to_string(), "bitlab.dill.moe".to_string(), None))
        );
        assert_eq!(
            parse_scheme_host_port("https://bitlab.dill.moe:443"),
            Some(("https".to_string(), "bitlab.dill.moe".to_string(), None))
        );
        assert_eq!(
            parse_scheme_host_port("http://localhost:3000/manifest.json"),
            Some(("http".to_string(), "localhost".to_string(), Some(3000)))
        );
        assert_eq!(
            parse_scheme_host_port("http://127.0.0.1:80/"),
            Some(("http".to_string(), "127.0.0.1".to_string(), None))
        );
        assert_eq!(parse_scheme_host_port("invalid_url"), None);
    }

    #[test]
    fn test_validate_imdb_id() {
        assert!(is_valid_imdb_id("tt0111161"));
        assert!(is_valid_imdb_id("tt12345678"));
        assert!(!is_valid_imdb_id("tt123"));
        assert!(!is_valid_imdb_id("tt1234abc"));
        assert!(!is_valid_imdb_id("kitsu:1"));
    }
}
