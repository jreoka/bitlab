use anyhow::{bail, Context, Result};
use bytes::Bytes;
use rand::RngCore;
use sha1::{Digest, Sha1};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, watch, Notify};

use super::bencode::{self, BVal, BWriter};
use super::metainfo::Id20;
use super::wire;

const K_BUCKET_SIZE: usize = 16;
const ALPHA: usize = 8;
const MAX_LOOKUP_ROUNDS: usize = 12;
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_millis(750);
const DEFAULT_LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);
const TOKEN_ROTATE_INTERVAL: Duration = Duration::from_secs(300);
const PEER_TTL: Duration = Duration::from_secs(1800);
const MAX_PEERS_PER_IH: usize = 64;
const MAX_STORED_IHS: usize = 2048;
const MAX_PERSISTED_NODES: usize = 256;

const BOOTSTRAP_HOSTS: &[(&str, u16)] = &[
    ("router.bittorrent.com", 6881),
    ("dht.transmissionbt.com", 6881),
    ("router.utorrent.com", 6881),
    ("dht.libtorrent.org", 25401),
];

#[derive(Clone, Debug)]
pub struct DhtConfig {
    pub state_path: Option<PathBuf>,
    pub bind_ip: Ipv4Addr,
    pub port: u16,
    pub enable_ipv6: bool,
    pub use_default_bootstrap: bool,
    pub bootstrap_addrs: Vec<SocketAddr>,
    pub allow_private: bool,
    pub rpc_timeout: Duration,
    pub lookup_timeout: Duration,
}

impl Default for DhtConfig {
    fn default() -> Self {
        Self {
            state_path: None,
            bind_ip: Ipv4Addr::UNSPECIFIED,
            port: 0,
            enable_ipv6: true,
            use_default_bootstrap: true,
            bootstrap_addrs: Vec::new(),
            allow_private: false,
            rpc_timeout: DEFAULT_RPC_TIMEOUT,
            lookup_timeout: DEFAULT_LOOKUP_TIMEOUT,
        }
    }
}

#[derive(Clone, Debug)]
struct BucketNode {
    id: Id20,
    addr: SocketAddr,
    last_seen: Instant,
    rtt_ms: u32,
    failures: u8,
}

struct RoutingTable {
    self_id: Id20,
    buckets: Vec<Vec<BucketNode>>,
    bucket_touched: Vec<Instant>,
}

impl RoutingTable {
    fn new(self_id: Id20) -> Self {
        let now = Instant::now();
        Self {
            self_id,
            buckets: vec![Vec::new(); 160],
            bucket_touched: vec![now; 160],
        }
    }

    fn insert_or_update(&mut self, id: Id20, addr: SocketAddr, rtt: Option<Duration>) {
        if id == self.self_id || addr.port() == 0 {
            return;
        }
        let Some(bidx) = self.self_id.bucket_index(&id) else {
            return;
        };
        let now = Instant::now();
        self.bucket_touched[bidx] = now;
        let bucket = &mut self.buckets[bidx];
        let rtt_sample = rtt.map(|d| d.as_millis().min(60_000) as u32);

        if let Some(pos) = bucket.iter().position(|n| n.id == id || n.addr == addr) {
            let entry = &mut bucket[pos];
            entry.id = id;
            entry.addr = addr;
            entry.last_seen = now;
            entry.failures = 0;
            if let Some(ms) = rtt_sample {
                entry.rtt_ms = if entry.rtt_ms == 0 {
                    ms
                } else {
                    (entry.rtt_ms * 3 + ms) / 4
                };
            }
            let item = bucket.remove(pos);
            bucket.push(item);
            return;
        }

        let node = BucketNode {
            id,
            addr,
            last_seen: now,
            rtt_ms: rtt_sample.unwrap_or(200),
            failures: 0,
        };
        if bucket.len() < K_BUCKET_SIZE {
            bucket.push(node);
            return;
        }

        // Evict stale/high-failure nodes first, preserving low-RTT reliable nodes.
        if let Some((evict_idx, _)) = bucket
            .iter()
            .enumerate()
            .filter(|(_, n)| n.failures > 0 || now.duration_since(n.last_seen) > Duration::from_secs(900))
            .max_by_key(|(_, n)| (n.failures as u64, now.duration_since(n.last_seen).as_secs()))
        {
            bucket.remove(evict_idx);
            bucket.push(node);
        }
    }

    fn mark_failure(&mut self, addr: SocketAddr) {
        for bucket in &mut self.buckets {
            if let Some(pos) = bucket.iter().position(|n| n.addr == addr) {
                bucket[pos].failures = bucket[pos].failures.saturating_add(1);
                if bucket[pos].failures >= 3 {
                    bucket.remove(pos);
                }
                return;
            }
        }
    }

    fn len(&self) -> usize {
        self.buckets.iter().map(|b| b.len()).sum()
    }

    fn closest(&self, target: &Id20, limit: usize) -> Vec<(Id20, SocketAddr)> {
        let mut all: Vec<([u8; 20], u32, Id20, SocketAddr)> = Vec::new();
        for bucket in &self.buckets {
            for n in bucket {
                all.push((n.id.distance(target), n.rtt_ms, n.id, n.addr));
            }
        }
        all.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        all.into_iter()
            .take(limit)
            .map(|(_, _, id, addr)| (id, addr))
            .collect()
    }

