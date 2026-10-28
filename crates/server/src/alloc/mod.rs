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
            let engine = Arc::new(
                Engine::new(
                    Arc::new(KubeBackend::new(ctx.client(), timeout)),
                    ctx.pools(),
                    ctx.pods(),
                    Box::new(move || sub_ctx.pod_events()),
                    Arc::new(MaterialSource::new(material)),
                    cfg,
                    metrics.clone(),
                    Arc::new(Queues::new(MAX_WAITERS, metrics)),
                )
                .with_stopping(shutdown.clone()),
            );
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

/// Keeps this replica's `status.waiters` entries current until `stop`, then
/// sets every entry it wrote back to 0. The requests it counted have been
/// refused by then and retry on another replica, which counts them again; a
/// stale count here would make the leader start surplus workers until the entry
/// ages out.
///
/// `stop` is checked only between ticks. A tick that has started runs to the
/// end, each apply bounded by `kube.requestTimeout`. The writer records an
/// entry before its non-zero apply is sent, so `clear` also zeroes one whose
/// apply timed out and may still have landed.
async fn write_waiters(
    mut writer: WaitersWriter,
    queues: Arc<Queues>,
    backend: Arc<dyn Backend>,
    stop: CancellationToken,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => break,
            _ = tick.tick() => {}
        }
        writer.tick(&queues, &*backend).await;
    }
    writer.clear(&*backend).await;
}

