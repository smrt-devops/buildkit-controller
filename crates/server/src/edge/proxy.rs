//! The edge's back half: admission against
//! the Pod store, the upstream mTLS leg, byte copying, the per-connection
//! `notAfter` timer, revocation and drain accounting.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use prometheus_client::metrics::counter::Counter;
use rustls::ClientConfig;
use time::OffsetDateTime;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use x509_parser::prelude::{FromDer, X509Certificate};

use super::handler::{Accepted, ConnectionHandler, Refusal};
use super::metrics::{Direction, EdgeMetrics, Revocation};
use super::pods::PodView;
use super::revoke::Registry;
use super::tls::EdgeTls;
use super::upstream::{dial, worker_name};
use crate::pki::AllocationId;
use crate::runtime::stores::PodSubscription;

/// Copy buffer per direction.
const BUF: usize = 64 * 1024;
/// Bound on sending `close_notify` to both legs once a connection ends.
const CLOSE_GRACE: Duration = Duration::from_secs(1);

/// Proxy limits from `edge.*`.
#[derive(Debug, Clone, Copy)]
pub struct ProxyOptions {
    /// `edge.upstreamConnectTimeout`: TCP connect + TLS to the worker.
    pub upstream_connect_timeout: Duration,
    /// The worker's buildkitd port (1234; tests override it).
    pub upstream_port: u16,
}

/// The [`ConnectionHandler`] the binary runs.
#[derive(Clone)]
pub struct ProxyHandler {
    inner: Arc<Inner>,
}

struct Inner {
    view: PodView,
    tls: watch::Receiver<Option<Arc<EdgeTls>>>,
    metrics: EdgeMetrics,
    options: ProxyOptions,
    live: Arc<Registry>,
}

impl ProxyHandler {
    /// `tls` is the generation the listener uses; its `upstream` config is
    /// the worker-leg client config.
    #[must_use]
    pub fn new(
        view: PodView,
        tls: watch::Receiver<Option<Arc<EdgeTls>>>,
        metrics: EdgeMetrics,
        options: ProxyOptions,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                view,
                tls,
                metrics,
                options,
                live: Arc::new(Registry::default()),
            }),
        }
    }

    /// Cancels live connections from Pod events until `stop`.
    pub async fn revoke_on(&self, sub: PodSubscription, stop: CancellationToken) {
        self.inner
            .live
            .follow(self.inner.view.clone(), sub, stop)
            .await;
    }

    /// Open proxied connections (tests, debugging).
    #[must_use]
    pub fn live_connections(&self) -> usize {
        self.inner.live.len()
    }
}

impl ConnectionHandler for ProxyHandler {
    fn admit(&self, id: &AllocationId) -> Result<(), Refusal> {
        self.inner
            .view
            .resolve(id, OffsetDateTime::now_utc())
            .map(|_| ())
    }

    fn handle(&self, conn: Accepted) -> BoxFuture<'static, ()> {
        Box::pin(Arc::clone(&self.inner).serve(conn))
    }
}

impl Inner {
    /// The worker-leg client config of the current generation. The one place
    /// the proxy obtains the edge client cert.
    fn upstream_config(&self) -> Option<Arc<ClientConfig>> {
        self.tls.borrow().as_ref()?.upstream.clone()
    }

    async fn serve(self: Arc<Self>, conn: Accepted) {
        let Accepted { stream, id, .. } = conn;
        let mut client = stream;
        // The deadline is absolute and fixed at accept, so dial time cannot stretch the session.
        let not_after = leaf_not_after(&client);
        // Index first, so a revocation racing the re-resolve below still lands.
        let (_reg, revoked) = self.live.register(&id);
        let target = match self.view.resolve(&id, OffsetDateTime::now_utc()) {
            Ok(t) => t,
            Err(r) => {
                self.metrics.outcome(Some(&id), r.result_label());
                debug!(result = r.result_label(), "forwarding refused");
                let _ = tokio::time::timeout(CLOSE_GRACE, client.shutdown()).await;
                return;
            }
        };
        let start = Instant::now();
        let addr = SocketAddr::new(target.ip, self.options.upstream_port);
        let dialed = match (self.upstream_config(), worker_name(&target.worker)) {
            (Some(cfg), Ok(name)) => {
                tokio::time::timeout(self.options.upstream_connect_timeout, dial(addr, cfg, name))
                    .await
                    .map_err(|_| "upstream connect timed out".to_owned())
                    .and_then(|r| r.map_err(|e| e.to_string()))
            }
            (None, _) => Err("edge client cert not loaded".to_owned()),
            (_, Err(e)) => Err(e.to_string()),
        };
        let mut upstream = match dialed {
            Ok(s) => s,
            Err(error) => {
                self.metrics.outcome(Some(&id), "upstream_error");
                warn!(%addr, %error, "upstream connect failed");
                let _ = tokio::time::timeout(CLOSE_GRACE, client.shutdown()).await;
                return;
            }
        };
        self.metrics.outcome(Some(&id), "ok");
        self.metrics
            .upstream_connected(start.elapsed().as_secs_f64());
        info!(%addr, "edge connection proxied");

        let active = self.metrics.active(&id);
        active.inc();
        let guard = DrainGuard(Some(self.metrics.clone()));
        let end = pump(
            &mut client,
            &mut upstream,
            &self.metrics,
            not_after,
            revoked,
        )
        .await;
        guard.disarm();
        active.dec();
        match end {
            End::Closed(Ok((up, down))) => debug!(up, down, "edge connection closed"),
            End::Closed(Err(e)) => debug!(error = %e, "edge connection ended"),
            End::Revoked(r) => {
                self.metrics.revoked(r);
                info!(reason = r.label(), "edge connection revoked");
            }
        }
        let _ = tokio::time::timeout(CLOSE_GRACE, async {
            let _ = client.shutdown().await;
            let _ = upstream.shutdown().await;
        })
        .await;
    }
}