    fn closest_v4(&self, target: &Id20, limit: usize) -> Vec<(Id20, SocketAddr)> {
        let mut all: Vec<([u8; 20], u32, Id20, SocketAddr)> = Vec::new();
        for bucket in &self.buckets {
            for n in bucket {
                if n.addr.is_ipv4() {
                    all.push((n.id.distance(target), n.rtt_ms, n.id, n.addr));
                }
            }
        }
        all.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        all.into_iter()
            .take(limit)
            .map(|(_, _, id, addr)| (id, addr))
            .collect()
    }

    fn closest_v6(&self, target: &Id20, limit: usize) -> Vec<(Id20, SocketAddr)> {
        let mut all: Vec<([u8; 20], u32, Id20, SocketAddr)> = Vec::new();
        for bucket in &self.buckets {
            for n in bucket {
                if n.addr.is_ipv6() {
                    all.push((n.id.distance(target), n.rtt_ms, n.id, n.addr));
                }
            }
        }
        all.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        all.into_iter()
            .take(limit)
            .map(|(_, _, id, addr)| (id, addr))
            .collect()
    }

    fn export_good_addrs(&self, limit: usize) -> Vec<String> {
        let mut nodes: Vec<&BucketNode> = self.buckets.iter().flatten().collect();
        nodes.sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then_with(|| a.rtt_ms.cmp(&b.rtt_ms)));
        nodes
            .into_iter()
            .filter(|n| n.failures == 0)
            .take(limit)
            .map(|n| n.addr.to_string())
            .collect()
    }

    fn stale_refresh_target(&self) -> Option<Id20> {
        let now = Instant::now();
        let (idx, _) = self
            .bucket_touched
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.buckets[*i].is_empty() && *i < 32)
            .max_by_key(|(_, t)| now.duration_since(**t))?;
        if now.duration_since(self.bucket_touched[idx]) < Duration::from_secs(600) {
            return None;
        }
        Some(random_id_in_bucket(self.self_id, idx))
    }
}

#[derive(Debug)]
struct DhtResponse {
    responder_id: Id20,
    token: Option<Bytes>,
    values: Vec<SocketAddr>,
    nodes: Vec<(Id20, SocketAddr)>,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct DhtStateFile {
    node_id: String,
    nodes: Vec<String>,
}

struct TokenSecrets {
    curr: [u8; 20],
    prev: [u8; 20],
    rotated_at: Instant,
}

type PendingMap = HashMap<[u8; 6], (Instant, oneshot::Sender<Result<DhtResponse>>)>;

struct DhtInner {
    cfg: DhtConfig,
    self_id: Id20,
    v4_sock: UdpSocket,
    v6_sock: Option<UdpSocket>,
    table: RwLock<RoutingTable>,
    pending: Mutex<PendingMap>,
    tx_seq: AtomicU32,
    tx_salt: [u8; 2],
    secrets: Mutex<TokenSecrets>,
    peer_store: Mutex<HashMap<Id20, Vec<(SocketAddr, Instant)>>>,
    bootstrap_cache: RwLock<Vec<SocketAddr>>,
    shutdown: Notify,
}

pub struct DhtNode {
    inner: Arc<DhtInner>,
}

impl DhtNode {
    pub async fn start(cfg: DhtConfig) -> Result<Arc<Self>> {
        let (persisted_id, seed_addrs) = load_state(cfg.state_path.as_deref());
        let self_id = persisted_id.unwrap_or_else(random_id20);

        let v4_sock = UdpSocket::bind(SocketAddr::new(IpAddr::V4(cfg.bind_ip), cfg.port))
            .await
            .context("bind dht udp v4 socket")?;
        let local_port = v4_sock.local_addr()?.port();

        let v6_sock = if cfg.enable_ipv6 && cfg.bind_ip.is_unspecified() {
            bind_v6_only(local_port)
        } else {
            None
        };

        let mut salt = [0u8; 2];
        rand::rng().fill_bytes(&mut salt);
        let mut curr = [0u8; 20];
        let mut prev = [0u8; 20];
        rand::rng().fill_bytes(&mut curr);
        rand::rng().fill_bytes(&mut prev);

        let mut initial_cache = cfg.bootstrap_addrs.clone();
        for addr in seed_addrs {
            if !initial_cache.contains(&addr) {
                initial_cache.push(addr);
            }
        }

        let inner = Arc::new(DhtInner {
            cfg,
            self_id,
            v4_sock,
            v6_sock,
            table: RwLock::new(RoutingTable::new(self_id)),
            pending: Mutex::new(HashMap::new()),
            tx_seq: AtomicU32::new(1),
            tx_salt: salt,
            secrets: Mutex::new(TokenSecrets {
                curr,
                prev,
                rotated_at: Instant::now(),
            }),
            peer_store: Mutex::new(HashMap::new()),
            bootstrap_cache: RwLock::new(initial_cache),
            shutdown: Notify::new(),
        });

        let weak_v4 = Arc::downgrade(&inner);
        tokio::spawn(async move {
            recv_loop(weak_v4, false).await;
        });

        if inner.v6_sock.is_some() {
            let weak_v6 = Arc::downgrade(&inner);
            tokio::spawn(async move {
                recv_loop(weak_v6, true).await;
            });
        }

        let weak_maint = Arc::downgrade(&inner);
        tokio::spawn(async move {
            maintenance_loop(weak_maint).await;
        });

        Ok(Arc::new(Self { inner }))
    }

    #[cfg(test)]
    pub fn node_id(&self) -> Id20 {
        self.inner.self_id
    }