/// Wakes queue heads on pod events and keeps `status.waiters` current, until
/// `shutdown`.
pub async fn drive(engine: &Engine, replica: String, shutdown: CancellationToken) {
    let mut sub = (engine.subscribe)();
    // Status applies wait on the API server; in their own task they cannot
    // hold back pod events and queue wakes.
    let writer_stop = shutdown.child_token();
    let _writer_guard = writer_stop.clone().drop_guard();
    let writer = tokio::spawn(write_waiters(
        WaitersWriter::new(replica, engine.pools.clone()),
        Arc::clone(&engine.queues),
        Arc::clone(&engine.backend),
        writer_stop,
    ));
    loop {
        tokio::select! {
            () = shutdown.cancelled() => {
                // The final zero write must land before the replica exits.
                let _ = writer.await;
                return;
            }
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

#[cfg(test)]
mod waiters_tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Mutex;

    use buildkit_controller_api::v1alpha1::BuildKitPool;
    use futures::future::BoxFuture;
    use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Secret};
    use kube::runtime::reflector::store::Writer;
    use kube::runtime::watcher;
    use serde_json::{Value, json};

    use super::backend::{CreateOutcome, PatchOutcome, PvcCreate};
    use super::*;
    use crate::reconcile::spawn::WorkerSecrets;
    use crate::render::fixtures;

    /// Records each `status.waiters` count when the apply is sent; the apply
    /// then takes 2 s and fails with `timeout` while its flag is set. Nothing
    /// else is called.
    #[derive(Default)]
    struct SlowWaiters(Mutex<Vec<i32>>, std::sync::atomic::AtomicBool);

    fn unused<T: Send + 'static>(out: T) -> BoxFuture<'static, T> {
        Box::pin(async move { out })
    }

    impl Backend for SlowWaiters {
        fn patch_pod(&self, _: &str, _: &str, _: Value) -> BoxFuture<'static, PatchOutcome> {
            unused(PatchOutcome::Failed("unused".into()))
        }
        fn create_pod(&self, _: &str, _: Pod) -> BoxFuture<'static, CreateOutcome> {
            unused(CreateOutcome::Failed("unused".into()))
        }
        fn delete_pod(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: Option<u32>,
        ) -> BoxFuture<'static, Result<(), String>> {
            unused(Err("unused".into()))
        }
        fn apply_waiters(
            &self,
            _: &str,
            _: &str,
            _: &str,
            count: i32,
            _: &str,
        ) -> BoxFuture<'static, Result<(), String>> {
            self.0.lock().unwrap().push(count);
            let fail = self.1.load(std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if fail { Err("timeout".into()) } else { Ok(()) }
            })
        }
        fn get_pvc(
            &self,
            _: &str,
            _: &str,
        ) -> BoxFuture<'static, Result<Option<PersistentVolumeClaim>, String>> {
            unused(Err("unused".into()))
        }
        fn create_pvc(&self, _: &str, _: PersistentVolumeClaim) -> BoxFuture<'static, PvcCreate> {
            unused(PvcCreate::Failed("unused".into()))
        }
        fn delete_pvc(&self, _: &str, _: &str, _: &str) -> BoxFuture<'static, Result<(), String>> {
            unused(Err("unused".into()))
        }
        fn annotate_pvc(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
        ) -> BoxFuture<'static, Result<(), String>> {
            unused(Err("unused".into()))
        }
    }

    impl WorkerSecrets for SlowWaiters {
        fn create_worker_secret(&self, _: &Secret) -> BoxFuture<'static, Result<(), String>> {
            unused(Err("unused".into()))
        }
        fn get_worker_pod(
            &self,
            _: &str,
            _: &str,
        ) -> BoxFuture<'static, Result<Option<Pod>, String>> {
            unused(Err("unused".into()))
        }
    }

    /// A stop that lands while the first non-zero apply is in flight must
    /// not lose it: the PATCH may land, and an entry `clear` does not know
    /// about stays above 0 until the leader prunes it.
    #[tokio::test(start_paused = true)]
    async fn stop_during_the_first_apply_still_sets_the_entry_back_to_zero() {
        let spec = json!({
            "template": {"spec": {"containers": [{"name": "buildkitd", "image": "moby/buildkit:v0.26.3"}]}},
            "scaling": {"min": 0, "max": 1},
            "access": {"rules": [{"serviceAccount": {}}]},
        });
        let mut pools = Writer::<BuildKitPool>::default();
        pools.apply_watcher_event(&watcher::Event::Apply(fixtures::pool("p", spec).unwrap()));
        let queues = Arc::new(Queues::new(MAX_WAITERS, AllocMetrics::default()));
        let _queued = queues
            .enqueue(&(fixtures::POOL_NAMESPACE.into(), "p".into()), None)
            .unwrap();
        let backend = Arc::new(SlowWaiters::default());
        let stop = CancellationToken::new();
        let writer = tokio::spawn(write_waiters(
            WaitersWriter::new("r1".into(), pools.as_reader()),
            Arc::clone(&queues),
            Arc::clone(&backend) as Arc<dyn Backend>,
            stop.clone(),
        ));
        // The first tick is due at once and its apply is now in flight.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(*backend.0.lock().unwrap(), [1]);
        stop.cancel();
        writer.await.unwrap();
        assert_eq!(*backend.0.lock().unwrap(), [1, 0]);
    }

    /// A non-zero apply that times out may still land, so `clear` must zero
    /// its entry too.
    #[tokio::test(start_paused = true)]
    async fn clear_zeroes_an_entry_whose_apply_timed_out() {
        let spec = json!({
            "template": {"spec": {"containers": [{"name": "buildkitd", "image": "moby/buildkit:v0.26.3"}]}},
            "scaling": {"min": 0, "max": 1},
            "access": {"rules": [{"serviceAccount": {}}]},
        });
        let mut pools = Writer::<BuildKitPool>::default();
        pools.apply_watcher_event(&watcher::Event::Apply(fixtures::pool("p", spec).unwrap()));
        let queues = Queues::new(MAX_WAITERS, AllocMetrics::default());
        let queues = Arc::new(queues);
        let _queued = queues
            .enqueue(&(fixtures::POOL_NAMESPACE.into(), "p".into()), None)
            .unwrap();
        let backend = SlowWaiters::default();
        backend.1.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut writer = WaitersWriter::new("r1".into(), pools.as_reader());
        writer.tick(&queues, &backend).await;
        backend.1.store(false, std::sync::atomic::Ordering::SeqCst);
        writer.clear(&backend).await;
        assert_eq!(*backend.0.lock().unwrap(), [1, 0]);
        // Settled: a second clear sends nothing.
        writer.clear(&backend).await;
        assert_eq!(*backend.0.lock().unwrap(), [1, 0]);
    }
}
