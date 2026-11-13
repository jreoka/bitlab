use anyhow::{bail, Context, Result};
use bytes::{Bytes, BytesMut};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch, Notify};

use super::dht::DhtNode;
use super::metainfo::{Id20, TorrentMeta, CHUNK_SIZE};
use super::storage::TorrentStorage;
use super::tracker::{AnnounceRequest, TrackerClient, TrackerEvent};
use super::transport::{InboundPeer, PeerStream, TransportManager};
use super::wire::{
    self, Handshake, PeerExtInfo, PeerReader, UtMetadataMsg, WireMsg, MY_EXT_UT_METADATA,
};

const MAX_ACTIVE_PEERS: usize = 64;
const MAX_CONNECT_IN_FLIGHT: usize = 64;
const MAX_ACTIVE_PIECES: usize = 32;
const META_PIECE_SIZE: usize = 16_384;
const MAX_META_SIZE: usize = 16 * 1024 * 1024;
const HEDGE_DELAY: Duration = Duration::from_millis(150);
const STALE_REQUEST_TIMEOUT: Duration = Duration::from_millis(2000);
const META_HEDGE_DELAY: Duration = Duration::from_millis(250);
const CHOKE_CHURN_AFTER: Duration = Duration::from_secs(10);
const INITIAL_QUEUE_TARGET: usize = 64;
const MAX_QUEUE_TARGET: usize = 256;
const FAST_TICK_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Clone, Debug, Default)]
pub struct TorrentStats {
    pub name: String,
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub peers: usize,
    pub seeders: usize,
    pub rate_bps: u64,
    pub readers: usize,
    pub paused: bool,
}

#[derive(Clone, Debug)]
pub struct TorrentFileInfo {
    pub idx: usize,
    pub name: String,
    pub len: u64,
    pub is_padding: bool,
}

pub struct TorrentDeps {
    pub dht: Option<Arc<DhtNode>>,
    pub tracker: Arc<TrackerClient>,
    pub transport: Arc<TransportManager>,
    pub readahead: Arc<AtomicI64>,
    pub allow_private: bool,
}

enum ActorCmd {
    InjectMeta(Bytes),
    AddPeers(Vec<SocketAddr>),
    TrackerSummary { interval: Option<u32>, seeders: u32 },
    SelectFile(usize),
    Pause,
    Unpause,
    Shutdown,
}

enum PeerEvent {
    Connected {
        conn_id: u64,
        addr: SocketAddr,
        supports_fast: bool,
        write_tx: mpsc::UnboundedSender<WireMsg>,
    },
    ConnectFailed {
        addr: SocketAddr,
    },
    ExtHandshake {
        conn_id: u64,
        ext: PeerExtInfo,
    },
    Msg {
        conn_id: u64,
        msg: WireMsg,
    },
    Disconnected {
        conn_id: u64,
    },
}

struct TorrentShared {
    info_hash: Id20,
    save_dir: PathBuf,
    readahead: Arc<AtomicI64>,
    meta_slot: RwLock<Option<Arc<TorrentMeta>>>,
    meta_notify: Notify,
    storage: RwLock<Option<Arc<TorrentStorage>>>,
    stats: RwLock<TorrentStats>,
    cursor_piece: AtomicUsize,
    wait_pieces: Mutex<HashMap<usize, usize>>,
    active_readers: AtomicUsize,
    work_notify: Notify,
    shutdown_tx: watch::Sender<bool>,
}

pub struct TorrentHandle {
    shared: Arc<TorrentShared>,
    cmd_tx: mpsc::UnboundedSender<ActorCmd>,
}

impl TorrentHandle {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        info_hash: Id20,
        name_hint: Option<String>,
        save_dir: PathBuf,
        initial_trackers: Vec<String>,
        initial_peers: Vec<SocketAddr>,
        initial_meta: Option<Arc<TorrentMeta>>,
        paused: bool,
        deps: Arc<TorrentDeps>,
    ) -> Arc<Self> {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let initial_storage = initial_meta.as_ref().and_then(|m| {
            TorrentStorage::open(save_dir.clone(), m.clone(), deps.readahead.clone()).ok()
        });
        let init_name = initial_meta
            .as_ref()
            .map(|m| m.name.clone())
            .or(name_hint)
            .unwrap_or_else(|| info_hash.as_string());
        let init_total = initial_meta.as_ref().map(|m| m.total_length).unwrap_or(0);
        let init_done = initial_storage
            .as_ref()
            .map(|s| s.completed_selected_bytes())
            .unwrap_or(0);

        let shared = Arc::new(TorrentShared {
            info_hash,
            save_dir,
            readahead: deps.readahead.clone(),
            meta_slot: RwLock::new(initial_meta),
            meta_notify: Notify::new(),
            storage: RwLock::new(initial_storage),
            stats: RwLock::new(TorrentStats {
                name: init_name,
                total_bytes: init_total,
                downloaded_bytes: init_done,
                peers: 0,
                seeders: 0,
                rate_bps: 0,
                readers: 0,
                paused,
            }),
            cursor_piece: AtomicUsize::new(0),
            wait_pieces: Mutex::new(HashMap::new()),
            active_readers: AtomicUsize::new(0),
            work_notify: Notify::new(),
            shutdown_tx,
        });

        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        deps.transport.register_torrent(info_hash, inbound_tx);

        let shared_c = shared.clone();
        let deps_c = deps.clone();
        let cmd_tx_c = cmd_tx.clone();
        tokio::spawn(async move {
            let mut actor = TorrentActor::new(
                shared_c,
                deps_c,
                cmd_tx_c,
                cmd_rx,
                inbound_rx,
                shutdown_rx,
                initial_trackers,
                initial_peers,
                paused,
            );
            actor.run().await;
        });

        Arc::new(Self { shared, cmd_tx })
    }

    pub fn info_hash(&self) -> Id20 {
        self.shared.info_hash
    }

    pub fn stats(&self) -> TorrentStats {
        let mut s = self.shared.stats.read().unwrap().clone();
        s.readers = self.shared.active_readers.load(Ordering::Relaxed);
        s
    }

    pub fn inject_meta_bytes(&self, info_bytes: Bytes) {
        let _ = self.cmd_tx.send(ActorCmd::InjectMeta(info_bytes));
    }

    pub fn add_peers(&self, peers: Vec<SocketAddr>) {
        if !peers.is_empty() {
            let _ = self.cmd_tx.send(ActorCmd::AddPeers(peers));
        }
    }

    pub fn pause(&self) {
        let _ = self.cmd_tx.send(ActorCmd::Pause);
    }

    pub fn unpause(&self) {
        let _ = self.cmd_tx.send(ActorCmd::Unpause);
    }

    pub fn shutdown(&self) {
        let _ = self.shared.shutdown_tx.send(true);
        let _ = self.cmd_tx.send(ActorCmd::Shutdown);
        self.shared.meta_notify.notify_waiters();
        self.shared.work_notify.notify_waiters();
    }

    pub async fn wait_metadata(&self, timeout: Duration) -> Result<Arc<TorrentMeta>> {
        if let Some(m) = self.shared.meta_slot.read().unwrap().clone() {
            return Ok(m);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let mut shut_rx = self.shared.shutdown_tx.subscribe();
        loop {
            let notified = self.shared.meta_notify.notified();
            if let Some(m) = self.shared.meta_slot.read().unwrap().clone() {
                return Ok(m);
            }
            if *shut_rx.borrow() {
                bail!("torrent closed while waiting for metadata");
            }
            tokio::select! {
                _ = notified => {}
                _ = shut_rx.changed() => {
                    if *shut_rx.borrow() {
                        bail!("torrent closed while waiting for metadata");
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    if let Some(m) = self.shared.meta_slot.read().unwrap().clone() {
                        return Ok(m);
                    }
                    bail!("metadata timeout");
                }
            }
        }
    }

    pub fn files(&self) -> Vec<TorrentFileInfo> {
        let Some(meta) = self.shared.meta_slot.read().unwrap().clone() else {
            return Vec::new();
        };
        meta.file_infos
            .iter()
            .map(|f| TorrentFileInfo {
                idx: f.idx,
                name: f.relative_filename.to_string_lossy().into_owned(),
                len: f.len,
                is_padding: f.is_padding,
            })
            .collect()
    }

    pub fn open_stream(&self, file_idx: usize, start_offset: u64, len: u64) -> Result<BtByteStream> {
        let st = self
            .shared
            .storage
            .read()
            .unwrap()
            .clone()
            .context("metadata not ready")?;
        st.select_file(file_idx)?;
        let meta = st.meta().clone();
        let entry = meta
            .file_infos
            .get(file_idx)
            .context("file index out of bounds")?;
        if start_offset.saturating_add(len) > entry.len {
            bail!("stream range out of bounds");
        }

        if let Some((start_piece, _)) = meta.file_offset_to_piece(file_idx, start_offset) {
            self.shared.cursor_piece.store(start_piece, Ordering::Relaxed);
        }
        let _ = self.cmd_tx.send(ActorCmd::SelectFile(file_idx));
        self.shared.work_notify.notify_one();

        Ok(BtByteStream::new(
            self.shared.clone(),
            st,
            file_idx,
            start_offset,
            len,
        ))
    }

    #[cfg(test)]
    pub fn storage_for_seed(&self) -> Option<Arc<TorrentStorage>> {
        self.shared.storage.read().unwrap().clone()
    }
}

struct ReaderGuard {
    shared: Arc<TorrentShared>,
}
impl Drop for ReaderGuard {
    fn drop(&mut self) {
        self.shared.active_readers.fetch_sub(1, Ordering::Relaxed);
    }
}

type FetchFuture = Pin<Box<dyn Future<Output = (Arc<TorrentStorage>, io::Result<Option<Bytes>>)> + Send>>;

enum StreamState {
    Idle,
    Fetching(FetchFuture),
}

pub struct BtByteStream {
    shared: Arc<TorrentShared>,
    storage: Option<Arc<TorrentStorage>>,
    file_idx: usize,
    pos: u64,
    remaining: u64,
    cancel_rx: watch::Receiver<bool>,
    state: StreamState,
    _guard: ReaderGuard,
}

struct WaitPieceGuard {
    shared: Arc<TorrentShared>,
    piece: usize,
}

impl WaitPieceGuard {
    fn new(shared: Arc<TorrentShared>, piece: usize) -> Self {
        {
            let mut w = shared.wait_pieces.lock().unwrap();
            *w.entry(piece).or_insert(0) += 1;
        }
        Self { shared, piece }
    }
}

impl Drop for WaitPieceGuard {
    fn drop(&mut self) {
        let mut w = self.shared.wait_pieces.lock().unwrap();
        if let Some(c) = w.get_mut(&self.piece) {
            *c = c.saturating_sub(1);
            if *c == 0 {
                w.remove(&self.piece);
            }
        }
    }
}

impl BtByteStream {
    fn new(
        shared: Arc<TorrentShared>,
        storage: Arc<TorrentStorage>,
        file_idx: usize,
        start_offset: u64,
        len: u64,
    ) -> Self {
        shared.active_readers.fetch_add(1, Ordering::Relaxed);
        let cancel_rx = shared.shutdown_tx.subscribe();
        Self {
            shared: shared.clone(),
            storage: Some(storage),
            file_idx,
            pos: start_offset,
            remaining: len,
            cancel_rx,
            state: StreamState::Idle,
            _guard: ReaderGuard { shared },
        }
    }
}

impl futures::Stream for BtByteStream {
    type Item = io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        if let StreamState::Idle = self.state {
            if self.remaining == 0 {
                return Poll::Ready(None);
            }
            let Some(storage) = self.storage.take() else {
                return Poll::Ready(None);
            };
            let meta = storage.meta();
            let Some((piece, piece_off)) = meta.file_offset_to_piece(self.file_idx, self.pos)
            else {
                return Poll::Ready(None);
            };
            let piece_len = meta.piece_len(piece) as usize;
            let file_entry = &meta.file_infos[self.file_idx];
            let max_in_file = (file_entry.len - self.pos) as usize;
            let take = (piece_len - piece_off)
                .min(self.remaining as usize)
                .min(max_in_file);
            if take == 0 {
                return Poll::Ready(None);
            }

            let _wait_guard = WaitPieceGuard::new(self.shared.clone(), piece);
            self.shared.cursor_piece.store(piece, Ordering::Relaxed);
            self.shared.work_notify.notify_one();

            self.pos += take as u64;
            self.remaining -= take as u64;

            let mut cancel = self.cancel_rx.clone();
            self.state = StreamState::Fetching(Box::pin(async move {
                let _wg = _wait_guard;
                if !storage.wait_piece(piece, &mut cancel).await {
                    return (
                        storage,
                        Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "torrent stopped while waiting for piece",
                        )),
                    );
                }
                let res = storage
                    .read_piece_slice(piece, piece_off, take)
                    .await
                    .map(Some)
                    .map_err(io::Error::other);
                (storage, res)
            }));
        }

        if let StreamState::Fetching(ref mut fut) = self.state {
            match fut.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready((storage, res)) => {
                    self.storage = Some(storage);
                    self.state = StreamState::Idle;
                    match res {
                        Ok(Some(b)) => return Poll::Ready(Some(Ok(b))),
                        Ok(None) => return Poll::Ready(None),
                        Err(e) => return Poll::Ready(Some(Err(e))),
                    }
                }
            }
        }
        Poll::Pending
    }
}

