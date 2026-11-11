//! The edge's TLS front half: listener, rustls server config, client-certificate
//! verification (chain and clientAuth EKU), hot reload and drain.
//!
//! The proxy plugs in by implementing
//! [`ConnectionHandler`]. `admit` runs inside the handshake on the verified
//! [`AllocationId`](crate::pki::AllocationId) and maps a [`Refusal`] to the
//! spec's alert and `result` label; `handle` receives each [`Accepted`]
//! connection (TLS stream, identity, peer) and owns it until it ends.
//! [`ProxyHandler`] is the handler.
//!
//! rustls verifies the client certificate before the client's `CertificateVerify`
//! signature, so `admit` runs before the peer has proved it holds the key. A peer
//! with a copy of an allocation certificate but not its key learns from the
//! alert whether that allocation is still live, and its attempts are counted
//! under the allocation's labels. It gains no access: the handshake then fails
//! on the signature. This is the price of refusing inside the handshake with
//! the spec's alerts.

mod handler;
mod listener;
mod material;
mod metrics;
mod pods;
mod proxy;
mod revoke;
mod tls;
mod upstream;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub use handler::{Accepted, BoxIo, ClientIo, ConnectionHandler, Refusal};
pub use listener::{
    Arrival, EdgeClosed, ListenerOptions, PeerKey, PeerSlot, PerPeer, Serve, TunnelDoor,
    TunnelQueue, serve, serve_with_proxy, serve_with_tunnels, tune_socket, tunnel_channel,
};
pub use metrics::{Direction, EdgeMetrics, Revocation, Transport};
pub use pods::{PodView, revocation};
pub use proxy::{End, ProxyHandler, ProxyOptions, pump};
pub use tls::{EdgeTls, TlsBuilder};
pub use upstream::client_config;

use crate::alloc::{EdgeActivity, kube_backend};
use crate::certs::CertMaterial;
use crate::net::ProxyProtocol;
use crate::render::BUILDKIT_PORT;
use crate::runtime::AppContext;
use crate::runtime::hooks::{BoxResult, Component};
use buildkit_controller_api::v1alpha1::Duration as CrdDuration;

/// The runtime's `edge` component.
pub struct EdgeComponent {
    material: watch::Receiver<CertMaterial>,
    /// Taken by the one run.
    tunnels: std::sync::Mutex<Option<TunnelQueue>>,
}

impl EdgeComponent {
    /// The production edge: proxies to workers, from its TCP listener and
    /// from the tunnels the API hands over through `tunnels`.
    #[must_use]
    pub fn proxy(material: watch::Receiver<CertMaterial>, tunnels: TunnelQueue) -> Self {
        Self {
            material,
            tunnels: std::sync::Mutex::new(Some(tunnels)),
        }
    }
}

/// `key` from the config. Validation has already rejected a value that does not
/// parse, so the error is unreachable short of a validation bug.
fn secs(d: &CrdDuration, key: &str) -> anyhow::Result<Duration> {
    let s = d.seconds().with_context(|| key.to_owned())?;
    Ok(Duration::from_secs(s))
}

impl Component for EdgeComponent {
    fn name(&self) -> &'static str {
        "edge"
    }

    fn run(&self, ctx: AppContext, shutdown: CancellationToken) -> BoxResult {
        let certs = self.material.clone();
        let tunnels = self
            .tunnels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        Box::pin(async move {
            let ready = ctx.readiness().register("edge_listener");
            let metrics = EdgeMetrics::register(ctx.metrics());
            let edge = &ctx.config().edge;
            let options = ListenerOptions {
                handshake_timeout: secs(&edge.handshake_timeout, "edge.handshakeTimeout")?,
                max_connections: usize::try_from(edge.max_connections).unwrap_or(usize::MAX),
                max_handshakes: usize::try_from(edge.max_handshakes).unwrap_or(usize::MAX),
                max_handshakes_per_ip: usize::try_from(edge.max_handshakes_per_ip)
                    .unwrap_or(usize::MAX),
            };
            let proxy_protocol =
                ProxyProtocol::from_config(&edge.proxy_protocol, ctx.metrics(), "edge")?;
            if proxy_protocol.is_on() {
                info!(trusted = ?proxy_protocol.trusted(), "edge reads PROXY protocol v2 from trusted sources");
            }
            let addr = ctx.config().listen.edge;
            let listener = crate::runtime::listen::bind(addr)?;
            let (tx, rx) = watch::channel(None);
            let proxy = ProxyHandler::new(
                PodView {
                    pods: ctx.pods(),
                    pools: ctx.pools(),
                    fresh: ctx.store_freshness(),
                },
                rx.clone(),
                metrics.clone(),
                ProxyOptions {
                    upstream_connect_timeout: secs(
                        &edge.upstream_connect_timeout,
                        "edge.upstreamConnectTimeout",
                    )?,
                    upstream_port: u16::try_from(BUILDKIT_PORT)?,
                    max_per_namespace: revoke::namespace_cap(options.max_connections),
                },
            );
            let revoke_stop = CancellationToken::new();
            let revoker = tokio::spawn({
                let (p, sub, stop) = (proxy.clone(), ctx.pod_events(), revoke_stop.clone());
                async move { p.revoke_on(sub, stop).await }
            });
            let activity = EdgeActivity::new(
                kube_backend(&ctx)?,
                ctx.claims(),
                ctx.namespace().to_owned(),
            );
            let recorder = tokio::spawn({
                let (p, stop) = (proxy.clone(), revoke_stop.clone());
                async move { p.record_activity(activity, stop).await }
            });
            let handler: Arc<dyn ConnectionHandler> = Arc::new(proxy);
            let builder = TlsBuilder::new(Arc::clone(&handler), metrics.clone());
            let material_stop = CancellationToken::new();
            let material =
                tokio::spawn(material::follow(certs, builder, tx, material_stop.clone()));
            let s = Serve {
                tls: rx,
                handler,
                metrics,
                options,
                ready,
            };
            match tunnels {
                Some(t) => serve_with_tunnels(listener, s, proxy_protocol, t, shutdown).await,
                None => serve_with_proxy(listener, s, proxy_protocol, shutdown).await,
            }
            material_stop.cancel();
            let _ = material.await;
            revoke_stop.cancel();
            let _ = revoker.await;
            let _ = recorder.await;
            Ok(())
        })
    }
}
