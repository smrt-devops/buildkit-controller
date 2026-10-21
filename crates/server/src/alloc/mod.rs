//! Allocation service: warm claim,
//! cold pre-claimed create, the per-replica FIFO queue, owner-only release.
//!
//! Runs on every replica (not leader-gated) as the runtime's `alloc`
//! [`Component`]. The HTTP layer holds a clone of the same [`AllocService`]
//! and calls [`AllocService::allocate`] after authenticating the caller, and
//! [`AllocService::release`]. This module does the pool lookup **and the
//! access check**,
//! so a caller of `allocate` cannot skip it. HTTP counts in
//! `buildkit_allocations_total` only the rejections it makes itself
//! (`unauthenticated`, `payload_too_large`, `unsupported_media_type`, bad JSON);
//! every outcome of `allocate` is counted here.
//!
//! A successful [`Allocation`] still owns its pod through the orphan guard:
//! the handler calls [`Allocation::commit`] while writing the 201. Dropping it
//! (handler error, panic, client disconnect) retires the pod.

pub mod backend;
mod cache;
pub mod certs;
pub mod engine;
pub mod error;
pub mod metrics;
pub mod pods;
pub mod queue;

#[cfg(test)]
mod tests;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use buildkit_controller_api::dto::{CreateAllocationRequest, ErrorCode};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

pub use backend::{Backend, KubeBackend};
pub use certs::{CertSource, MaterialSource};
pub use engine::{Allocation, Engine, EngineConfig};
pub use error::AllocError;

use crate::auth::Identity;
use crate::certs::CertMaterial;
use crate::render::{ANN_ALLOC, LABEL_POOL};
use crate::runtime::AppContext;
use crate::runtime::hooks::{BoxResult, Component};
use crate::runtime::stores::PodEvent;
use metrics::AllocMetrics;
use pods::{FREE, ann, is_ready};
use queue::{MAX_WAITERS, PoolKey, Queues, WaitersWriter};

/// Handle to the allocation service; clone it into the HTTP layer.
#[derive(Clone)]
pub struct AllocService {
    material: watch::Receiver<CertMaterial>,
    engine: Arc<OnceLock<Arc<Engine>>>,
    metrics: Arc<OnceLock<AllocMetrics>>,
}

impl AllocService {
    /// An idle handle over the shared cert material; it serves once
    /// [`Component::run`] has started.
    #[must_use]
    pub fn new(material: watch::Receiver<CertMaterial>) -> Self {
        Self {
            material,
            engine: Arc::default(),
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
        self.engine.get().is_some()
    }

    fn engine(&self) -> Result<&Arc<Engine>, AllocError> {
        self.engine.get().ok_or_else(|| {
            error!("allocation service called before it started");
            AllocError::Internal
        })
    }

    /// POST /v1/allocations for an authenticated caller.
    ///
    /// # Errors
    /// [`AllocError`], one variant per the API error code.
    pub async fn allocate(
        &self,
        who: &Identity,
        req: &CreateAllocationRequest,
    ) -> Result<Allocation, AllocError> {
        self.engine()?.allocate(who, req).await
    }

    /// DELETE /v1/allocations/{id}: owner-only.
    ///
    /// # Errors
    /// `not_found`, `forbidden`, `upstream_error`, `internal`.
    pub async fn release(&self, who: &Identity, alloc_id: &str) -> Result<(), AllocError> {
        self.engine()?.release(who, alloc_id).await
    }
}

impl Component for AllocService {
    fn name(&self) -> &'static str {
        "alloc"
    }

    fn run(&self, ctx: AppContext, shutdown: CancellationToken) -> BoxResult {
        let cell = Arc::clone(&self.engine);
        let material = self.material.clone();
        let metrics_cell = Arc::clone(&self.metrics);
        Box::pin(async move {
            let metrics = metrics_cell
                .get_or_init(|| AllocMetrics::with_releases(ctx.metrics().releases()))
                .clone();
            ctx.metrics().with_registry(|r| metrics.register(r));
            let c = ctx.config();
            let cfg = EngineConfig {
                endpoint: c.edge.endpoint.clone(),
                server_name: c.edge.server_name.clone(),
                allocation_domain: Some(c.edge.allocation_domain.clone()).filter(|d| !d.is_empty()),
            };
            let timeout = c
                .kube
                .request_timeout
                .seconds()
                .map(Duration::from_secs)
                .map_err(|e| anyhow::anyhow!("kube.requestTimeout: {e}"))?;
            let sub_ctx = ctx.clone();
            let engine = Arc::new(Engine::new(
                Arc::new(KubeBackend::new(ctx.client(), timeout)),
                ctx.pools(),
                ctx.pods(),
                Box::new(move || sub_ctx.pod_events()),
                Arc::new(MaterialSource::new(material)),
                cfg,
                metrics.clone(),
                Arc::new(Queues::new(MAX_WAITERS, metrics)),
            ));
            ctx.wait_synced().await;
            let _ = cell.set(Arc::clone(&engine));
            info!(component = "alloc", "allocation service running");
            drive(&engine, ctx.replica().to_owned(), shutdown).await;
            Ok(())
        })
    }
}

fn key_of(p: &k8s_openapi::api::core::v1::Pod) -> Option<PoolKey> {
    let pool = p.metadata.labels.as_ref()?.get(LABEL_POOL)?;
    Some((p.metadata.namespace.clone()?, pool.clone()))
}

/// Keeps this replica's `status.waiters` entries current until `stop`.
async fn write_waiters(
    mut writer: WaitersWriter,
    queues: Arc<Queues>,
    backend: Arc<dyn Backend>,
    stop: CancellationToken,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    stop.run_until_cancelled(async {
        loop {
            tick.tick().await;
            writer.tick(&queues, &*backend).await;
        }
    })
    .await;
}

/// Wakes queue heads on pod events and keeps `status.waiters` current, until
/// `shutdown`.
pub async fn drive(engine: &Engine, replica: String, shutdown: CancellationToken) {
    let mut sub = (engine.subscribe)();
    // Status applies wait on the API server; in their own task they cannot
    // hold back pod events and queue wakes.
    let writer_stop = shutdown.child_token();
    let _writer_guard = writer_stop.clone().drop_guard();
    tokio::spawn(write_waiters(
        WaitersWriter::new(replica, engine.pools.clone()),
        Arc::clone(&engine.queues),
        Arc::clone(&engine.backend),
        writer_stop,
    ));
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            ev = sub.recv() => match ev {
                None => return,
                Some(PodEvent::Resync) => engine.queues.wake_all(),
                Some(PodEvent::Apply(p)) => {
                    let free = ann(&p, ANN_ALLOC) == Some(FREE)
                        && is_ready(&p)
                        && p.metadata.deletion_timestamp.is_none();
                    if let (true, Some(k)) = (free, key_of(&p)) {
                        engine.queues.wake(&k);
                    }
                }
                Some(PodEvent::Delete(p)) => {
                    if let Some(k) = key_of(&p) {
                        engine.queues.wake(&k);
                    }
                }
            },
        }
    }
}