struct PeerConn {
    conn_id: u64,
    addr: SocketAddr,
    supports_fast: bool,
    write_tx: mpsc::UnboundedSender<WireMsg>,
    am_choking: bool,
    am_interested: bool,
    peer_choking: bool,
    peer_bitfield: Vec<bool>,
    pending_bitfield: Option<Bytes>,
    pending_have_all: bool,
    pending_haves: Vec<u32>,
    is_seeder: bool,
    allowed_fast: HashSet<u32>,
    suggested: Vec<u32>,
    in_flight: HashMap<(u32, u32), Instant>,
    target_queue: usize,
    peer_reqq: usize,
    bytes_this_tick: u64,
    ewma_bps: u64,
    choked_since: Instant,
    connected_at: Instant,
    ut_metadata_id: Option<u8>,
    ext_handshake_received: bool,
    meta_in_flight: HashMap<u32, Instant>,
    sent_bitfield: bool,
}

struct PieceBuf {
    piece_len: usize,
    chunks: Vec<Option<Bytes>>,
    chunk_inflight: Vec<Vec<(u64, Instant)>>,
    verifying: bool,
}

impl PieceBuf {
    fn new(meta: &TorrentMeta, piece: usize) -> Self {
        let piece_len = meta.piece_len(piece) as usize;
        let num_chunks = meta.num_chunks(piece);
        Self {
            piece_len,
            chunks: vec![None; num_chunks],
            chunk_inflight: vec![Vec::new(); num_chunks],
            verifying: false,
        }
    }

    fn all_received(&self) -> bool {
        self.chunks.iter().all(|c| c.is_some())
    }

    fn assemble(&self) -> Bytes {
        let mut out = BytesMut::with_capacity(self.piece_len);
        for b in self.chunks.iter().flatten() {
            out.extend_from_slice(b);
        }
        out.freeze()
    }
}

struct MetaAssembler {
    total_size: usize,
    num_pieces: usize,
    pieces: Vec<Option<Bytes>>,
    inflight: Vec<Vec<(u64, Instant)>>,
    verifying: bool,
}

impl MetaAssembler {
    fn new(total_size: usize) -> Self {
        let num_pieces = total_size.div_ceil(META_PIECE_SIZE);
        Self {
            total_size,
            num_pieces,
            pieces: vec![None; num_pieces],
            inflight: vec![Vec::new(); num_pieces],
            verifying: false,
        }
    }

    fn complete(&self) -> bool {
        self.pieces.iter().all(|p| p.is_some())
    }

    fn assemble(&self) -> Bytes {
        let mut out = BytesMut::with_capacity(self.total_size);
        for b in self.pieces.iter().flatten() {
            out.extend_from_slice(b);
        }
        out.truncate(self.total_size);
        out.freeze()
    }
}

#[derive(Clone, Copy)]
enum DispatchScope<'a> {
    Set { set: &'a HashSet<usize>, hedge: bool },
    Future,
}

struct PieceSelectionRanges {
    ranges: Vec<std::ops::RangeInclusive<usize>>,
    tails: Vec<usize>,
}

struct TorrentActor {
    shared: Arc<TorrentShared>,
    deps: Arc<TorrentDeps>,
    cmd_tx: mpsc::UnboundedSender<ActorCmd>,
    cmd_rx: mpsc::UnboundedReceiver<ActorCmd>,
    inbound_rx: mpsc::UnboundedReceiver<InboundPeer>,
    shutdown_rx: watch::Receiver<bool>,
    peer_evt_tx: mpsc::UnboundedSender<PeerEvent>,
    peer_evt_rx: mpsc::UnboundedReceiver<PeerEvent>,
    verify_tx: mpsc::UnboundedSender<(usize, Result<Bytes>)>,
    verify_rx: mpsc::UnboundedReceiver<(usize, Result<Bytes>)>,
    trackers: Vec<String>,
    tracker_seeders: u32,
    next_conn_id: u64,
    peers: HashMap<u64, PeerConn>,
    known_peers: HashSet<SocketAddr>,
    peer_queue: VecDeque<SocketAddr>,
    peer_queue_v6: VecDeque<SocketAddr>,
    connect_in_flight: HashSet<SocketAddr>,
    peer_backoff: HashMap<SocketAddr, Instant>,
    active_pieces: HashMap<usize, PieceBuf>,
    selected_files: HashSet<usize>,
    meta_asm: Option<MetaAssembler>,
    paused: bool,
    discovery_started: bool,
    last_tracker_announce: Option<Instant>,
    tracker_interval: Duration,
    last_dht_lookup: Option<Instant>,
    total_session_downloaded: Arc<AtomicU64>,
    last_rate_bytes: u64,
    last_rate_tick: Instant,
}

impl TorrentActor {
    #[allow(clippy::too_many_arguments)]
    fn new(
        shared: Arc<TorrentShared>,
        deps: Arc<TorrentDeps>,
        cmd_tx: mpsc::UnboundedSender<ActorCmd>,
        cmd_rx: mpsc::UnboundedReceiver<ActorCmd>,
        inbound_rx: mpsc::UnboundedReceiver<InboundPeer>,
        shutdown_rx: watch::Receiver<bool>,
        initial_trackers: Vec<String>,
        initial_peers: Vec<SocketAddr>,
        paused: bool,
    ) -> Self {
        let (peer_evt_tx, peer_evt_rx) = mpsc::unbounded_channel();
        let (verify_tx, verify_rx) = mpsc::unbounded_channel();

        let mut tracks = initial_trackers;
        if let Some(m) = shared.meta_slot.read().unwrap().as_ref() {
            for t in &m.trackers {
                if !tracks.contains(t) {
                    tracks.push(t.clone());
                }
            }
        }

        let mut known_peers = HashSet::new();
        let mut peer_queue = VecDeque::new();
        let mut peer_queue_v6 = VecDeque::new();
        for p in initial_peers {
            if known_peers.insert(p) {
                if p.is_ipv4() {
                    peer_queue.push_back(p);
                } else {
                    peer_queue_v6.push_back(p);
                }
            }
        }

        Self {
            shared,
            deps,
            cmd_tx,
            cmd_rx,
            inbound_rx,
            shutdown_rx,
            peer_evt_tx,
            peer_evt_rx,
            verify_tx,
            verify_rx,
            trackers: tracks,
            tracker_seeders: 0,
            next_conn_id: 1,
            peers: HashMap::new(),
            known_peers,
            peer_queue,
            peer_queue_v6,
            connect_in_flight: HashSet::new(),
            peer_backoff: HashMap::new(),
            active_pieces: HashMap::new(),
            selected_files: HashSet::new(),
            meta_asm: None,
            paused,
            discovery_started: false,
            last_tracker_announce: None,
            tracker_interval: Duration::from_secs(120),
            last_dht_lookup: None,
            total_session_downloaded: Arc::new(AtomicU64::new(0)),
            last_rate_bytes: 0,
            last_rate_tick: Instant::now(),
        }
    }

