use anyhow::{bail, Context, Result};
use bytes::Bytes;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, Notify};

use super::bencode::{self, BVal};
use super::metainfo::Id20;
use super::wire;

const PROTOCOL_ID: i64 = 0x41727101980;
const DNS_CACHE_TTL: Duration = Duration::from_secs(300);
const UDP_CONN_ID_TTL: Duration = Duration::from_secs(55);
const UDP_RETRIES_NORMAL: [Duration; 2] = [Duration::from_millis(300), Duration::from_millis(800)];
const UDP_RETRIES_STOPPED: [Duration; 1] = [Duration::from_millis(350)];
const HTTP_TRACKER_TIMEOUT: Duration = Duration::from_millis(2500);
const MAX_HTTP_BODY_BYTES: usize = 512 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackerEvent {
    None = 0,
    Started = 2,
    Stopped = 3,
}

impl TrackerEvent {
    fn as_str(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Started => Some("started"),
            Self::Stopped => Some("stopped"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct AnnounceRequest {
    pub info_hash: Id20,
    pub peer_id: [u8; 20],
    pub port: u16,
    pub downloaded: u64,
    pub uploaded: u64,
    pub left: u64,
    pub event: TrackerEvent,
    pub key: u32,
}

#[derive(Clone, Debug, Default)]
pub struct TrackerResponse {
    pub interval: Option<u32>,
    pub seeders: Option<u32>,
    pub peers: Vec<SocketAddr>,
}

struct CachedDns {
    addrs: Vec<SocketAddr>,
    cached_at: Instant,
}

struct CachedConnId {
    conn_id: i64,
    obtained_at: Instant,
}

struct TrackerInner {
    http: reqwest::Client,
    allow_private: bool,
    v4_sock: UdpSocket,
    v6_sock: Option<UdpSocket>,
    pending_udp: Mutex<HashMap<u32, oneshot::Sender<Bytes>>>,
    dns_cache: RwLock<HashMap<(String, u16), CachedDns>>,
    conn_cache: Mutex<HashMap<SocketAddr, CachedConnId>>,
    shutdown: Notify,
}

pub struct TrackerClient {
    inner: Arc<TrackerInner>,
}

impl TrackerClient {
    pub async fn new(http: reqwest::Client, allow_private: bool) -> Result<Arc<Self>> {
        let bind_ip = if allow_private {
            Ipv4Addr::LOCALHOST
        } else {
            Ipv4Addr::UNSPECIFIED
        };
        let v4_sock = UdpSocket::bind(SocketAddr::new(IpAddr::V4(bind_ip), 0))
            .await
            .context("bind tracker udp v4 socket")?;
        let v6_sock = if !allow_private {
            bind_v6_ephemeral()
        } else {
            None
        };

        let inner = Arc::new(TrackerInner {
            http,
            allow_private,
            v4_sock,
            v6_sock,
            pending_udp: Mutex::new(HashMap::new()),
            dns_cache: RwLock::new(HashMap::new()),
            conn_cache: Mutex::new(HashMap::new()),
            shutdown: Notify::new(),
        });

        let weak_v4 = Arc::downgrade(&inner);
        tokio::spawn(async move {
            udp_recv_loop(weak_v4, false).await;
        });
        if inner.v6_sock.is_some() {
            let weak_v6 = Arc::downgrade(&inner);
            tokio::spawn(async move {
                udp_recv_loop(weak_v6, true).await;
            });
        }

        Ok(Arc::new(Self { inner }))
    }

    pub async fn announce(&self, tracker_url: &str, req: &AnnounceRequest) -> Result<TrackerResponse> {
        let trimmed = tracker_url.trim();
        let url = url::Url::parse(trimmed).context("invalid tracker url")?;
        match url.scheme().to_ascii_lowercase().as_str() {
            "udp" => self.announce_udp(&url, req).await,
            "http" | "https" => self.announce_http(&url, req).await,
            other => bail!("unsupported tracker scheme {other}"),
        }
    }

    /// Fans out tracker announces concurrently and streams discovered peers immediately as each replies.
    pub async fn announce_many(
        self: &Arc<Self>,
        trackers: &[String],
        req: AnnounceRequest,
        peer_tx: mpsc::UnboundedSender<SocketAddr>,
    ) -> (Option<u32>, u32) {
        if trackers.is_empty() {
            return (None, 0);
        }
        let mut unique = Vec::with_capacity(trackers.len());
        let mut seen_urls = HashSet::new();
        for t in trackers {
            let s = t.trim();
            if !s.is_empty() && seen_urls.insert(s.to_string()) {
                unique.push(s.to_string());
            }
        }

        unique.sort_by_key(|u| {
            !u.get(..6)
                .is_some_and(|p| p.eq_ignore_ascii_case("udp://"))
        });

        let sem = Arc::new(tokio::sync::Semaphore::new(48));
        let mut handles = Vec::with_capacity(unique.len());
        let seen_peers = Arc::new(Mutex::new(HashSet::new()));

        for url in unique {
            let this = self.clone();
            let req_c = req.clone();
            let tx_c = peer_tx.clone();
            let sem_c = sem.clone();
            let seen_c = seen_peers.clone();
            handles.push(tokio::spawn(async move {
                let Ok(_permit) = sem_c.acquire().await else {
                    return None;
                };
                match this.announce(&url, &req_c).await {
                    Ok(resp) => {
                        let fresh: Vec<SocketAddr> = {
                            let mut guard = seen_c.lock().unwrap();
                            resp.peers
                                .iter()
                                .copied()
                                .filter(|p| guard.insert(*p))
                                .collect()
                        };
                        for p in fresh {
                            let _ = tx_c.send(p);
                        }
                        Some((resp.interval, resp.seeders.unwrap_or(0)))
                    }
                    Err(e) => {
                        tracing::debug!("tracker announce failed for {url}: {e}");
                        None
                    }
                }
            }));
        }

        let mut min_interval: Option<u32> = None;
        let mut max_seeders = 0u32;
        for h in handles {
            if let Ok(Some((iv, seeders))) = h.await {
                max_seeders = max_seeders.max(seeders);
                if let Some(secs) = iv {
                    min_interval = Some(match min_interval {
                        Some(cur) => cur.min(secs),
                        None => secs,
                    });
                }
            }
        }
        (min_interval, max_seeders)
    }

    async fn announce_udp(&self, url: &url::Url, req: &AnnounceRequest) -> Result<TrackerResponse> {
        let host = url.host_str().context("udp tracker missing host")?;
        let port = url.port().unwrap_or(80);
        let addrs: Vec<SocketAddr> = self
            .resolve_udp_addrs(host, port)
            .await?
            .into_iter()
            .filter(|a| a.is_ipv4() || self.inner.v6_sock.is_some())
            .collect();
        if addrs.is_empty() {
            bail!("no reachable udp tracker address");
        }
        let first_v4 = addrs.iter().copied().find(|a| a.is_ipv4());
        let first_v6 = addrs.iter().copied().find(|a| a.is_ipv6());
        let mut last_err = anyhow::anyhow!("no reachable udp tracker address");

        if let (Some(v4), Some(v6)) = (first_v4, first_v6) {
            let v4_failed = Arc::new(Notify::new());
            let v4_failed_c = v4_failed.clone();
            let v4_fut = async {
                let res = self.announce_udp_addr(v4, req).await;
                if res.is_err() {
                    v4_failed_c.notify_one();
                }
                res
            };
            let v6_fut = async {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(150)) => {}
                    _ = v4_failed.notified() => {}
                }
                self.announce_udp_addr(v6, req).await
            };
            tokio::pin!(v4_fut);
            tokio::pin!(v6_fut);
            let res = tokio::select! {
                r = &mut v4_fut => match r {
                    Ok(ok) => Ok(ok),
                    Err(e4) => v6_fut.await.map_err(|_| e4),
                },
                r = &mut v6_fut => match r {
                    Ok(ok) => Ok(ok),
                    Err(e6) => v4_fut.await.map_err(|_| e6),
                },
            };
            match res {
                Ok(ok) => return Ok(ok),
                Err(e) => last_err = e,
            }
        }

        for addr in addrs {
            if Some(addr) == first_v4 && first_v6.is_some() {
                continue;
            }
            if Some(addr) == first_v6 && first_v4.is_some() {
                continue;
            }
            match self.announce_udp_addr(addr, req).await {
                Ok(r) => return Ok(r),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    async fn resolve_udp_addrs(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
        if !self.inner.allow_private && crate::netguard::is_private_hostname(host) {
            bail!("refused private tracker hostname {host}");
        }
        let clean = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = clean.parse::<IpAddr>() {
            if !self.inner.allow_private && crate::netguard::is_private_address_ip(&ip) {
                bail!("refused private tracker ip {ip}");
            }
            return Ok(vec![SocketAddr::new(ip, port)]);
        }

        let key = (host.to_ascii_lowercase(), port);
        {
            let cache = self.inner.dns_cache.read().unwrap();
            if let Some(entry) = cache.get(&key) {
                if entry.cached_at.elapsed() < DNS_CACHE_TTL {
                    return Ok(entry.addrs.clone());
                }
            }
        }

        let mut resolved: Vec<SocketAddr> = tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::lookup_host((host, port)),
        )
        .await
        .context("udp tracker dns timeout")?
        .context("udp tracker dns failed")?
        .filter(|a| {
            self.inner.allow_private || !crate::netguard::is_private_address_ip(&a.ip())
        })
        .collect();
        if resolved.is_empty() {
            bail!("no permitted addresses resolved for {host}");
        }
        // Prefer IPv4 first to avoid dual-stack stalls when IPv6 egress is unreachable.
        resolved.sort_by_key(|a| a.is_ipv6());
        self.inner.dns_cache.write().unwrap().insert(
            key,
            CachedDns {
                addrs: resolved.clone(),
                cached_at: Instant::now(),
            },
        );
        Ok(resolved)
    }

    async fn announce_udp_addr(
        &self,
        addr: SocketAddr,
        req: &AnnounceRequest,
    ) -> Result<TrackerResponse> {
        let retries: &[Duration] = if req.event == TrackerEvent::Stopped {
            &UDP_RETRIES_STOPPED
        } else {
            &UDP_RETRIES_NORMAL
        };

        let mut conn_id = self.get_cached_conn_id(addr);
        if conn_id.is_none() {
            conn_id = Some(self.udp_connect(addr, retries).await?);
        }

        let cid = conn_id.unwrap();
        match self.udp_send_announce(addr, cid, req, retries).await {
            Ok(resp) => Ok(resp),
            Err(e) => {
                // Invalidate potentially expired connection_id once and retry connect+announce if non-stopped.
                self.inner.conn_cache.lock().unwrap().remove(&addr);
                if req.event == TrackerEvent::Stopped {
                    return Err(e);
                }
                let fresh_cid = self.udp_connect(addr, retries).await?;
                self.udp_send_announce(addr, fresh_cid, req, retries).await
            }
        }
    }

    fn get_cached_conn_id(&self, addr: SocketAddr) -> Option<i64> {
        let mut cache = self.inner.conn_cache.lock().unwrap();
        if let Some(entry) = cache.get(&addr) {
            if entry.obtained_at.elapsed() < UDP_CONN_ID_TTL {
                return Some(entry.conn_id);
            }
        }
        cache.remove(&addr);
        None
    }

    async fn udp_connect(&self, addr: SocketAddr, retries: &[Duration]) -> Result<i64> {
        let tx_id: u32 = rand::random();
        let mut pkt = [0u8; 16];
        pkt[0..8].copy_from_slice(&PROTOCOL_ID.to_be_bytes());
        pkt[8..12].copy_from_slice(&0u32.to_be_bytes());
        pkt[12..16].copy_from_slice(&tx_id.to_be_bytes());

        let data = self.inner.udp_rpc(addr, tx_id, &pkt, retries).await?;
        if data.len() < 16 {
            bail!("short udp connect reply {}", data.len());
        }
        let action = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        if action == 3 {
            let msg = String::from_utf8_lossy(&data[8..]);
            bail!("udp tracker connect error: {msg}");
        }
        if action != 0 {
            bail!("unexpected udp connect action {action}");
        }
        let conn_id = i64::from_be_bytes(data[8..16].try_into().unwrap());
        self.inner.conn_cache.lock().unwrap().insert(
            addr,
            CachedConnId {
                conn_id,
                obtained_at: Instant::now(),
            },
        );
        Ok(conn_id)
    }

    async fn udp_send_announce(
        &self,
        addr: SocketAddr,
        conn_id: i64,
        req: &AnnounceRequest,
        retries: &[Duration],
    ) -> Result<TrackerResponse> {
        let tx_id: u32 = rand::random();
        let mut pkt = [0u8; 98];
        pkt[0..8].copy_from_slice(&conn_id.to_be_bytes());
        pkt[8..12].copy_from_slice(&1u32.to_be_bytes());
        pkt[12..16].copy_from_slice(&tx_id.to_be_bytes());
        pkt[16..36].copy_from_slice(&req.info_hash.0);
        pkt[36..56].copy_from_slice(&req.peer_id);
        pkt[56..64].copy_from_slice(&req.downloaded.to_be_bytes());
        pkt[64..72].copy_from_slice(&req.left.to_be_bytes());
        pkt[72..80].copy_from_slice(&req.uploaded.to_be_bytes());
        pkt[80..84].copy_from_slice(&(req.event as u32).to_be_bytes());
        pkt[84..88].copy_from_slice(&0u32.to_be_bytes()); // IPv4/default
        pkt[88..92].copy_from_slice(&req.key.to_be_bytes());
        pkt[92..96].copy_from_slice(&(-1i32).to_be_bytes()); // num_want
        pkt[96..98].copy_from_slice(&req.port.to_be_bytes());

        let data = self.inner.udp_rpc(addr, tx_id, &pkt, retries).await?;
        if data.len() < 8 {
            bail!("short udp announce reply {}", data.len());
        }
        let action = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        if action == 3 {
            let msg = String::from_utf8_lossy(&data[8..]);
            bail!("udp tracker announce error: {msg}");
        }
        if action != 1 || data.len() < 20 {
            bail!("unexpected udp announce action {action} len {}", data.len());
        }
        let interval = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
        let seeders = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);

        let mut peers = Vec::new();
        if addr.is_ipv6() {
            wire::parse_compact_v6(&data[20..], &mut peers);
        } else {
            wire::parse_compact_v4(&data[20..], &mut peers);
        }
        peers.retain(|p| self.inner.is_peer_allowed(p));

        Ok(TrackerResponse {
            interval: Some(interval),
            seeders: Some(seeders),
            peers,
        })
    }

    async fn announce_http(&self, url: &url::Url, req: &AnnounceRequest) -> Result<TrackerResponse> {
        if !crate::netguard::url_allowed(url, self.inner.allow_private) {
            bail!("refused private http tracker {url}");
        }

        let sep = if url.query().is_some() { '&' } else { '?' };
        let ih_enc = pct_encode_bytes(&req.info_hash.0);
        let pid_enc = pct_encode_bytes(&req.peer_id);
        let mut full = format!(
            "{url}{sep}info_hash={ih_enc}&peer_id={pid_enc}&port={}&uploaded={}&downloaded={}&left={}&compact=1&numwant=100",
            req.port, req.uploaded, req.downloaded, req.left
        );
        if let Some(ev) = req.event.as_str() {
            full.push_str("&event=");
            full.push_str(ev);
        }

        let resp = self
            .inner
            .http
            .get(&full)
            .timeout(HTTP_TRACKER_TIMEOUT)
            .send()
            .await
            .context("http tracker request failed")?;
        if !resp.status().is_success() {
            bail!("http tracker status {}", resp.status());
        }
        let body = resp.bytes().await.context("read http tracker body")?;
        if body.len() > MAX_HTTP_BODY_BYTES {
            bail!("http tracker body too large");
        }
        self.parse_http_bencode(&body)
    }

    fn parse_http_bencode(&self, body: &[u8]) -> Result<TrackerResponse> {
        let val = bencode::decode(body).context("decode http tracker bencode")?;
        if let Some(fail) = val.dict_str(b"failure reason") {
            bail!("tracker failure: {fail}");
        }
        let interval = val
            .dict_i64(b"interval")
            .and_then(|v| u32::try_from(v).ok());
        let seeders = val
            .dict_i64(b"complete")
            .and_then(|v| u32::try_from(v).ok());

        let mut peers = Vec::new();
        match val.dict_get(b"peers") {
            Some(BVal::Bytes(compact)) => {
                wire::parse_compact_v4(compact, &mut peers);
            }
            Some(BVal::List(list)) => {
                for item in list {
                    let Some(ip_s) = item.dict_str(b"ip") else {
                        continue;
                    };
                    let Some(port) = item.dict_i64(b"port").and_then(|v| u16::try_from(v).ok()) else {
                        continue;
                    };
                    if let Ok(ip) = ip_s.parse::<IpAddr>() {
                        peers.push(SocketAddr::new(ip, port));
                    }
                }
            }
            _ => {}
        }
        if let Some(compact6) = val.dict_bytes(b"peers6") {
            wire::parse_compact_v6(compact6, &mut peers);
        }
        peers.retain(|p| self.inner.is_peer_allowed(p));

        Ok(TrackerResponse {
            interval,
            seeders,
            peers,
        })
    }
}

impl Drop for TrackerClient {
    fn drop(&mut self) {
        self.inner.shutdown.notify_waiters();
    }
}

impl TrackerInner {
    fn is_peer_allowed(&self, addr: &SocketAddr) -> bool {
        addr.port() != 0
            && (self.allow_private || !crate::netguard::is_private_address_ip(&addr.ip()))
    }

    async fn udp_rpc(
        &self,
        addr: SocketAddr,
        tx_id: u32,
        pkt: &[u8],
        retries: &[Duration],
    ) -> Result<Bytes> {
        let (tx, mut rx) = oneshot::channel();
        {
            self.pending_udp.lock().unwrap().insert(tx_id, tx);
        }

        for wait in retries {
            let send_res = match addr {
                SocketAddr::V4(_) => self.v4_sock.send_to(pkt, addr).await,
                SocketAddr::V6(_) => match &self.v6_sock {
                    Some(v6) => v6.send_to(pkt, addr).await,
                    None => {
                        self.pending_udp.lock().unwrap().remove(&tx_id);
                        bail!("ipv6 tracker socket unavailable");
                    }
                },
            };
            if let Err(e) = send_res {
                self.pending_udp.lock().unwrap().remove(&tx_id);
                return Err(e.into());
            }

            tokio::select! {
                res = &mut rx => {
                    match res {
                        Ok(bytes) => return Ok(bytes),
                        Err(_) => bail!("udp tracker tx dropped"),
                    }
                }
                _ = tokio::time::sleep(*wait) => {}
            }
        }

        self.pending_udp.lock().unwrap().remove(&tx_id);
        bail!("udp tracker timeout for {addr}")
    }
}

async fn udp_recv_loop(weak: Weak<TrackerInner>, ipv6: bool) {
    let mut buf = vec![0u8; 4096];
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let sock = if ipv6 {
            let Some(v6) = inner.v6_sock.as_ref() else {
                return;
            };
            v6
        } else {
            &inner.v4_sock
        };

        tokio::select! {
            _ = inner.shutdown.notified() => return,
            res = sock.recv_from(&mut buf) => {
                if let Ok((n, _from)) = res {
                    if n >= 8 {
                        let tx_id = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
                        if let Some(ch) = inner.pending_udp.lock().unwrap().remove(&tx_id) {
                            let _ = ch.send(Bytes::copy_from_slice(&buf[..n]));
                        }
                    }
                }
            }
        }
    }
}

fn bind_v6_ephemeral() -> Option<UdpSocket> {
    let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)).ok()?;
    let _ = sock.set_only_v6(true);
    let _ = sock.set_nonblocking(true);
    let addr: SocketAddr = ([0u16; 8], 0u16).into();
    sock.bind(&addr.into()).ok()?;
    let std_sock: std::net::UdpSocket = sock.into();
    UdpSocket::from_std(std_sock).ok()
}

