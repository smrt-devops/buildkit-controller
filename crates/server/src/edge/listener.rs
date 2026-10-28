//! Accept loop: handshake and connection caps, keepalive,
//! handshake timeout, readiness and drain.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use socket2::{SockRef, TcpKeepalive};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, info, info_span, warn};

use super::handler::{Accepted, ConnectionHandler};
use super::metrics::EdgeMetrics;
use super::tls::{EdgeTls, counted_by_verifier};
use crate::net::ProxyProtocol;
use crate::runtime::readiness::ReadyFlag;

/// Listener limits from `edge.*`.
#[derive(Debug, Clone, Copy)]
pub struct ListenerOptions {
    /// `edge.handshakeTimeout`; also the longest wait for a connection slot
    /// after a successful handshake.
    pub handshake_timeout: Duration,
    /// `edge.maxConnections` (at least 1): established connections. A
    /// connection takes a slot only once its handshake has succeeded.
    pub max_connections: usize,
    /// `edge.maxHandshakes` (at least 1): connections still in the TLS
    /// handshake. More are closed on accept.
    pub max_handshakes: usize,
    /// `edge.maxHandshakesPerIp`: in-handshake connections from one source
    /// address ([`PeerKey`]). More are closed on accept. 0 disables the cap.
    pub max_handshakes_per_ip: usize,
}

impl Default for ListenerOptions {
    fn default() -> Self {
        Self {
            handshake_timeout: Duration::from_secs(10),
            max_connections: 10_000,
            max_handshakes: 1024,
            max_handshakes_per_ip: 32,
        }
    }
}

/// The unit a per-client cap counts: an IPv4 address, or the /64 prefix of
/// an IPv6 address (one host usually owns a whole /64). IPv4-mapped IPv6
/// addresses count as their IPv4 address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerKey(IpAddr);

impl From<IpAddr> for PeerKey {
    fn from(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(_) => Self(ip),
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or_else(
                || {
                    let s = v6.segments();
                    Self(IpAddr::V6(std::net::Ipv6Addr::new(
                        s[0], s[1], s[2], s[3], 0, 0, 0, 0,
                    )))
                },
                |v4| Self(IpAddr::V4(v4)),
            ),
        }
    }
}

/// Open connections per [`PeerKey`], capped at `max` each (0: no cap).
#[derive(Clone, Debug)]
pub struct PerPeer {
    max: usize,
    open: Arc<Mutex<HashMap<PeerKey, usize>>>,
}

impl PerPeer {
    /// A cap of `max` per peer; 0 disables the cap and counts nothing.
    #[must_use]
    pub fn new(max: usize) -> Self {
        Self {
            max,
            open: Arc::default(),
        }
    }

    /// A slot for `ip`, or `None` when its peer already holds `max`.
    #[must_use]
    pub fn try_take(&self, ip: IpAddr) -> Option<PeerSlot> {
        let key = PeerKey::from(ip);
        if self.max == 0 {
            return Some(PeerSlot { key, open: None });
        }
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        let n = open.entry(key).or_insert(0);
        if *n >= self.max {
            return None;
        }
        *n += 1;
        Some(PeerSlot {
            key,
            open: Some(Arc::clone(&self.open)),
        })
    }

    /// Peers holding at least one slot (tests).
    #[must_use]
    pub fn peers(&self) -> usize {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// One connection's share of its peer's cap; released on drop.
#[derive(Debug)]
pub struct PeerSlot {
    key: PeerKey,
    /// `None` when the cap is disabled.
    open: Option<Arc<Mutex<HashMap<PeerKey, usize>>>>,
}

impl Drop for PeerSlot {
    fn drop(&mut self) {
        let Some(open) = &self.open else { return };
        let mut open = open.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(n) = open.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                open.remove(&self.key);
            }
        }
    }
}

/// Everything [`serve`] needs besides the socket.
pub struct Serve {
    /// Current TLS generation; `None` until a serving cert and CA are loaded.
    pub tls: watch::Receiver<Option<Arc<EdgeTls>>>,
    pub handler: Arc<dyn ConnectionHandler>,
    pub metrics: EdgeMetrics,
    pub options: ListenerOptions,
    /// `edge_listener`: set once bound with a loaded cert.
    pub ready: ReadyFlag,
}