    fn alloc_conn_id(&mut self) -> u64 {
        let id = self.next_conn_id;
        self.next_conn_id = self.next_conn_id.wrapping_add(1);
        id
    }

    async fn run(&mut self) {
        if !self.paused {
            self.start_discovery(false);
            self.dial_peers();
        }

        let mut tick = tokio::time::interval(FAST_TICK_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = self.shutdown_rx.changed() => {
                    if *self.shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = self.shared.work_notify.notified() => {
                    self.schedule_all();
                }
                Some(cmd) = self.cmd_rx.recv() => {
                    if !self.handle_cmd(cmd) {
                        break;
                    }
                }
                Some(inbound) = self.inbound_rx.recv() => {
                    self.handle_inbound_peer(inbound);
                }
                Some(evt) = self.peer_evt_rx.recv() => {
                    self.handle_peer_event(evt);
                }
                Some((piece, res)) = self.verify_rx.recv() => {
                    self.handle_verified_piece(piece, res);
                }
                _ = tick.tick() => {
                    self.on_tick();
                }
            }
        }

        if self.discovery_started {
            self.send_tracker_event(TrackerEvent::Stopped);
        }
        self.deps
            .transport
            .unregister_torrent(&self.shared.info_hash);
        if let Some(st) = self.shared.storage.read().unwrap().as_ref() {
            st.save_fastresume_now();
        }
    }

    fn handle_cmd(&mut self, cmd: ActorCmd) -> bool {
        match cmd {
            ActorCmd::InjectMeta(raw) => {
                self.try_apply_info_bytes(raw);
            }
            ActorCmd::AddPeers(addrs) => {
                for a in addrs {
                    self.enqueue_peer(a);
                }
                if !self.paused {
                    self.dial_peers();
                }
            }
            ActorCmd::TrackerSummary { interval, seeders } => {
                if seeders > 0 || self.tracker_seeders == 0 {
                    self.tracker_seeders = seeders;
                }
                if let Some(iv) = interval {
                    self.tracker_interval = Duration::from_secs(u64::from(iv).clamp(30, 1800));
                }
                let rate_bps = self.shared.stats.read().unwrap().rate_bps;
                self.update_stats(rate_bps);
            }
            ActorCmd::SelectFile(idx) => {
                self.selected_files.insert(idx);
                if let Some(st) = self.shared.storage.read().unwrap().clone() {
                    let _ = st.select_file(idx);
                }
                self.prune_out_of_selection_pieces();
                self.schedule_all();
            }
            ActorCmd::Pause => {
                if !self.paused {
                    self.paused = true;
                    let peer_ids: Vec<u64> = self.peers.keys().copied().collect();
                    for id in peer_ids {
                        self.drop_peer(id);
                    }
                    self.active_pieces.clear();
                    self.send_tracker_event(TrackerEvent::Stopped);
                    self.discovery_started = false;
                    self.last_rate_tick = Instant::now();
                    self.update_stats(0);
                }
            }
            ActorCmd::Unpause => {
                if self.paused {
                    self.paused = false;
                    self.last_rate_tick = Instant::now();
                    self.start_discovery(false);
                    self.dial_peers();
                    self.update_stats(0);
                }
            }
            ActorCmd::Shutdown => return false,
        }
        true
    }

    fn enqueue_peer(&mut self, addr: SocketAddr) {
        if addr.port() == 0 {
            return;
        }
        if !self.deps.allow_private && crate::netguard::is_private_address_ip(&addr.ip()) {
            return;
        }
        if addr == self.deps.transport.local_addr() {
            return;
        }
        if self.known_peers.insert(addr) {
            if addr.is_ipv4() {
                self.peer_queue.push_back(addr);
            } else {
                self.peer_queue_v6.push_back(addr);
            }
        }
    }

    fn dial_peers(&mut self) {
        if self.paused {
            return;
        }
        let now = Instant::now();
        while self.peers.len() + self.connect_in_flight.len() < MAX_ACTIVE_PEERS
            && self.connect_in_flight.len() < MAX_CONNECT_IN_FLIGHT
        {
            let Some(addr) = self
                .peer_queue
                .pop_front()
                .or_else(|| self.peer_queue_v6.pop_front())
            else {
                break;
            };
            if self.peers.values().any(|p| p.addr == addr) || self.connect_in_flight.contains(&addr)
            {
                continue;
            }
            if let Some(&retry_at) = self.peer_backoff.get(&addr) {
                if retry_at > now {
                    continue;
                }
            }

            self.connect_in_flight.insert(addr);
            let conn_id = self.alloc_conn_id();
            let transport = self.deps.transport.clone();
            let ih = self.shared.info_hash;
            let evt_tx = self.peer_evt_tx.clone();
            let meta_len = self
                .shared
                .meta_slot
                .read()
                .unwrap()
                .as_ref()
                .map(|m| m.info_bytes.len());
            let mut shut = self.shutdown_rx.clone();

            tokio::spawn(async move {
                let res = tokio::select! {
                    _ = shut.changed() => return,
                    r = transport.connect_peer(addr, ih) => r,
                };
                match res {
                    Ok((stream, hs)) => {
                        tracing::debug!(
                            "peer connected {} via {}",
                            addr,
                            stream.transport_name()
                        );
                        let supports_fast = hs.supports_fast();
                        let write_tx = spawn_peer_io(
                            conn_id,
                            stream,
                            hs,
                            meta_len,
                            evt_tx.clone(),
                            shut,
                        );
                        let _ = evt_tx.send(PeerEvent::Connected {
                            conn_id,
                            addr,
                            supports_fast,
                            write_tx,
                        });
                    }
                    Err(_) => {
                        let _ = evt_tx.send(PeerEvent::ConnectFailed { addr });
                    }
                }
            });
        }
    }

    fn handle_inbound_peer(&mut self, inbound: InboundPeer) {
        if self.paused || self.peers.len() >= MAX_ACTIVE_PEERS {
            return;
        }
        let conn_id = self.alloc_conn_id();
        let meta_len = self
            .shared
            .meta_slot
            .read()
            .unwrap()
            .as_ref()
            .map(|m| m.info_bytes.len());
        let supports_fast = inbound.handshake.supports_fast();
        let write_tx = spawn_peer_io(
            conn_id,
            inbound.stream,
            inbound.handshake,
            meta_len,
            self.peer_evt_tx.clone(),
            self.shutdown_rx.clone(),
        );
        self.register_connected_peer(conn_id, inbound.addr, supports_fast, write_tx);
    }

    fn register_connected_peer(
        &mut self,
        conn_id: u64,
        addr: SocketAddr,
        supports_fast: bool,
        write_tx: mpsc::UnboundedSender<WireMsg>,
    ) {
        let num_pieces = self
            .shared
            .meta_slot
            .read()
            .unwrap()
            .as_ref()
            .map(|m| m.num_pieces())
            .unwrap_or(0);
        let now = Instant::now();
        let mut peer = PeerConn {
            conn_id,
            addr,
            supports_fast,
            write_tx,
            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_bitfield: vec![false; num_pieces],
            pending_bitfield: None,
            pending_have_all: false,
            pending_haves: Vec::new(),
            is_seeder: false,
            allowed_fast: HashSet::new(),
            suggested: Vec::new(),
            in_flight: HashMap::new(),
            target_queue: INITIAL_QUEUE_TARGET,
            peer_reqq: MAX_QUEUE_TARGET,
            bytes_this_tick: 0,
            ewma_bps: 0,
            choked_since: now,
            connected_at: now,
            ut_metadata_id: None,
            ext_handshake_received: false,
            meta_in_flight: HashMap::new(),
            sent_bitfield: false,
        };
        if let Some(st) = self.shared.storage.read().unwrap().as_ref() {
            send_our_bitfield(&mut peer, st);
        }
        self.peers.insert(conn_id, peer);
        self.refresh_stats();
    }

    fn drop_peer(&mut self, conn_id: u64) {
        if let Some(peer) = self.peers.remove(&conn_id) {
            for ((piece, begin), _) in peer.in_flight {
                let cidx = (begin / CHUNK_SIZE) as usize;
                if let Some(pb) = self.active_pieces.get_mut(&(piece as usize)) {
                    if let Some(list) = pb.chunk_inflight.get_mut(cidx) {
                        list.retain(|(cid, _)| *cid != conn_id);
                    }
                }
            }
            if let Some(asm) = &mut self.meta_asm {
                for (mp, _) in peer.meta_in_flight {
                    if let Some(list) = asm.inflight.get_mut(mp as usize) {
                        list.retain(|(cid, _)| *cid != conn_id);
                    }
                }
            }
        }
        self.refresh_stats();
    }

    fn handle_peer_event(&mut self, evt: PeerEvent) {
        match evt {
            PeerEvent::Connected {
                conn_id,
                addr,
                supports_fast,
                write_tx,
            } => {
                self.connect_in_flight.remove(&addr);
                if self.paused || self.peers.len() >= MAX_ACTIVE_PEERS {
                    return;
                }
                self.register_connected_peer(conn_id, addr, supports_fast, write_tx);
                self.schedule_all();
                self.dial_peers();
            }
            PeerEvent::ConnectFailed { addr } => {
                self.connect_in_flight.remove(&addr);
                self.peer_backoff
                    .insert(addr, Instant::now() + Duration::from_secs(30));
                if !self.paused {
                    self.dial_peers();
                }
            }
            PeerEvent::ExtHandshake { conn_id, ext } => {
                if let Some(p) = self.peers.get_mut(&conn_id) {
                    p.ext_handshake_received = true;
                    p.ut_metadata_id = ext.ut_metadata_id;
                    if let Some(reqq) = ext.reqq {
                        let cap = (reqq as usize).clamp(8, MAX_QUEUE_TARGET);
                        p.peer_reqq = cap;
                        p.target_queue = if p.ewma_bps == 0 {
                            INITIAL_QUEUE_TARGET.min(cap)
                        } else {
                            p.target_queue.min(cap)
                        };
                    }
                }
                if self.shared.meta_slot.read().unwrap().is_none() {
                    if let Some(sz) = ext.metadata_size {
                        if sz > 0 && sz <= MAX_META_SIZE && self.meta_asm.is_none() {
                            self.meta_asm = Some(MetaAssembler::new(sz));
                        }
                    }
                }
                self.schedule_all();
            }
            PeerEvent::Msg { conn_id, msg } => {
                self.handle_wire_msg(conn_id, msg);
            }
            PeerEvent::Disconnected { conn_id } => {
                self.drop_peer(conn_id);
                self.schedule_all();
                if !self.paused {
                    self.dial_peers();
                }
            }
        }
    }

