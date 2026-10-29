//! The registry's mTLS listener.
//!
//! A client must present a cert that chains to `ca.crt`, the client CA (rustls checks
//! the chain, validity and that the EKU allows client auth), and the cert must
//! name a worker ([`peer_identity`]). Otherwise the connection closes before
//! any request. HTTP/1.1 only: containerd opens a connection per parallel
//! transfer, and one protocol keeps the bounds simple. The TLS files are
//! re-read every minute, and a new config applies to new connections.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::body::Body;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::{GracefulShutdown, Watcher};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{NoServerSessionStorage, WebPkiClientVerifier};
use rustls::{RootCertStore, ServerConfig};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::api::{Registry, handle};
use super::auth::{PeerError, peer_identity};
use crate::edge::{PerPeer, tune_socket};
use crate::runtime::listen::canonical;

/// Connection bounds.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Open connections; more are closed on accept.
    pub max_connections: usize,
    /// Open connections from one address (0: no cap).
    pub max_connections_per_ip: usize,
    pub handshake_timeout: Duration,
    /// Request head, also the keep-alive idle bound between requests.
    pub header_timeout: Duration,
    /// How long open requests may finish after shutdown before they are cut.
    pub drain_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 1024,
            // A worker opens about as many as containerd transfers in
            // parallel. The cap is taken before the handshake, so it must be
            // well below the total: at 256, four addresses filled it.
            max_connections_per_ip: 64,
            handshake_timeout: Duration::from_secs(5),
            header_timeout: Duration::from_secs(30),
            drain_timeout: Duration::from_secs(30),
        }
    }
}

/// Request head bound.
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// How often the TLS files are re-read.
const RELOAD_EVERY: Duration = Duration::from_mins(1);

/// TLS 1.2 and 1.3, a client cert chaining to `client_ca` required, ALPN
/// `http/1.1`, and no session resumption (a resumed session skips the verifier).
pub fn server_config(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    client_ca: CertificateDer<'static>,
) -> anyhow::Result<Arc<ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    roots.add(client_ca).context("client CA")?;
    let verifier =
        WebPkiClientVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
            .build()
            .context("client verifier")?;
    let mut c = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain, key)
        .context("serving cert and key")?;
    c.session_storage = Arc::new(NoServerSessionStorage {});
    c.send_tls13_tickets = 0;
    c.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(c))
}

/// The raw bytes of `tls.crt`, `tls.key` and `ca.crt` under `dir`.
fn read(dir: &Path) -> anyhow::Result<[Vec<u8>; 3]> {
    let f = |n: &str| {
        let p = dir.join(n);
        std::fs::read(&p).with_context(|| format!("reading {}", p.display()))
    };
    Ok([f("tls.crt")?, f("tls.key")?, f("ca.crt")?])
}

fn build(files: &[Vec<u8>; 3]) -> anyhow::Result<Arc<ServerConfig>> {
    let [crt, key, ca] = files;
    let chain = CertificateDer::pem_slice_iter(crt)
        .collect::<Result<Vec<_>, _>>()
        .context("tls.crt")?;
    let key = PrivateKeyDer::from_pem_slice(key).context("tls.key")?;
    let ca = CertificateDer::from_pem_slice(ca).context("ca.crt")?;
    server_config(chain, key, ca)
}

/// The TLS config in `dir`, to start with.
pub fn load(dir: &Path) -> anyhow::Result<([Vec<u8>; 3], Arc<ServerConfig>)> {
    let files = read(dir)?;
    let c = build(&files)?;
    Ok((files, c))
}

