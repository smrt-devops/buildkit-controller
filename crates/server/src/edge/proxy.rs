//! The edge's back half: admission against
//! the Pod store, the upstream mTLS leg, byte copying, the per-connection
//! `notAfter` timer, revocation and drain accounting.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::future::BoxFuture;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use rustls::ClientConfig;
use time::OffsetDateTime;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use x509_parser::prelude::{FromDer, X509Certificate};

use super::handler::{Accepted, ConnectionHandler, Refusal};
use super::listener::PerKey;
use super::metrics::{Direction, EdgeMetrics, Revocation};
use super::pods::PodView;
use super::revoke::{MAX_CONNECTIONS_PER_ALLOCATION, Registry};
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
    /// Proxied connections one pool namespace (from the allocation
    /// certificate) may hold on this replica; 0 disables the cap.
    pub max_per_namespace: usize,
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
    namespaces: PerKey<String>,
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
                live: Arc::new(Registry::new(MAX_CONNECTIONS_PER_ALLOCATION)),
                namespaces: PerKey::new(options.max_per_namespace),
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
        // The deadline is absolute and fixed at accept, so dial time cannot stretch the session.
        let not_after = leaf_not_after(&stream);
        self.forward(stream, &id, not_after).await;
    }

    /// Everything after the client handshake, over any client stream.
    async fn forward<C>(&self, mut client: C, id: &AllocationId, not_after: Option<Instant>)
    where
        C: AsyncRead + AsyncWrite + Unpin,
    {
        // Index first, so a revocation racing the re-resolve below still lands.
        let Some((_reg, revoked)) = self.live.register(id) else {
            self.metrics.outcome(Some(id), "allocation_limit");
            warn!("edge per-allocation connection limit reached; closing");
            let _ = tokio::time::timeout(CLOSE_GRACE, client.shutdown()).await;
            return;
        };
        let target = match self.view.resolve(id, OffsetDateTime::now_utc()) {
            Ok(t) => t,
            Err(r) => {
                self.metrics.outcome(Some(id), r.result_label());
                debug!(result = r.result_label(), "forwarding refused");
                let _ = tokio::time::timeout(CLOSE_GRACE, client.shutdown()).await;
                return;
            }
        };
        // One tenant's allocations together may not fill the replica.
        let Some(_ns) = self.namespaces.try_take(id.namespace.as_str()) else {
            self.metrics.outcome(Some(id), "namespace_limit");
            warn!("edge per-namespace connection limit reached; closing");
            let _ = tokio::time::timeout(CLOSE_GRACE, client.shutdown()).await;
            return;
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
                self.metrics.outcome(Some(id), "upstream_error");
                warn!(%addr, %error, "upstream connect failed");
                let _ = tokio::time::timeout(CLOSE_GRACE, client.shutdown()).await;
                return;
            }
        };
        self.metrics.outcome(Some(id), "ok");
        self.metrics
            .upstream_connected(start.elapsed().as_secs_f64());
        info!(%addr, "edge connection proxied");
        relay(
            &self.metrics,
            id,
            &mut client,
            &mut upstream,
            not_after,
            revoked,
        )
        .await;
        let _ = tokio::time::timeout(CLOSE_GRACE, async {
            let _ = client.shutdown().await;
            let _ = upstream.shutdown().await;
        })
        .await;
    }
}

/// [`pump`] with its accounting: the active gauge while it runs, the
/// revocation counter when it is cut, and `drain` when the future is dropped
/// mid-copy (the drain deadline aborting the edge).
async fn relay<C, U>(
    metrics: &EdgeMetrics,
    id: &AllocationId,
    client: &mut C,
    upstream: &mut U,
    not_after: Option<Instant>,
    revoked: oneshot::Receiver<Revocation>,
) where
    C: AsyncRead + AsyncWrite + Unpin + ?Sized,
    U: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let active = Active::start(metrics, id);
    let end = pump(client, upstream, metrics, not_after, revoked).await;
    active.end();
    match end {
        End::Closed(Ok((up, down))) => debug!(up, down, "edge connection closed"),
        End::Closed(Err(e)) => debug!(error = %e, "edge connection ended"),
        End::Revoked(r) => {
            metrics.revoked(r);
            info!(reason = r.label(), "edge connection revoked");
        }
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
    not_after: Option<Instant>,
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
fn leaf_not_after<S>(s: &tokio_rustls::server::TlsStream<S>) -> Option<Instant> {
    let leaf = s.get_ref().1.peer_certificates()?.first()?;
    let (_, cert) = X509Certificate::from_der(leaf).ok()?;
    let at = cert.validity().not_after.timestamp();
    let left = at.saturating_sub(OffsetDateTime::now_utc().unix_timestamp());
    let left = Duration::from_secs(u64::try_from(left).unwrap_or(0));
    Some(Instant::now() + left)
}

/// One connection in `edge_active_connections`, taken out again on drop. Dropped
/// without [`Active::end`], the connection was aborted mid-copy by the drain
/// deadline and is counted as a `drain` revocation.
struct Active {
    gauge: Gauge,
    drained: Option<EdgeMetrics>,
}

impl Active {
    fn start(metrics: &EdgeMetrics, id: &AllocationId) -> Self {
        let gauge = metrics.active(id);
        gauge.inc();
        Self {
            gauge,
            drained: Some(metrics.clone()),
        }
    }

    fn end(mut self) {
        self.drained = None;
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        self.gauge.dec();
        if let Some(m) = self.drained.take() {
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