    fn handle_wire_msg(&mut self, conn_id: u64, msg: WireMsg) {
        match msg {
            WireMsg::KeepAlive | WireMsg::NotInterested => {}
            WireMsg::Choke => {
                if let Some(p) = self.peers.get_mut(&conn_id) {
                    p.peer_choking = true;
                    p.choked_since = Instant::now();
                }
            }
            WireMsg::Unchoke => {
                if let Some(p) = self.peers.get_mut(&conn_id) {
                    p.peer_choking = false;
                }
                self.schedule_pieces();
            }
            WireMsg::Interested => {
                let have_any = self
                    .shared
                    .storage
                    .read()
                    .unwrap()
                    .as_ref()
                    .map(|s| s.have_count() > 0)
                    .unwrap_or(false);
                if have_any {
                    if let Some(p) = self.peers.get_mut(&conn_id) {
                        if p.am_choking {
                            p.am_choking = false;
                            let _ = p.write_tx.send(WireMsg::Unchoke);
                        }
                    }
                }
            }
            WireMsg::Have(idx) => {
                let num_pieces = self
                    .shared
                    .meta_slot
                    .read()
                    .unwrap()
                    .as_ref()
                    .map(|m| m.num_pieces());
                if let Some(p) = self.peers.get_mut(&conn_id) {
                    if let Some(np) = num_pieces {
                        if (idx as usize) < np {
                            p.peer_bitfield[idx as usize] = true;
                            p.is_seeder = p.peer_bitfield.iter().all(|&b| b);
                        }
                    } else if p.pending_haves.len() < 4096 {
                        p.pending_haves.push(idx);
                    }
                }
                self.schedule_pieces();
            }
            WireMsg::Bitfield(bf) => {
                let num_pieces = self
                    .shared
                    .meta_slot
                    .read()
                    .unwrap()
                    .as_ref()
                    .map(|m| m.num_pieces());
                if let Some(p) = self.peers.get_mut(&conn_id) {
                    if let Some(np) = num_pieces {
                        apply_bitfield_slice(&mut p.peer_bitfield, &bf, np);
                        p.is_seeder = !p.peer_bitfield.is_empty() && p.peer_bitfield.iter().all(|&b| b);
                    } else {
                        p.pending_bitfield = Some(bf);
                    }
                }
                self.schedule_pieces();
            }
            WireMsg::HaveAll => {
                let num_pieces = self
                    .shared
                    .meta_slot
                    .read()
                    .unwrap()
                    .as_ref()
                    .map(|m| m.num_pieces());
                if let Some(p) = self.peers.get_mut(&conn_id) {
                    if num_pieces.is_some() {
                        p.peer_bitfield.fill(true);
                        p.is_seeder = !p.peer_bitfield.is_empty();
                    } else {
                        p.pending_have_all = true;
                    }
                }
                self.schedule_pieces();
            }
            WireMsg::HaveNone => {
                if let Some(p) = self.peers.get_mut(&conn_id) {
                    p.peer_bitfield.fill(false);
                    p.pending_have_all = false;
                    p.pending_bitfield = None;
                    p.pending_haves.clear();
                    p.is_seeder = false;
                }
            }
            WireMsg::AllowedFast(piece) => {
                if let Some(p) = self.peers.get_mut(&conn_id) {
                    if p.allowed_fast.len() < 64 {
                        p.allowed_fast.insert(piece);
                    }
                }
                self.schedule_pieces();
            }
            WireMsg::SuggestPiece(piece) => {
                if let Some(p) = self.peers.get_mut(&conn_id) {
                    if p.suggested.len() < 32 && !p.suggested.contains(&piece) {
                        p.suggested.push(piece);
                    }
                }
                self.schedule_pieces();
            }
            WireMsg::RejectRequest { index, begin, .. } => {
                if let Some(p) = self.peers.get_mut(&conn_id) {
                    p.in_flight.remove(&(index, begin));
                }
                let cidx = (begin / CHUNK_SIZE) as usize;
                if let Some(pb) = self.active_pieces.get_mut(&(index as usize)) {
                    if let Some(list) = pb.chunk_inflight.get_mut(cidx) {
                        list.retain(|(cid, _)| *cid != conn_id);
                    }
                }
                self.schedule_pieces();
            }
            WireMsg::Piece { index, begin, data } => {
                self.handle_piece_chunk(conn_id, index, begin, data);
            }
            WireMsg::Request {
                index,
                begin,
                length,
            } => {
                self.handle_peer_request(conn_id, index, begin, length);
            }
            WireMsg::Extended { ext_id, payload } => {
                self.handle_extended_msg(conn_id, ext_id, payload);
            }
            WireMsg::Cancel { .. } | WireMsg::Port(_) => {}
        }
    }

    fn handle_peer_request(&mut self, conn_id: u64, index: u32, begin: u32, length: u32) {
        let Some(p) = self.peers.get(&conn_id) else {
            return;
        };
        if p.am_choking || length == 0 || length > 131_072 {
            return;
        }
        let Some(st) = self.shared.storage.read().unwrap().clone() else {
            return;
        };
        if !st.have_piece(index as usize) {
            return;
        }
        let write_tx = p.write_tx.clone();
        tokio::spawn(async move {
            if let Ok(data) = st
                .read_piece_slice(index as usize, begin as usize, length as usize)
                .await
            {
                let _ = write_tx.send(WireMsg::Piece { index, begin, data });
            }
        });
    }

    fn handle_extended_msg(&mut self, conn_id: u64, ext_id: u8, payload: Bytes) {
        match ext_id {
            MY_EXT_UT_METADATA => {
                let Ok(um) = wire::parse_ut_metadata(&payload) else {
                    return;
                };
                match um {
                    UtMetadataMsg::Request { piece } => {
                        let meta_opt = self.shared.meta_slot.read().unwrap().clone();
                        let Some(meta) = meta_opt else {
                            return;
                        };
                        let start = (piece as usize).saturating_mul(META_PIECE_SIZE);
                        if start >= meta.info_bytes.len() {
                            return;
                        }
                        let end = (start + META_PIECE_SIZE).min(meta.info_bytes.len());
                        let slice = meta.info_bytes.slice(start..end);
                        if let Some(p) = self.peers.get(&conn_id) {
                            if let Some(remote_id) = p.ut_metadata_id {
                                let msg = wire::build_ut_metadata_data(
                                    remote_id,
                                    piece,
                                    meta.info_bytes.len(),
                                    &slice,
                                );
                                let _ = p.write_tx.send(msg);
                            }
                        }
                    }
                    UtMetadataMsg::Data {
                        piece,
                        total_size,
                        bytes,
                    } => {
                        if let Some(p) = self.peers.get_mut(&conn_id) {
                            p.meta_in_flight.remove(&piece);
                        }
                        if self.shared.meta_slot.read().unwrap().is_some() {
                            return;
                        }
                        if self.meta_asm.is_none() {
                            if let Some(sz) = total_size {
                                if sz > 0 && sz <= MAX_META_SIZE {
                                    self.meta_asm = Some(MetaAssembler::new(sz));
                                }
                            }
                        }
                        let mut completed_bytes: Option<Bytes> = None;
                        if let Some(asm) = &mut self.meta_asm {
                            let idx = piece as usize;
                            if idx < asm.num_pieces && asm.pieces[idx].is_none() {
                                let expected_len = if idx + 1 == asm.num_pieces {
                                    asm.total_size - idx * META_PIECE_SIZE
                                } else {
                                    META_PIECE_SIZE
                                };
                                if bytes.len() == expected_len {
                                    asm.pieces[idx] = Some(bytes);
                                    asm.inflight[idx].clear();
                                    if asm.complete() && !asm.verifying {
                                        asm.verifying = true;
                                        completed_bytes = Some(asm.assemble());
                                    }
                                }
                            }
                        }
                        if let Some(raw) = completed_bytes {
                            if !self.try_apply_info_bytes(raw) {
                                // Reset corrupted metadata assembly cleanly so other peers can fulfill.
                                self.meta_asm = None;
                            }
                        } else {
                            self.schedule_metadata();
                        }
                    }
                    UtMetadataMsg::Reject { piece } => {
                        if let Some(p) = self.peers.get_mut(&conn_id) {
                            p.meta_in_flight.remove(&piece);
                        }
                        if let Some(asm) = &mut self.meta_asm {
                            if let Some(list) = asm.inflight.get_mut(piece as usize) {
                                list.retain(|(cid, _)| *cid != conn_id);
                            }
                        }
                        self.schedule_metadata();
                    }
                }
            }
            wire::MY_EXT_UT_PEX => {
                for addr in wire::parse_ut_pex_addrs(&payload) {
                    self.enqueue_peer(addr);
                }
                if !self.paused {
                    self.dial_peers();
                }
            }
            _ => {}
        }
    }

