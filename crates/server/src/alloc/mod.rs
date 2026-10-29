//! Allocation. Every allocation is a `BuildKitClaim`: the claim is the
//! queue entry, and once bound it holds the result.
//!
//! - [`front`]: every replica's stateless front door. The HTTP layer calls
//!   [`AllocService::allocate`], [`AllocService::get`] and
//!   [`AllocService::release`] after authenticating the caller. The front door
//!   does the pool lookup **and the access check**, so a caller cannot skip
//!   it, then creates, reads or deletes the caller's claim in the controller's
//!   namespace.
//! - [`bind`]: the leader's allocator. One pass per pool binds pending claims,
//!   oldest first, to free warm pods and signs their certificates. The claim
//!   controller ([`crate::claim`]) schedules the passes.
//!
//! HTTP counts in `buildkit_allocations_total` only the rejections it makes
//! itself (`unauthenticated`, `payload_too_large`, `unsupported_media_type`,
//! bad JSON). The front door counts the requests it refuses; the leader counts
//! how each claim ends.

pub mod backend;
pub mod bind;
mod cache;
pub mod certs;
pub mod error;
pub mod front;
pub mod metrics;
pub mod pods;

#[cfg(test)]
mod tests;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use buildkit_controller_api::dto::{CreateAllocationRequest, ErrorCode};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

pub use backend::{Backend, KubeBackend};
pub use bind::{BindConfig, Binder};
pub use certs::{CertSource, MaterialSource};
pub use error::AllocError;
pub use front::{Front, Outcome};

use crate::auth::Identity;
use crate::certs::CertMaterial;
use crate::runtime::AppContext;
use crate::runtime::hooks::{BoxResult, Component};
use metrics::AllocMetrics;

/// Handle to allocation; clone it into the HTTP layer.
#[derive(Clone)]
pub struct AllocService {
    material: watch::Receiver<CertMaterial>,
    front: Arc<OnceLock<Arc<Front>>>,
    metrics: Arc<OnceLock<AllocMetrics>>,
}

impl AllocService {
    /// An idle handle over the shared cert material; it serves once
    /// [`Component::run`] has started.
    #[must_use]
    pub fn new(material: watch::Receiver<CertMaterial>) -> Self {
        Self {
            material,
            front: Arc::default(),
            metrics: Arc::default(),
        }
    }

    /// A handle counting into `metrics`; [`Component::run`] then registers and
    /// uses these families instead of fresh ones.
    #[must_use]
    pub fn with_metrics(material: watch::Receiver<CertMaterial>, metrics: AllocMetrics) -> Self {
        let s = Self::new(material);
        let _ = s.metrics.set(metrics);
        s
    }

    /// Count a POST the HTTP layer refused before [`Self::allocate`]
    /// (`unauthenticated`, `unsupported_media_type`, `payload_too_large`,
    /// `invalid_request` for an undecodable body, auth `upstream_error`/`internal`)
    /// in `buildkit_allocations_total`, with empty `namespace` and `pool`,
    /// because no parsed, authorized pool exists yet. A no-op before [`Component::run`].
    pub fn count_rejection(&self, code: ErrorCode) {
        if let Some(m) = self.metrics.get() {
            m.allocation("", "", code.as_str());
        }
    }

    /// True once the service accepts calls.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.front.get().is_some()
    }

    fn front(&self) -> Result<&Arc<Front>, AllocError> {
        self.front.get().ok_or_else(|| {
            error!("allocation service called before it started");
            AllocError::Internal
        })
    }

    /// `POST /v1/allocations` for an authenticated caller.
    ///
    /// # Errors
    /// [`AllocError`], one variant per the API error code.
    pub async fn allocate(
        &self,
        who: &Identity,
        req: &CreateAllocationRequest,
    ) -> Result<Outcome, AllocError> {
        self.front()?.allocate(who, req).await
    }

    /// `GET /v1/allocations/{id}`: owner-only.
    ///
    /// # Errors
    /// `not_found`, `forbidden`, the error of a failed allocation, `upstream_error`, `internal`.
    pub async fn get(
        &self,
        who: &Identity,
        id: &str,
        wait: Duration,
    ) -> Result<Outcome, AllocError> {
        self.front()?.get(who, id, wait).await
    }

    /// `DELETE /v1/allocations/{id}`: owner-only.
    ///
    /// # Errors
    /// `not_found`, `forbidden`, `upstream_error`, `internal`.
    pub async fn release(&self, who: &Identity, alloc_id: &str) -> Result<(), AllocError> {
        self.front()?.release(who, alloc_id).await
    }
}

impl Component for AllocService {
    fn name(&self) -> &'static str {
        "alloc"
    }

    /// The front door on every replica, and the claim controller (the
    /// allocator among it) while this replica leads.
    fn run(&self, ctx: AppContext, shutdown: CancellationToken) -> BoxResult {
        let cell = Arc::clone(&self.front);
        let material = self.material.clone();
        let metrics_cell = Arc::clone(&self.metrics);
        Box::pin(async move {
            let metrics = metrics_cell
                .get_or_init(|| AllocMetrics::with_releases(ctx.metrics().releases()))
                .clone();
            ctx.metrics().with_registry(|r| metrics.register(r));
            let c = ctx.config();
            let timeout = c
                .kube
                .request_timeout
                .seconds()
                .map(Duration::from_secs)
                .map_err(|e| anyhow::anyhow!("kube.requestTimeout: {e}"))?;
            let backend: Arc<dyn Backend> = Arc::new(KubeBackend::new(ctx.client(), timeout));
            let certs: Arc<dyn CertSource> = Arc::new(MaterialSource::new(material));
            let own = ctx.namespace().to_owned();
            let served = c.served_claim_namespaces(&own);
            let sub_ctx = ctx.clone();
            let front = Front::new(
                Arc::clone(&backend),
                ctx.pools(),
                ctx.claims(),
                Box::new(move || sub_ctx.claim_events()),
                Arc::clone(&certs),
                own.clone(),
                served.clone(),
                metrics.clone(),
            )
            .with_stopping(shutdown.clone())
            .with_pods(ctx.pods());
            let binder = Arc::new(Binder::new(
                backend,
                (ctx.pools(), ctx.pods(), ctx.claims()),
                certs,
                BindConfig {
                    endpoint: c.edge.endpoint.clone(),
                    server_name: c.edge.server_name.clone(),
                    allocation_domain: Some(c.edge.allocation_domain.clone())
                        .filter(|d| !d.is_empty()),
                    namespace: own,
                    served,
                },
                metrics,
            ));
            ctx.wait_synced().await;
            let _ = cell.set(Arc::new(front));
            info!(component = "alloc", "allocation service running");
            crate::claim::run(ctx, binder, shutdown).await?;
            Ok(())
        })
    }
}