fn pct_encode_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for &b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            let hi = b >> 4;
            let lo = b & 0x0f;
            out.push(hex_char(hi));
            out.push(hex_char(lo));
        }
    }
    out
}

fn hex_char(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'A' + (nibble - 10)) as char,
        _ => '0',
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};

    #[tokio::test]
    async fn test_udp_and_http_tracker_announce() {
        // 1. Mock BEP 15 UDP tracker on loopback
        let udp_srv = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let udp_addr = udp_srv.local_addr().unwrap();
        let advertised_peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 54321);
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            while let Ok((n, from)) = udp_srv.recv_from(&mut buf).await {
                if n < 16 {
                    continue;
                }
                let action = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
                let tx_id = &buf[12..16];
                if action == 0 {
                    let mut reply = [0u8; 16];
                    reply[0..4].copy_from_slice(&0u32.to_be_bytes());
                    reply[4..8].copy_from_slice(tx_id);
                    reply[8..16].copy_from_slice(&0x1122334455667788i64.to_be_bytes());
                    let _ = udp_srv.send_to(&reply, from).await;
                } else if action == 1 && n >= 98 {
                    let mut reply = vec![0u8; 26];
                    reply[0..4].copy_from_slice(&1u32.to_be_bytes());
                    reply[4..8].copy_from_slice(tx_id);
                    reply[8..12].copy_from_slice(&120u32.to_be_bytes());
                    reply[12..16].copy_from_slice(&5u32.to_be_bytes());
                    reply[16..20].copy_from_slice(&42u32.to_be_bytes());
                    reply[20..24].copy_from_slice(&Ipv4Addr::new(127, 0, 0, 1).octets());
                    reply[24..26].copy_from_slice(&advertised_peer.port().to_be_bytes());
                    let _ = udp_srv.send_to(&reply, from).await;
                }
            }
        });

        // 2. Mock HTTP bencoded tracker on loopback
        let mut bw = bencode::BWriter::with_capacity(64);
        bw.start_dict();
        bw.push_str("complete");
        bw.push_int(17);
        bw.push_str("incomplete");
        bw.push_int(3);
        bw.push_str("interval");
        bw.push_int(90);
        bw.push_str("peers");
        let mut compact = Vec::new();
        compact.extend_from_slice(&Ipv4Addr::new(127, 0, 0, 1).octets());
        compact.extend_from_slice(&55555u16.to_be_bytes());
        bw.push_bytes(&compact);
        bw.end();
        let body_bytes = bw.finish();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/announce",
            get(move || {
                let b = body_bytes.clone();
                async move { b }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = reqwest::Client::new();
        let tracker = TrackerClient::new(client, true).await.unwrap();
        let req = AnnounceRequest {
            info_hash: Id20::from_hex("abcdef0123456789abcdef0123456789abcdef01").unwrap(),
            peer_id: *b"-BL0001-123456789012",
            port: 6881,
            downloaded: 0,
            uploaded: 0,
            left: 1024,
            event: TrackerEvent::Started,
            key: 99,
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        let urls = vec![
            format!("udp://{udp_addr}/announce"),
            format!("http://{http_addr}/announce?passkey=abc"),
        ];
        let (interval, seeders) = tracker.announce_many(&urls, req.clone(), tx).await;
        assert_eq!(interval, Some(90));
        assert_eq!(seeders, 42);

        let mut peers = Vec::new();
        while let Ok(p) = rx.try_recv() {
            peers.push(p);
        }
        assert!(peers.contains(&advertised_peer));
        assert!(peers.iter().any(|p| p.port() == 55555));

        // Second UDP announce should reuse cached connection_id seamlessly.
        let r2 = tracker
            .announce(&format!("udp://{udp_addr}/announce"), &req)
            .await
            .unwrap();
        assert_eq!(r2.peers, vec![advertised_peer]);
    }

    #[tokio::test]
    async fn test_announce_many_udp_not_blocked_by_http_or_dead_v6() {
        let udp_srv = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let udp_addr = udp_srv.local_addr().unwrap();
        let expected_peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9)), 40001);
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            while let Ok((n, from)) = udp_srv.recv_from(&mut buf).await {
                if n < 16 {
                    continue;
                }
                let action = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
                let tx_id = &buf[12..16];
                if action == 0 {
                    let mut reply = [0u8; 16];
                    reply[4..8].copy_from_slice(tx_id);
                    reply[8..16].copy_from_slice(&0x5566778811223344i64.to_be_bytes());
                    let _ = udp_srv.send_to(&reply, from).await;
                } else if action == 1 && n >= 98 {
                    let mut reply = vec![0u8; 26];
                    reply[0..4].copy_from_slice(&1u32.to_be_bytes());
                    reply[4..8].copy_from_slice(tx_id);
                    reply[8..12].copy_from_slice(&60u32.to_be_bytes());
                    reply[12..16].copy_from_slice(&1u32.to_be_bytes());
                    reply[16..20].copy_from_slice(&3u32.to_be_bytes());
                    reply[20..24].copy_from_slice(&Ipv4Addr::new(203, 0, 113, 9).octets());
                    reply[24..26].copy_from_slice(&40001u16.to_be_bytes());
                    let _ = udp_srv.send_to(&reply, from).await;
                }
            }
        });

        let client = reqwest::Client::new();
        let tracker = TrackerClient::new(client, true).await.unwrap();
        let req = AnnounceRequest {
            info_hash: Id20::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap(),
            peer_id: *b"-BL0001-998877665544",
            port: 6881,
            downloaded: 0,
            uploaded: 0,
            left: 4096,
            event: TrackerEvent::Started,
            key: 7,
        };

        // Populate DNS cache with an unreachable IPv6 entry ahead of live IPv4 and verify fast completion.
        let blackhole_v6: SocketAddr = ([0u16, 0, 0, 0, 0, 0, 0, 1], 1).into();
        tracker.inner.dns_cache.write().unwrap().insert(
            ("dualstack.example".to_string(), udp_addr.port()),
            CachedDns {
                addrs: vec![blackhole_v6, udp_addr],
                cached_at: Instant::now(),
            },
        );
        let resp = tokio::time::timeout(
            Duration::from_millis(600),
            tracker.announce(&format!("udp://dualstack.example:{}/announce", udp_addr.port()), &req),
        )
        .await
        .expect("udp announce stalled behind dead IPv6")
        .unwrap();
        assert_eq!(resp.peers, vec![expected_peer]);

        // Place 14 slow/stalling HTTP trackers before a UDP tracker and ensure the UDP peer arrives immediately.
        let mut urls = Vec::new();
        for i in 0..14 {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let a = l.local_addr().unwrap();
            tokio::spawn(async move {
                while let Ok((_s, _)) = l.accept().await {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            });
            urls.push(format!("http://{a}/announce?n={i}"));
        }
        urls.push(format!("udp://{udp_addr}/announce"));

        let (tx, mut rx) = mpsc::unbounded_channel();
        let tracker_c = tracker.clone();
        let req_c = req.clone();
        let handle = tokio::spawn(async move { tracker_c.announce_many(&urls, req_c, tx).await });
        let first_peer = tokio::time::timeout(Duration::from_millis(600), rx.recv())
            .await
            .expect("udp tracker delayed behind slow http trackers")
            .expect("missing peer");
        assert_eq!(first_peer, expected_peer);
        handle.abort();
    }
}
