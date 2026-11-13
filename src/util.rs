const B32_ALPHABET_LOWER: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";

pub fn rand_token(nbytes: usize) -> String {
    let mut b = vec![0u8; nbytes];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut b);
    // base32 lower-case, no padding, URL-safe (matches Go b32)
    let mut out = String::new();
    let mut bits: u32 = 0;
    let mut nbits = 0;
    for byte in b {
        bits = (bits << 8) | byte as u32;
        nbits += 8;
        while nbits >= 5 {
            nbits -= 5;
            out.push(B32_ALPHABET_LOWER[((bits >> nbits) & 31) as usize] as char);
        }
    }
    if nbits > 0 {
        out.push(B32_ALPHABET_LOWER[((bits << (5 - nbits)) & 31) as usize] as char);
    }
    out
}

pub fn rand_hex(nbytes: usize) -> String {
    let mut b = vec![0u8; nbytes];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut b);
    hex::encode(b)
}

pub fn valid_infohash(ih: &str) -> bool {
    ih.len() == 40 && ih.bytes().all(|c| c.is_ascii_hexdigit())
}

pub fn human_bytes(n: i64) -> String {
    const UNIT: i64 = 1000;
    if n < UNIT {
        return format!("{n} B");
    }
    let units = ["KB", "MB", "GB", "TB"];
    let mut div = UNIT as f64;
    let mut exp = 0usize;
    let mut v = n as f64 / UNIT as f64;
    while v >= 1000.0 && exp < units.len() - 1 {
        div *= 1000.0;
        v = n as f64 / div;
        exp += 1;
    }
    format!("{v:.1} {}", units[exp])
}

pub fn valid_tracker(raw: &str) -> bool {
    use std::net::IpAddr;
    let raw = raw.trim();
    let u = match url::Url::parse(raw) {
        Ok(u) => u,
        Err(_) => return false,
    };
    match u.scheme() {
        "http" | "https" | "udp" => {}
        _ => return false,
    }
    match u.host() {
        Some(url::Host::Ipv4(v4)) => !crate::netguard::is_private_address_ip(&IpAddr::V4(v4)),
        Some(url::Host::Ipv6(v6)) => !crate::netguard::is_private_address_ip(&IpAddr::V6(v6)),
        Some(url::Host::Domain(d)) => {
            if crate::netguard::is_private_hostname(d) {
                return false;
            }
            let host = d.trim_end_matches('.').to_ascii_lowercase();
            if let Ok(ip) = host.parse::<IpAddr>() {
                return !crate::netguard::is_private_address_ip(&ip);
            }
            // Non-special schemes (udp://) do not invoke WHATWG IPv4 parsing in url::Url,
            // so octal/hex/short IPv4s like 0x7f.0.0.1 or 127.1 arrive as Domain.
            if let Ok(http_probe) = url::Url::parse(&format!("http://{host}")) {
                match http_probe.host() {
                    Some(url::Host::Ipv4(v4)) => {
                        return !crate::netguard::is_private_address_ip(&IpAddr::V4(v4));
                    }
                    Some(url::Host::Ipv6(v6)) => {
                        return !crate::netguard::is_private_address_ip(&IpAddr::V6(v6));
                    }
                    _ => {}
                }
            }
            host.contains('.')
        }
        None => false,
    }
}

pub fn hosts_equal(got: &str, want: &str, scheme: &str) -> bool {
    let got = got.trim();
    let want = want.trim();
    if got.is_empty() || want.is_empty() {
        return false;
    }
    if got.eq_ignore_ascii_case(want) {
        return true;
    }
    let def = match scheme.to_ascii_lowercase().as_str() {
        "https" => "443",
        "http" => "80",
        _ => return false,
    };
    fn strip<'a>(h: &'a str, def: &str) -> &'a str {
        let suffix = format!(":{def}");
        h.strip_suffix(&suffix).unwrap_or(h)
    }
    strip(got, def).eq_ignore_ascii_case(strip(want, def))
}

pub fn range_start(h: &str) -> Option<i64> {
    let h = h.trim();
    if h.len() < 6 || !h[..6].eq_ignore_ascii_case("bytes=") {
        return None;
    }
    let rest = &h[6..];
    let spec = rest.split(',').next().unwrap_or("").trim();
    let dash = spec.find('-')?;
    if dash == 0 {
        return None;
    }
    let start_str = spec[..dash].trim();
    if !start_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    start_str.parse::<i64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_human_bytes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1_000), "1.0 KB");
        assert_eq!(human_bytes(1_500), "1.5 KB");
        assert_eq!(human_bytes(1_000_000), "1.0 MB");
        assert_eq!(human_bytes(1_000_000_000), "1.0 GB");
        assert_eq!(human_bytes(1_500_000_000), "1.5 GB");
        assert_eq!(human_bytes(25_000_000_000), "25.0 GB");
        assert_eq!(human_bytes(1_000_000_000_000), "1.0 TB");
    }

    #[test]
    fn test_valid_tracker() {
        assert!(valid_tracker("udp://tracker.opentrackr.org:1337/announce"));
        assert!(valid_tracker("https://1.tracker.eu.org:443/announce"));
        assert!(valid_tracker("http://[2606:4700:4700::1111]:80/announce"));
        assert!(!valid_tracker("http://127.0.0.1:8080/announce"));
        assert!(!valid_tracker("http://[::1]:8080/announce"));
        assert!(!valid_tracker("http://[::ffff:127.0.0.1]:8080/announce"));
        assert!(!valid_tracker("udp://[::ffff:10.0.0.1]:1337/announce"));
        assert!(!valid_tracker("http://localhost./announce"));
        assert!(!valid_tracker("udp://0x7f.0.0.1:1337/announce"));
        assert!(!valid_tracker("udp://127.1:1337/announce"));
        assert!(!valid_tracker("udp://0177.0.0.1:1337/announce"));
        assert!(!valid_tracker("file:///etc/passwd"));
    }

    #[test]
    fn test_hosts_equal() {
        assert!(hosts_equal("example.com:443", "example.com", "https"));
        assert!(hosts_equal("example.com", "example.com:80", "http"));
        assert!(!hosts_equal("", "", "http"));
        assert!(!hosts_equal("example.com:8080", "example.com", "http"));
    }

    #[test]
    fn test_range_start() {
        assert_eq!(range_start("bytes=0-100"), Some(0));
        assert_eq!(range_start("Bytes=1024-"), Some(1024));
        assert_eq!(range_start("bytes=-500"), None);
        assert_eq!(range_start("bytes=+10-20"), None);
    }
}