    #[cfg(test)]
    pub fn local_addr_v4(&self) -> SocketAddr {
        self.inner
            .v4_sock
            .local_addr()
            .unwrap_or_else(|_| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
    }

    #[cfg(test)]
    pub fn routing_table_len(&self) -> usize {
        self.inner.table.read().unwrap().len()
    }

    pub fn save_state(&self) {
        self.inner.save_state();
    }

    #[cfg(test)]
    pub async fn ping(&self, addr: SocketAddr) -> Result<Id20> {
        let resp = self.inner.rpc_ping(addr).await?;
        Ok(resp.responder_id)
    }

    /// Runs a high-concurrency unthrottled Kademlia lookup (`ALPHA = 8`) strictly on user click.
    /// Streams discovered peers into `peer_tx` immediately as each node responds,
    /// then sends `announce_peer` to the closest nodes when `announce_port` is `Some`.
    pub async fn lookup_peers(
        &self,
        info_hash: Id20,
        announce_port: Option<u16>,
        peer_tx: mpsc::UnboundedSender<SocketAddr>,
        mut cancel: watch::Receiver<bool>,
    ) {
        if *cancel.borrow() {
            return;
        }

        let inner = self.inner.clone();
        let initial_candidates = inner.collect_initial_candidates(&info_hash).await;
        if initial_candidates.is_empty() {
            return;
        }

        #[derive(Eq, PartialEq)]
        struct Candidate {
            dist: [u8; 20],
            addr: SocketAddr,
        }
        impl Ord for Candidate {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                // BinaryHeap is max-heap; reverse distance so closest node pops first.
                other.dist.cmp(&self.dist).then_with(|| self.addr.cmp(&other.addr))
            }
        }
        impl PartialOrd for Candidate {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }

        let mut frontier: BinaryHeap<Candidate> = BinaryHeap::new();
        let mut queued_addrs: HashSet<SocketAddr> = HashSet::new();
        let mut queried_addrs: HashSet<SocketAddr> = HashSet::new();
        let mut seen_peers: HashSet<SocketAddr> = HashSet::new();
        let mut responded_nodes: Vec<([u8; 20], SocketAddr, Bytes)> = Vec::new();

        for (maybe_id, addr) in initial_candidates {
            if queued_addrs.insert(addr) {
                let dist = maybe_id
                    .map(|id| id.distance(&info_hash))
                    .unwrap_or([0xff; 20]);
                frontier.push(Candidate { dist, addr });
            }
        }

        let deadline = tokio::time::Instant::now() + inner.cfg.lookup_timeout;
        let (res_tx, mut res_rx) =
            mpsc::unbounded_channel::<(SocketAddr, Result<DhtResponse>)>();
        let mut in_flight = 0usize;
        let mut rounds = 0usize;

        while (!frontier.is_empty() || in_flight > 0) && rounds < MAX_LOOKUP_ROUNDS {
            if *cancel.borrow() || tokio::time::Instant::now() >= deadline {
                break;
            }

            let mut launched_any = false;
            while in_flight < ALPHA {
                let Some(cand) = frontier.pop() else {
                    break;
                };
                if !queried_addrs.insert(cand.addr) {
                    continue;
                }
                launched_any = true;
                in_flight += 1;
                let inner_c = inner.clone();
                let res_tx_c = res_tx.clone();
                tokio::spawn(async move {
                    let res = inner_c.rpc_get_peers(cand.addr, info_hash).await;
                    let _ = res_tx_c.send((cand.addr, res));
                });
            }
            if launched_any {
                rounds += 1;
            }
            if in_flight == 0 {
                break;
            }

            let wait_until = deadline;
            tokio::select! {
                _ = cancel.changed() => {
                    if *cancel.borrow() {
                        break;
                    }
                }
                _ = tokio::time::sleep_until(wait_until) => {
                    break;
                }
                msg = res_rx.recv() => {
                    let Some((from_addr, outcome)) = msg else {
                        break;
                    };
                    in_flight = in_flight.saturating_sub(1);
                    let Ok(resp) = outcome else {
                        inner.table.write().unwrap().mark_failure(from_addr);
                        continue;
                    };
                    for peer in resp.values {
                        if inner.is_addr_allowed(&peer) && seen_peers.insert(peer) {
                            let _ = peer_tx.send(peer);
                        }
                    }
                    for (nid, naddr) in resp.nodes {
                        if inner.is_addr_allowed(&naddr)
                            && !queried_addrs.contains(&naddr)
                            && queued_addrs.insert(naddr)
                        {
                            frontier.push(Candidate {
                                dist: nid.distance(&info_hash),
                                addr: naddr,
                            });
                        }
                    }
                    if let Some(tok) = resp.token {
                        responded_nodes.push((resp.responder_id.distance(&info_hash), from_addr, tok));
                    }
                }
            }
        }

        if let Some(port) = announce_port {
            if port > 0 && !responded_nodes.is_empty() && !*cancel.borrow() {
                responded_nodes.sort_unstable_by_key(|a| a.0);
                responded_nodes.truncate(K_BUCKET_SIZE / 2);
                let mut futs = Vec::with_capacity(responded_nodes.len());
                for (_, addr, token) in responded_nodes {
                    let inner_c = inner.clone();
                    futs.push(tokio::spawn(async move {
                        let _ = inner_c.rpc_announce_peer(addr, info_hash, port, &token).await;
                    }));
                }
                let _ = tokio::time::timeout(Duration::from_millis(450), async {
                    for f in futs {
                        let _ = f.await;
                    }
                })
                .await;
            }
        }
    }
}

impl Drop for DhtNode {
    fn drop(&mut self) {
        self.inner.save_state();
        self.inner.shutdown.notify_waiters();
    }
}

impl DhtInner {
    fn is_addr_allowed(&self, addr: &SocketAddr) -> bool {
        if addr.port() == 0 {
            return false;
        }
        self.cfg.allow_private || !crate::netguard::is_private_address_ip(&addr.ip())
    }

