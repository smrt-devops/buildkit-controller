//! Accept loop: handshake and connection caps, keepalive,
//! `ClientHello` and handshake timeouts, readiness and drain.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use socket2::{SockRef, TcpKeepalive};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinSet;
use tokio_rustls::LazyConfigAcceptor;
use tokio_rustls::server::TlsStream;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, info, info_span, warn};

use super::handler::{Accepted, ConnectionHandler};
use super::metrics::EdgeMetrics;
use super::tls::{EdgeTls, counted_by_verifier};
use crate::net::ProxyProtocol;
use crate::pki::AllocationId;
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
    /// handshake or waiting for an established slot. One more evicts the oldest.
    pub max_handshakes: usize,
    /// `edge.maxHandshakesPerIp`: in-handshake connections from one source
    /// address ([`PeerKey`]). One more evicts that address's oldest. 0
    /// disables the cap.
    pub max_handshakes_per_ip: usize,
}

impl Default for ListenerOptions {
    fn default() -> Self {
        Self {
            handshake_timeout: Duration::from_secs(10),
            max_connections: usize::try_from(crate::config::EDGE_MAX_CONNECTIONS)
                .unwrap_or(usize::MAX),
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

/// Open connections per key, capped at `max` each (0: no cap).
#[derive(Clone, Debug)]
pub struct PerKey<K: Hash + Eq> {
    max: usize,
    open: Arc<Mutex<HashMap<K, usize>>>,
}

/// Open connections per [`PeerKey`].
pub type PerPeer = PerKey<PeerKey>;
/// One connection's share of its peer's cap.
pub type PeerSlot = Slot<PeerKey>;

impl<K: Hash + Eq + Clone> PerKey<K> {
    /// A cap of `max` per key; 0 disables the cap and counts nothing.
    #[must_use]
    pub fn new(max: usize) -> Self {
        Self {
            max,
            open: Arc::default(),
        }
    }

    /// A slot for `key`, or `None` when it already holds `max`.
    #[must_use]
    pub fn try_take(&self, key: impl Into<K>) -> Option<Slot<K>> {
        let key = key.into();
        if self.max == 0 {
            return Some(Slot { key, open: None });
        }
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        let n = open.entry(key.clone()).or_insert(0);
        if *n >= self.max {
            return None;
        }
        *n += 1;
        Some(Slot {
            key,
            open: Some(Arc::clone(&self.open)),
        })
    }

    /// Keys holding at least one slot (tests).
    #[must_use]
    pub fn peers(&self) -> usize {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// One connection's share of its key's cap; released on drop.
#[derive(Debug)]
pub struct Slot<K: Hash + Eq> {
    key: K,
    /// `None` when the cap is disabled.
    open: Option<Arc<Mutex<HashMap<K, usize>>>>,
}

impl<K: Hash + Eq> Drop for Slot<K> {
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

/// The longest the edge waits for a complete `ClientHello`, within
/// `handshake_timeout`. A peer that sends nothing, or a partial hello, is
/// dropped long before the full handshake budget runs out.
const CLIENT_HELLO_TIMEOUT: Duration = Duration::from_secs(2);

/// Why a connection lost its handshake slot to a newer one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Evicted {
    /// `max_handshakes` was reached.
    Total,
    /// Its peer's `max_handshakes_per_ip` was reached.
    Peer,
}

impl Evicted {
    /// The `result` label the evicted connection is counted under.
    fn label(self) -> &'static str {
        match self {
            Self::Total => "handshake_limit",
            Self::Peer => "peer_handshake_limit",
        }
    }
}

/// Connections in the handshake (or waiting for an established slot), capped
/// in all and per [`PeerKey`]. A connection over a cap evicts the oldest one
/// it competes with rather than being refused, so connections that stall
/// cannot hold the budget. To lock out a client whose handshake completes
/// promptly, an attacker must open a whole cap's worth of connections within
/// that handshake's few round trips, instead of a trickle every
/// `handshake_timeout`.
#[derive(Clone)]
struct Handshakes {
    max: usize,
    max_per_peer: usize,
    state: Arc<Mutex<Pending>>,
}

/// Open handshakes, oldest first.
#[derive(Default)]
struct Pending {
    next: u64,
    open: BTreeMap<u64, Waiting>,
    per_peer: HashMap<PeerKey, usize>,
}

struct Waiting {
    /// `None` until the client address is known (after the PROXY header).
    peer: Option<PeerKey>,
    evict: CancellationToken,
}

impl Pending {
    /// Takes `seq` out of the count; `None` when it is already gone.
    fn remove(&mut self, seq: u64) -> Option<Waiting> {
        let w = self.open.remove(&seq)?;
        if let Some(key) = w.peer
            && let Some(n) = self.per_peer.get_mut(&key)
        {
            *n -= 1;
            if *n == 0 {
                self.per_peer.remove(&key);
            }
        }
        Some(w)
    }
}

/// One connection's handshake slot, freed on drop. `evicted` fires when a
/// newer connection takes the slot.
struct HandshakeSlot {
    state: Arc<Mutex<Pending>>,
    seq: u64,
    evicted: CancellationToken,
}

impl Drop for HandshakeSlot {
    fn drop(&mut self) {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        s.remove(self.seq);
    }
}

impl Handshakes {
    fn new(max: usize, max_per_peer: usize) -> Self {
        Self {
            max: max.max(1),
            max_per_peer,
            state: Arc::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A slot for a new connection. With `max` already open, the oldest is
    /// evicted, and the result says so.
    fn take(&self) -> (HandshakeSlot, Option<Evicted>) {
        let mut s = self.lock();
        let mut evicted = None;
        if s.open.len() >= self.max
            && let Some(seq) = s.open.keys().next().copied()
            && let Some(w) = s.remove(seq)
        {
            w.evict.cancel();
            evicted = Some(Evicted::Total);
        }
        let seq = s.next;
        s.next += 1;
        let evict = CancellationToken::new();
        s.open.insert(
            seq,
            Waiting {
                peer: None,
                evict: evict.clone(),
            },
        );
        let slot = HandshakeSlot {
            state: Arc::clone(&self.state),
            seq,
            evicted: evict,
        };
        (slot, evicted)
    }

    /// Counts `slot` against the cap of `ip`'s [`PeerKey`]. When that peer
    /// already holds `max_per_peer`, its oldest is evicted, and the result
    /// says so.
    fn assign(&self, slot: &HandshakeSlot, ip: IpAddr) -> Option<Evicted> {
        if self.max_per_peer == 0 {
            return None;
        }
        let key = PeerKey::from(ip);
        let mut s = self.lock();
        if !s.open.contains_key(&slot.seq) {
            return None;
        }
        let mut evicted = None;
        if s.per_peer.get(&key).copied().unwrap_or(0) >= self.max_per_peer
            && let Some(seq) = s
                .open
                .iter()
                .find(|(_, w)| w.peer == Some(key))
                .map(|(seq, _)| *seq)
            && let Some(w) = s.remove(seq)
        {
            w.evict.cancel();
            evicted = Some(Evicted::Peer);
        }
        if let Some(w) = s.open.get_mut(&slot.seq) {
            w.peer = Some(key);
        }
        *s.per_peer.entry(key).or_insert(0) += 1;
        evicted
    }

    /// Open slots (tests).
    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().open.len()
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

/// Serves `listener` until `shutdown`, then stops accepting (dropping the
/// socket) and waits for every open connection to end on its own: release,
/// revocation, the worker pod going away, or the client cert's `notAfter`. The
/// runtime bounds the wait by the drain budget and aborts the rest by dropping
/// this future.
///
/// An unauthenticated client can only hold handshake slots: at most
/// `max_handshakes` in all and `max_handshakes_per_ip` per peer, each for at
/// most [`CLIENT_HELLO_TIMEOUT`] without a `ClientHello` and `handshake_timeout`
/// in all, and each evictable by a newer connection ([`Handshakes`]).
/// Established connections draw on the separate `max_connections` budget.
pub async fn serve(listener: TcpListener, s: Serve, shutdown: CancellationToken) {
    serve_with_proxy(listener, s, ProxyProtocol::off(), shutdown).await;
}

/// [`serve`] behind a gateway: a connection from a source `proxy` trusts must
/// open with a PROXY header, whose source address then stands for the client
/// in the per-address cap and the `peer` log field. The header read holds a
/// handshake slot (not yet counted against any address) and is bounded by
/// the header timeout.
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
        handshakes: Handshakes::new(options.max_handshakes, options.max_handshakes_per_ip),
        proxy,
    });
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
                let Some(current) = tls.borrow().clone() else { continue };
                let (slot, evicted) = shared.handshakes.take();
                if let Some(e) = evicted {
                    shared.metrics.outcome(None, e.label());
                    debug!(remote = %peer, "edge handshake limit reached; evicted the oldest");
                }
                conns.spawn(admit(tcp, peer, current, slot, Arc::clone(&shared)));
            }
        }
    }
    drop(listener);
    info!(open = conns.len(), "edge listener closed, draining");
    let mut conns = Draining(conns);
    let mut progress = tokio::time::interval_at(
        tokio::time::Instant::now() + DRAIN_LOG_EVERY,
        DRAIN_LOG_EVERY,
    );
    loop {
        tokio::select! {
            done = conns.0.join_next() => if done.is_none() { break },
            _ = progress.tick() => info!(open = conns.0.len(), "edge draining"),
        }
    }
    info!("edge drained");
}

/// The connections a draining listener waits for. Dropped with some still open,
/// the drain deadline aborted the edge, and it logs how many it cut.
struct Draining(JoinSet<()>);

impl Drop for Draining {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            warn!(
                open = self.0.len(),
                "edge drain deadline reached; cutting open connections"
            );
        }
    }
}