    fn try_apply_info_bytes(&mut self, raw: Bytes) -> bool {
        if self.shared.meta_slot.read().unwrap().is_some() {
            return true;
        }
        let Ok(meta) = TorrentMeta::from_info_bytes(raw, Some(self.shared.info_hash)) else {
            return false;
        };
        let meta = Arc::new(meta);
        let Ok(st) = TorrentStorage::open(
            self.shared.save_dir.clone(),
            meta.clone(),
            self.deps.readahead.clone(),
        ) else {
            return false;
        };
        for &fidx in &self.selected_files {
            let _ = st.select_file(fidx);
        }
        *self.shared.storage.write().unwrap() = Some(st.clone());
        *self.shared.meta_slot.write().unwrap() = Some(meta.clone());
        self.meta_asm = None;

        for t in &meta.trackers {
            if !self.trackers.contains(t) {
                self.trackers.push(t.clone());
            }
        }

        // Migrate every already-connected socket's bitfield/haves WITHOUT dropping connections.
        let num_pieces = meta.num_pieces();
        for peer in self.peers.values_mut() {
            peer.meta_in_flight.clear();
            peer.peer_bitfield = vec![false; num_pieces];
            if peer.pending_have_all {
                peer.peer_bitfield.fill(true);
            } else if let Some(raw_bf) = peer.pending_bitfield.take() {
                apply_bitfield_slice(&mut peer.peer_bitfield, &raw_bf, num_pieces);
            }
            for idx in peer.pending_haves.drain(..) {
                if (idx as usize) < num_pieces {
                    peer.peer_bitfield[idx as usize] = true;
                }
            }
            peer.pending_have_all = false;
            peer.is_seeder = !peer.peer_bitfield.is_empty() && peer.peer_bitfield.iter().all(|&b| b);
            send_our_bitfield(peer, &st);
        }

        self.shared.meta_notify.notify_waiters();
        self.refresh_stats();
        self.schedule_all();
        true
    }

    fn handle_piece_chunk(&mut self, conn_id: u64, piece: u32, begin: u32, data: Bytes) {
        let Some(meta) = self.shared.meta_slot.read().unwrap().clone() else {
            return;
        };
        let p_idx = piece as usize;
        if p_idx >= meta.num_pieces() || !begin.is_multiple_of(CHUNK_SIZE) {
            return;
        }
        let c_idx = (begin / CHUNK_SIZE) as usize;
        let expected_len = meta.chunk_len(p_idx, c_idx) as usize;
        if expected_len == 0 || data.len() != expected_len {
            return;
        }

        let got_len = data.len() as u64;
        self.total_session_downloaded
            .fetch_add(got_len, Ordering::Relaxed);
        if let Some(p) = self.peers.get_mut(&conn_id) {
            p.in_flight.remove(&(piece, begin));
            p.bytes_this_tick = p.bytes_this_tick.saturating_add(got_len);
        }

        let mut ready_to_verify: Option<Bytes> = None;
        let mut cancel_peers: Vec<u64> = Vec::new();
        if let Some(pb) = self.active_pieces.get_mut(&p_idx) {
            if c_idx < pb.chunks.len() && pb.chunks[c_idx].is_none() {
                pb.chunks[c_idx] = Some(data);
                if let Some(list) = pb.chunk_inflight.get_mut(c_idx) {
                    for &(cid, _) in list.iter() {
                        if cid != conn_id {
                            cancel_peers.push(cid);
                        }
                    }
                    list.clear();
                }
                if pb.all_received() && !pb.verifying {
                    pb.verifying = true;
                    ready_to_verify = Some(pb.assemble());
                }
            }
        }

        for cid in cancel_peers {
            if let Some(p) = self.peers.get_mut(&cid) {
                p.in_flight.remove(&(piece, begin));
                let _ = p.write_tx.send(WireMsg::Cancel {
                    index: piece,
                    begin,
                    length: expected_len as u32,
                });
            }
        }

        if let Some(full_bytes) = ready_to_verify {
            let expected_hash = meta.pieces[p_idx];
            let vtx = self.verify_tx.clone();
            tokio::task::spawn_blocking(move || {
                let got = Sha1::digest(&full_bytes);
                let res = if got.as_slice() == expected_hash {
                    Ok(full_bytes)
                } else {
                    Err(anyhow::anyhow!("sha1 mismatch for piece {p_idx}"))
                };
                let _ = vtx.send((p_idx, res));
            });
        }

        self.schedule_pieces();
    }

    fn handle_verified_piece(&mut self, piece: usize, res: Result<Bytes>) {
        self.active_pieces.remove(&piece);
        match res {
            Ok(bytes) => {
                if let Some(st) = self.shared.storage.read().unwrap().clone() {
                    let cur = self.shared.cursor_piece.load(Ordering::Relaxed);
                    st.mark_verified_and_flush(piece, bytes, cur);
                }
                let have_msg = WireMsg::Have(piece as u32);
                for peer in self.peers.values() {
                    let _ = peer.write_tx.send(have_msg.clone());
                }
            }
            Err(e) => {
                tracing::warn!("piece {piece} failed hash verification: {e}");
            }
        }
        self.refresh_stats();
        self.schedule_pieces();
    }

    fn schedule_all(&mut self) {
        if self.paused {
            return;
        }
        if self.shared.meta_slot.read().unwrap().is_none() {
            self.schedule_metadata();
        } else {
            self.schedule_pieces();
        }
    }

    fn schedule_metadata(&mut self) {
        let Some(asm) = &mut self.meta_asm else {
            return;
        };
        let now = Instant::now();
        let peer_ids: Vec<u64> = self
            .peers
            .iter()
            .filter(|(_, p)| p.ut_metadata_id.is_some() && p.meta_in_flight.len() < 4)
            .map(|(id, _)| *id)
            .collect();
        if peer_ids.is_empty() {
            return;
        }

        let mut rr = 0usize;
        for mp in 0..asm.num_pieces {
            if asm.pieces[mp].is_some() {
                continue;
            }
            let inflight = &mut asm.inflight[mp];
            let can_hedge = inflight.is_empty()
                || (inflight.len() < 2
                    && inflight
                        .iter()
                        .all(|(_, sent)| now.duration_since(*sent) >= META_HEDGE_DELAY));
            if !can_hedge {
                continue;
            }
            let mut assigned = false;
            for attempt in 0..peer_ids.len() {
                let cid = peer_ids[(rr + attempt) % peer_ids.len()];
                if inflight.iter().any(|(id, _)| *id == cid) {
                    continue;
                }
                let Some(peer) = self.peers.get_mut(&cid) else {
                    continue;
                };
                if peer.meta_in_flight.len() >= 4 {
                    continue;
                }
                let Some(remote_id) = peer.ut_metadata_id else {
                    continue;
                };
                peer.meta_in_flight.insert(mp as u32, now);
                inflight.push((cid, now));
                let _ = peer
                    .write_tx
                    .send(wire::build_ut_metadata_request(remote_id, mp as u32));
                rr = (rr + attempt + 1) % peer_ids.len();
                assigned = true;
                break;
            }
            if !assigned {
                break;
            }
        }
    }

    fn cancel_piece_inflight(&mut self, meta: &TorrentMeta, p_idx: usize, pb: PieceBuf) {
        for (c_idx, list) in pb.chunk_inflight.into_iter().enumerate() {
            if pb.chunks[c_idx].is_some() || list.is_empty() {
                continue;
            }
            let begin = (c_idx as u32) * CHUNK_SIZE;
            let length = meta.chunk_len(p_idx, c_idx);
            for (cid, _) in list {
                if let Some(peer) = self.peers.get_mut(&cid) {
                    if peer.in_flight.remove(&(p_idx as u32, begin)).is_some() {
                        let _ = peer.write_tx.send(WireMsg::Cancel {
                            index: p_idx as u32,
                            begin,
                            length,
                        });
                    }
                }
            }
        }
    }

    fn prune_out_of_selection_pieces(&mut self) {
        let Some(meta) = self.shared.meta_slot.read().unwrap().clone() else {
            return;
        };
        let selection = self.build_piece_selection(&meta);
        if selection.ranges.is_empty() {
            return;
        }
        let stale_keys: Vec<usize> = self
            .active_pieces
            .iter()
            .filter(|(p_idx, pb)| {
                !pb.verifying && !selection.ranges.iter().any(|r| r.contains(p_idx))
            })
            .map(|(&p_idx, _)| p_idx)
            .collect();
        for p_idx in stale_keys {
            if let Some(pb) = self.active_pieces.remove(&p_idx) {
                self.cancel_piece_inflight(&meta, p_idx, pb);
            }
        }
    }

    fn ensure_active_piece_slot(
        &mut self,
        meta: &TorrentMeta,
        p_idx: usize,
        priority: &[usize],
        imminent: &HashSet<usize>,
    ) -> bool {
        if self.active_pieces.contains_key(&p_idx) {
            return true;
        }
        if self.active_pieces.len() < MAX_ACTIVE_PIECES {
            self.active_pieces.insert(p_idx, PieceBuf::new(meta, p_idx));
            return true;
        }
        let target_pos = priority.iter().position(|&p| p == p_idx).unwrap_or(usize::MAX);
        let evict_candidate = self
            .active_pieces
            .iter()
            .filter(|(k, pb)| !pb.verifying && !imminent.contains(k))
            .filter_map(|(&k, pb)| {
                let completed = pb.chunks.iter().filter(|c| c.is_some()).count();
                match priority.iter().position(|&p| p == k) {
                    None => Some((usize::MAX, std::cmp::Reverse(completed), k)),
                    Some(epos) if epos > target_pos => {
                        Some((epos, std::cmp::Reverse(completed), k))
                    }
                    _ => None,
                }
            })
            .max_by_key(|&(pos_key, rev_completed, _)| (pos_key, rev_completed))
            .map(|(_, _, k)| k);
        if let Some(evict_idx) = evict_candidate {
            if let Some(pb) = self.active_pieces.remove(&evict_idx) {
                self.cancel_piece_inflight(meta, evict_idx, pb);
            }
            self.active_pieces.insert(p_idx, PieceBuf::new(meta, p_idx));
            return true;
        }
        false
    }

