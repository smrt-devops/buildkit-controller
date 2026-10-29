//! Allocation. Every allocation is a `BuildKitClaim`: the claim is the
//! queue entry, and once bound it holds the result.
//!
//! - [`front`]: every replica's stateless front door. The HTTP layer calls
//!   [`AllocService::allocate`], [`AllocService::get`] and
//!   [`AllocService::release`] after authenticating the caller. The front door
//!   does the pool lookup **and the access check**, so a caller cannot skip
//!   it, then creates, reads or deletes the caller's claim in the controller's
//!   namespace.
//! - [`leader`]: the [`Allocator`] component, which runs only on the leader.
//!   It schedules [`bind`] passes, which bind pending claims, oldest first, to
//!   free warm pods, and tidies claims that ended or lost their pod.
//! - [`claims`]: pure decisions about claims. [`pods`]: pure pod logic.
//!   [`pool`]: pool access rules, readiness and durations.
//!
//! HTTP counts in `buildkit_allocations_total` only the rejections it makes
//! itself (`unauthenticated`, `payload_too_large`, `unsupported_media_type`,
//! bad JSON). The front door counts the requests it refuses; the leader counts
//! how each claim ends.

pub mod backend;
pub mod bind;
mod cache;
pub mod certs;
pub mod claims;
pub mod error;
pub mod front;
pub mod leader;
pub mod metrics;
pub mod pods;
pub mod pool;

#[cfg(test)]
mod tests;

use std::sync::{Arc, Once, OnceLock};
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
pub use leader::Allocator;

use crate::auth::Identity;
use crate::certs::CertMaterial;
use crate::runtime::AppContext;
use crate::runtime::hooks::{BoxResult, Component};
use metrics::AllocMetrics;

/// What the front door and the [`Allocator`] share.
#[derive(Clone)]
struct Shared {
    material: watch::Receiver<CertMaterial>,
    metrics: Arc<OnceLock<AllocMetrics>>,
    registered: Arc<Once>,
}

impl Shared {
    /// The alloc metric families, registered once per process.
    fn metrics(&self, ctx: &AppContext) -> AllocMetrics {
        let m = self
            .metrics
            .get_or_init(|| AllocMetrics::with_releases(ctx.metrics().releases()))
            .clone();
        self.registered
            .call_once(|| ctx.metrics().with_registry(|r| m.register(r)));
        m
    }

    /// The API server and certificate sources both components use.
    fn sources(&self, ctx: &AppContext) -> anyhow::Result<(Arc<dyn Backend>, Arc<dyn CertSource>)> {
        let timeout = ctx
            .config()
            .kube
            .request_timeout
            .seconds()
            .map(Duration::from_secs)
            .map_err(|e| anyhow::anyhow!("kube.requestTimeout: {e}"))?;
        Ok((
            Arc::new(KubeBackend::new(ctx.client(), timeout)),
            Arc::new(MaterialSource::new(self.material.clone())),
        ))
    }
}

/// Handle to allocation; clone it into the HTTP layer.
#[derive(Clone)]
pub struct AllocService {
    shared: Shared,
    front: Arc<OnceLock<Arc<Front>>>,
}

impl AllocService {
    /// An idle handle over the shared cert material; it serves once
    /// [`Component::run`] has started.
    #[must_use]
    pub fn new(material: watch::Receiver<CertMaterial>) -> Self {
        Self {
            shared: Shared {
                material,
                metrics: Arc::default(),
                registered: Arc::new(Once::new()),
            },
            front: Arc::default(),
        }
    }

    /// A handle counting into `metrics`; [`Component::run`] then registers and
    /// uses these families instead of fresh ones.
    #[must_use]
    pub fn with_metrics(material: watch::Receiver<CertMaterial>, metrics: AllocMetrics) -> Self {
        let s = Self::new(material);
        let _ = s.shared.metrics.set(metrics);
        s
    }

    /// The leader's half of allocation, sharing this handle's cert material
    /// and metrics. `main` runs it in the leader stage.
    #[must_use]
    pub fn allocator(&self) -> Allocator {
        Allocator::new(self.shared.clone())
    }

    /// Count a POST the HTTP layer refused before [`Self::allocate`]
    /// (`unauthenticated`, `unsupported_media_type`, `payload_too_large`,
    /// `invalid_request` for an undecodable body, auth `upstream_error`/`internal`)
    /// in `buildkit_allocations_total`, with empty `namespace` and `pool`,
    /// because no parsed, authorized pool exists yet. A no-op before [`Component::run`].
    pub fn count_rejection(&self, code: ErrorCode) {
        if let Some(m) = self.shared.metrics.get() {
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

    /// The front door, on every replica. Binding is the [`Allocator`]'s.
    fn run(&self, ctx: AppContext, shutdown: CancellationToken) -> BoxResult {
        let cell = Arc::clone(&self.front);
        let shared = self.shared.clone();
        Box::pin(async move {
            let metrics = shared.metrics(&ctx);
            let (backend, certs) = shared.sources(&ctx)?;
            let own = ctx.namespace().to_owned();
            let served = ctx.config().served_claim_namespaces(&own);
            let sub_ctx = ctx.clone();
            let front = Front::new(
                backend,
                ctx.pools(),
                ctx.claims(),
                Box::new(move || sub_ctx.claim_events()),
                certs,
                own,
                served,
                metrics,
            )
            .with_stopping(shutdown.clone())
            // v0.1.0 upgrade shim: remove in the release after 0.2.0.
            .with_pods(ctx.pods());
            ctx.wait_synced().await;
            let _ = cell.set(Arc::new(front));
            info!(component = "alloc", "allocation service running");
            shutdown.cancelled().await;
            Ok(())
        })
    }
}