/// How often a draining edge logs its open connection count.
const DRAIN_LOG_EVERY: Duration = Duration::from_mins(1);

/// The next connection, its peer an IPv4 address when it came as an
/// IPv4-mapped one (a dual-stack listener). Accept errors (e.g. `EMFILE`) are logged and retried
/// after a short pause.
async fn accept(listener: &TcpListener) -> (TcpStream, SocketAddr) {
    loop {
        match listener.accept().await {
            Ok((tcp, peer)) => return (tcp, crate::runtime::listen::canonical(peer)),
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
    /// `max_handshakes` and `max_handshakes_per_ip`.
    handshakes: Handshakes,
    proxy: ProxyProtocol,
}

/// Finds the client address (the PROXY source behind a trusted gateway, else
/// the socket's peer), counts the handshake slot against that address and
/// runs the connection in a span whose `peer` is the client. Everything after
/// this point sees only the client address; the gateway's is logged as `via`.
async fn admit(
    mut tcp: TcpStream,
    remote: SocketAddr,
    tls: Arc<EdgeTls>,
    slot: HandshakeSlot,
    shared: Arc<Shared>,
) {
    if let Err(e) = tune_socket(&tcp) {
        debug!(error = %e, "socket options");
    }
    let header = tokio::select! {
        () = slot.evicted.cancelled() => return,
        c = shared.proxy.client(&mut tcp, remote) => c,
    };
    let client = match header {
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
    if let Some(e) = shared.handshakes.assign(&slot, client.addr.ip()) {
        shared.metrics.outcome(None, e.label());
        debug!(peer = %client.addr, via = ?via, "edge per-address handshake limit reached; evicted the oldest");
    }
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
    connection(tcp, client.addr, tls, &shared, slot)
        .instrument(span)
        .await;
}

/// TLS handshake, identity and connection slot under the handshake slot, then
/// the handler. `peer` is the client address from [`admit`].
async fn connection(
    tcp: TcpStream,
    peer: SocketAddr,
    tls: Arc<EdgeTls>,
    shared: &Shared,
    slot: HandshakeSlot,
) {
    let established = tokio::select! {
        () = slot.evicted.cancelled() => {
            debug!("edge handshake evicted by a newer connection; closing");
            return;
        }
        e = establish(tcp, &tls, shared) => e,
    };
    let Some((stream, id, _permit)) = established else {
        return;
    };
    drop(slot);
    let span = tracing::Span::current();
    span.record("alloc_id", id.alloc_id.as_str());
    span.record("pod_uid", id.pod_uid.as_str());
    shared.handler.handle(Accepted { stream, id, peer }).await;
}

/// The `ClientHello` within [`CLIENT_HELLO_TIMEOUT`], the rest of the handshake
/// within `handshake_timeout`, the identity, then an established slot within
/// another `handshake_timeout`. `None` once the connection is closed and
/// counted.
async fn establish(
    tcp: TcpStream,
    tls: &EdgeTls,
    shared: &Shared,
) -> Option<(TlsStream<TcpStream>, AllocationId, OwnedSemaphorePermit)> {
    let Shared {
        metrics, options, ..
    } = shared;
    let start = Instant::now();
    let hello = tokio::time::timeout(
        CLIENT_HELLO_TIMEOUT.min(options.handshake_timeout),
        LazyConfigAcceptor::new(rustls::server::Acceptor::default(), tcp),
    );
    let handshake = async {
        let hello = hello.await.map_err(|_| None)?.map_err(Some)?;
        let rest = options.handshake_timeout.saturating_sub(start.elapsed());
        tokio::time::timeout(rest, hello.into_stream(Arc::clone(&tls.server)))
            .await
            .map_err(|_| None)?
            .map_err(Some)
    };
    let stream = match handshake.await {
        Ok(s) => s,
        Err(Some(e)) => {
            if !counted_by_verifier(&e) {
                metrics.outcome(None, "tls_error");
            }
            debug!(error = %e, "edge handshake failed");
            return None;
        }
        Err(None) => {
            metrics.outcome(None, "tls_error");
            debug!("edge handshake timed out");
            return None;
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
        return None;
    };
    // The wait stays under the handshake slot, so waiters are bounded by the
    // same caps as handshakes.
    let Ok(Ok(permit)) = tokio::time::timeout(
        options.handshake_timeout,
        Arc::clone(&shared.established).acquire_owned(),
    )
    .await
    else {
        metrics.outcome(Some(&id), "connection_limit");
        debug!("edge connection limit reached; closing");
        return None;
    };
    Some((stream, id, permit))
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

    #[test]
    fn a_full_handshake_budget_evicts_its_oldest() {
        let h = Handshakes::new(2, 0);
        let (first, none) = h.take();
        assert_eq!(none, None);
        let (second, _) = h.take();
        let (third, evicted) = h.take();
        assert_eq!(evicted, Some(Evicted::Total));
        assert!(first.evicted.is_cancelled(), "the oldest goes");
        assert!(!second.evicted.is_cancelled() && !third.evicted.is_cancelled());
        assert_eq!(h.len(), 2);
        drop(first);
        assert_eq!(h.len(), 2, "an evicted slot is not freed twice");
        drop(second);
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn a_full_peer_evicts_its_own_oldest_only() {
        let h = Handshakes::new(100, 2);
        let (other, _) = h.take();
        assert_eq!(h.assign(&other, ip("10.0.0.9")), None);
        let slots: Vec<_> = (0..2)
            .map(|_| {
                let (s, _) = h.take();
                assert_eq!(h.assign(&s, ip("10.0.0.1")), None);
                s
            })
            .collect();
        let (s, _) = h.take();
        assert_eq!(h.assign(&s, ip("10.0.0.1")), Some(Evicted::Peer));
        assert!(slots[0].evicted.is_cancelled());
        assert!(!slots[1].evicted.is_cancelled() && !s.evicted.is_cancelled());
        assert!(
            !other.evicted.is_cancelled(),
            "another address keeps its slot"
        );
        drop(slots);
        drop(s);
        let (t, _) = h.take();
        assert_eq!(h.assign(&t, ip("10.0.0.1")), None, "freed on drop");
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

        /// Reports each accepted connection's `peer`, then holds it until
        /// `release` is true.
        struct Peers {
            tx: mpsc::UnboundedSender<SocketAddr>,
            release: watch::Receiver<bool>,
        }

        impl ConnectionHandler for Peers {
            fn handle(&self, conn: Accepted) -> futures::future::BoxFuture<'static, ()> {
                let _ = self.tx.send(conn.peer);
                let mut release = self.release.clone();
                Box::pin(async move {
                    let _ = release.wait_for(|r| *r).await;
                    drop(conn);
                })
            }
        }

        struct Rig {
            addr: SocketAddr,
            peers: mpsc::UnboundedReceiver<SocketAddr>,
            ca: CertificateDer<'static>,
            leaf: (CertificateDer<'static>, PrivateKeyDer<'static>),
            metrics: EdgeMetrics,
            stop: CancellationToken,
            /// Set to true to end the connections the handler holds.
            release: watch::Sender<bool>,
        }

        /// A 2 s handshake timeout, defaults otherwise.
        fn options() -> ListenerOptions {
            ListenerOptions {
                handshake_timeout: Duration::from_secs(2),
                ..ListenerOptions::default()
            }
        }

        async fn start(trusted: &str) -> Rig {
            start_with(trusted, options(), true).await
        }

        /// `released`: the handler drops each connection at once.
        async fn start_with(trusted: &str, options: ListenerOptions, released: bool) -> Rig {
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
            let (release, release_rx) = watch::channel(released);
            let handler: Arc<dyn ConnectionHandler> = Arc::new(Peers {
                tx,
                release: release_rx,
            });
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
                options,
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
                release,
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

        /// A TLS session with the allocation cert after writing `prefix`, left
        /// open. The server may still refuse it after this returns.
        async fn connect(
            r: &Rig,
            prefix: &[u8],
        ) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
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
            tokio_rustls::TlsConnector::from(Arc::new(cfg))
                .connect(ServerName::try_from(HOST).unwrap(), tcp)
                .await
        }

        /// A TLS session with the allocation cert after writing `prefix`.
        async fn dial(r: &Rig, prefix: &[u8]) -> std::io::Result<()> {
            let mut s = connect(r, prefix).await?;
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

        /// Reads until the server closes `s`; panics if it stays open `secs`.
        async fn closed_within(s: &mut TcpStream, secs: u64) {
            let mut b = [0u8; 64];
            tokio::time::timeout(Duration::from_secs(secs), async {
                while !matches!(
                    tokio::io::AsyncReadExt::read(s, &mut b).await,
                    Ok(0) | Err(_)
                ) {}
            })
            .await
            .expect("the server closed the connection");
        }

        /// `max_handshakes_per_ip` counts the PROXY source: a client that
        /// stalls its handshake loses its slot to its own next connection,
        /// and a second client behind the same gateway address is untouched.
        #[tokio::test]
        async fn handshake_cap_counts_the_proxy_source() {
            let one = ListenerOptions {
                max_handshakes_per_ip: 1,
                ..options()
            };
            let mut r = start_with("127.0.0.0/8", one, true).await;
            // Client A: header, then no `ClientHello`, so it stays in the
            // handshake and holds A's only slot.
            let mut stalled = TcpStream::connect(r.addr).await.unwrap();
            stalled
                .write_all(&header("192.0.2.7:1000".parse().unwrap()))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            dial(&r, &header("192.0.2.7:2000".parse().unwrap()))
                .await
                .expect("A's new handshake evicts its stalled one");
            assert_eq!(next_peer(&mut r).await, "192.0.2.7:2000".parse().unwrap());
            closed_within(&mut stalled, 1).await;
            assert_eq!(r.metrics.outcome_count(None, "peer_handshake_limit"), 1);
            dial(&r, &header("192.0.2.8:1000".parse().unwrap()))
                .await
                .expect("B, behind the same gateway address, has its own budget");
            assert_eq!(next_peer(&mut r).await, "192.0.2.8:1000".parse().unwrap());
            r.stop.cancel();
        }

        /// Stalled connections cannot hold the whole handshake budget either:
        /// with it full, a real client still gets in.
        #[tokio::test]
        async fn stalled_handshakes_cannot_lock_out_a_client() {
            let tight = ListenerOptions {
                max_handshakes: 4,
                max_handshakes_per_ip: 0,
                ..options()
            };
            let mut r = start_with("10.0.0.0/8", tight, true).await;
            let mut stalled = Vec::new();
            for _ in 0..4 {
                stalled.push(TcpStream::connect(r.addr).await.unwrap());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            dial(&r, &[]).await.expect("evicts the oldest stalled one");
            next_peer(&mut r).await;
            closed_within(&mut stalled[0], 1).await;
            assert_eq!(r.metrics.outcome_count(None, "handshake_limit"), 1);
            r.stop.cancel();
        }

        /// A peer that sends no `ClientHello` is closed after
        /// `CLIENT_HELLO_TIMEOUT`, not the full `handshake_timeout`.
        #[tokio::test]
        async fn a_silent_peer_is_closed_before_the_handshake_timeout() {
            let long = ListenerOptions {
                handshake_timeout: Duration::from_secs(30),
                ..options()
            };
            let r = start_with("10.0.0.0/8", long, true).await;
            let mut silent = TcpStream::connect(r.addr).await.unwrap();
            let start = Instant::now();
            closed_within(&mut silent, CLIENT_HELLO_TIMEOUT.as_secs() + 2).await;
            assert!(start.elapsed() >= Duration::from_millis(1900));
            assert_eq!(r.metrics.outcome_count(None, "tls_error"), 1);
            r.stop.cancel();
        }

        /// TLS 1.2 would send the allocation cert in cleartext.
        #[tokio::test]
        async fn tls_1_2_is_refused() {
            let mut r = start("10.0.0.0/8").await;
            let mut roots = rustls::RootCertStore::empty();
            roots.add(r.ca.clone()).unwrap();
            let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_protocol_versions(&[&rustls::version::TLS12])
            .unwrap()
            .with_root_certificates(roots)
            .with_client_auth_cert(vec![r.leaf.0.clone()], r.leaf.1.clone_key())
            .unwrap();
            let tcp = TcpStream::connect(r.addr).await.unwrap();
            let res = tokio_rustls::TlsConnector::from(Arc::new(cfg))
                .connect(ServerName::try_from(HOST).unwrap(), tcp)
                .await;
            assert!(res.is_err(), "a TLS 1.2 handshake fails");
            assert!(r.peers.try_recv().is_err(), "never reached the handler");
            dial(&r, &[]).await.expect("TLS 1.3 works");
            next_peer(&mut r).await;
            r.stop.cancel();
        }

        /// With every established slot held, a further valid client is closed
        /// after `handshake_timeout` and counted; it gets in once a slot frees.
        #[tokio::test]
        async fn a_full_edge_closes_after_the_handshake_timeout_then_admits() {
            let full = ListenerOptions {
                max_connections: 1,
                handshake_timeout: Duration::from_secs(1),
                ..ListenerOptions::default()
            };
            let mut r = start_with("10.0.0.0/8", full, false).await;
            let _held = connect(&r, &[]).await.unwrap();
            next_peer(&mut r).await;
            let id: crate::pki::AllocationId = ID.parse().unwrap();
            let start = Instant::now();
            let mut second = connect(&r, &[]).await.unwrap();
            let mut b = [0u8; 1];
            let n = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::io::AsyncReadExt::read(&mut second, &mut b),
            )
            .await
            .unwrap();
            assert!(!matches!(n, Ok(1..)), "closed without data: {n:?}");
            assert!(start.elapsed() >= Duration::from_secs(1));
            assert_eq!(r.metrics.outcome_count(Some(&id), "connection_limit"), 1);
            assert!(r.peers.try_recv().is_err(), "never reached the handler");
            r.release.send_replace(true);
            dial(&r, &[]).await.unwrap();
            next_peer(&mut r).await;
            r.stop.cancel();
        }
    }
}
