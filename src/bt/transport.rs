use anyhow::{bail, Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, RwLock, Weak};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use super::metainfo::Id20;
use super::wire::{self, Handshake};

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_millis(3500);
const DEFAULT_UTP_STAGGER: Duration = Duration::from_millis(120);
const INBOUND_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_CONCURRENT_INBOUND_HANDSHAKES: usize = 64;

pub enum PeerStream {
    Tcp(TcpStream),
    Utp(librqbit_utp::UtpStream),
}

impl PeerStream {
    pub fn set_nodelay(&self) {
        if let PeerStream::Tcp(s) = self {
            let _ = s.set_nodelay(true);
        }
    }

    pub fn transport_name(&self) -> &'static str {
        match self {
            PeerStream::Tcp(_) => "tcp",
            PeerStream::Utp(_) => "utp",
        }
    }
}

impl AsyncRead for PeerStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            PeerStream::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            PeerStream::Utp(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for PeerStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            PeerStream::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            PeerStream::Utp(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            PeerStream::Tcp(s) => Pin::new(s).poll_flush(cx),
            PeerStream::Utp(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            PeerStream::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            PeerStream::Utp(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

pub struct InboundPeer {
    pub stream: PeerStream,
    pub addr: SocketAddr,
    pub handshake: Handshake,
}

#[derive(Clone, Debug)]
pub struct TransportConfig {
    pub listen_port: u16,
    pub bind_v4_only: bool,
    pub enable_utp: bool,
    pub allow_private: bool,
    pub connect_timeout: Duration,
    pub utp_stagger: Duration,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            listen_port: 0,
            bind_v4_only: false,
            enable_utp: true,
            allow_private: false,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            utp_stagger: DEFAULT_UTP_STAGGER,
        }
    }
}

struct TransportInner {
    cfg: TransportConfig,
    peer_id: [u8; 20],
    listen_port: u16,
    local_addr: SocketAddr,
    tcp_listener: Arc<TcpListener>,
    utp_sock: Option<Arc<librqbit_utp::UtpSocketUdp>>,
    utp_cancel: CancellationToken,
    inbound_sem: Arc<Semaphore>,
    torrents: RwLock<HashMap<Id20, mpsc::UnboundedSender<InboundPeer>>>,
    shutdown: Notify,
}

pub struct TransportManager {
    inner: Arc<TransportInner>,
}

impl TransportManager {
    pub async fn start(cfg: TransportConfig, peer_id: [u8; 20]) -> Result<Arc<Self>> {
        let tcp_listener = Arc::new(bind_tcp_listener(cfg.listen_port, cfg.bind_v4_only)?);
        let local_addr = tcp_listener.local_addr()?;
        let actual_port = local_addr.port();

        let utp_cancel = CancellationToken::new();
        let utp_sock = if cfg.enable_utp {
            let utp_bind = if cfg.bind_v4_only {
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), actual_port)
            } else {
                SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), actual_port)
            };
            let opts = librqbit_utp::SocketOpts {
                disable_nagle: true,
                dont_wait_for_lastack: true,
                mtu_probe_max_retransmissions: Some(0),
                remote_inactivity_timeout: Some(Duration::from_secs(30)),
                cancellation_token: utp_cancel.clone(),
                ..Default::default()
            };
            match librqbit_utp::UtpSocketUdp::new_udp_with_opts(utp_bind, opts, Default::default())
                .await
            {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!("utp bind on {utp_bind} failed (tcp-only): {e}");
                    None
                }
            }
        } else {
            None
        };

        let inner = Arc::new(TransportInner {
            cfg,
            peer_id,
            listen_port: actual_port,
            local_addr,
            tcp_listener,
            utp_sock,
            utp_cancel,
            inbound_sem: Arc::new(Semaphore::new(MAX_CONCURRENT_INBOUND_HANDSHAKES)),
            torrents: RwLock::new(HashMap::new()),
            shutdown: Notify::new(),
        });

        let weak_tcp = Arc::downgrade(&inner);
        tokio::spawn(async move {
            tcp_accept_loop(weak_tcp).await;
        });

        if inner.utp_sock.is_some() {
            let weak_utp = Arc::downgrade(&inner);
            tokio::spawn(async move {
                utp_accept_loop(weak_utp).await;
            });
        }

        Ok(Arc::new(Self { inner }))
    }

    pub fn listen_port(&self) -> u16 {
        self.inner.listen_port
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr
    }

    pub fn peer_id(&self) -> [u8; 20] {
        self.inner.peer_id
    }

    pub fn register_torrent(&self, info_hash: Id20, tx: mpsc::UnboundedSender<InboundPeer>) {
        self.inner.torrents.write().unwrap().insert(info_hash, tx);
    }

    pub fn unregister_torrent(&self, info_hash: &Id20) {
        self.inner.torrents.write().unwrap().remove(info_hash);
    }

    /// Connects using Happy-Eyeballs TCP + staggered uTP (falling back cleanly between them),
    /// then completes the BitTorrent handshake.
    pub async fn connect_peer(
        &self,
        addr: SocketAddr,
        info_hash: Id20,
    ) -> Result<(PeerStream, Handshake)> {
        if addr.port() == 0 {
            bail!("invalid peer port 0");
        }
        if !self.inner.cfg.allow_private && crate::netguard::is_private_address_ip(&addr.ip()) {
            bail!("refused private peer address {addr}");
        }

        let stream = self.race_connect(addr).await?;
        stream.set_nodelay();
        self.client_handshake(stream, addr, info_hash).await
    }

    async fn race_connect(&self, addr: SocketAddr) -> Result<PeerStream> {
        let timeout = self.inner.cfg.connect_timeout;
        let Some(utp) = self.inner.utp_sock.clone() else {
            let tcp = tokio::time::timeout(timeout, TcpStream::connect(addr))
                .await
                .context("tcp connect timeout")?
                .context("tcp connect failed")?;
            return Ok(PeerStream::Tcp(tcp));
        };

        let stagger = self.inner.cfg.utp_stagger;
        let tcp_failed = Arc::new(Notify::new());
        let tcp_failed_c = tcp_failed.clone();

        let tcp_fut = async move {
            match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
                Ok(Ok(s)) => Ok(PeerStream::Tcp(s)),
                Ok(Err(e)) => {
                    tcp_failed_c.notify_one();
                    Err(anyhow::anyhow!("tcp connect error: {e}"))
                }
                Err(_) => {
                    tcp_failed_c.notify_one();
                    Err(anyhow::anyhow!("tcp connect timeout"))
                }
            }
        };

        let utp_fut = async move {
            tokio::select! {
                _ = tokio::time::sleep(stagger) => {}
                _ = tcp_failed.notified() => {}
            }
            let s = tokio::time::timeout(timeout, utp.connect(addr))
                .await
                .context("utp connect timeout")?
                .context("utp connect failed")?;
            Ok::<PeerStream, anyhow::Error>(PeerStream::Utp(s))
        };

        tokio::pin!(tcp_fut);
        tokio::pin!(utp_fut);

        tokio::select! {
            res = &mut tcp_fut => {
                match res {
                    Ok(s) => Ok(s),
                    Err(tcp_err) => match utp_fut.await {
                        Ok(s) => Ok(s),
                        Err(utp_err) => bail!("tcp ({tcp_err}) & utp ({utp_err}) failed for {addr}"),
                    }
                }
            }
            res = &mut utp_fut => {
                match res {
                    Ok(s) => Ok(s),
                    Err(utp_err) => match tcp_fut.await {
                        Ok(s) => Ok(s),
                        Err(tcp_err) => bail!("utp ({utp_err}) & tcp ({tcp_err}) failed for {addr}"),
                    }
                }
            }
        }
    }

    async fn client_handshake(
        &self,
        mut stream: PeerStream,
        _addr: SocketAddr,
        info_hash: Id20,
    ) -> Result<(PeerStream, Handshake)> {
        let hs = Handshake::new(info_hash, self.inner.peer_id);
        let timeout = self.inner.cfg.connect_timeout;
        let remote_hs = tokio::time::timeout(timeout, async {
            wire::write_handshake(&mut stream, &hs).await?;
            wire::read_handshake(&mut stream).await
        })
        .await
        .context("handshake timeout")??;

        if remote_hs.info_hash != info_hash {
            bail!("peer info_hash mismatch");
        }
        if remote_hs.peer_id == self.inner.peer_id {
            bail!("connected to self");
        }
        Ok((stream, remote_hs))
    }
}

