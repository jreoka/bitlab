use axum::http::HeaderMap;
use serde::Deserialize;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

pub const VPN_CACHE_TTL_SECS: u64 = 24 * 60 * 60;
const VPN_CACHE_MAX_ITEMS: usize = 5000;

pub type VpnCache = Arc<RwLock<HashMap<String, (bool, Instant)>>>;

pub fn new_cache() -> VpnCache {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Extracts the client's public IP from the usual proxy headers, falling back
/// to the socket address when no proxy headers are present.
pub fn client_ip(headers: &HeaderMap, fallback: &IpAddr) -> String {
    for name in [
        "x-forwarded-for",
        "x-forwarded-real",
        "cf-connecting-ip",
        "x-real-ip",
    ] {
        if let Some(value) = headers.get(name).and_then(|h| h.to_str().ok()) {
            let first = value
                .split(',')
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('"');
            if !first.is_empty() {
                return first.to_string();
            }
        }
    }
    fallback.to_string()
}

/// Private, loopback, link-local and unspecified addresses cannot be checked
/// (and are never VPN exits), so they always pass.
pub fn is_private_or_loopback(ip: &str) -> bool {
    if let Ok(v4) = ip.parse::<Ipv4Addr>() {
        return v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified();
    }
    if let Ok(v6) = ip.parse::<Ipv6Addr>() {
        return v6.is_loopback() || v6.is_unspecified() || v6.is_unique_local();
    }
    false
}

#[derive(Deserialize)]
struct IpApiResponse {
    status: String,
    #[allow(dead_code)]
    message: Option<String>,
    #[serde(default)]
    proxy: bool,
    #[serde(default)]
    hosting: bool,
}

/// Returns true when the client IP is a VPN/datacenter exit (or the check
/// cannot be performed). Fail-open: API errors and unknown/private addresses
/// allow access so an outage never breaks the addon for everyone.
pub async fn is_vpn(client: &reqwest::Client, cache: &VpnCache, ip: &str) -> bool {
    if is_private_or_loopback(ip) {
        return true;
    }

    {
        let cache = cache.read().await;
        if let Some((vpn, timestamp)) = cache.get(ip) {
            if timestamp.elapsed().as_secs() < VPN_CACHE_TTL_SECS {
                return *vpn;
            }
        }
    }

    // ip-api.com free tier: HTTP-only, 45 req/min from a single origin IP.
    // Results are cached for 24h, so this is well within limits for normal use.
    let url = format!(
        "http://ip-api.com/json/{}?fields=status,message,proxy,hosting",
        ip
    );
    let vpn = match client.get(&url).send().await {
        Ok(resp) => match resp.json::<IpApiResponse>().await {
            Ok(info) if info.status == "success" => info.proxy || info.hosting,
            _ => false,
        },
        Err(_) => false,
    };

    {
        let mut cache = cache.write().await;
        if cache.len() >= VPN_CACHE_MAX_ITEMS {
            cache.clear();
        }
        cache.insert(ip.to_string(), (vpn, Instant::now()));
    }

    vpn
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn test_is_private_or_loopback() {
        assert!(is_private_or_loopback("127.0.0.1"));
        assert!(is_private_or_loopback("192.168.1.10"));
        assert!(is_private_or_loopback("10.0.0.1"));
        assert!(is_private_or_loopback("::1"));
        assert!(!is_private_or_loopback("8.8.8.8"));
        assert!(!is_private_or_loopback("not-an-ip"));
    }

    #[test]
    fn test_client_ip() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("1.2.3.4, 5.6.7.8"),
        );
        assert_eq!(
            client_ip(&headers, &IpAddr::from([0, 0, 0, 0])),
            "1.2.3.4"
        );
        headers.remove("x-forwarded-for");
        headers.insert("x-real-ip", HeaderValue::from_static("9.9.9.9"));
        assert_eq!(
            client_ip(&headers, &IpAddr::from([0, 0, 0, 0])),
            "9.9.9.9"
        );
        headers.remove("x-real-ip");
        assert_eq!(
            client_ip(&headers, &IpAddr::from([1, 1, 1, 1])),
            "1.1.1.1"
        );
    }
}