    fn next_tx(&self) -> [u8; 6] {
        let seq = self.tx_seq.fetch_add(1, Ordering::Relaxed).to_be_bytes();
        [
            self.tx_salt[0],
            self.tx_salt[1],
            seq[0],
            seq[1],
            seq[2],
            seq[3],
        ]
    }

    async fn send_packet(&self, addr: SocketAddr, buf: &[u8]) -> Result<()> {
        match addr {
            SocketAddr::V4(_) => {
                self.v4_sock.send_to(buf, addr).await?;
            }
            SocketAddr::V6(_) => {
                if let Some(v6) = &self.v6_sock {
                    v6.send_to(buf, addr).await?;
                } else {
                    bail!("ipv6 dht socket unavailable");
                }
            }
        }
        Ok(())
    }

    async fn send_query(&self, addr: SocketAddr, packet_with_tx: ([u8; 6], Vec<u8>)) -> Result<DhtResponse> {
        let (tx, bytes) = packet_with_tx;
        let (resp_tx, resp_rx) = oneshot::channel();
        {
            let mut p = self.pending.lock().unwrap();
            p.insert(tx, (Instant::now(), resp_tx));
        }
        if let Err(e) = self.send_packet(addr, &bytes).await {
            self.pending.lock().unwrap().remove(&tx);
            return Err(e);
        }
        match tokio::time::timeout(self.cfg.rpc_timeout, resp_rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => bail!("dht rpc canceled"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&tx);
                bail!("dht rpc timeout")
            }
        }
    }

    #[cfg(test)]
    async fn rpc_ping(&self, addr: SocketAddr) -> Result<DhtResponse> {
        let tx = self.next_tx();
        let mut w = BWriter::with_capacity(80);
        w.start_dict();
        w.push_str("a");
        w.start_dict();
        w.push_str("id");
        w.push_bytes(&self.self_id.0);
        w.end();
        w.push_str("q");
        w.push_str("ping");
        w.push_str("t");
        w.push_bytes(&tx);
        w.push_str("v");
        w.push_str("BL01");
        w.push_str("y");
        w.push_str("q");
        w.end();
        self.send_query(addr, (tx, w.finish())).await
    }

    async fn rpc_find_node(&self, addr: SocketAddr, target: Id20) -> Result<DhtResponse> {
        let tx = self.next_tx();
        let mut w = BWriter::with_capacity(110);
        w.start_dict();
        w.push_str("a");
        w.start_dict();
        w.push_str("id");
        w.push_bytes(&self.self_id.0);
        w.push_str("target");
        w.push_bytes(&target.0);
        w.push_str("want");
        w.start_list();
        w.push_str("n4");
        if self.v6_sock.is_some() {
            w.push_str("n6");
        }
        w.end();
        w.end();
        w.push_str("q");
        w.push_str("find_node");
        w.push_str("t");
        w.push_bytes(&tx);
        w.push_str("v");
        w.push_str("BL01");
        w.push_str("y");
        w.push_str("q");
        w.end();
        self.send_query(addr, (tx, w.finish())).await
    }

    async fn rpc_get_peers(&self, addr: SocketAddr, info_hash: Id20) -> Result<DhtResponse> {
        let tx = self.next_tx();
        let mut w = BWriter::with_capacity(120);
        w.start_dict();
        w.push_str("a");
        w.start_dict();
        w.push_str("id");
        w.push_bytes(&self.self_id.0);
        w.push_str("info_hash");
        w.push_bytes(&info_hash.0);
        w.push_str("want");
        w.start_list();
        w.push_str("n4");
        if self.v6_sock.is_some() {
            w.push_str("n6");
        }
        w.end();
        w.end();
        w.push_str("q");
        w.push_str("get_peers");
        w.push_str("t");
        w.push_bytes(&tx);
        w.push_str("v");
        w.push_str("BL01");
        w.push_str("y");
        w.push_str("q");
        w.end();
        self.send_query(addr, (tx, w.finish())).await
    }

    async fn rpc_announce_peer(
        &self,
        addr: SocketAddr,
        info_hash: Id20,
        port: u16,
        token: &[u8],
    ) -> Result<DhtResponse> {
        let tx = self.next_tx();
        let mut w = BWriter::with_capacity(150);
        w.start_dict();
        w.push_str("a");
        w.start_dict();
        w.push_str("id");
        w.push_bytes(&self.self_id.0);
        w.push_str("implied_port");
        w.push_int(0);
        w.push_str("info_hash");
        w.push_bytes(&info_hash.0);
        w.push_str("port");
        w.push_int(port as i64);
        w.push_str("token");
        w.push_bytes(token);
        w.end();
        w.push_str("q");
        w.push_str("announce_peer");
        w.push_str("t");
        w.push_bytes(&tx);
        w.push_str("v");
        w.push_str("BL01");
        w.push_str("y");
        w.push_str("q");
        w.end();
        self.send_query(addr, (tx, w.finish())).await
    }