    fn schedule_pieces(&mut self) {
        if self.paused {
            return;
        }
        let Some(meta) = self.shared.meta_slot.read().unwrap().clone() else {
            return;
        };
        let Some(storage) = self.shared.storage.read().unwrap().clone() else {
            return;
        };
        let selection = self.build_piece_selection(&meta);
        if selection.ranges.is_empty() {
            return;
        }

        let cursor = self.shared.cursor_piece.load(Ordering::Relaxed);
        let ra_bytes = self.shared.readahead.load(Ordering::Relaxed).max(8 << 20) as u64;
        let ahead_pieces = (ra_bytes / meta.piece_length.max(1)).clamp(8, 256) as usize;
        let waited: Vec<usize> = {
            let mut v: Vec<usize> = self
                .shared
                .wait_pieces
                .lock()
                .unwrap()
                .keys()
                .copied()
                .collect();
            v.sort_unstable();
            v
        };
        let priority = build_priority_list(
            &storage,
            &selection,
            cursor,
            &waited,
            ahead_pieces,
            meta.num_pieces(),
        );

        // Update Interested state on all connected peers for needed pieces.
        for peer in self.peers.values_mut() {
            let needs_any = priority
                .iter()
                .any(|&p| peer.peer_bitfield.get(p).copied().unwrap_or(false));
            if needs_any != peer.am_interested {
                peer.am_interested = needs_any;
                let _ = peer.write_tx.send(if needs_any {
                    WireMsg::Interested
                } else {
                    WireMsg::NotInterested
                });
            }
        }

        if priority.is_empty() {
            return;
        }

        let mut urgent: HashSet<usize> = HashSet::with_capacity(waited.len() + 2);
        if priority.len() <= 4 {
            urgent.extend(priority.iter().copied());
        } else {
            urgent.extend(waited.iter().copied());
            urgent.insert(cursor);
        }
        let mut next_tier: HashSet<usize> = HashSet::new();
        let next_p = cursor.saturating_add(1);
        if !urgent.contains(&next_p) {
            next_tier.insert(next_p);
        }
        let mut protected = urgent.clone();
        protected.extend(next_tier.iter().copied());

        // Tiered scheduling: complete & hedge active waited/cursor pieces FIRST so a slow peer
        // never blocks playback while faster peers fetch cursor+1 or future readahead.
        for scope in [
            DispatchScope::Set { set: &urgent, hedge: false },
            DispatchScope::Set { set: &urgent, hedge: true },
            DispatchScope::Set { set: &next_tier, hedge: false },
            DispatchScope::Set { set: &next_tier, hedge: true },
            DispatchScope::Future,
        ] {
            self.dispatch_requests(&meta, &priority, &protected, scope);
        }
    }

    fn dispatch_requests(
        &mut self,
        meta: &TorrentMeta,
        priority: &[usize],
        protected: &HashSet<usize>,
        scope: DispatchScope<'_>,
    ) {
        let now = Instant::now();
        let hedge_pass = matches!(scope, DispatchScope::Set { hedge: true, .. });

        for &p_idx in priority {
            let in_scope = match scope {
                DispatchScope::Set { set, .. } => set.contains(&p_idx),
                DispatchScope::Future => !protected.contains(&p_idx),
            };
            if !in_scope {
                continue;
            }
            if !self.ensure_active_piece_slot(meta, p_idx, priority, protected) {
                break;
            }
            if self.active_pieces[&p_idx].verifying {
                continue;
            }

            let num_chunks = meta.num_chunks(p_idx);
            for c_idx in 0..num_chunks {
                let can_request = {
                    let pb = &self.active_pieces[&p_idx];
                    if pb.chunks[c_idx].is_some() {
                        false
                    } else {
                        let inf = &pb.chunk_inflight[c_idx];
                        if !hedge_pass {
                            inf.is_empty()
                        } else {
                            !inf.is_empty()
                                && inf.len() < 2
                                && inf
                                    .iter()
                                    .all(|(_, sent)| now.duration_since(*sent) >= HEDGE_DELAY)
                        }
                    }
                };
                if !can_request {
                    continue;
                }

                // Pick best available peer for this chunk.
                let excluded: Vec<u64> = self.active_pieces[&p_idx].chunk_inflight[c_idx]
                    .iter()
                    .map(|(cid, _)| *cid)
                    .collect();

                let chosen_cid = self
                    .peers
                    .values()
                    .filter(|p| {
                        !excluded.contains(&p.conn_id)
                            && p.in_flight.len() < p.target_queue
                            && p.peer_bitfield.get(p_idx).copied().unwrap_or(false)
                            && (!p.peer_choking || p.allowed_fast.contains(&(p_idx as u32)))
                    })
                    .max_by_key(|p| {
                        let boost = if p.suggested.contains(&(p_idx as u32)) {
                            1u64 << 40
                        } else {
                            0
                        };
                        let headroom = (p.target_queue - p.in_flight.len()) as u64;
                        boost + (p.ewma_bps << 8) + headroom
                    })
                    .map(|p| p.conn_id);

                let Some(cid) = chosen_cid else {
                    continue;
                };

                let begin = (c_idx as u32) * CHUNK_SIZE;
                let length = meta.chunk_len(p_idx, c_idx);
                if let Some(peer) = self.peers.get_mut(&cid) {
                    peer.in_flight.insert((p_idx as u32, begin), now);
                    let _ = peer.write_tx.send(WireMsg::Request {
                        index: p_idx as u32,
                        begin,
                        length,
                    });
                }
                self.active_pieces
                    .get_mut(&p_idx)
                    .unwrap()
                    .chunk_inflight[c_idx]
                    .push((cid, now));
            }
        }
    }

    fn build_piece_selection(&self, meta: &TorrentMeta) -> PieceSelectionRanges {
        if self.selected_files.is_empty() {
            let last = meta.num_pieces().saturating_sub(1);
            return PieceSelectionRanges {
                ranges: if meta.num_pieces() == 0 {
                    Vec::new()
                } else {
                    std::iter::once(0..=last).collect()
                },
                tails: if meta.num_pieces() > 1 { vec![last] } else { Vec::new() },
            };
        }
        let mut ranges = Vec::new();
        let mut tails = Vec::new();
        for &fidx in &self.selected_files {
            if let Some(entry) = meta.file_infos.get(fidx) {
                if entry.len > 0 {
                    if let Some(r) = meta.file_range_to_pieces(fidx, 0, entry.len) {
                        tails.push(*r.end());
                        ranges.push(r);
                    }
                }
            }
        }
        PieceSelectionRanges { ranges, tails }
    }

    fn on_tick(&mut self) {
        if self.paused {
            self.update_stats(0);
            return;
        }

        let now = Instant::now();
        let prev_bps = self.shared.stats.read().unwrap().rate_bps;
        let smoothed_bps = {
            let elapsed = now.duration_since(self.last_rate_tick);
            if elapsed >= Duration::from_secs(1) {
                let cur_total = self.total_session_downloaded.load(Ordering::Relaxed);
                let delta = cur_total.saturating_sub(self.last_rate_bytes);
                let inst_bps = delta.saturating_mul(1000) / (elapsed.as_millis().max(1) as u64);
                self.last_rate_bytes = cur_total;
                self.last_rate_tick = now;
                if prev_bps == 0 {
                    inst_bps
                } else {
                    (prev_bps * 3 + inst_bps) / 4
                }
            } else {
                prev_bps
            }
        };

        // Update peer EWMA speed, adaptive pipeline queue, and collect timed-out requests.
        let mut drop_ids = Vec::new();
        let mut timed_out_chunks: Vec<(u64, u32, u32)> = Vec::new();
        let has_waiting_peers = !self.peer_queue.is_empty() || !self.peer_queue_v6.is_empty();
        let active_peers_len = self.peers.len();

        for peer in self.peers.values_mut() {
            // 200ms tick -> multiply by 5 for instantaneous bytes/sec estimate.
            let pbps = peer.bytes_this_tick.saturating_mul(5);
            peer.bytes_this_tick = 0;
            if pbps > 0 && peer.ewma_bps == 0 {
                peer.ewma_bps = pbps;
            } else {
                peer.ewma_bps = (peer.ewma_bps * 3 + pbps) / 4;
            }
            let adaptive = (((peer.ewma_bps as f64) * 0.6) / (CHUNK_SIZE as f64)).round() as usize;
            let min_q = INITIAL_QUEUE_TARGET.min(peer.peer_reqq);
            peer.target_queue = adaptive.clamp(min_q, peer.peer_reqq);

            for (&(p, b), &sent) in &peer.in_flight {
                if now.duration_since(sent) > STALE_REQUEST_TIMEOUT {
                    timed_out_chunks.push((peer.conn_id, p, b));
                }
            }

            peer.meta_in_flight
                .retain(|_, sent| now.duration_since(*sent) <= STALE_REQUEST_TIMEOUT);

            // Churn dead/permanently-choked peers if spare candidates await dial slots.
            if has_waiting_peers && active_peers_len >= MAX_ACTIVE_PEERS / 2 {
                let stalled_choked = peer.peer_choking
                    && peer.allowed_fast.is_empty()
                    && peer.am_interested
                    && now.duration_since(peer.choked_since) > CHOKE_CHURN_AFTER;
                let silent_long = now.duration_since(peer.connected_at) > CHOKE_CHURN_AFTER
                    && !peer.ext_handshake_received
                    && peer.ewma_bps == 0;
                if stalled_choked || silent_long {
                    drop_ids.push(peer.conn_id);
                }
            }
        }

        for (cid, piece, begin) in timed_out_chunks {
            if let Some(p) = self.peers.get_mut(&cid) {
                p.in_flight.remove(&(piece, begin));
                p.target_queue = (p.target_queue / 2).max(16.min(p.peer_reqq));
            }
            let cidx = (begin / CHUNK_SIZE) as usize;
            if let Some(pb) = self.active_pieces.get_mut(&(piece as usize)) {
                if let Some(list) = pb.chunk_inflight.get_mut(cidx) {
                    list.retain(|(id, _)| *id != cid);
                }
            }
        }

        for cid in drop_ids {
            self.drop_peer(cid);
        }

        self.update_stats(smoothed_bps);
        self.schedule_all();
        self.dial_peers();

        // Re-announce trackers/DHT if peer pool stays thin.
        let need_more_peers = self.peers.len() < 12;
        if let Some(last) = self.last_tracker_announce {
            let reannounce_due = if need_more_peers {
                last.elapsed() >= Duration::from_secs(25)
            } else {
                last.elapsed() >= self.tracker_interval
            };
            if reannounce_due {
                self.start_discovery(true);
            }
        }
    }