/// The slots an in-handshake connection holds; dropped once the handshake ends.
struct Handshaking {
    _global: OwnedSemaphorePermit,
    _peer: PeerSlot,
}

/// Serves `listener` until `shutdown`, then stops accepting (dropping the
/// socket) and waits for every open connection to end on its own: release,
/// revocation, the worker pod going away, or the client cert's `notAfter`. The
/// runtime bounds the wait by the drain budget and aborts the rest by dropping
/// this future.
///
/// An unauthenticated client can only hold handshake slots: at most
/// `max_handshakes` in all and `max_handshakes_per_ip` per peer, each for at
/// most `handshake_timeout`. Established connections draw on the separate
/// `max_connections` budget.
pub async fn serve(listener: TcpListener, s: Serve, shutdown: CancellationToken) {
    serve_with_proxy(listener, s, ProxyProtocol::off(), shutdown).await;
}

/// [`serve`] behind a gateway: a connection from a source `proxy` trusts must
/// open with a PROXY header, whose source address then stands for the client
/// in the per-address cap and the `peer` log field. The header read holds a
/// global handshake slot and is bounded by the header timeout.
pub async fn serve_with_proxy(
    listener: TcpListener,
    s: Serve,
    proxy: ProxyProtocol,
    shutdown: CancellationToken,
) {
    let Serve {
        mut tls,
        handler,
        metrics,
        options,
        ready,
    } = s;
    tokio::select! {
        () = shutdown.cancelled() => return,
        r = tls.wait_for(Option::is_some) => if r.is_err() { return },
    }
    ready.set();
    info!(addr = ?listener.local_addr().ok(), "edge listening");

    let shared = Arc::new(Shared {
        handler,
        metrics,
        options,
        established: Arc::new(Semaphore::new(options.max_connections.max(1))),
        per_peer: PerPeer::new(options.max_handshakes_per_ip),
        proxy,
    });
    let handshakes = Arc::new(Semaphore::new(options.max_handshakes.max(1)));
    let mut conns = JoinSet::new();
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            Some(done) = conns.join_next() => {
                if matches!(&done, Err(e) if e.is_panic()) {
                    warn!("edge connection task panicked");
                }
            }
            (tcp, peer) = accept(&listener) => {
                let Ok(global) = Arc::clone(&handshakes).try_acquire_owned() else {
                    debug!(remote = %peer, "edge handshake limit reached; closing");
                    continue;
                };
                let Some(current) = tls.borrow().clone() else { continue };
                conns.spawn(admit(tcp, peer, current, global, Arc::clone(&shared)));
            }
        }
    }
    drop(listener);
    info!(open = conns.len(), "edge listener closed, draining");
    let mut progress = tokio::time::interval_at(
        tokio::time::Instant::now() + DRAIN_LOG_EVERY,
        DRAIN_LOG_EVERY,
    );
    loop {
        tokio::select! {
            done = conns.join_next() => if done.is_none() { break },
            _ = progress.tick() => info!(open = conns.len(), "edge draining"),
        }
    }
    info!("edge drained");
}

/// How often a draining edge logs its open connection count.
const DRAIN_LOG_EVERY: Duration = Duration::from_mins(1);