    async fn resolve_bootstrap_addrs(&self) -> Vec<SocketAddr> {
        let existing = self.bootstrap_cache.read().unwrap().clone();
        if !self.cfg.use_default_bootstrap {
            return existing;
        }
        let mut out = existing;
        for &(host, port) in BOOTSTRAP_HOSTS {
            if let Ok(Ok(addrs)) =
                tokio::time::timeout(Duration::from_secs(2), tokio::net::lookup_host((host, port)))
                    .await
            {
                for addr in addrs {
                    if self.is_addr_allowed(&addr)
                        && (addr.is_ipv4() || self.v6_sock.is_some())
                        && !out.contains(&addr)
                    {
                        out.push(addr);
                    }
                }
            }
        }
        *self.bootstrap_cache.write().unwrap() = out.clone();
        out
    }

    async fn collect_initial_candidates(&self, target: &Id20) -> Vec<(Option<Id20>, SocketAddr)> {
        let closest = self.table.read().unwrap().closest(target, K_BUCKET_SIZE * 2);
        let mut out: Vec<(Option<Id20>, SocketAddr)> = closest
            .into_iter()
            .map(|(id, addr)| (Some(id), addr))
            .collect();
        if out.len() < 4 {
            let boot = self.resolve_bootstrap_addrs().await;
            for addr in boot {
                if !out.iter().any(|(_, a)| *a == addr) {
                    out.push((None, addr));
                }
            }
        }
        out
    }

    async fn bootstrap_round(self: &Arc<Self>) {
        let seeds = {
            let cached = self.bootstrap_cache.read().unwrap().clone();
            if cached.is_empty() {
                self.resolve_bootstrap_addrs().await
            } else {
                cached
            }
        };
        if seeds.is_empty() {
            return;
        }

        let mut queried = HashSet::new();
        let mut to_query: Vec<SocketAddr> = seeds;
        for _ in 0..3 {
            let batch: Vec<SocketAddr> = to_query
                .drain(..)
                .filter(|a| self.is_addr_allowed(a) && queried.insert(*a))
                .take(ALPHA)
                .collect();
            if batch.is_empty() {
                break;
            }
            let (tx, mut rx) = mpsc::unbounded_channel();
            for addr in &batch {
                let inner_c = self.clone();
                let tx_c = tx.clone();
                let addr_c = *addr;
                tokio::spawn(async move {
                    let res = inner_c.rpc_find_node(addr_c, inner_c.self_id).await;
                    let _ = tx_c.send((addr_c, res));
                });
            }
            drop(tx);
            while let Some((_, res)) = rx.recv().await {
                if let Ok(resp) = res {
                    for (_, naddr) in resp.nodes {
                        if !queried.contains(&naddr) {
                            to_query.push(naddr);
                        }
                    }
                }
            }
        }
    }

    fn handle_packet(&self, from: SocketAddr, data: &[u8]) {
        if !self.is_addr_allowed(&from) {
            return;
        }
        let Ok(val) = bencode::decode(data) else {
            return;
        };
        let Some(y) = val.dict_bytes(b"y") else {
            return;
        };
        match y {
            b"r" => self.handle_response(from, &val),
            b"e" => self.handle_error(&val),
            b"q" => self.handle_query(from, &val),
            _ => {}
        }
    }

    fn handle_error(&self, val: &BVal<'_>) {
        let Some(t) = val.dict_bytes(b"t") else {
            return;
        };
        if t.len() != 6 {
            return;
        }
        let tx: [u8; 6] = t.try_into().unwrap();
        if let Some((_, ch)) = self.pending.lock().unwrap().remove(&tx) {
            let _ = ch.send(Err(anyhow::anyhow!("remote dht error")));
        }
    }

    fn handle_response(&self, from: SocketAddr, val: &BVal<'_>) {
        let Some(t) = val.dict_bytes(b"t") else {
            return;
        };
        if t.len() != 6 {
            return;
        }
        let tx: [u8; 6] = t.try_into().unwrap();
        let entry = self.pending.lock().unwrap().remove(&tx);
        let Some((started, ch)) = entry else {
            return;
        };
        let rtt = started.elapsed();

        let Some(r) = val.dict_get(b"r") else {
            let _ = ch.send(Err(anyhow::anyhow!("missing dht r dict")));
            return;
        };
        let Some(id_bytes) = r.dict_bytes(b"id") else {
            let _ = ch.send(Err(anyhow::anyhow!("missing responder id")));
            return;
        };
        let Ok(responder_id) = Id20::from_slice(id_bytes) else {
            let _ = ch.send(Err(anyhow::anyhow!("invalid responder id")));
            return;
        };

        self.table
            .write()
            .unwrap()
            .insert_or_update(responder_id, from, Some(rtt));

        let token = r.dict_bytes(b"token").map(Bytes::copy_from_slice);

        let mut values = Vec::new();
        if let Some(BVal::List(list)) = r.dict_get(b"values") {
            for item in list {
                if let BVal::Bytes(raw) = item {
                    match raw.len() {
                        6 => wire::parse_compact_v4(raw, &mut values),
                        18 => wire::parse_compact_v6(raw, &mut values),
                        _ => {}
                    }
                }
            }
        }

        let mut nodes = Vec::new();
        if let Some(raw4) = r.dict_bytes(b"nodes") {
            parse_compact_nodes_v4(raw4, &mut nodes);
        }
        if let Some(raw6) = r.dict_bytes(b"nodes6") {
            parse_compact_nodes_v6(raw6, &mut nodes);
        }

        let _ = ch.send(Ok(DhtResponse {
            responder_id,
            token,
            values,
            nodes,
        }));
    }

