//! TLS accept loop with a per-connection serving cert from a watch channel (hot swap).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use hyper::server::conn::{http1, http2};
use hyper::service::{Service as _, service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulConnection;
use hyper_util::service::TowerToHyperService;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{Error, ServerConfig};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, Semaphore, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use axum::http::{HeaderValue, header};

use super::api::Peer;
use crate::edge::{PerPeer, tune_socket};
use crate::net::ProxyProtocol;
use crate::runtime::listen::canonical;

/// Serving cert material: `None` until the first valid cert is loaded.
pub type ServingCerts = watch::Receiver<Option<Arc<CertifiedKey>>>;

/// HTTP/2 ping cadence and the wait for its ack before closing.
const H2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const H2_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);
/// Concurrent HTTP/2 streams per connection.
const H2_MAX_STREAMS: u32 = 64;
/// Request head bound (HTTP/1 buffer, HTTP/2 header list); the token alone may be 16 KiB.
const MAX_HEADER_BYTES: u32 = 64 * 1024;

/// Per-listener bounds.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// `api.maxConnections`: open API connections; more are closed on accept.
    pub max_connections: usize,
    /// `api.maxConnectionsPerIp`: open connections from one client address
    /// ([`PeerKey`](crate::edge::PeerKey)); more are closed on accept. 0 disables the cap.
    pub max_connections_per_ip: usize,
    /// TLS handshake.
    pub handshake_timeout: Duration,
    /// Time with no request open (TLS done to the first request, and HTTP/2
    /// between streams) before the connection is closed.
    pub idle_timeout: Duration,
    /// HTTP/1 request head, also the keep-alive idle bound between requests.
    pub header_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 4096,
            max_connections_per_ip: 128,
            handshake_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(10),
            header_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug)]
struct OneCert(Arc<CertifiedKey>);

impl ResolvesServerCert for OneCert {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(&self.0))
    }
}

/// TLS 1.2 + 1.3, no client auth, ALPN `h2`, `http/1.1`.
pub fn server_config(key: Arc<CertifiedKey>) -> Result<Arc<ServerConfig>, Error> {
    let mut c =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(OneCert(key)));
    c.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(c))
}

/// The `ServerConfig` for the current key, rebuilt only when the key changes.
struct Configs {
    certs: ServingCerts,
    built: Option<(Arc<CertifiedKey>, Arc<ServerConfig>)>,
}

impl Configs {
    fn current(&mut self) -> Option<Arc<ServerConfig>> {
        let key = self.certs.borrow_and_update().clone()?;
        if let Some((k, c)) = &self.built
            && Arc::ptr_eq(k, &key)
        {
            return Some(Arc::clone(c));
        }
        match server_config(Arc::clone(&key)) {
            Ok(c) => {
                self.built = Some((key, Arc::clone(&c)));
                Some(c)
            }
            Err(e) => {
                warn!(error = %e, "building the API TLS config");
                self.built.as_ref().map(|(_, c)| Arc::clone(c))
            }
        }
    }
}

/// Serve `app` on `listener` until `shutdown`, then stop accepting and let open
/// connections finish (see [`drive`] for how each protocol closes).
pub async fn serve(
    listener: TcpListener,
    app: Router,
    certs: ServingCerts,
    limits: Limits,
    shutdown: CancellationToken,
) {
    serve_with_proxy(listener, app, certs, limits, ProxyProtocol::off(), shutdown).await;
}