    fn refresh_stats(&self) {
        let rate_bps = self.shared.stats.read().unwrap().rate_bps;
        self.update_stats(rate_bps);
    }

    fn update_stats(&self, rate_bps: u64) {
        let mut s = self.shared.stats.write().unwrap();
        let connected_seeders = self.peers.values().filter(|p| p.is_seeder).count();
        s.peers = self.peers.len();
        s.seeders = connected_seeders.max(self.tracker_seeders as usize);
        s.rate_bps = rate_bps;
        s.paused = self.paused;
        if let Some(meta) = self.shared.meta_slot.read().unwrap().as_ref() {
            s.name = meta.name.clone();
            if self.selected_files.is_empty() {
                s.total_bytes = meta.total_length;
            } else {
                s.total_bytes = self
                    .selected_files
                    .iter()
                    .filter_map(|&idx| meta.file_infos.get(idx))
                    .map(|f| f.len)
                    .sum();
            }
        }
        if let Some(st) = self.shared.storage.read().unwrap().as_ref() {
            s.downloaded_bytes = st.completed_selected_bytes();
        }
    }

    fn start_discovery(&mut self, tracker_only_if_recent: bool) {
        let first_announce = !self.discovery_started;
        self.discovery_started = true;
        self.last_tracker_announce = Some(Instant::now());
        self.send_tracker_event(if first_announce {
            TrackerEvent::Started
        } else {
            TrackerEvent::None
        });

        let should_dht = match self.last_dht_lookup {
            Some(t) => !tracker_only_if_recent && t.elapsed() >= Duration::from_secs(30),
            None => true,
        };
        if should_dht {
            if let Some(dht) = self.deps.dht.clone() {
                self.last_dht_lookup = Some(Instant::now());
                let ih = self.shared.info_hash;
                let port = self.deps.transport.listen_port();
                let (ptx, mut prx) = mpsc::unbounded_channel();
                let cmd_tx = self.cmd_tx.clone();
                let shut = self.shutdown_rx.clone();
                tokio::spawn(async move {
                    let mut batch = Vec::new();
                    while let Some(addr) = prx.recv().await {
                        batch.push(addr);
                        while let Ok(more) = prx.try_recv() {
                            batch.push(more);
                            if batch.len() >= 16 {
                                break;
                            }
                        }
                        if cmd_tx
                            .send(ActorCmd::AddPeers(std::mem::take(&mut batch)))
                            .is_err()
                        {
                            break;
                        }
                    }
                });
                tokio::spawn(async move {
                    dht.lookup_peers(ih, (port > 0).then_some(port), ptx, shut)
                        .await;
                });
            }
        }
    }

    fn send_tracker_event(&self, event: TrackerEvent) {
        if self.trackers.is_empty() {
            return;
        }
        let downloaded = self
            .shared
            .storage
            .read()
            .unwrap()
            .as_ref()
            .map(|s| s.completed_selected_bytes())
            .unwrap_or(0);
        let total = self
            .shared
            .meta_slot
            .read()
            .unwrap()
            .as_ref()
            .map(|m| m.total_length)
            .unwrap_or(0);
        let req = AnnounceRequest {
            info_hash: self.shared.info_hash,
            peer_id: self.deps.transport.peer_id(),
            port: self.deps.transport.listen_port(),
            downloaded,
            uploaded: 0,
            left: total.saturating_sub(downloaded),
            event,
            key: 0x424C_0001,
        };
        let tracker = self.deps.tracker.clone();
        let urls = self.trackers.clone();
        let (ptx, mut prx) = mpsc::unbounded_channel();
        let cmd_tx = self.cmd_tx.clone();

        tokio::spawn(async move {
            while let Some(addr) = prx.recv().await {
                let mut batch = vec![addr];
                while let Ok(more) = prx.try_recv() {
                    batch.push(more);
                    if batch.len() >= 16 {
                        break;
                    }
                }
                if cmd_tx.send(ActorCmd::AddPeers(batch)).is_err() {
                    break;
                }
            }
        });
        let cmd_tx_summary = self.cmd_tx.clone();
        tokio::spawn(async move {
            let (interval, seeders) = tracker.announce_many(&urls, req, ptx).await;
            let _ = cmd_tx_summary.send(ActorCmd::TrackerSummary { interval, seeders });
        });
    }
}