impl Drop for TransportManager {
    fn drop(&mut self) {
        self.inner.utp_cancel.cancel();
        self.inner.shutdown.notify_waiters();
    }
}

async fn tcp_accept_loop(weak: Weak<TransportInner>) {
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        tokio::select! {
            _ = inner.shutdown.notified() => return,
            res = inner.tcp_listener.accept() => {
                if let Ok((stream, addr)) = res {
                    let s = PeerStream::Tcp(stream);
                    s.set_nodelay();
                    spawn_inbound_handler(Arc::downgrade(&inner), s, addr);
                }
            }
        }
    }
}

async fn utp_accept_loop(weak: Weak<TransportInner>) {
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let Some(utp) = inner.utp_sock.as_ref() else {
            return;
        };
        tokio::select! {
            _ = inner.shutdown.notified() => return,
            res = utp.accept() => {
                match res {
                    Ok(stream) => {
                        let addr = stream.remote_addr();
                        spawn_inbound_handler(Arc::downgrade(&inner), PeerStream::Utp(stream), addr);
                    }
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
        }
    }
}

fn spawn_inbound_handler(weak: Weak<TransportInner>, mut stream: PeerStream, addr: SocketAddr) {
    let Some(inner) = weak.upgrade() else {
        return;
    };
    if !inner.cfg.allow_private && crate::netguard::is_private_address_ip(&addr.ip()) {
        return;
    }
    let Ok(permit) = inner.inbound_sem.clone().try_acquire_owned() else {
        return;
    };
    tokio::spawn(async move {
        let _permit = permit;
        let Ok(Ok(remote_hs)) =
            tokio::time::timeout(INBOUND_HANDSHAKE_TIMEOUT, wire::read_handshake(&mut stream)).await
        else {
            return;
        };
        if remote_hs.peer_id == inner.peer_id {
            return;
        }
        let target_tx = inner.torrents.read().unwrap().get(&remote_hs.info_hash).cloned();
        let Some(tx) = target_tx else {
            return;
        };
        let reply_hs = Handshake::new(remote_hs.info_hash, inner.peer_id);
        if tokio::time::timeout(
            INBOUND_HANDSHAKE_TIMEOUT,
            wire::write_handshake(&mut stream, &reply_hs),
        )
        .await
        .is_err()
        {
            return;
        }
        let _ = tx.send(InboundPeer {
            stream,
            addr,
            handshake: remote_hs,
        });
    });
}

fn bind_tcp_listener(port: u16, bind_v4_only: bool) -> Result<TcpListener> {
    if !bind_v4_only {
        if let Ok(l) = bind_dualstack_tcp(port) {
            return Ok(l);
        }
    }
    let ip = if bind_v4_only {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    };
    let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    let _ = sock.set_reuse_address(true);
    let _ = sock.set_nonblocking(true);
    sock.bind(&SocketAddr::new(ip, port).into())?;
    sock.listen(128)?;
    let std_l: std::net::TcpListener = sock.into();
    Ok(TcpListener::from_std(std_l)?)
}

fn bind_dualstack_tcp(port: u16) -> Result<TcpListener> {
    let sock = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    let _ = sock.set_only_v6(false);
    let _ = sock.set_reuse_address(true);
    let _ = sock.set_nonblocking(true);
    let addr: SocketAddr = ([0u16; 8], port).into();
    sock.bind(&addr.into())?;
    sock.listen(128)?;
    let std_l: std::net::TcpListener = sock.into();
    Ok(TcpListener::from_std(std_l)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_tcp_and_utp_handshake_routing() {
        let ih = Id20::from_hex("1111111111111111111111111111111111111111").unwrap();
        let mgr_srv = TransportManager::start(
            TransportConfig {
                listen_port: 0,
                bind_v4_only: true,
                enable_utp: true,
                allow_private: true,
                connect_timeout: Duration::from_secs(2),
                utp_stagger: Duration::from_millis(80),
            },
            *b"-BL0001-SERVER000001",
        )
        .await
        .unwrap();

        let (in_tx, mut in_rx) = mpsc::unbounded_channel();
        mgr_srv.register_torrent(ih, in_tx);

        let mgr_cli = TransportManager::start(
            TransportConfig {
                listen_port: 0,
                bind_v4_only: true,
                enable_utp: true,
                allow_private: true,
                connect_timeout: Duration::from_secs(2),
                utp_stagger: Duration::from_millis(80),
            },
            *b"-BL0001-CLIENT000002",
        )
        .await
        .unwrap();

        let (stream, hs) = mgr_cli.connect_peer(mgr_srv.local_addr(), ih).await.unwrap();
        assert_eq!(hs.peer_id, *b"-BL0001-SERVER000001");
        assert_eq!(stream.transport_name(), "tcp");

        let inbound = tokio::time::timeout(Duration::from_secs(2), in_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(inbound.handshake.peer_id, *b"-BL0001-CLIENT000002");
        drop(inbound);

        // Verify direct uTP fallback works when TCP port refuses connection:
        // create an acceptor with closed TCP listener simulation by using uTP client connect directly.
        let utp_srv = mgr_srv.inner.utp_sock.as_ref().unwrap().bind_addr();
        let utp_cli = mgr_cli.inner.utp_sock.as_ref().unwrap().clone();
        let mut s = utp_cli.connect(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), utp_srv.port())).await.unwrap();
        let hs_req = Handshake::new(ih, *b"-BL0001-UTPCLIENT003");
        wire::write_handshake(&mut s, &hs_req).await.unwrap();
        let hs_ack = wire::read_handshake(&mut s).await.unwrap();
        assert_eq!(hs_ack.peer_id, *b"-BL0001-SERVER000001");
        let inbound2 = tokio::time::timeout(Duration::from_secs(2), in_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(inbound2.stream.transport_name(), "utp");
    }
}