/// Re-reads `dir` every minute until `stop` and publishes each changed config
/// that builds. One that does not build leaves the previous one in place.
pub async fn follow(
    dir: PathBuf,
    tx: watch::Sender<Arc<ServerConfig>>,
    mut last: [Vec<u8>; 3],
    stop: CancellationToken,
) {
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(RELOAD_EVERY) => {}
        }
        let files = match read(&dir) {
            Ok(f) if f == last => continue,
            Ok(f) => f,
            Err(e) => {
                warn!(
                    error = format!("{e:#}"),
                    "cache TLS files unreadable; keeping the previous"
                );
                continue;
            }
        };
        match build(&files) {
            Ok(c) => {
                tx.send_replace(c);
                last = files;
                info!("cache TLS config reloaded");
            }
            Err(e) => warn!(
                error = format!("{e:#}"),
                "cache TLS config not built; keeping the previous"
            ),
        }
    }
}

/// Serves `reg` on `listener` until `shutdown`, then lets open requests
/// finish for `limits.drain_timeout` and cuts the rest.
pub async fn serve(
    listener: TcpListener,
    tls: watch::Receiver<Arc<ServerConfig>>,
    reg: Arc<Registry>,
    limits: Limits,
    shutdown: CancellationToken,
) {
    let permits = Arc::new(Semaphore::new(limits.max_connections.max(1)));
    let per_peer = PerPeer::new(limits.max_connections_per_ip);
    let graceful = GracefulShutdown::new();
    let mut conns = JoinSet::new();
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
            accepted = listener.accept() => {
                let (tcp, peer) = match accepted {
                    Ok((tcp, peer)) => (tcp, canonical(peer)),
                    Err(e) => {
                        debug!(error = %e, "cache accept");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                    debug!(peer = %peer, "cache connection limit reached; closing");
                    continue;
                };
                let Some(slot) = per_peer.try_take(peer.ip()) else {
                    debug!(peer = %peer, "cache per-address connection limit reached; closing");
                    continue;
                };
                let config = Arc::clone(&tls.borrow());
                let (reg, watcher) = (Arc::clone(&reg), graceful.watcher());
                conns.spawn(async move {
                    connection(tcp, peer, config, reg, limits, watcher).await;
                    drop((permit, slot));
                });
            }
        }
    }
    drop(listener);
    tokio::select! {
        () = graceful.shutdown() => {}
        () = tokio::time::sleep(limits.drain_timeout) => {
            debug!("cache drain timed out; cutting open connections");
        }
    }
    conns.abort_all();
    while conns.join_next().await.is_some() {}
}

async fn connection(
    tcp: TcpStream,
    peer: SocketAddr,
    tls: Arc<ServerConfig>,
    reg: Arc<Registry>,
    limits: Limits,
    watcher: Watcher,
) {
    if let Err(e) = tune_socket(&tcp) {
        debug!(error = %e, "socket options");
    }
    let accept = TlsAcceptor::from(tls).accept(tcp);
    let stream = match tokio::time::timeout(limits.handshake_timeout, accept).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            reg.metrics.refused("tls_error");
            debug!(peer = %peer, error = %e, "cache TLS handshake");
            return;
        }
        Err(_) => {
            reg.metrics.refused("handshake_timeout");
            debug!(peer = %peer, "cache TLS handshake timed out");
            return;
        }
    };
    let id = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|c| c.first())
        .ok_or(PeerError::Parse)
        .and_then(peer_identity);
    let id = match id {
        Ok(id) => Arc::new(id),
        Err(e) => {
            reg.metrics.refused(match e {
                PeerError::Parse => "bad_certificate",
                PeerError::ClientAuthEku => "client_auth_eku",
                PeerError::BadSan => "bad_san",
            });
            debug!(peer = %peer, error = %e, "cache client certificate names no worker; closing");
            return;
        }
    };
    let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let (reg, id) = (Arc::clone(&reg), Arc::clone(&id));
        async move { Ok::<_, Infallible>(handle(&reg, &id, req.map(Body::new)).await) }
    });
    let conn = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_timeout)
        .max_buf_size(MAX_HEADER_BYTES)
        .serve_connection(TokioIo::new(stream), svc);
    if let Err(e) = watcher.watch(conn).await {
        debug!(peer = %peer, error = %e, "cache connection");
    }
}
