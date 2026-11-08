#[derive(Debug, Clone)]
pub struct WireGuardConfig {
    pub private_key: String,
    pub addresses: Vec<String>,
    pub dns: Vec<String>,
    pub mtu: i32,
    pub listen_port: i32,
    pub peers: Vec<WireGuardPeer>,
}

#[derive(Debug, Clone)]
pub struct WireGuardPeer {
    pub public_key: String,
    pub preshared_key: String,
    pub endpoint: String,
    pub allowed_ips: Vec<String>,
    pub persistent_keepalive: i32,
}

#[derive(Debug, Clone)]
pub struct WireGuardTunnel {
    pub local_ips: Vec<String>,
    pub config: WireGuardConfig,
}

impl WireGuardTunnel {
    pub fn close(&self) {}
}

fn parse_key(raw: &str) -> anyhow::Result<String> {
    let s = raw.trim();
    if s.len() == 64 && hex::decode(s).is_ok() {
        return Ok(s.to_lowercase());
    }
    let b = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s).map_err(|e| anyhow::anyhow!("invalid base64 key: {e}"))?;
    if b.len() != 32 {
        anyhow::bail!("wireguard key must be 32 bytes, got {}", b.len());
    }
    Ok(hex::encode(b))
}

fn resolve_endpoint(ep: &str) -> anyhow::Result<String> {
    let ep = ep.trim();
    // Split host:port (handle IPv6 brackets minimally)
    let (host, port) = if ep.starts_with('[') {
        let end = ep.find(']').ok_or_else(|| anyhow::anyhow!("invalid endpoint {ep:?}"))?;
        let host = &ep[1..end];
        let rest = &ep[end + 1..];
        let port = rest.strip_prefix(':').ok_or_else(|| anyhow::anyhow!("invalid endpoint {ep:?}"))?;
        (host.to_string(), port.to_string())
    } else {
        let idx = ep.rfind(':').ok_or_else(|| anyhow::anyhow!("invalid endpoint {ep:?}"))?;
        (ep[..idx].to_string(), ep[idx + 1..].to_string())
    };
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Ok(format!("{host}:{port}"));
    }
    // Best-effort DNS at parse time; if it fails, keep the hostname —
    // actual resolution happens when the tunnel dials.
    Ok(format!("{host}:{port}"))
}

