use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::time::Duration;

fn reserved_nets_v4() -> Vec<(Ipv4Addr, u8)> {
    vec![
        ("0.0.0.0", 8),
        ("100.64.0.0", 10),
        ("192.0.0.0", 24),
        ("192.0.2.0", 24),
        ("198.18.0.0", 15),
        ("198.51.100.0", 24),
        ("203.0.113.0", 24),
        ("240.0.0.0", 4),
    ]
    .into_iter()
    .map(|(s, p)| (s.parse().unwrap(), p))
    .collect()
}

fn ipv4_in_cidr(ip: Ipv4Addr, net: Ipv4Addr, prefix: u8) -> bool {
    if prefix == 0 {
        return true;
    }
    let mask = if prefix >= 32 { u32::MAX } else { (!0u32) << (32 - prefix) };
    (u32::from(ip) & mask) == (u32::from(net) & mask)
}

pub fn is_private_address_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
            {
                return true;
            }
            // is_private covers 10/8, 172.16/12, 192.168/16. Add the rest manually.
            for (net, prefix) in reserved_nets_v4() {
                if ipv4_in_cidr(*v4, net, prefix) {
                    return true;
                }
            }
            false
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_multicast() || v6.is_unspecified() {
                return true;
            }
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_address_ip(&IpAddr::V4(v4));
            }
            let seg = v6.segments();
            // ::/96 deprecated IPv4-compatible IPv6
            if seg[0] == 0 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0 && seg[4] == 0 && seg[5] == 0 {
                let v4 = Ipv4Addr::from(((seg[6] as u32) << 16) | (seg[7] as u32));
                return is_private_address_ip(&IpAddr::V4(v4));
            }
            // 100::/64 discard-only (RFC 6666)
            if seg[0] == 0x0100 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0 {
                return true;
            }
            // fe80::/10 link-local
            if seg[0] & 0xffc0 == 0xfe80 {
                return true;
            }
            // fc00::/7 unique-local
            if seg[0] & 0xfe00 == 0xfc00 {
                return true;
            }
            // 2001:db8::/32 documentation
            if seg[0] == 0x2001 && seg[1] == 0x0db8 {
                return true;
            }
            // 2001:2::/48 benchmarking
            if seg[0] == 0x2001 && seg[1] == 0x0002 && seg[2] == 0 {
                return true;
            }
            // 2001:10::/28 & 2001:20::/28 ORCHID / ORCHIDv2
            if seg[0] == 0x2001 && (seg[1] & 0xfff0 == 0x0010 || seg[1] & 0xfff0 == 0x0020) {
                return true;
            }
            // 64:ff9b::/96 & 64:ff9b:1::/48 NAT64
            if seg[0] == 0x0064 && seg[1] == 0xff9b {
                if seg[2] == 0 && seg[3] == 0 && seg[4] == 0 && seg[5] == 0 {
                    let v4 = Ipv4Addr::from(((seg[6] as u32) << 16) | (seg[7] as u32));
                    return is_private_address_ip(&IpAddr::V4(v4));
                }
                if seg[2] == 1 {
                    return true;
                }
            }
            // 2002::/16 6to4 embedding an IPv4 address
            if seg[0] == 0x2002 {
                let v4 = Ipv4Addr::from(((seg[1] as u32) << 16) | (seg[2] as u32));
                return is_private_address_ip(&IpAddr::V4(v4));
            }
            false
        }
    }
}

pub fn is_private_hostname(host: &str) -> bool {
    let h = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if h.is_empty()
        || h == "localhost"
        || h.ends_with(".localhost")
        || h.ends_with(".local")
        || h.ends_with(".internal")
        || h.ends_with(".localdomain")
    {
        return true;
    }
    if let Ok(ip) = h.parse::<IpAddr>() {
        return is_private_address_ip(&ip);
    }
    false
}

pub fn url_allowed(u: &url::Url, allow_private: bool) -> bool {
    let scheme = u.scheme();
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    match u.host() {
        Some(url::Host::Ipv4(v4)) => allow_private || !is_private_address_ip(&IpAddr::V4(v4)),
        Some(url::Host::Ipv6(v6)) => allow_private || !is_private_address_ip(&IpAddr::V6(v6)),
        Some(url::Host::Domain(d)) => {
            if d.trim().is_empty() {
                return false;
            }
            allow_private || !is_private_hostname(d)
        }
        None => false,
    }
}

struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            if is_private_hostname(&host) {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("refused private/reserved host {host:?}"),
                )) as Box<dyn std::error::Error + Send + Sync>);
            }
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if addrs.is_empty() {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("no addresses resolved for {host:?}"),
                )) as Box<dyn std::error::Error + Send + Sync>);
            }
            for addr in &addrs {
                if is_private_address_ip(&addr.ip()) {
                    return Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!("refused private/reserved address {} for {host:?}", addr.ip()),
                    )) as Box<dyn std::error::Error + Send + Sync>);
                }
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        }) as Pin<Box<dyn Future<Output = Result<reqwest::dns::Addrs, _>> + Send>>
    }
}

pub fn build_client(allow_private: bool) -> reqwest::Client {
    let mut b = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(25))
        .user_agent("bitlab/1.0");
    if allow_private {
        b = b.redirect(reqwest::redirect::Policy::limited(5));
    } else {
        b = b.dns_resolver(std::sync::Arc::new(PublicOnlyResolver)).redirect(
            reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 5 {
                    attempt.error("too many redirects")
                } else if !url_allowed(attempt.url(), false) {
                    attempt.error("redirect to private or disallowed url blocked")
                } else {
                    attempt.follow()
                }
            }),
        );
    }
    b.build().expect("http client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_private_address_ip() {
        assert!(is_private_address_ip(&"127.0.0.1".parse().unwrap()));
        assert!(is_private_address_ip(&"10.0.0.1".parse().unwrap()));
        assert!(is_private_address_ip(&"172.16.5.4".parse().unwrap()));
        assert!(is_private_address_ip(&"192.168.1.1".parse().unwrap()));
        assert!(is_private_address_ip(&"169.254.169.254".parse().unwrap()));
        assert!(is_private_address_ip(&"100.64.0.1".parse().unwrap()));
        assert!(is_private_address_ip(&"0.0.0.1".parse().unwrap()));

        assert!(is_private_address_ip(&"::1".parse().unwrap()));
        assert!(is_private_address_ip(&"::".parse().unwrap()));
        assert!(is_private_address_ip(&"fe80::1".parse().unwrap()));
        assert!(is_private_address_ip(&"fc00::1".parse().unwrap()));
        assert!(is_private_address_ip(&"fd12:3456::1".parse().unwrap()));
        assert!(is_private_address_ip(&"::ffff:127.0.0.1".parse().unwrap()));
        assert!(is_private_address_ip(&"::ffff:169.254.169.254".parse().unwrap()));
        assert!(is_private_address_ip(&"::ffff:10.0.0.1".parse().unwrap()));
        assert!(is_private_address_ip(&"::127.0.0.1".parse().unwrap()));
        assert!(is_private_address_ip(&"100::1".parse().unwrap()));
        assert!(is_private_address_ip(&"2002:7f00:0001::1".parse().unwrap()));
        assert!(is_private_address_ip(&"64:ff9b::127.0.0.1".parse().unwrap()));

        assert!(!is_private_address_ip(&"8.8.8.8".parse().unwrap()));
        assert!(!is_private_address_ip(&"1.1.1.1".parse().unwrap()));
        assert!(!is_private_address_ip(&"2606:4700:4700::1111".parse().unwrap()));
        assert!(!is_private_address_ip(&"::ffff:8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn test_url_allowed() {
        assert!(url_allowed(&url::Url::parse("https://api.knaben.org/v1").unwrap(), false));
        assert!(!url_allowed(&url::Url::parse("http://127.0.0.1:8080/api").unwrap(), false));
        assert!(!url_allowed(&url::Url::parse("http://[::ffff:127.0.0.1]/api").unwrap(), false));
        assert!(!url_allowed(&url::Url::parse("http://localhost./api").unwrap(), false));
        assert!(!url_allowed(&url::Url::parse("file:///etc/passwd").unwrap(), false));
        assert!(url_allowed(&url::Url::parse("http://127.0.0.1:8080/api").unwrap(), true));
    }
}