    fn handle_query(&self, from: SocketAddr, val: &BVal<'_>) {
        let Some(t) = val.dict_bytes(b"t") else {
            return;
        };
        if t.is_empty() || t.len() > 32 {
            return;
        }
        let Some(q) = val.dict_bytes(b"q") else {
            return;
        };
        let Some(a) = val.dict_get(b"a") else {
            return;
        };
        let Some(id_bytes) = a.dict_bytes(b"id") else {
            return;
        };
        let Ok(sender_id) = Id20::from_slice(id_bytes) else {
            return;
        };

        self.table.write().unwrap().insert_or_update(sender_id, from, None);

        let reply: Option<Vec<u8>> = match q {
            b"ping" => {
                let mut w = BWriter::with_capacity(80);
                w.start_dict();
                w.push_str("r");
                w.start_dict();
                w.push_str("id");
                w.push_bytes(&self.self_id.0);
                w.end();
                write_reply_tail(&mut w, t);
                Some(w.finish())
            }
            b"find_node" => {
                let Some(target_b) = a.dict_bytes(b"target") else {
                    return;
                };
                let Ok(target) = Id20::from_slice(target_b) else {
                    return;
                };
                let (c4, c6) = self.encode_closest_nodes(&target);
                let mut w = BWriter::with_capacity(128 + c4.len() + c6.len());
                w.start_dict();
                w.push_str("r");
                w.start_dict();
                w.push_str("id");
                w.push_bytes(&self.self_id.0);
                if !c4.is_empty() {
                    w.push_str("nodes");
                    w.push_bytes(&c4);
                }
                if !c6.is_empty() {
                    w.push_str("nodes6");
                    w.push_bytes(&c6);
                }
                w.end();
                write_reply_tail(&mut w, t);
                Some(w.finish())
            }
            b"get_peers" => {
                let Some(ih_b) = a.dict_bytes(b"info_hash") else {
                    return;
                };
                let Ok(ih) = Id20::from_slice(ih_b) else {
                    return;
                };
                let token = self.issue_token(from.ip());
                let stored = self.get_stored_peers(&ih);
                let mut w = BWriter::with_capacity(256);
                w.start_dict();
                w.push_str("r");
                w.start_dict();
                w.push_str("id");
                w.push_bytes(&self.self_id.0);
                if !stored.is_empty() {
                    w.push_str("nodes");
                    let (c4, _) = self.encode_closest_nodes(&ih);
                    w.push_bytes(&c4);
                    w.push_str("token");
                    w.push_bytes(&token);
                    w.push_str("values");
                    w.start_list();
                    for p in stored {
                        match p {
                            SocketAddr::V4(v4) => {
                                let mut buf = [0u8; 6];
                                buf[..4].copy_from_slice(&v4.ip().octets());
                                buf[4..6].copy_from_slice(&v4.port().to_be_bytes());
                                w.push_bytes(&buf);
                            }
                            SocketAddr::V6(v6) => {
                                let mut buf = [0u8; 18];
                                buf[..16].copy_from_slice(&v6.ip().octets());
                                buf[16..18].copy_from_slice(&v6.port().to_be_bytes());
                                w.push_bytes(&buf);
                            }
                        }
                    }
                    w.end();
                } else {
                    let (c4, c6) = self.encode_closest_nodes(&ih);
                    if !c4.is_empty() {
                        w.push_str("nodes");
                        w.push_bytes(&c4);
                    }
                    if !c6.is_empty() {
                        w.push_str("nodes6");
                        w.push_bytes(&c6);
                    }
                    w.push_str("token");
                    w.push_bytes(&token);
                }
                w.end();
                write_reply_tail(&mut w, t);
                Some(w.finish())
            }
            b"announce_peer" => {
                let Some(ih_b) = a.dict_bytes(b"info_hash") else {
                    return;
                };
                let Ok(ih) = Id20::from_slice(ih_b) else {
                    return;
                };
                let Some(token) = a.dict_bytes(b"token") else {
                    return;
                };
                if !self.verify_token(from.ip(), token) {
                    return;
                }
                let implied = a.dict_i64(b"implied_port").unwrap_or(0) != 0;
                let port = if implied {
                    from.port()
                } else {
                    let Some(p) = a.dict_i64(b"port").and_then(|v| u16::try_from(v).ok()) else {
                        return;
                    };
                    p
                };
                let announced = SocketAddr::new(from.ip(), port);
                if self.is_addr_allowed(&announced) {
                    self.record_peer(ih, announced);
                }
                let mut w = BWriter::with_capacity(80);
                w.start_dict();
                w.push_str("r");
                w.start_dict();
                w.push_str("id");
                w.push_bytes(&self.self_id.0);
                w.end();
                write_reply_tail(&mut w, t);
                Some(w.finish())
            }
            _ => None,
        };

        if let Some(pkt) = reply {
            // Best-effort non-blocking UDP response via socket try_send_to to avoid task spawn overhead.
            let _ = match from {
                SocketAddr::V4(_) => self.v4_sock.try_send_to(&pkt, from),
                SocketAddr::V6(_) => match &self.v6_sock {
                    Some(v6) => v6.try_send_to(&pkt, from),
                    None => return,
                },
            };
        }
    }