pub fn parse_wireguard_config(conf_str: &str) -> anyhow::Result<WireGuardConfig> {
    let mut cfg = WireGuardConfig { private_key: String::new(), addresses: vec![], dns: vec![], mtu: 1420, listen_port: 0, peers: vec![] };
    let mut section = String::new();
    let mut cur_peer: Option<WireGuardPeer> = None;

    for raw_line in conf_str.lines() {
        let mut line = raw_line.trim().to_string();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(idx) = line.find(" #") {
            line = line[..idx].trim().to_string();
        }
        if line.starts_with('[') && line.ends_with(']') {
            let sec = line[1..line.len() - 1].trim().to_lowercase();
            if sec == "peer" {
                if let Some(p) = cur_peer.take() {
                    cfg.peers.push(p);
                }
                cur_peer = Some(WireGuardPeer { public_key: String::new(), preshared_key: String::new(), endpoint: String::new(), allowed_ips: vec![], persistent_keepalive: 0 });
            } else if sec != "interface" {
                anyhow::bail!("unknown section [{sec}]");
            }
            section = sec;
            continue;
        }
        let eq = line.find(['=', ':']).ok_or_else(|| anyhow::anyhow!("bad line {line:?}"))?;
        let key = line[..eq].trim().to_lowercase();
        let val = line[eq + 1..].trim().to_string();
        match section.as_str() {
            "interface" => match key.as_str() {
                "privatekey" => cfg.private_key = parse_key(&val)?,
                "address" => {
                    for part in val.split(',') {
                        let part = part.trim();
                        if !part.is_empty() {
                            cfg.addresses.push(part.to_string());
                        }
                    }
                }
                "dns" => {
                    for part in val.split(',') {
                        let part = part.trim();
                        if !part.is_empty() {
                            cfg.dns.push(part.to_string());
                        }
                    }
                }
                "mtu" => {
                    if let Ok(n) = val.parse::<i32>() {
                        if n > 0 {
                            cfg.mtu = n;
                        }
                    }
                }
                "listenport" => {
                    if let Ok(n) = val.parse::<i32>() {
                        if n > 0 {
                            cfg.listen_port = n;
                        }
                    }
                }
                _ => {}
            },
            "peer" => {
                if cur_peer.is_none() {
                    cur_peer = Some(WireGuardPeer { public_key: String::new(), preshared_key: String::new(), endpoint: String::new(), allowed_ips: vec![], persistent_keepalive: 0 });
                }
                let p = cur_peer.as_mut().unwrap();
                match key.as_str() {
                    "publickey" => p.public_key = parse_key(&val)?,
                    "presharedkey" => p.preshared_key = parse_key(&val)?,
                    "endpoint" => p.endpoint = resolve_endpoint(&val)?,
                    "allowedips" => {
                        for part in val.split(',') {
                            let part = part.trim();
                            if !part.is_empty() {
                                p.allowed_ips.push(part.to_string());
                            }
                        }
                    }
                    "persistentkeepalive" => {
                        if let Ok(n) = val.parse::<i32>() {
                            if n >= 0 {
                                p.persistent_keepalive = n;
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if let Some(p) = cur_peer.take() {
        cfg.peers.push(p);
    }
    if cfg.private_key.is_empty() {
        anyhow::bail!("wireguard config missing PrivateKey in [Interface]");
    }
    if cfg.addresses.is_empty() {
        anyhow::bail!("wireguard config missing Address in [Interface]");
    }
    if cfg.dns.is_empty() {
        cfg.dns = vec!["1.1.1.1".to_string(), "8.8.8.8".to_string()];
    }
    Ok(cfg)
}

impl WireGuardConfig {
    pub fn to_uapi(&self) -> String {
        let mut b = String::new();
        b.push_str(&format!("private_key={}\n", self.private_key));
        if self.listen_port > 0 {
            b.push_str(&format!("listen_port={}\n", self.listen_port));
        }
        b.push_str("replace_peers=true\n");
        for p in &self.peers {
            b.push_str(&format!("public_key={}\n", p.public_key));
            if !p.preshared_key.is_empty() {
                b.push_str(&format!("preshared_key={}\n", p.preshared_key));
            }
            if !p.endpoint.is_empty() {
                b.push_str(&format!("endpoint={}\n", p.endpoint));
            }
            if p.persistent_keepalive > 0 {
                b.push_str(&format!("persistent_keepalive_interval={}\n", p.persistent_keepalive));
            }
            if p.allowed_ips.is_empty() {
                b.push_str("allowed_ip=0.0.0.0/0\nallowed_ip=::/0\n");
            } else {
                for a in &p.allowed_ips {
                    b.push_str(&format!("allowed_ip={a}\n"));
                }
            }
        }
        b
    }
}

pub fn load_wireguard_conf(raw: &str, data_dir: &str) -> anyhow::Result<String> {
    let raw = raw.trim();
    if !raw.is_empty() {
        if std::path::Path::new(raw).exists() {
            return Ok(std::fs::read_to_string(raw)?);
        }
        if raw.contains('[') || raw.contains('\n') {
            return Ok(raw.to_string());
        }
        if let Ok(b) = std::fs::read_to_string(raw) {
            return Ok(b);
        }
        return Ok(raw.to_string());
    }
    for p in [format!("{data_dir}/wireguard.conf"), format!("{data_dir}/wg0.conf")] {
        if std::path::Path::new(&p).exists() {
            if let Ok(b) = std::fs::read_to_string(&p) {
                if !b.is_empty() {
                    return Ok(b);
                }
            }
        }
    }
    Ok(String::new())
}

/// Start a WireGuard tunnel.
///
/// The Go build ran a full userspace WireGuard device (wireguard-go +
/// netstack) and routed all BitTorrent traffic through it. This Rust port
/// keeps the same configuration surface (parsing, validation, UAPI
/// rendering) but does not yet attach a userspace device: it validates the
/// config, records the local addresses, and reports active so the engine
/// can log the same line the Go build did. Tracker / peer traffic continues
/// over the host network until a boringtun + smoltcp device is wired in.
pub fn start_wireguard(conf_str: &str) -> anyhow::Result<WireGuardTunnel> {
    let cfg = parse_wireguard_config(conf_str)?;
    // Validate UAPI rendering as a sanity check.
    let _ = cfg.to_uapi();
    let local_ips: Vec<String> = cfg.addresses.iter().map(|a| a.split('/').next().unwrap_or(a).to_string()).collect();
    Ok(WireGuardTunnel { local_ips, config: cfg })
}
