//! Public HTTPS API, on every replica.
//!
//! [`HttpComponent`] binds `listen.api`, serves the `/v1` routes of [`api`]
//! over TLS with the edge's serving cert, and marks `api_listener` ready once
//! bound. The serving cert comes from the certs component (one loader for API
//! and edge), mapped onto a [`ServingCerts`] watch channel, and is
//! applied per connection, so a rotation needs no restart.

pub mod api;
pub mod body;
pub mod listener;
pub mod metrics;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub use api::{Allocator, ApiState, Authenticate, EdgeInfo, Grant, router};
pub use listener::{Limits, ServingCerts, Stopping, serve, server_config};
pub use metrics::HttpMetrics;

use crate::alloc::AllocService;
use crate::auth::{
    AuthConfig, Authenticator, KubeTokenReviewer, OidcIssuer, https_client, public_roots,
};
use crate::certs::CertMaterial;
use crate::runtime::AppContext;
use crate::runtime::hooks::{BoxResult, Component};

/// The `http` hook.
pub struct HttpComponent {
    alloc: AllocService,
    certs: watch::Receiver<CertMaterial>,
}

impl HttpComponent {
    /// `certs` is the certs component's channel ([`crate::certs::Certs::subscribe`]):
    /// the API serves the same cert as the edge, swapped per connection.
    #[must_use]
    pub fn new(alloc: AllocService, certs: watch::Receiver<CertMaterial>) -> Self {
        Self { alloc, certs }
    }
}

impl Component for HttpComponent {
    fn name(&self) -> &'static str {
        "http"
    }

    fn run(&self, ctx: AppContext, shutdown: CancellationToken) -> BoxResult {
        let alloc = self.alloc.clone();
        let material = self.certs.clone();
        Box::pin(async move {
            let ready = ctx.readiness().register("api_listener");
            let metrics = HttpMetrics::register(ctx.metrics());
            let c = ctx.config();
            let oidc: Vec<OidcIssuer> = c
                .auth
                .oidc
                .iter()
                .map(|o| {
                    Ok(OidcIssuer {
                        url: o.url.clone(),
                        audiences: o.audiences.clone(),
                        jwks_url: Some(o.jwks_url.clone()).filter(|u| !u.is_empty()),
                        ca_certs: o
                            .ca_certs()
                            .map_err(|e| anyhow::anyhow!("auth.oidc {}: {e}", o.url))?,
                    })
                })
                .collect::<anyhow::Result<_>>()?;
            let sa = &c.auth.service_account;
            if sa.enabled {
                refuse_api_audience(&sa.audiences)?;
            }
            let cfg = AuthConfig {
                oidc: oidc.clone(),
                service_account_audiences: sa.enabled.then(|| sa.audiences.clone()),
            };
            let observer = metrics.clone();
            let auth = Authenticator::new(
                cfg,
                https_client(public_roots())?,
                KubeTokenReviewer::new(ctx.client(), c.request_timeout()),
            )?
            .with_max_concurrent_reviews(
                usize::try_from(sa.max_concurrent_reviews).unwrap_or(usize::MAX),
            )
            .with_max_reviews_per_ip(usize::try_from(sa.max_reviews_per_ip).unwrap_or(usize::MAX))
            .with_reserved_reviews(
                usize::try_from(sa.reserved_reviews).unwrap_or(usize::MAX),
                std::time::Duration::from_secs(sa.recent_subject_ttl.seconds().unwrap_or(600)),
            )
            .with_jwks_observer(move |issuer, r| observer.jwks(issuer, r));
            let state = Arc::new(ApiState {
                alloc,
                auth: Arc::new(auth),
                oidc_issuers: oidc.into_iter().map(|o| o.url).collect::<HashSet<_>>(),
                service_accounts: sa.enabled,
                pools: ctx.pools(),
                edge: EdgeInfo {
                    endpoint: c.edge.endpoint.clone(),
                    server_name: c.edge.server_name.clone(),
                    per_allocation_endpoints: !c.edge.allocation_domain.is_empty(),
                },
                metrics,
                refusals: api::LogSampler::default(),
            });
            let follower = CancellationToken::new();
            let certs = serving_of(material, follower.clone());
            let addr = c.listen.api;
            let listener = TcpListener::bind(addr)
                .await
                .with_context(|| format!("binding listen.api {addr}"))?;
            ready.set();
            info!(component = "http", addr = %addr, "API listening");
            let limits = Limits {
                max_connections: usize::try_from(c.api.max_connections).unwrap_or(usize::MAX),
                max_connections_per_ip: usize::try_from(c.api.max_connections_per_ip)
                    .unwrap_or(usize::MAX),
                ..Limits::default()
            };
            serve(listener, router(state), certs, limits, shutdown).await;
            follower.cancel();
            Ok(())
        })
    }
}

/// The serving cert of each [`CertMaterial`] generation, until `stop`.
fn serving_of(
    mut material: watch::Receiver<CertMaterial>,
    stop: CancellationToken,
) -> ServingCerts {
    let (tx, rx) = watch::channel(material.borrow_and_update().serving());
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = stop.cancelled() => return,
                r = material.changed() => if r.is_err() { return },
            }
            tx.send_replace(material.borrow_and_update().serving());
        }
    });
    rx
}

/// Fails startup when an SA audience is one the controller's own token carries,
/// i.e. the API server's audience.
fn refuse_api_audience(audiences: &[String]) -> anyhow::Result<()> {
    match std::fs::read_to_string(crate::auth::OWN_TOKEN_PATH) {
        Ok(own) => {
            let overlap = crate::auth::api_audience_overlap(&own, audiences);
            anyhow::ensure!(
                overlap.is_empty(),
                "auth.serviceAccount.audiences {overlap:?} include the API server's audience; \
                 every pod's default token would authenticate"
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the own service-account token; API server audience not checked");
        }
    }
    Ok(())
}