    fn encode_closest_nodes(&self, target: &Id20) -> (Vec<u8>, Vec<u8>) {
        let tbl = self.table.read().unwrap();
        let n4 = tbl.closest_v4(target, K_BUCKET_SIZE);
        let n6 = tbl.closest_v6(target, K_BUCKET_SIZE);
        let mut c4 = Vec::with_capacity(n4.len() * 26);
        for (id, addr) in n4 {
            if let SocketAddr::V4(v4) = addr {
                c4.extend_from_slice(&id.0);
                c4.extend_from_slice(&v4.ip().octets());
                c4.extend_from_slice(&v4.port().to_be_bytes());
            }
        }
        let mut c6 = Vec::with_capacity(n6.len() * 38);
        for (id, addr) in n6 {
            if let SocketAddr::V6(v6) = addr {
                c6.extend_from_slice(&id.0);
                c6.extend_from_slice(&v6.ip().octets());
                c6.extend_from_slice(&v6.port().to_be_bytes());
            }
        }
        (c4, c6)
    }

    fn issue_token(&self, ip: IpAddr) -> [u8; 8] {
        let mut s = self.secrets.lock().unwrap();
        if s.rotated_at.elapsed() >= TOKEN_ROTATE_INTERVAL {
            s.prev = s.curr;
            rand::rng().fill_bytes(&mut s.curr);
            s.rotated_at = Instant::now();
        }
        compute_token(ip, &s.curr)
    }

    fn verify_token(&self, ip: IpAddr, tok: &[u8]) -> bool {
        let s = self.secrets.lock().unwrap();
        tok == compute_token(ip, &s.curr) || tok == compute_token(ip, &s.prev)
    }

    fn record_peer(&self, ih: Id20, peer: SocketAddr) {
        let mut store = self.peer_store.lock().unwrap();
        let now = Instant::now();
        if !store.contains_key(&ih) && store.len() >= MAX_STORED_IHS {
            if let Some(evict_key) = store
                .iter()
                .min_by_key(|(_, peers)| peers.iter().map(|(_, t)| *t).max().unwrap_or(now))
                .map(|(k, _)| *k)
            {
                store.remove(&evict_key);
            }
        }
        let list = store.entry(ih).or_default();
        list.retain(|(_, t)| now.duration_since(*t) < PEER_TTL);
        if let Some(existing) = list.iter_mut().find(|(a, _)| *a == peer) {
            existing.1 = now;
        } else {
            if list.len() >= MAX_PEERS_PER_IH {
                list.remove(0);
            }
            list.push((peer, now));
        }
    }

    fn get_stored_peers(&self, ih: &Id20) -> Vec<SocketAddr> {
        let mut store = self.peer_store.lock().unwrap();
        let now = Instant::now();
        let Some(list) = store.get_mut(ih) else {
            return Vec::new();
        };
        list.retain(|(_, t)| now.duration_since(*t) < PEER_TTL);
        list.iter().map(|(a, _)| *a).collect()
    }

    fn save_state(&self) {
        let Some(path) = self.cfg.state_path.as_deref() else {
            return;
        };
        let nodes = self.table.read().unwrap().export_good_addrs(MAX_PERSISTED_NODES);
        if nodes.is_empty() {
            return;
        }
        let state = DhtStateFile {
            node_id: self.self_id.as_string(),
            nodes,
        };
        let Ok(bytes) = serde_json::to_vec(&state) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

async fn recv_loop(weak: Weak<DhtInner>, ipv6: bool) {
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
                match res {
                    Ok((n, from)) if n > 0 => {
                        inner.handle_packet(from, &buf[..n]);
                    }
                    Ok(_) => {}
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }
            }
        }
    }
}

async fn maintenance_loop(weak: Weak<DhtInner>) {
    if let Some(inner) = weak.upgrade() {
        inner.bootstrap_round().await;
    } else {
        return;
    }

    let mut tick = tokio::time::interval(Duration::from_secs(60));
    tick.tick().await;
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        tokio::select! {
            _ = inner.shutdown.notified() => return,
            _ = tick.tick() => {}
        }

        // Purge expired pending RPC entries and peer store entries.
        let now = Instant::now();
        {
            let mut p = inner.pending.lock().unwrap();
            p.retain(|_, (started, _)| now.duration_since(*started) < Duration::from_secs(10));
        }
        {
            let mut ps = inner.peer_store.lock().unwrap();
            ps.retain(|_, list| {
                list.retain(|(_, t)| now.duration_since(*t) < PEER_TTL);
                !list.is_empty()
            });
        }

        let (tlen, refresh_target) = {
            let tbl = inner.table.read().unwrap();
            (tbl.len(), tbl.stale_refresh_target())
        };
        if tlen < 16 {
            inner.bootstrap_round().await;
        } else if let Some(target) = refresh_target {
            let closest = inner.table.read().unwrap().closest(&target, 4);
            for (_, addr) in closest {
                let _ = inner.rpc_find_node(addr, target).await;
            }
        }
        inner.save_state();
    }
}

fn write_reply_tail(w: &mut BWriter, t: &[u8]) {
    w.push_str("t");
    w.push_bytes(t);
    w.push_str("v");
    w.push_str("BL01");
    w.push_str("y");
    w.push_str("r");
    w.end();
}

fn compute_token(ip: IpAddr, secret: &[u8; 20]) -> [u8; 8] {
    let mut h = Sha1::new();
    match ip {
        IpAddr::V4(v4) => h.update(v4.octets()),
        IpAddr::V6(v6) => h.update(v6.octets()),
    }
    h.update(secret);
    let dig = h.finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(&dig[..8]);
    out
}