fn spawn_peer_io(
    conn_id: u64,
    stream: PeerStream,
    remote_hs: Handshake,
    meta_size: Option<usize>,
    evt_tx: mpsc::UnboundedSender<PeerEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> mpsc::UnboundedSender<WireMsg> {
    let (write_tx, mut write_rx) = mpsc::unbounded_channel::<WireMsg>();
    let (rd, mut wr) = tokio::io::split(stream);

    if remote_hs.supports_ext() {
        let _ = write_tx.send(wire::build_ext_handshake(meta_size));
    }

    let evt_tx_r = evt_tx.clone();
    let shut_w = shutdown_rx.clone();

    tokio::spawn(async move {
        let mut scratch = BytesMut::with_capacity(4096);
        let mut batch = Vec::with_capacity(32);
        let mut shut = shut_w;
        loop {
            let first = tokio::select! {
                _ = shut.changed() => break,
                m = write_rx.recv() => match m {
                    Some(m) => m,
                    None => break,
                }
            };
            batch.clear();
            batch.push(first);
            while let Ok(more) = write_rx.try_recv() {
                batch.push(more);
                if batch.len() >= 64 {
                    break;
                }
            }
            if wire::write_batch(&mut wr, &mut scratch, &batch)
                .await
                .is_err()
            {
                break;
            }
        }
    });

    tokio::spawn(async move {
        let mut reader = PeerReader::new(rd);
        loop {
            let msg_res = tokio::select! {
                _ = shutdown_rx.changed() => break,
                res = reader.read_msg() => res,
            };
            let msg = match msg_res {
                Ok(Some(m)) => m,
                Ok(None) | Err(_) => break,
            };
            if let WireMsg::Extended { ext_id: 0, payload } = &msg {
                if let Ok(ext) = wire::parse_ext_handshake(payload) {
                    let _ = evt_tx_r.send(PeerEvent::ExtHandshake { conn_id, ext });
                }
                continue;
            }
            if evt_tx_r.send(PeerEvent::Msg { conn_id, msg }).is_err() {
                break;
            }
        }
        let _ = evt_tx_r.send(PeerEvent::Disconnected { conn_id });
    });

    write_tx
}

fn send_our_bitfield(peer: &mut PeerConn, storage: &TorrentStorage) {
    if peer.sent_bitfield {
        return;
    }
    peer.sent_bitfield = true;
    if peer.supports_fast {
        if storage.is_complete() {
            let _ = peer.write_tx.send(WireMsg::HaveAll);
            return;
        }
        if storage.have_count() == 0 {
            let _ = peer.write_tx.send(WireMsg::HaveNone);
            return;
        }
    }
    let bf = storage.have_bitfield_bytes();
    let _ = peer.write_tx.send(WireMsg::Bitfield(Bytes::from(bf)));
}

fn apply_bitfield_slice(dst: &mut [bool], raw: &[u8], num_pieces: usize) {
    for (i, slot) in dst.iter_mut().enumerate().take(num_pieces) {
        let byte_idx = i / 8;
        if byte_idx >= raw.len() {
            break;
        }
        *slot = (raw[byte_idx] & (1 << (7 - (i % 8)))) != 0;
    }
}

fn build_priority_list(
    storage: &TorrentStorage,
    selection: &PieceSelectionRanges,
    cursor: usize,
    waited: &[usize],
    ahead_pieces: usize,
    num_pieces: usize,
) -> Vec<usize> {
    if num_pieces == 0 || selection.ranges.is_empty() {
        return Vec::new();
    }
    let in_selection = |p: usize| selection.ranges.iter().any(|r| r.contains(&p));
    let mut out = Vec::with_capacity(ahead_pieces + selection.tails.len() + waited.len());
    let mut seen = HashSet::new();

    // 1. Every piece currently awaited by an active HTTP stream reader (supports concurrent seeks/MOOV probes).
    for &p in waited {
        if p < num_pieces && in_selection(p) && !storage.have_piece(p) && seen.insert(p) {
            out.push(p);
        }
    }

    // 2. Imminent streaming pieces at playback cursor (cursor..cursor + 3).
    for p in [cursor, cursor.saturating_add(1), cursor.saturating_add(2)] {
        if p < num_pieces && in_selection(p) && !storage.have_piece(p) && seen.insert(p) {
            out.push(p);
        }
    }

    // 3. Last piece of each selected file right behind immediate startup pieces (MOOV/Cues seek readiness).
    for &t in &selection.tails {
        if t < num_pieces && !storage.have_piece(t) && seen.insert(t) {
            out.push(t);
        }
    }

    // 4. Forward sequential readahead window from cursor.
    let window_end = (cursor.saturating_add(ahead_pieces)).min(num_pieces);
    for p in cursor.min(num_pieces)..window_end {
        if in_selection(p) && !storage.have_piece(p) && seen.insert(p) {
            out.push(p);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bt::bencode::BWriter;
    use crate::bt::transport::TransportConfig;
    use futures::StreamExt;

    async fn make_two_file_torrent(
        dir: &std::path::Path,
    ) -> (Arc<TorrentMeta>, Vec<u8>, Vec<u8>) {
        let f0: Vec<u8> = (0..(40 * 1024)).map(|i| (i % 251) as u8).collect();
        let f1: Vec<u8> = (0..(24 * 1024)).map(|i| ((i * 7) % 251) as u8).collect();
        let mut combined = Vec::new();
        combined.extend_from_slice(&f0);
        combined.extend_from_slice(&f1);
        let piece_len = 32 * 1024usize;
        let mut pieces_cat = Vec::new();
        for ch in combined.chunks(piece_len) {
            pieces_cat.extend_from_slice(&Sha1::digest(ch));
        }

        let mut bw = BWriter::with_capacity(256);
        bw.start_dict();
        bw.push_str("files");
        bw.start_list();
        for (idx, len) in [f0.len(), f1.len()].iter().enumerate() {
            bw.start_dict();
            bw.push_str("length");
            bw.push_int(*len as i64);
            bw.push_str("path");
            bw.start_list();
            bw.push_str(&format!("ep{idx}.bin"));
            bw.end();
            bw.end();
        }
        bw.end();
        bw.push_str("name");
        bw.push_str("pack");
        bw.push_str("piece length");
        bw.push_int(piece_len as i64);
        bw.push_str("pieces");
        bw.push_bytes(&pieces_cat);
        bw.end();

        let meta = Arc::new(TorrentMeta::from_info_bytes(Bytes::from(bw.finish()), None).unwrap());
        let readahead = Arc::new(AtomicI64::new(4 << 20));
        let st = TorrentStorage::open(dir.to_path_buf(), meta.clone(), readahead).unwrap();
        for p in 0..meta.num_pieces() {
            let start = p * piece_len;
            let end = (start + piece_len).min(combined.len());
            st.mark_verified_and_flush(p, Bytes::copy_from_slice(&combined[start..end]), 0);
        }
        st.flush_pending().await;
        (meta, f0, f1)
    }

    #[tokio::test]
    async fn test_magnet_ut_metadata_and_selected_file_streaming() {
        let base =
            std::env::temp_dir().join(format!("bitlab_tor_test_{}", rand::random::<u32>()));
        let seed_dir = base.join("seed");
        let leech_dir = base.join("leech");
        let _ = std::fs::create_dir_all(&seed_dir);
        let _ = std::fs::create_dir_all(&leech_dir);

        let (meta, _f0, f1) = make_two_file_torrent(&seed_dir).await;

        let readahead = Arc::new(AtomicI64::new(8 << 20));
        let tracker_s = TrackerClient::new(reqwest::Client::new(), true)
            .await
            .unwrap();
        let tr_s = TransportManager::start(
            TransportConfig {
                listen_port: 0,
                bind_v4_only: true,
                enable_utp: false,
                allow_private: true,
                ..Default::default()
            },
            *b"-BL0001-SEEDER000001",
        )
        .await
        .unwrap();
        let deps_s = Arc::new(TorrentDeps {
            dht: None,
            tracker: tracker_s,
            transport: tr_s.clone(),
            readahead: readahead.clone(),
            allow_private: true,
        });

        let seeder = TorrentHandle::spawn(
            meta.info_hash,
            None,
            seed_dir.clone(),
            Vec::new(),
            Vec::new(),
            Some(meta.clone()),
            false,
            deps_s,
        );
        if let Some(st) = seeder.storage_for_seed() {
            st.flush_pending().await;
        }

        let tracker_l = TrackerClient::new(reqwest::Client::new(), true)
            .await
            .unwrap();
        let tr_l = TransportManager::start(
            TransportConfig {
                listen_port: 0,
                bind_v4_only: true,
                enable_utp: false,
                allow_private: true,
                ..Default::default()
            },
            *b"-BL0001-LEECHER00002",
        )
        .await
        .unwrap();
        let deps_l = Arc::new(TorrentDeps {
            dht: None,
            tracker: tracker_l,
            transport: tr_l,
            readahead,
            allow_private: true,
        });

        // Leecher starts purely with info_hash + peer hint (NO metadata yet, testing ut_metadata + zero-drop!).
        let leecher = TorrentHandle::spawn(
            meta.info_hash,
            Some("test".into()),
            leech_dir.clone(),
            Vec::new(),
            vec![tr_s.local_addr()],
            None,
            false,
            deps_l,
        );

        let got_meta = leecher.wait_metadata(Duration::from_secs(5)).await.unwrap();
        assert_eq!(got_meta.info_hash, meta.info_hash);

        // Select ONLY file 1 (ep1.bin), stream via BtByteStream, verify exact bytes and NO ep0.bin created!
        let mut stream = leecher.open_stream(1, 0, f1.len() as u64).unwrap();
        let mut received = Vec::new();
        while let Some(chunk) = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap()
        {
            received.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(received, f1);
        if let Some(st) = leecher.storage_for_seed() {
            st.flush_pending().await;
        }
        assert!(leech_dir.join("pack/ep1.bin").exists());
        assert!(!leech_dir.join("pack/ep0.bin").exists());

        seeder.shutdown();
        leecher.shutdown();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn test_concurrent_waits_seek_eviction_and_imminent_hedge_first() {
        let base =
            std::env::temp_dir().join(format!("bitlab_sched_test_{}", rand::random::<u32>()));
        let _ = std::fs::create_dir_all(&base);

        let piece_len = 32 * 1024usize;
        let num_pieces = 40usize;
        let mut bw = BWriter::with_capacity(2048);
        bw.start_dict();
        bw.push_str("length");
        bw.push_int((piece_len * num_pieces) as i64);
        bw.push_str("name");
        bw.push_str("movie.mkv");
        bw.push_str("piece length");
        bw.push_int(piece_len as i64);
        bw.push_str("pieces");
        bw.push_bytes(&vec![0x11u8; num_pieces * 20]);
        bw.end();

        let meta = Arc::new(TorrentMeta::from_info_bytes(Bytes::from(bw.finish()), None).unwrap());
        let readahead = Arc::new(AtomicI64::new(4 << 20));
        let st = TorrentStorage::open(base.clone(), meta.clone(), readahead.clone()).unwrap();

        let selection = PieceSelectionRanges {
            ranges: std::iter::once(0..=(num_pieces - 1)).collect(),
            tails: vec![num_pieces - 1],
        };
        let priority = build_priority_list(&st, &selection, 0, &[0, 38], 16, num_pieces);
        assert_eq!(priority[0], 0);
        assert_eq!(priority[1], 38);
        assert_eq!(priority[2], 1);
        assert_eq!(priority[3], 2);
        assert_eq!(priority[4], 39);

        let tracker = TrackerClient::new(reqwest::Client::new(), true)
            .await
            .unwrap();
        let transport = TransportManager::start(
            TransportConfig {
                listen_port: 0,
                bind_v4_only: true,
                enable_utp: false,
                allow_private: true,
                ..Default::default()
            },
            *b"-BL0001-SCHEDTEST001",
        )
        .await
        .unwrap();
        let deps = Arc::new(TorrentDeps {
            dht: None,
            tracker,
            transport,
            readahead: readahead.clone(),
            allow_private: true,
        });
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (_inb_tx, inbound_rx) = mpsc::unbounded_channel();
        let (shut_tx, shutdown_rx) = watch::channel(false);
        let shared = Arc::new(TorrentShared {
            info_hash: meta.info_hash,
            save_dir: base.clone(),
            readahead,
            meta_slot: RwLock::new(Some(meta.clone())),
            meta_notify: Notify::new(),
            storage: RwLock::new(Some(st)),
            stats: RwLock::new(TorrentStats::default()),
            cursor_piece: AtomicUsize::new(0),
            wait_pieces: Mutex::new(HashMap::new()),
            active_readers: AtomicUsize::new(0),
            work_notify: Notify::new(),
            shutdown_tx: shut_tx,
        });
        let mut actor = TorrentActor::new(
            shared.clone(),
            deps,
            cmd_tx,
            cmd_rx,
            inbound_rx,
            shutdown_rx,
            Vec::new(),
            Vec::new(),
            false,
        );

        let (wtx1, mut wrx1) = mpsc::unbounded_channel();
        let (wtx2, mut wrx2) = mpsc::unbounded_channel();
        actor.register_connected_peer(1, "127.0.0.1:41001".parse().unwrap(), false, wtx1);
        actor.register_connected_peer(2, "127.0.0.1:41002".parse().unwrap(), false, wtx2);
        while wrx1.try_recv().is_ok() {}
        while wrx2.try_recv().is_ok() {}

        for cid in [1u64, 2] {
            let p = actor.peers.get_mut(&cid).unwrap();
            p.peer_choking = false;
            p.peer_bitfield.fill(true);
            p.target_queue = 2;
        }

        // Saturate active_pieces at MAX_ACTIVE_PIECES (32) with old pieces 5..37 on peer 1.
        let stale_ts = Instant::now() - HEDGE_DELAY - Duration::from_millis(20);
        for p in 5..(5 + MAX_ACTIVE_PIECES) {
            let mut pb = PieceBuf::new(&meta, p);
            pb.chunk_inflight[0].push((1, stale_ts));
            actor.active_pieces.insert(p, pb);
            actor
                .peers
                .get_mut(&1)
                .unwrap()
                .in_flight
                .insert((p as u32, 0), stale_ts);
        }

        // Seed piece 0 with 1 slow/stalled chunk on peer 1, chunk 1 already received, while peer 2
        // has only 2 request slots (target_queue = 2).
        let mut p0 = PieceBuf::new(&meta, 0);
        p0.chunk_inflight[0].push((1, stale_ts));
        p0.chunks[1] = Some(Bytes::from(vec![0u8; 16_384]));
        // Make room cleanly via eviction: insert after removing one or test eviction of piece 36->0.
        actor.schedule_pieces();

        // Out-of-priority piece from 5..37 must have been evicted & cancelled, and cursor piece 0 inserted & requested!
        assert!(actor.active_pieces.contains_key(&0));
        assert!(actor.active_pieces.len() <= MAX_ACTIVE_PIECES);

        let mut msgs1 = Vec::new();
        while let Ok(m) = wrx1.try_recv() {
            msgs1.push(m);
        }
        assert!(msgs1.iter().any(|m| matches!(m, WireMsg::Cancel { .. })));

        // Verify peer 2 receives a HEDGED request for stalled chunk (0, 0) BEFORE future pieces fill its queue!
        actor.active_pieces.insert(0, p0);
        actor.peers.get_mut(&2).unwrap().in_flight.clear();
        actor.peers.get_mut(&2).unwrap().target_queue = 1;
        while wrx2.try_recv().is_ok() {}

        actor.schedule_pieces();

        let mut msgs2 = Vec::new();
        while let Ok(m) = wrx2.try_recv() {
            msgs2.push(m);
        }
        assert!(
            msgs2
                .iter()
                .any(|m| matches!(m, WireMsg::Request { index: 0, begin: 0, .. })),
            "expected hedged imminent request for piece 0 chunk 0 ahead of future pieces, got {msgs2:?}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
