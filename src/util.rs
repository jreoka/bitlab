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
    let raw = raw.trim();
    let u = match url::Url::parse(raw) {
        Ok(u) => u,
        Err(_) => return false,
    };
    match u.scheme().to_lowercase().as_str() {
        "http" | "https" | "udp" => {}
        _ => return false,
    }
    let host = match u.host_str() {
        Some(h) => h.to_lowercase(),
        None => return false,
    };
    if host.is_empty() || host == "localhost" || host.ends_with(".local") || host.ends_with(".internal") || host.ends_with(".localdomain") {
        return false;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return !crate::netguard::is_private_address_ip(&ip);
    }
    host.contains('.')
}

pub fn hosts_equal(got: &str, want: &str, scheme: &str) -> bool {
    if got.eq_ignore_ascii_case(want) {
        return true;
    }
    let def = match scheme {
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
    let rest = h.strip_prefix("bytes=")?;
    let spec = rest.split(',').next().unwrap_or("").trim();
    let dash = spec.find('-')?;
    if dash == 0 {
        return None;
    }
    spec[..dash].trim().parse::<i64>().ok()
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
}