/// [`serve`] behind a gateway: a connection from a source `proxy` trusts must
/// open with a PROXY header, whose source address then stands for the client
/// in `max_connections_per_ip`, the [`Peer`] extension (logs, the per-address
/// review cap) and the debug logs here. The header read holds a connection
/// slot and is bounded by the header timeout.
pub async fn serve_with_proxy(
    listener: TcpListener,
    app: Router,
    certs: ServingCerts,
    limits: Limits,
    proxy: ProxyProtocol,
    shutdown: CancellationToken,
) {
    let mut configs = Configs { certs, built: None };
    let permits = Arc::new(Semaphore::new(limits.max_connections.max(1)));
    let per_peer = PerPeer::new(limits.max_connections_per_ip);
    let mut conns = JoinSet::new();
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
            accepted = listener.accept() => {
                let (tcp, peer) = match accepted {
                    Ok((tcp, peer)) => (tcp, canonical(peer)),
                    Err(e) => {
                        debug!(error = %e, "accept");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                    debug!(remote = %peer, "API connection limit reached; closing");
                    continue;
                };
                let Some(tls) = configs.current() else {
                    debug!(remote = %peer, "no serving cert yet; closing");
                    continue;
                };
                let (app, stop) = (app.clone(), shutdown.clone());
                let (proxy, per_peer) = (proxy.clone(), per_peer.clone());
                conns.spawn(async move {
                    // From here on `peer` is the client: the PROXY source behind a
                    // trusted gateway, else the socket's peer.
                    let Some((tcp, peer)) = client(tcp, peer, &proxy).await else { return };
                    let Some(slot) = per_peer.try_take(peer.ip()) else {
                        debug!(peer = %peer, "API per-address connection limit reached; closing");
                        return;
                    };
                    connection(tcp, peer, tls, app, limits, stop).await;
                    drop((permit, slot));
                });
            }
        }
    }
    // Close the socket now, so a new connection is refused rather than left in the backlog.
    drop(listener);
    while conns.join_next().await.is_some() {}
}

/// The connection and its client address, or `None` when a trusted source
/// sent no valid PROXY header (the connection is then closed).
async fn client(
    mut tcp: TcpStream,
    peer: SocketAddr,
    proxy: &ProxyProtocol,
) -> Option<(TcpStream, SocketAddr)> {
    match proxy.client(&mut tcp, peer).await {
        Ok(c) => {
            if let Some(via) = c.origin.via() {
                debug!(peer = %c.addr, via = %via, "API PROXY header accepted");
            }
            Some((tcp, c.addr))
        }
        Err(e) => {
            debug!(via = %peer, reason = e.label(), error = %e, "API PROXY header refused; closing");
            None
        }
    }
}

async fn connection(
    tcp: TcpStream,
    peer: SocketAddr,
    tls: Arc<ServerConfig>,
    app: Router,
    limits: Limits,
    shutdown: CancellationToken,
) {
    if let Err(e) = tune_socket(&tcp) {
        debug!(error = %e, "socket options");
    }
    let accept = TlsAcceptor::from(tls).accept(tcp);
    let stream = match tokio::time::timeout(limits.handshake_timeout, accept).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            debug!(peer = %peer, error = %e, "TLS handshake");
            return;
        }
        Err(_) => {
            debug!(peer = %peer, "TLS handshake timed out");
            return;
        }
    };
    // The protocol comes from ALPN, so there is no sniffing phase without a deadline.
    let h2 = stream.get_ref().1.alpn_protocol() == Some(b"h2");
    let inner = TowerToHyperService::new(app.layer(axum::Extension(Peer(peer))));
    let flight = Arc::new(InFlight::default());
    let svc = {
        let (flight, stop) = (Arc::clone(&flight), shutdown.clone());
        service_fn(move |req| {
            let active = InFlight::start(&flight);
            let call = inner.call(req);
            let stop = stop.clone();
            async move {
                let mut resp = call.await;
                // HTTP/1 only: HTTP/2 forbids the header and closes with GOAWAY.
                if !h2
                    && stop.is_cancelled()
                    && let Ok(r) = &mut resp
                {
                    r.headers_mut()
                        .insert(header::CONNECTION, HeaderValue::from_static("close"));
                }
                drop(active);
                resp
            }
        })
    };
    let io = TokioIo::new(stream);
    if h2 {
        let conn = http2::Builder::new(TokioExecutor::new())
            .timer(TokioTimer::new())
            .keep_alive_interval(Some(H2_KEEPALIVE_INTERVAL))
            .keep_alive_timeout(H2_KEEPALIVE_TIMEOUT)
            .max_concurrent_streams(H2_MAX_STREAMS)
            .max_header_list_size(MAX_HEADER_BYTES)
            .serve_connection(io, svc);
        drive(conn, peer, &flight, limits, shutdown, true).await;
    } else {
        let conn = http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(limits.header_timeout)
            .max_buf_size(MAX_HEADER_BYTES as usize)
            .serve_connection(io, svc);
        drive(conn, peer, &flight, limits, shutdown, false).await;
    }
}