fn parse_compact_nodes_v4(raw: &[u8], out: &mut Vec<(Id20, SocketAddr)>) {
    for &chunk in raw.as_chunks::<26>().0 {
        if let Ok(id) = Id20::from_slice(&chunk[..20]) {
            let ip = Ipv4Addr::new(chunk[20], chunk[21], chunk[22], chunk[23]);
            let port = u16::from_be_bytes([chunk[24], chunk[25]]);
            if port != 0 {
                out.push((id, SocketAddr::new(IpAddr::V4(ip), port)));
            }
        }
    }
}

fn parse_compact_nodes_v6(raw: &[u8], out: &mut Vec<(Id20, SocketAddr)>) {
    for &chunk in raw.as_chunks::<38>().0 {
        if let Ok(id) = Id20::from_slice(&chunk[..20]) {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&chunk[20..36]);
            let port = u16::from_be_bytes([chunk[36], chunk[37]]);
            if port != 0 {
                out.push((id, SocketAddr::new(IpAddr::V6(octets.into()), port)));
            }
        }
    }
}

fn bind_v6_only(port: u16) -> Option<UdpSocket> {
    let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)).ok()?;
    let _ = sock.set_only_v6(true);
    let _ = sock.set_nonblocking(true);
    let addr: SocketAddr = ([0u16; 8], port).into();
    sock.bind(&addr.into()).ok()?;
    let std_sock: std::net::UdpSocket = sock.into();
    UdpSocket::from_std(std_sock).ok()
}

fn load_state(path: Option<&std::path::Path>) -> (Option<Id20>, Vec<SocketAddr>) {
    let Some(p) = path else {
        return (None, Vec::new());
    };
    let Ok(bytes) = std::fs::read(p) else {
        return (None, Vec::new());
    };
    let Ok(st) = serde_json::from_slice::<DhtStateFile>(&bytes) else {
        return (None, Vec::new());
    };
    let id = Id20::from_hex(&st.node_id).ok();
    let addrs = st
        .nodes
        .into_iter()
        .filter_map(|s| s.parse::<SocketAddr>().ok())
        .collect();
    (id, addrs)
}

fn random_id20() -> Id20 {
    let mut b = [0u8; 20];
    rand::rng().fill_bytes(&mut b);
    Id20(b)
}

fn random_id_in_bucket(base: Id20, bucket_idx: usize) -> Id20 {
    let mut out = random_id20().0;
    let byte = bucket_idx / 8;
    let bit = 7 - (bucket_idx % 8);
    out[..byte].copy_from_slice(&base.0[..byte]);
    let upper_mask = !((1u8 << (bit + 1)) - 1);
    out[byte] = (base.0[byte] & upper_mask) | ((base.0[byte] ^ (1 << bit)) & (1 << bit))
        | (out[byte] & ((1u8 << bit) - 1));
    Id20(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_dht_loopback_announce_and_lookup() {
        let dir = std::env::temp_dir().join(format!("bitlab_dht_test_{}", rand::random::<u32>()));
        let _ = std::fs::create_dir_all(&dir);
        let state_a = dir.join("dht_a.json");

        let n_a = DhtNode::start(DhtConfig {
            state_path: Some(state_a.clone()),
            bind_ip: Ipv4Addr::LOCALHOST,
            port: 0,
            enable_ipv6: false,
            use_default_bootstrap: false,
            bootstrap_addrs: Vec::new(),
            allow_private: true,
            rpc_timeout: Duration::from_millis(500),
            lookup_timeout: Duration::from_secs(3),
        })
        .await
        .unwrap();

        let n_b = DhtNode::start(DhtConfig {
            state_path: None,
            bind_ip: Ipv4Addr::LOCALHOST,
            port: 0,
            enable_ipv6: false,
            use_default_bootstrap: false,
            bootstrap_addrs: vec![n_a.local_addr_v4()],
            allow_private: true,
            rpc_timeout: Duration::from_millis(500),
            lookup_timeout: Duration::from_secs(3),
        })
        .await
        .unwrap();

        let n_c = DhtNode::start(DhtConfig {
            state_path: None,
            bind_ip: Ipv4Addr::LOCALHOST,
            port: 0,
            enable_ipv6: false,
            use_default_bootstrap: false,
            bootstrap_addrs: vec![n_a.local_addr_v4()],
            allow_private: true,
            rpc_timeout: Duration::from_millis(500),
            lookup_timeout: Duration::from_secs(3),
        })
        .await
        .unwrap();

        let pinged = n_b.ping(n_c.local_addr_v4()).await.unwrap();
        assert_eq!(pinged, n_c.node_id());
        assert!(n_a.routing_table_len() >= 1);

        let ih = Id20::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap();
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        // Node B announces port 51413 for info_hash.
        let (tx1, _rx1) = mpsc::unbounded_channel();
        n_b.lookup_peers(ih, Some(51413), tx1, cancel_rx.clone()).await;

        // Node C queries the swarm and immediately receives Node B's announced endpoint.
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        n_c.lookup_peers(ih, None, tx2, cancel_rx).await;

        let mut found = Vec::new();
        while let Ok(p) = rx2.try_recv() {
            found.push(p);
        }
        assert!(
            found.iter().any(|p| p.port() == 51413),
            "expected announced peer port 51413, got {found:?}"
        );

        n_a.save_state();
        assert!(state_a.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
