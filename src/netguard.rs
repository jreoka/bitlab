use std::net::{IpAddr, Ipv4Addr};

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
            if v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_multicast() || v4.is_unspecified() || v4.is_broadcast() || v4.is_documentation() {
                return true;
            }
            // is_private covers 10/8, 172.16/12, 192.168/16. Add the rest manually.
            for (net, prefix) in reserved_nets_v4() {
                if ipv4_in_cidr(*v4, net, prefix) {
                    return true;
                }
            }
            // 169.254/16 link-local already covered; 100.64/10 covered.
            false
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_multicast() || v6.is_unspecified() {
                return true;
            }
            let seg = v6.segments();
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
            // ::ffff:0:0/96 + 100::/64 discard (simplified)
            if seg[0] == 0 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0 && seg[4] == 0 && (seg[5] == 0 || seg[5] == 0xffff) {
                // v4-mapped: check embedded v4?
                return false;
            }
            false
        }
    }
}

pub fn build_client(allow_private: bool) -> reqwest::Client {
    let _ = allow_private;
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(25))
        .user_agent("bitlab/1.0")
        .build()
        .expect("http client")
}
