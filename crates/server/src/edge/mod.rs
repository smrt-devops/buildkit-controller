//! The edge's TLS front half: listener, rustls server config, client-certificate
//! verification (chain and clientAuth EKU), hot reload and drain.
//!
//! The proxy plugs in by implementing
//! [`ConnectionHandler`]. `admit` runs inside the handshake on the verified
//! [`AllocationId`](crate::pki::AllocationId) and maps a [`Refusal`] to the
//! spec's alert and `result` label; `handle` receives each [`Accepted`]
//! connection (TLS stream, identity, peer) and owns it until it ends.
//! [`ProxyHandler`] is the production handler; [`CloseHandler`] a stub for tests.

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

use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub use handler::{Accepted, CloseHandler, ConnectionHandler, Refusal};
pub use listener::{ListenerOptions, PeerKey, PeerSlot, PerPeer, Serve, serve, tune_socket};
pub use metrics::{Direction, EdgeMetrics, Revocation};
pub use pods::{PodView, revocation};
pub use proxy::{End, ProxyHandler, ProxyOptions, pump};
pub use tls::{EdgeTls, TlsBuilder};
pub use upstream::client_config;

use crate::certs::CertMaterial;
use crate::render::BUILDKIT_PORT;
use crate::runtime::AppContext;
use crate::runtime::hooks::{BoxResult, Component};

/// The runtime's `edge` component.
pub struct EdgeComponent {
    material: watch::Receiver<CertMaterial>,
    /// `None`: the [`ProxyHandler`] over the runtime's stores.
    handler: Option<Arc<dyn ConnectionHandler>>,
}

impl EdgeComponent {
    /// The production edge: proxies to workers.
    #[must_use]
    pub fn proxy(material: watch::Receiver<CertMaterial>) -> Self {
        Self {
            material,
            handler: None,
        }
    }

    /// An edge with a fixed handler (tests).
    #[must_use]
    pub fn new(
        handler: Arc<dyn ConnectionHandler>,
        material: watch::Receiver<CertMaterial>,
    ) -> Self {
        Self {
            material,
            handler: Some(handler),
        }
    }
}

impl Component for EdgeComponent {
    fn name(&self) -> &'static str {
        "edge"
    }

    fn run(&self, ctx: AppContext, shutdown: CancellationToken) -> BoxResult {
        let fixed = self.handler.clone();
        let certs = self.material.clone();
        Box::pin(async move {
            let ready = ctx.readiness().register("edge_listener");
            let metrics = EdgeMetrics::register(ctx.metrics());
            let edge = &ctx.config().edge;
            let options = ListenerOptions {
                handshake_timeout: Duration::from_secs(
                    edge.handshake_timeout.seconds().unwrap_or(10),
                ),
                max_connections: usize::try_from(edge.max_connections).unwrap_or(usize::MAX),
                max_handshakes: usize::try_from(edge.max_handshakes).unwrap_or(usize::MAX),
                max_handshakes_per_ip: usize::try_from(edge.max_handshakes_per_ip)
                    .unwrap_or(usize::MAX),
            };
            let addr = ctx.config().listen.edge;
            let listener = TcpListener::bind(addr).await?;
            let (tx, rx) = watch::channel(None);
            let revoke_stop = CancellationToken::new();
            let mut revoker = None;
            let handler: Arc<dyn ConnectionHandler> = if let Some(h) = fixed {
                h
            } else {
                let proxy = ProxyHandler::new(
                    PodView {
                        pods: ctx.pods(),
                        pools: ctx.pools(),
                        fresh: ctx.store_freshness(),
                    },
                    rx.clone(),
                    metrics.clone(),
                    ProxyOptions {
                        upstream_connect_timeout: Duration::from_secs(
                            edge.upstream_connect_timeout.seconds().unwrap_or(5),
                        ),
                        upstream_port: u16::try_from(BUILDKIT_PORT).unwrap_or(1234),
                    },
                );
                let (p, sub, stop) = (proxy.clone(), ctx.pod_events(), revoke_stop.clone());
                revoker = Some(tokio::spawn(async move { p.revoke_on(sub, stop).await }));
                Arc::new(proxy)
            };
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
            serve(listener, s, shutdown).await;
            material_stop.cancel();
            let _ = material.await;
            revoke_stop.cancel();
            if let Some(r) = revoker {
                let _ = r.await;
            }
            Ok(())
        })
    }
}
