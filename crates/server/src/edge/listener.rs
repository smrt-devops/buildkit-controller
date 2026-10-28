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

    let established = Arc::new(Semaphore::new(options.max_connections.max(1)));
    let handshakes = Arc::new(Semaphore::new(options.max_handshakes.max(1)));
    let per_peer = PerPeer::new(options.max_handshakes_per_ip);
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
                    debug!(%peer, "edge handshake limit reached; closing");
                    continue;
                };
                let Some(slot) = per_peer.try_take(peer.ip()) else {
                    debug!(%peer, "edge per-address handshake limit reached; closing");
                    continue;
                };
                let Some(current) = tls.borrow().clone() else { continue };
                let hs = Handshaking { _global: global, _peer: slot };
                let conn = connection(
                    tcp, peer, current, Arc::clone(&handler), metrics.clone(), options, hs,
                    Arc::clone(&established),
                );
                conns.spawn(conn.instrument(info_span!(
                    "edge_conn",
                    %peer,
                    alloc_id = tracing::field::Empty,
                    pod_uid = tracing::field::Empty,
                )));
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

#[allow(
    clippy::too_many_arguments,
    reason = "one call site, all per-connection state"
)]
async fn connection(
    tcp: TcpStream,
    peer: SocketAddr,
    tls: Arc<EdgeTls>,
    handler: Arc<dyn ConnectionHandler>,
    metrics: EdgeMetrics,
    options: ListenerOptions,
    handshaking: Handshaking,
    established: Arc<Semaphore>,
) {
    if let Err(e) = tune_socket(&tcp) {
        debug!(error = %e, "socket options");
    }
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
    let Ok(Ok(_slot)) =
        tokio::time::timeout(options.handshake_timeout, established.acquire_owned()).await
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
}
