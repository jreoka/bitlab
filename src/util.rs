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
    if ih.len() != 40 {
        return false;
    }
    ih.bytes().all(|c| c.is_ascii_hexdigit() && (c.is_ascii_digit() || c.is_ascii_lowercase() || (c >= b'A' && c <= b'F') || (c >= b'a' && c <= b'f')))
        && ih.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F'))
        && ih.to_lowercase() == ih.to_lowercase() // keep case-insensitive check simple
        && is_lower_hex_or_digit(ih)
}

fn is_lower_hex_or_digit(s: &str) -> bool {
    // Accept any hex; the canonical form is lowercase. Uppercase is tolerated
    // by lowercasing at call sites, but raw validation requires hex only.
    s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F'))
}

pub fn valid_infohash_strict_lower(ih: &str) -> bool {
    ih.len() == 40 && ih.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn human_bytes(n: i64) -> String {
    const UNIT: i64 = 1024;
    if n < UNIT {
        return format!("{n} B");
    }
    let units = ["KiB", "MiB", "GiB", "TiB"];
    let mut div = UNIT as f64;
    let mut exp = 0usize;
    let mut v = n as f64 / UNIT as f64;
    while v >= 1024.0 && exp < units.len() - 1 {
        div *= 1024.0;
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

#[allow(dead_code)]
pub fn sniff_container(b: &[u8]) -> Option<(&'static str, &'static str)> {
    if b.len() >= 8 && &b[4..8] == b"ftyp" {
        let brand = if b.len() >= 12 { String::from_utf8_lossy(&b[8..12.min(b.len())]).trim().to_string() } else { String::new() };
        if brand.starts_with("qt") {
            return Some((".mov", "video/quicktime"));
        }
        return Some((".mp4", "video/mp4"));
    }
    if b.len() >= 4 && b[0] == 0x1a && b[1] == 0x45 && b[2] == 0xdf && b[3] == 0xa3 {
        return Some((".mkv", "video/x-matroska"));
    }
    if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"AVI " {
        return Some((".avi", "video/x-msvideo"));
    }
    if b.len() >= 4 && &b[0..4] == b"OggS" {
        return Some((".ogv", "video/ogg"));
    }
    if b.len() >= 3 && &b[0..3] == b"FLV" {
        return Some((".flv", "video/x-flv"));
    }
    if !b.is_empty() && b[0] == 0x47 {
        return Some((".ts", "video/mp2t"));
    }
    if b.len() >= 4 && b[0] == 0x00 && b[1] == 0x00 && b[2] == 0x01 && (b[3] == 0xba || b[3] == 0xb3) {
        return Some((".mpg", "video/mpeg"));
    }
    None
}