/// How [`pump`] ended.
#[derive(Debug)]
pub enum End {
    /// One side closed or errored; the byte totals on success.
    Closed(io::Result<(u64, u64)>),
    /// Cut before either side closed.
    Revoked(Revocation),
}

/// Copies both ways (half-close preserved, 64 KiB buffers) until a side
/// ends, `revoked` fires, or the absolute `not_after` instant (the cert's
/// notAfter, fixed at accept time) is reached.
pub async fn pump<C, U>(
    client: &mut C,
    upstream: &mut U,
    metrics: &EdgeMetrics,
    not_after: Option<tokio::time::Instant>,
    revoked: oneshot::Receiver<Revocation>,
) -> End
where
    C: AsyncRead + AsyncWrite + Unpin + ?Sized,
    U: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let mut c = Counted::new(client, metrics.bytes(Direction::ClientToWorker));
    let mut u = Counted::new(upstream, metrics.bytes(Direction::WorkerToClient));
    let deadline = async {
        match not_after {
            Some(at) => tokio::time::sleep_until(at).await,
            None => std::future::pending().await,
        }
    };
    let revoked = async {
        match revoked.await {
            Ok(r) => r,
            // The registry dropped the sender without a reason, so this never fires.
            Err(_) => std::future::pending().await,
        }
    };
    tokio::select! {
        r = tokio::io::copy_bidirectional_with_sizes(&mut c, &mut u, BUF, BUF) => End::Closed(r),
        r = revoked => End::Revoked(r),
        () = deadline => End::Revoked(Revocation::Expired),
    }
}

/// The verified client leaf's notAfter as a tokio instant, computed now
/// (at accept); `None` if unreadable. A past notAfter maps to now.
fn leaf_not_after<S>(s: &tokio_rustls::server::TlsStream<S>) -> Option<tokio::time::Instant> {
    let leaf = s.get_ref().1.peer_certificates()?.first()?;
    let (_, cert) = X509Certificate::from_der(leaf).ok()?;
    let at = cert.validity().not_after.timestamp();
    let left = at.saturating_sub(OffsetDateTime::now_utc().unix_timestamp());
    let left = Duration::from_secs(u64::try_from(left).unwrap_or(0));
    Some(tokio::time::Instant::now() + left)
}

/// Counts a connection aborted at the drain deadline (dropped mid-copy).
struct DrainGuard(Option<EdgeMetrics>);

impl DrainGuard {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for DrainGuard {
    fn drop(&mut self) {
        if let Some(m) = self.0.take() {
            m.revoked(Revocation::Drain);
        }
    }
}

/// Counts bytes read from `S` as they flow, so revoked and aborted
/// connections are counted too.
struct Counted<'a, S: ?Sized> {
    inner: &'a mut S,
    read: Counter,
}

impl<'a, S: ?Sized> Counted<'a, S> {
    fn new(inner: &'a mut S, read: Counter) -> Self {
        Self { inner, read }
    }
}

impl<S: AsyncRead + Unpin + ?Sized> AsyncRead for Counted<'_, S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut *self.inner).poll_read(cx, buf);
        let n = buf.filled().len().saturating_sub(before);
        if n > 0 {
            self.read.inc_by(n as u64);
        }
        r
    }
}

impl<S: AsyncWrite + Unpin + ?Sized> AsyncWrite for Counted<'_, S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
#[path = "proxy_tests.rs"]
mod tests;