/// The next connection; accept errors (e.g. `EMFILE`) are logged and retried
/// after a short pause.
async fn accept(listener: &TcpListener) -> (TcpStream, SocketAddr) {
    loop {
        match listener.accept().await {
            Ok(a) => return a,
            Err(e) => {
                warn!(error = %e, "edge accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// `TCP_NODELAY` and keepalive idle 30 s / interval 10 s / 3 probes.
pub fn tune_socket(tcp: &TcpStream) -> std::io::Result<()> {
    tcp.set_nodelay(true)?;
    let ka = TcpKeepalive::new()
        .with_time(Duration::from_secs(30))
        .with_interval(Duration::from_secs(10))
        .with_retries(3);
    SockRef::from(tcp).set_tcp_keepalive(&ka)
}

/// State every connection task of one listener shares.
struct Shared {
    handler: Arc<dyn ConnectionHandler>,
    metrics: EdgeMetrics,
    options: ListenerOptions,
    /// `max_connections`: established connections.
    established: Arc<Semaphore>,
    /// `max_handshakes_per_ip`, keyed by the client address.
    per_peer: PerPeer,
    proxy: ProxyProtocol,
}

/// Finds the client address (the PROXY source behind a trusted gateway, else
/// the socket's peer), takes that address's handshake slot and runs the
/// connection in a span whose `peer` is the client. Everything after this
/// point sees only the client address; the gateway's is logged as `via`.
async fn admit(
    mut tcp: TcpStream,
    remote: SocketAddr,
    tls: Arc<EdgeTls>,
    global: OwnedSemaphorePermit,
    shared: Arc<Shared>,
) {
    if let Err(e) = tune_socket(&tcp) {
        debug!(error = %e, "socket options");
    }
    let client = match shared.proxy.client(&mut tcp, remote).await {
        Ok(c) => c,
        Err(e) => {
            debug!(via = %remote, reason = e.label(), error = %e, "edge PROXY header refused; closing");
            return;
        }
    };
    let via = client.origin.via();
    if let Some(v) = via {
        debug!(peer = %client.addr, via = %v, "edge PROXY header accepted");
    }
    let Some(slot) = shared.per_peer.try_take(client.addr.ip()) else {
        debug!(peer = %client.addr, via = ?via, "edge per-address handshake limit reached; closing");
        return;
    };
    let hs = Handshaking {
        _global: global,
        _peer: slot,
    };
    let span = info_span!(
        "edge_conn",
        peer = %client.addr,
        via = tracing::field::Empty,
        alloc_id = tracing::field::Empty,
        pod_uid = tracing::field::Empty,
    );
    if let Some(via) = via {
        span.record("via", tracing::field::display(via));
    }
    connection(tcp, client.addr, tls, &shared, hs)
        .instrument(span)
        .await;
}

/// TLS handshake, identity, connection slot, then the handler. `peer` is the
/// client address from [`admit`].
async fn connection(
    tcp: TcpStream,
    peer: SocketAddr,
    tls: Arc<EdgeTls>,
    shared: &Shared,
    handshaking: Handshaking,
) {
    let Shared {
        handler,
        metrics,
        options,
        established,
        ..
    } = shared;
    let start = Instant::now();
    let acceptor = TlsAcceptor::from(Arc::clone(&tls.server));
    let stream = match tokio::time::timeout(options.handshake_timeout, acceptor.accept(tcp)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            if !counted_by_verifier(&e) {
                metrics.outcome(None, "tls_error");
            }
            debug!(error = %e, "edge handshake failed");
            return;
        }
        Err(_) => {
            metrics.outcome(None, "tls_error");
            debug!("edge handshake timed out");
            return;
        }
    };
    metrics.handshake_done(start.elapsed().as_secs_f64());
    let leaf = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(<[_]>::first);
    let Some(Ok(id)) = leaf.map(|l| tls.identify(l)) else {
        metrics.outcome(None, "tls_error");
        return;
    };
    drop(handshaking);
    let Ok(Ok(_slot)) = tokio::time::timeout(
        options.handshake_timeout,
        Arc::clone(established).acquire_owned(),
    )
    .await
    else {
        debug!("edge connection limit reached; closing");
        return;
    };
    let span = tracing::Span::current();
    span.record("alloc_id", id.alloc_id.as_str());
    span.record("pod_uid", id.pod_uid.as_str());
    handler.handle(Accepted { stream, id, peer }).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn per_peer_caps_each_address_and_releases_on_drop() {
        let p = PerPeer::new(2);
        let a = p.try_take(ip("10.0.0.1")).unwrap();
        let _b = p.try_take(ip("10.0.0.1")).unwrap();
        assert!(p.try_take(ip("10.0.0.1")).is_none());
        let _other = p.try_take(ip("10.0.0.2")).unwrap();
        drop(a);
        assert!(p.try_take(ip("10.0.0.1")).is_some());
        assert_eq!(p.peers(), 2);
    }

    #[test]
    fn ipv6_counts_per_slash_64_and_mapped_v4_as_v4() {
        let p = PerPeer::new(1);
        let _a = p.try_take(ip("2001:db8:1:2::1")).unwrap();
        assert!(p.try_take(ip("2001:db8:1:2:ffff::9")).is_none());
        let _b = p.try_take(ip("2001:db8:1:3::1")).unwrap();
        let _c = p.try_take(ip("192.0.2.7")).unwrap();
        assert!(p.try_take(ip("::ffff:192.0.2.7")).is_none());
    }

    #[test]
    fn zero_disables_the_cap() {
        let p = PerPeer::new(0);
        let held: Vec<_> = (0..100)
            .map(|_| p.try_take(ip("10.0.0.1")).unwrap())
            .collect();
        assert_eq!((held.len(), p.peers()), (100, 0));
    }

    /// The edge behind a gateway, over real TLS on 127.0.0.1.
    mod proxied {
        use std::net::SocketAddrV4;

        use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
        use time::OffsetDateTime;
        use tokio::io::AsyncWriteExt;
        use tokio::sync::mpsc;

        use super::super::super::TlsBuilder;
        use super::super::super::handler::{Accepted, ConnectionHandler};
        use super::*;
        use crate::net::{ProxyMetrics, ProxyProtocol};
        use crate::pki::{CaBundle, EdgeNames, LeafKey};
        use crate::runtime::metrics::Metrics;
        use crate::runtime::readiness::Readiness;

        const HOST: &str = "edge.example.test";
        const ID: &str = "buildkit://team-a/pool-1/0f8fad5b-d9cb-469f-a165-70867728950e/abcdefghijklmnopqrstuvwxyz";

        /// Reports each accepted connection's `peer`.
        struct Peers(mpsc::UnboundedSender<SocketAddr>);

        impl ConnectionHandler for Peers {
            fn handle(&self, conn: Accepted) -> futures::future::BoxFuture<'static, ()> {
                let _ = self.0.send(conn.peer);
                Box::pin(async move { drop(conn) })
            }
        }

        struct Rig {
            addr: SocketAddr,
            peers: mpsc::UnboundedReceiver<SocketAddr>,
            ca: CertificateDer<'static>,
            leaf: (CertificateDer<'static>, PrivateKeyDer<'static>),
            metrics: EdgeMetrics,
            stop: CancellationToken,
        }

        async fn start(trusted: &str) -> Rig {
            start_with(trusted, ListenerOptions::default().max_handshakes_per_ip).await
        }

        async fn start_with(trusted: &str, max_handshakes_per_ip: usize) -> Rig {
            let now = OffsetDateTime::now_utc();
            let bundle = CaBundle::generate(now).unwrap();
            let names = EdgeNames::new(HOST, &[], "bkc", &"ops".parse().unwrap()).unwrap();
            let serving = bundle.issue_edge_serving(&names, now).unwrap();
            let provider = rustls::crypto::ring::default_provider();
            let signer = provider
                .key_provider
                .load_private_key(serving.private_key_der().unwrap())
                .unwrap();
            let key = Arc::new(rustls::sign::CertifiedKey::new(
                vec![serving.cert_der().clone()],
                signer,
            ));
            let (tx, peers) = mpsc::unbounded_channel();
            let handler: Arc<dyn ConnectionHandler> = Arc::new(Peers(tx));
            let metrics = EdgeMetrics::register(&Metrics::new());
            let tls = TlsBuilder::new(Arc::clone(&handler), metrics.clone())
                .build(key, bundle.client_ca().cert_der().clone())
                .unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (tls_tx, rx) = watch::channel(Some(Arc::new(tls)));
            std::mem::forget(tls_tx);
            let s = Serve {
                tls: rx,
                handler,
                metrics: metrics.clone(),
                options: ListenerOptions {
                    handshake_timeout: Duration::from_secs(2),
                    max_handshakes_per_ip,
                    ..ListenerOptions::default()
                },
                ready: Readiness::with_spec_checks().register("edge_listener"),
            };
            let proxy = ProxyProtocol::trusting(
                vec![trusted.parse().unwrap()],
                Duration::from_secs(2),
                ProxyMetrics::register(&Metrics::new(), "edge"),
            );
            let stop = CancellationToken::new();
            tokio::spawn(serve_with_proxy(listener, s, proxy, stop.clone()));
            let leaf = bundle
                .issue_allocation(
                    &ID.parse().unwrap(),
                    LeafKey::Generate,
                    now,
                    now + time::Duration::hours(1)
                        - time::Duration::nanoseconds(i64::from(now.nanosecond())),
                )
                .unwrap();
            Rig {
                addr,
                peers,
                ca: bundle.client_ca().cert_der().clone(),
                leaf: (leaf.cert_der().clone(), leaf.private_key_der().unwrap()),
                metrics,
                stop,
            }
        }

        fn header(src: SocketAddrV4) -> Vec<u8> {
            let mut h = b"\r\n\r\n\0\r\nQUIT\n".to_vec();
            h.extend_from_slice(&[0x21, 0x11, 0, 12]);
            h.extend_from_slice(&src.ip().octets());
            h.extend_from_slice(&[10, 0, 0, 1, 0x30, 0x39, 0x20, 0xfc]);
            h[24..26].copy_from_slice(&src.port().to_be_bytes());
            h
        }

        /// A TLS session with the allocation cert after writing `prefix`.
        async fn dial(r: &Rig, prefix: &[u8]) -> std::io::Result<()> {
            let mut roots = rustls::RootCertStore::empty();
            roots.add(r.ca.clone()).unwrap();
            let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_client_auth_cert(vec![r.leaf.0.clone()], r.leaf.1.clone_key())
            .unwrap();
            let mut tcp = TcpStream::connect(r.addr).await?;
            tcp.write_all(prefix).await?;
            let mut s = tokio_rustls::TlsConnector::from(Arc::new(cfg))
                .connect(ServerName::try_from(HOST).unwrap(), tcp)
                .await?;
            // TLS 1.3: a refusal shows as an alert on the first read after the
            // client's flight. The test handler closes without close_notify.
            let mut b = [0u8; 1];
            match tokio::io::AsyncReadExt::read(&mut s, &mut b).await {
                Err(e) if e.kind() != std::io::ErrorKind::UnexpectedEof => Err(e),
                _ => Ok(()),
            }
        }

        async fn next_peer(r: &mut Rig) -> SocketAddr {
            tokio::time::timeout(Duration::from_secs(5), r.peers.recv())
                .await
                .unwrap()
                .unwrap()
        }

        #[tokio::test]
        async fn the_handler_sees_the_proxy_source() {
            let mut r = start("127.0.0.0/8").await;
            dial(&r, &header("192.0.2.7:1000".parse().unwrap()))
                .await
                .unwrap();
            assert_eq!(next_peer(&mut r).await, "192.0.2.7:1000".parse().unwrap());
            r.stop.cancel();
        }

        #[tokio::test]
        async fn an_untrusted_forged_header_fails_the_handshake() {
            let mut r = start("10.0.0.0/8").await;
            let forged = header("203.0.113.9:1".parse().unwrap());
            assert!(dial(&r, &forged).await.is_err());
            tokio::time::timeout(Duration::from_secs(3), async {
                while r.metrics.outcome_count(None, "tls_error") == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            dial(&r, &[]).await.unwrap();
            let seen = next_peer(&mut r).await;
            assert_eq!(seen.ip(), ip("127.0.0.1"), "direct, as itself");
            assert!(
                r.peers.try_recv().is_err(),
                "the forged connection never reached the handler"
            );
            r.stop.cancel();
        }

        /// `max_handshakes_per_ip` counts the PROXY source: two clients behind
        /// one trusted gateway address have separate budgets, and one client
        /// is still capped.
        #[tokio::test]
        async fn handshake_cap_counts_the_proxy_source() {
            let mut r = start_with("127.0.0.0/8", 1).await;
            // Client A: header, then no ClientHello, so it stays in the
            // handshake and holds A's only slot.
            let mut stalled = TcpStream::connect(r.addr).await.unwrap();
            stalled
                .write_all(&header("192.0.2.7:1000".parse().unwrap()))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(
                dial(&r, &header("192.0.2.7:2000".parse().unwrap()))
                    .await
                    .is_err(),
                "A's second handshake is over its cap"
            );
            dial(&r, &header("192.0.2.8:1000".parse().unwrap()))
                .await
                .expect("B, behind the same gateway address, has its own budget");
            assert_eq!(next_peer(&mut r).await, "192.0.2.8:1000".parse().unwrap());
            drop(stalled);
            r.stop.cancel();
        }
    }
}