/// Requests open on one connection, counted from the service call until its future
/// resolves (response head ready) or is dropped (stream reset). Body delivery
/// is not counted, so a peer that never opens its flow-control window cannot
/// hold the connection. The server cuts an undelivered 201 and retires its grant.
#[derive(Default)]
struct InFlight {
    open: AtomicUsize,
    /// Signalled each time `open` falls to zero.
    idle: Notify,
}

impl InFlight {
    fn start(this: &Arc<Self>) -> Active {
        this.open.fetch_add(1, Ordering::AcqRel);
        Active(Arc::clone(this))
    }

    fn is_idle(&self) -> bool {
        self.open.load(Ordering::Acquire) == 0
    }
}

/// One open request; dropping it may make the connection idle.
struct Active(Arc<InFlight>);

impl Drop for Active {
    fn drop(&mut self) {
        if self.0.open.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_one();
        }
    }
}

/// Run `conn`, drain it on `shutdown`, and bound the time it may spend with no
/// request open. The bound is `idle_timeout` from the end of the TLS handshake,
/// re-armed each time the last open request ends. On expiry the server asks the
/// connection to close (HTTP/2 GOAWAY, so a stream that raced in still
/// completes). If the connection is still idle one more `idle_timeout` later,
/// the server drops it. This function never bounds an open request, because a
/// POST waits up to 20 s for its allocation and a GET up to 25 s.
///
/// On `shutdown` an HTTP/2 connection gets GOAWAY at once, and is dropped once
/// it has had no request open for `idle_timeout`. An HTTP/1 connection keeps
/// running, its responses carry `Connection: close` from then on, and the idle
/// bound closes it if no request comes. So a request already on the wire gets
/// an answer, not a reset: a waiting allocation answers at once with its state.
async fn drive<C>(
    conn: C,
    peer: SocketAddr,
    flight: &InFlight,
    limits: Limits,
    shutdown: CancellationToken,
    h2: bool,
) where
    C: GracefulConnection,
    C::Error: std::fmt::Display,
{
    tokio::pin!(conn);
    let deadline = tokio::time::sleep(limits.idle_timeout);
    tokio::pin!(deadline);
    let mut closing = false;
    let mut stopped = false;
    loop {
        tokio::select! {
            // An elapsed deadline must not beat a request that just finished.
            // `notified()` re-arms it first.
            biased;
            r = conn.as_mut() => {
                if let Err(e) = r {
                    debug!(peer = %peer, error = %e, "connection");
                }
                return;
            }
            () = flight.idle.notified() => {
                deadline.as_mut().reset(Instant::now() + limits.idle_timeout);
            }
            () = &mut deadline, if flight.is_idle() => {
                if !flight.is_idle() {
                    continue;
                }
                if closing {
                    debug!(peer = %peer, "still idle after asking to close; dropping");
                    return;
                }
                debug!(peer = %peer, "no request open before the deadline; closing");
                conn.as_mut().graceful_shutdown();
                closing = true;
                deadline.as_mut().reset(Instant::now() + limits.idle_timeout);
            }
            () = shutdown.cancelled(), if !stopped => {
                stopped = true;
                if h2 && !closing {
                    // GOAWAY, then the idle bound drops the connection, so a
                    // response body the peer never reads cannot hold it open.
                    conn.as_mut().graceful_shutdown();
                    closing = true;
                    deadline.as_mut().reset(Instant::now() + limits.idle_timeout);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::{Future, poll_fn};
    use std::pin::pin;
    use std::task::Poll;

    use super::*;

    /// After shutdown an HTTP/2 stream whose response body the peer never
    /// accepts (a zero flow-control window) cannot hold the connection open.
    #[tokio::test]
    async fn shutdown_drops_an_h2_connection_stuck_on_a_response_body() {
        let idle = Duration::from_millis(100);
        let limits = Limits {
            idle_timeout: idle,
            ..Limits::default()
        };
        let (server, client) = tokio::io::duplex(64 * 1024);
        let flight = Arc::new(InFlight::default());
        let svc = {
            let flight = Arc::clone(&flight);
            service_fn(move |_: hyper::Request<hyper::body::Incoming>| {
                let active = InFlight::start(&flight);
                async move {
                    drop(active);
                    Ok::<_, std::convert::Infallible>(hyper::Response::new("x".repeat(4096)))
                }
            })
        };
        let conn = http2::Builder::new(TokioExecutor::new())
            .timer(TokioTimer::new())
            .serve_connection(TokioIo::new(server), svc);
        let peer: SocketAddr = ([127, 0, 0, 1], 1).into();
        let stop = CancellationToken::new();
        let driven = tokio::spawn({
            let (flight, stop) = (Arc::clone(&flight), stop.clone());
            async move { drive(conn, peer, &flight, limits, stop, true).await }
        });

        let (mut send, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .initial_stream_window_size(0)
            .handshake::<_, String>(TokioIo::new(client))
            .await
            .unwrap();
        tokio::spawn(async move { drop(conn.await) });
        let resp = send
            .send_request(hyper::Request::new(String::new()))
            .await
            .unwrap();
        assert_eq!(resp.status(), hyper::StatusCode::OK);

        stop.cancel();
        tokio::time::timeout(idle * 20, driven)
            .await
            .expect("the connection outlived shutdown")
            .unwrap();
        drop(resp);
    }

    /// A request that resolves just as the idle deadline elapses must re-arm
    /// the deadline, not lose the race to it and close the connection. `drive`
    /// is polled by hand so that, on the poll under test, the deadline timer
    /// has fired and the idle notification is pending, so both arms are ready, and
    /// an unbiased `select!` picks the deadline in about half of the rounds.
    #[tokio::test(start_paused = true)]
    async fn finished_request_beats_an_elapsed_idle_deadline() {
        let idle = Duration::from_millis(100);
        let limits = Limits {
            idle_timeout: idle,
            ..Limits::default()
        };
        for round in 0..32 {
            let flight = InFlight::default();
            let flight = Arc::new(flight);
            let (server, _client) = tokio::io::duplex(1024);
            let svc = service_fn(|_: hyper::Request<hyper::body::Incoming>| async {
                Ok::<_, std::convert::Infallible>(hyper::Response::new(String::new()))
            });
            let conn = http1::Builder::new().serve_connection(TokioIo::new(server), svc);
            let peer: SocketAddr = ([127, 0, 0, 1], 1).into();
            let mut drive = pin!(drive(
                conn,
                peer,
                &flight,
                limits,
                CancellationToken::new(),
                false
            ));
            let poll_once = async |d: &mut std::pin::Pin<&mut dyn Future<Output = ()>>| {
                poll_fn(|cx| Poll::Ready(d.as_mut().poll(cx).is_ready())).await
            };
            let mut d: std::pin::Pin<&mut dyn Future<Output = ()>> = drive.as_mut();
            assert!(!poll_once(&mut d).await);
            // A request opens and finishes while `drive` is not running...
            drop(InFlight::start(&flight));
            // ...and the deadline elapses (the runtime fires timers on park).
            tokio::time::sleep(idle * 2).await;
            assert!(
                !poll_once(&mut d).await,
                "round {round}: closed after the request"
            );
            // The fresh bound still holds, so the idle connection closes.
            let mut closed = false;
            for _ in 0..8 {
                tokio::time::sleep(idle * 2).await;
                closed = poll_once(&mut d).await;
                if closed {
                    break;
                }
            }
            assert!(closed, "round {round}: never closed");
        }
    }
}
