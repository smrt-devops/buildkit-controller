//! The allocation flow: claim a warm pod or create one, queue, release.

use std::sync::Arc;
use std::time::Duration;

use buildkit_controller_api::dto::{AllocationResponse, CreateAllocationRequest};
use buildkit_controller_api::v1alpha1::{self as crd, BuildKitPool};
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::reflector::{ObjectRef, Store};
use time::OffsetDateTime;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::backend::{Backend, CreateOutcome, PatchOutcome};
use super::cache::{self, Volume};
use super::certs::CertSource;
use super::error::AllocError;
use super::metrics::AllocMetrics;
use super::pods::{
    CERT_MARGIN, FREE, MAX_TRIES, Want, ann, claim_patch, extend_patch, fmt_ts, free_slots,
    is_candidate, is_ready, new_alloc_id, of_pool, retire_patch, whole_seconds,
};
use super::queue::{PoolKey, Queues};
use crate::auth::{AccessRule, AccessRules, Identity};
use crate::pki::{AllocId, AllocationId, CaBundle, ClientPublicKey, LeafKey, Namespace, PoolName};
use crate::reconcile::spawn;
use crate::render as rnd;
use crate::render::{
    ANN_ALLOC, ANN_OWNER, CONTROLLER, Claim, LABEL_MANAGED_BY, LABEL_POOL, template_hash,
};
use crate::runtime::stores::{PodEvent, PodSubscription};

/// How long release waits for a just-claimed pod to reach the store.
pub const RELEASE_WAIT: Duration = Duration::from_secs(2);
/// At most this many releases wait for their pod at once. Each waiter holds
/// a Pod broadcast receiver, so unknown ids must not pile up without limit.
pub const RELEASE_WAITERS: usize = 64;
const MIN_TTL: u64 = 60;
/// How long a claim outlives the moment its pod can be ready (the claim for
/// a warm pod, the end of `workers.startTimeout` for a cold one) before the
/// allocation commits. A claim is written with this short `expires-at` and
/// extended to the real expiry just before the response exists, so a
/// replica that dies in between (SIGKILL, OOM, shutdown abort) leaves a
/// claim the leader retires within this bound, not at `maxTTL`. The bound
/// ends at the commit: a replica that dies after it but before the response
/// is written leaves an allocation no client knows, held until its expiry.
pub const COMMIT_WINDOW: time::Duration = time::Duration::seconds(60);
/// Time left between a cold pod turning Ready and the end of its claim, to
/// sign the cert and send the commit. A pod not Ready by then is a start timeout.
const READY_MARGIN: time::Duration = time::Duration::seconds(5);
/// After a claim lost to another writer (or with an unknown outcome), the
/// queue is woken again after this long. The loss means this replica's pod
/// store lags, and no pod event will wake the queue for the pod it lost.
const CONFLICT_RETRY: Duration = Duration::from_secs(1);

/// Static inputs from the config file.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// `edge.endpoint`.
    pub endpoint: String,
    /// `edge.serverName`.
    pub server_name: String,
    /// `edge.allocationDomain`, `None` when unset.
    pub allocation_domain: Option<String>,
}

/// Everything allocation reads and writes.
pub struct Engine {
    pub backend: Arc<dyn Backend>,
    pub pools: Store<BuildKitPool>,
    pub pods: Store<Pod>,
    pub subscribe: Box<dyn Fn() -> PodSubscription + Send + Sync>,
    pub certs: Arc<dyn CertSource>,
    pub cfg: EngineConfig,
    pub metrics: AllocMetrics,
    pub queues: Arc<Queues>,
    /// Permits of [`RELEASE_WAITERS`].
    pub release_waiters: Arc<tokio::sync::Semaphore>,
    /// Fires when the replica starts shutting down. From then on a request that
    /// has not claimed a pod gets [`AllocError::ShuttingDown`].
    pub stopping: CancellationToken,
}

/// A successful allocation. The orphan guard still owns the pod. Call
/// [`Allocation::commit`] when you write the response. Dropping an uncommitted
/// `Allocation` retires the pod.
pub struct Allocation {
    response: AllocationResponse,
    pod_name: String,
    pod_uid: String,
    guard: OrphanGuard,
}

impl Allocation {
    /// The response body.
    #[must_use]
    pub fn response(&self) -> &AllocationResponse {
        &self.response
    }

    /// The claimed pod `(name, uid)`.
    #[must_use]
    pub fn pod(&self) -> (&str, &str) {
        (&self.pod_name, &self.pod_uid)
    }

    /// Hand the allocation to the caller; the pod is no longer retired on drop.
    #[must_use]
    pub fn commit(mut self) -> AllocationResponse {
        self.guard.armed = false;
        self.response
    }
}

/// Retires a claimed pod on drop unless disarmed.
pub struct OrphanGuard {
    backend: Arc<dyn Backend>,
    metrics: AllocMetrics,
    ns: String,
    pool: String,
    name: String,
    /// `None` for a create whose response never arrived (undo by name).
    uid: Option<String>,
    alloc_id: String,
    grace: Option<u32>,
    reason: &'static str,
    armed: bool,
    /// The claim or create is known to have landed. An unconfirmed guard was
    /// armed before the request was sent (it may still land after a
    /// cancellation), so it retries a lost retire a few times. A confirmed guard
    /// retries an unknown retire outcome until it lands or the pod is gone.
    confirmed: bool,
}

/// Gaps between retire attempts of an unconfirmed guard (attempts at 0, 2, 5
/// and 10 s after the drop).
const UNCONFIRMED_GAPS: [u64; 3] = [2, 3, 5];
/// Gaps between retire attempts of a confirmed guard whose retire outcome is
/// unknown; after the last one it retries every [`CONFIRMED_MAX_GAP`].
const CONFIRMED_GAPS: [u64; 6] = [2, 3, 5, 10, 20, 30];
const CONFIRMED_MAX_GAP: u64 = 60;

/// The wait before retire attempt `n + 1`, or `None` to give up.
fn retire_gap(confirmed: bool, n: usize) -> Option<Duration> {
    let s = if confirmed {
        CONFIRMED_GAPS.get(n).copied().unwrap_or(CONFIRMED_MAX_GAP)
    } else {
        *UNCONFIRMED_GAPS.get(n)?
    };
    Some(Duration::from_secs(s))
}

impl Drop for OrphanGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            error!(pod = %self.name, alloc_id = %self.alloc_id, "no runtime to retire orphaned pod");
            return;
        };
        let (backend, metrics) = (Arc::clone(&self.backend), self.metrics.clone());
        let g = (
            self.ns.clone(),
            self.pool.clone(),
            self.name.clone(),
            self.uid.clone(),
            self.alloc_id.clone(),
        );
        let (grace, reason, confirmed) = (self.grace, self.reason, self.confirmed);
        rt.spawn(async move {
            let (ns, pool, name, uid, id) = g;
            for n in 0.. {
                match retire(&*backend, &ns, &name, uid.as_deref(), &id, grace).await {
                    Retired::Yes => {
                        metrics.release(&ns, &pool, reason);
                        info!(namespace = %ns, pool = %pool, pod = %name, alloc_id = %id, reason, "retired claimed pod");
                        return;
                    }
                    // A landed claim that the test no longer matches is gone or
                    // already retired by another writer.
                    Retired::Lost if confirmed => return,
                    Retired::Lost | Retired::Unknown => {}
                    Retired::Failed => return,
                }
                let Some(gap) = retire_gap(confirmed, n) else {
                    warn!(namespace = %ns, pool = %pool, pod = %name, alloc_id = %id, "gave up retiring claimed pod");
                    return;
                };
                tokio::time::sleep(gap).await;
            }
        });
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Retired {
    Yes,
    Lost,
    Unknown,
    Failed,
}

/// The retire patch, then delete on success (delete failure is logged only).
async fn retire(
    backend: &dyn Backend,
    ns: &str,
    name: &str,
    uid: Option<&str>,
    expected: &str,
    grace: Option<u32>,
) -> Retired {
    match backend
        .patch_pod(ns, name, retire_patch(uid, expected))
        .await
    {
        PatchOutcome::Applied(p) => {
            let uid = uid.or(p.metadata.uid.as_deref()).unwrap_or_default();
            if let Err(error) = backend.delete_pod(ns, name, uid, grace).await {
                warn!(namespace = %ns, pod = %name, error, "deleting retired pod");
            }
            Retired::Yes
        }
        PatchOutcome::Lost => Retired::Lost,
        PatchOutcome::Unknown => Retired::Unknown,
        PatchOutcome::Failed(error) => {
            warn!(namespace = %ns, pod = %name, error, "retire patch failed");
            Retired::Failed
        }
    }
}

/// A pod won by claim or create, guarded.
struct Won {
    guard: OrphanGuard,
    /// The CA bundle the winning attempt selected with; it signs the cert.
    ca: Arc<CaBundle>,
    uid: String,
    pod: Pod,
    cold: Option<PodSubscription>,
    expires_at: OffsetDateTime,
    /// The `expires-at` the claim was written with, see [`COMMIT_WINDOW`].
    claimed_until: OffsetDateTime,
    /// `P cache-keys` of the slot before this claim.
    keys_before: Option<String>,
    /// Created for this request on a volume another scope owned.
    wiped: bool,
}

/// What every attempt of one request shares.
struct Req<'a> {
    key: PoolKey,
    /// Re-checked against the pool on every attempt.
    who: &'a Identity,
    /// uid of the pool the caller was authorized for; a recreated pool is not it.
    pool_uid: String,
    alloc_id: &'a AllocId,
    owner: String,
    ttl: time::Duration,
    /// Key and `keysPerVolume` when the local layer applies with soft affinity.
    affinity: Option<(String, usize)>,
    /// Whether the local layer applies. Without it, uncached slots go first.
    local: bool,
    /// The caller's cache scope id when the pool has `cache.local`. A pod in a
    /// cached slot serves only its own scope.
    scope: Option<String>,
}

/// The `expires-at` a claim is written with: [`COMMIT_WINDOW`] past now
/// plus `start`, and always before the allocation's own expiry, so every
/// commit moves it. The leader's retires test that value, which makes each
/// of them lose to a commit made after the state it judged.
fn claim_until(expires_at: OffsetDateTime, start: time::Duration) -> OffsetDateTime {
    (whole_seconds(OffsetDateTime::now_utc()) + start + COMMIT_WINDOW)
        .min(expires_at - time::Duration::SECOND)
}

fn grace_of(pod: &Pod) -> Option<u32> {
    let g = pod.spec.as_ref()?.termination_grace_period_seconds?;
    u32::try_from(g).ok()
}

fn seconds(d: &crd::Duration) -> Result<u64, AllocError> {
    d.seconds().map_err(|e| {
        error!(error = %e, "invalid duration in pool spec");
        AllocError::Internal
    })
}

/// `spec.access.rules` into the auth rule type. Any malformed rule
/// denies the whole pool (fails closed).
#[must_use]
pub fn access_rules(pool: &BuildKitPool) -> Option<AccessRules> {
    let rules = pool
        .spec
        .access
        .rules
        .iter()
        .map(|r| match (&r.oidc, &r.service_account, &r.claims) {
            (None, None, Some(c)) => Some(AccessRule::Claims {
                namespaces: c.namespaces.clone(),
            }),
            (Some(o), None, None) => Some(AccessRule::Oidc {
                issuer: o.issuer.clone(),
                claims: o
                    .claims
                    .iter()
                    .map(|(k, v)| (k.clone(), v.0.clone()))
                    .collect(),
            }),
            (None, Some(s), None) => Some(AccessRule::ServiceAccount {
                namespace: s.namespace.clone(),
                name: s.name.clone(),
            }),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    AccessRules::new(rules).ok()
}

impl Engine {
    /// Assemble; `subscribe` returns a fresh pod event subscription.
    #[allow(clippy::too_many_arguments, reason = "plain constructor")]
    #[must_use]
    pub fn new(
        backend: Arc<dyn Backend>,
        pools: Store<BuildKitPool>,
        pods: Store<Pod>,
        subscribe: Box<dyn Fn() -> PodSubscription + Send + Sync>,
        certs: Arc<dyn CertSource>,
        cfg: EngineConfig,
        metrics: AllocMetrics,
        queues: Arc<Queues>,
    ) -> Self {
        Self {
            backend,
            pools,
            pods,
            subscribe,
            certs,
            cfg,
            metrics,
            queues,
            release_waiters: Arc::new(tokio::sync::Semaphore::new(RELEASE_WAITERS)),
            stopping: CancellationToken::new(),
        }
    }

    /// Refuse new and queued requests with [`AllocError::ShuttingDown`] once `stopping` fires.
    #[must_use]
    pub fn with_stopping(mut self, stopping: CancellationToken) -> Self {
        self.stopping = stopping;
        self
    }

    /// POST /v1/allocations after authentication.
    pub async fn allocate(
        &self,
        who: &Identity,
        req: &CreateAllocationRequest,
    ) -> Result<Allocation, AllocError> {
        let started = Instant::now();
        let r = match self.lookup(who, &req.namespace, &req.pool) {
            Ok(pool) => self.allocate_in(who, req, &pool, started).await,
            Err(e) => Err(e),
        };
        // The engine counts unknown pools with empty labels, so request strings never
        // become label values (bounded cardinality).
        let (ns, pool) = match &r {
            Err(AllocError::PoolNotFound) => ("", ""),
            _ => (req.namespace.as_str(), req.pool.as_str()),
        };
        match &r {
            Ok(a) => {
                self.metrics.allocation(ns, pool, "ok");
                info!(namespace = ns, pool, alloc_id = %a.response.id, owner = %a.response.owner,
                    pod = %a.pod_name, result = "ok", duration_ms = started.elapsed().as_millis(), "allocation");
            }
            Err(e) => {
                self.metrics.allocation(ns, pool, e.code().as_str());
                info!(
                    namespace = ns,
                    pool,
                    result = e.code().as_str(),
                    "allocation refused"
                );
            }
        }
        r
    }

    fn lookup(
        &self,
        who: &Identity,
        ns: &str,
        name: &str,
    ) -> Result<Arc<BuildKitPool>, AllocError> {
        if ns.parse::<Namespace>().is_err() || name.parse::<PoolName>().is_err() {
            return Err(AllocError::PoolNotFound);
        }
        let pool = self
            .pools
            .get(&ObjectRef::new(name).within(ns))
            .filter(|p| p.metadata.uid.is_some())
            .ok_or(AllocError::PoolNotFound)?;
        if !access_rules(&pool).is_some_and(|r| r.allows(who, ns)) {
            info!(
                namespace = ns,
                pool = name,
                reason = "access_denied",
                "allocation refused"
            );
            return Err(AllocError::PoolNotFound);
        }
        Ok(pool)
    }

    fn endpoint(&self, id: &str) -> String {
        match &self.cfg.allocation_domain {
            Some(d) => {
                let port = self.cfg.endpoint.rsplit_once(':').map_or("", |(_, p)| p);
                format!("tcp://{id}.{d}:{port}")
            }
            None => self.cfg.endpoint.clone(),
        }
    }

    #[allow(clippy::too_many_lines, reason = "one linear request flow")]
    async fn allocate_in(
        &self,
        who: &Identity,
        req: &CreateAllocationRequest,
        pool: &BuildKitPool,
        started: Instant,
    ) -> Result<Allocation, AllocError> {
        self.certs.bundle().ok_or(AllocError::CaUnavailable)?;
        let ready = pool
            .status
            .iter()
            .flat_map(|s| &s.conditions)
            .find(|c| c.type_ == "Ready");
        match ready {
            Some(c) if c.status == "True" => {}
            Some(c) if c.reason == "CAUnavailable" => {}
            _ => return Err(AllocError::NotReady),
        }
        let a = &pool.spec.allocation;
        let ttl = req
            .ttl_seconds
            .map_or_else(|| seconds(&a.default_ttl), Ok)?;
        if ttl < MIN_TTL || ttl > seconds(&a.max_ttl)? {
            return Err(AllocError::InvalidRequest(format!(
                "ttlSeconds must be between {MIN_TTL} and {}",
                a.max_ttl.as_str()
            )));
        }
        let qt = req
            .queue_timeout_seconds
            .map_or_else(|| seconds(&a.queue_timeout), Ok)?;
        if qt > seconds(&a.max_queue_timeout)? {
            return Err(AllocError::InvalidRequest(format!(
                "queueTimeoutSeconds must be at most {}",
                a.max_queue_timeout.as_str()
            )));
        }
        // Everything that can fail on the caller's input happens before the claim.
        let key = match &req.public_key {
            Some(pem) => LeafKey::Client(ClientPublicKey::from_pem(pem).map_err(|_| {
                AllocError::InvalidRequest("publicKey must be a PEM SPKI ECDSA P-256 key".into())
            })?),
            None => LeafKey::Generate,
        };
        let plan = cache::plan(pool, req.cache_key.as_ref(), req.cache)?;
        let alloc_id = new_alloc_id().ok_or(AllocError::Internal)?;
        let r = Req {
            key: (req.namespace.clone(), req.pool.clone()),
            who,
            pool_uid: pool.metadata.uid.clone().ok_or(AllocError::PoolNotFound)?,
            alloc_id: &alloc_id,
            owner: who.owner(),
            ttl: time::Duration::seconds(i64::try_from(ttl).map_err(|_| AllocError::Internal)?),
            affinity: plan.as_ref().and_then(|p| Some((p.key.clone(), p.soft?))),
            local: plan.as_ref().is_some_and(|p| p.local),
            scope: cache::scope(pool, who),
        };
        let mut won = self.acquire(&r, Duration::from_secs(qt)).await?;
        let cold = won.cold.is_some();
        if let Some(sub) = won.cold.take() {
            let start = Duration::from_secs(seconds(&pool.spec.workers.start_timeout)?);
            self.await_ready(&mut won, sub, start).await?;
        }
        // Only now: a cold pod's init container has run, so the volume holds
        // what the binding will say.
        let local = self.local_cache(&r, pool, plan.as_ref(), &won, cold);
        let path = if cold { "cold" } else { "warm" };
        let id = AllocationId {
            namespace: req.namespace.parse().map_err(|_| AllocError::Internal)?,
            pool: req.pool.parse().map_err(|_| AllocError::Internal)?,
            pod_uid: won.uid.parse().map_err(|_| AllocError::Internal)?,
            alloc_id: alloc_id.clone(),
        };
        let cert = won
            .ca
            .issue_allocation(&id, key, OffsetDateTime::now_utc(), won.expires_at)
            .map_err(|e| {
                error!(error = %e, "issuing allocation cert");
                AllocError::Internal
            })?;
        self.extend(&mut won).await?;
        self.metrics.duration(
            &req.namespace,
            &req.pool,
            path,
            started.elapsed().as_secs_f64(),
        );
        let served = plan.as_ref().is_some_and(|p| p.registry.is_some());
        // `reused` is not an affinity result, so the metric counts only the local layer.
        let affinity = local.filter(|_| r.local);
        self.metrics
            .cache(&req.namespace, &req.pool, served, affinity);
        let response = AllocationResponse {
            id: alloc_id.to_string(),
            namespace: req.namespace.clone(),
            pool: req.pool.clone(),
            owner: r.owner.clone(),
            endpoint: self.endpoint(alloc_id.as_str()),
            server_name: self.cfg.server_name.clone(),
            ca_cert: self.certs.ca_cert_pem(&won.ca),
            client_cert: cert.cert_pem().to_owned(),
            client_key: cert.private_key_pem(),
            expires_at: fmt_ts(won.expires_at),
            cache: plan.as_ref().and_then(|p| p.response(local)),
        };
        Ok(Allocation {
            response,
            pod_name: won.pod.metadata.name.clone().unwrap_or_default(),
            pod_uid: won.uid,
            guard: won.guard,
        })
    }

    /// Commits the claim on the cluster: `expires-at` goes from the claim value
    /// to the allocation's expiry. It is the last step of an allocation that
    /// can fail. A failure fails the request and the orphan guard retires the
    /// pod, so no client holds an allocation the leader is about to reap at
    /// the claim value.
    async fn extend(&self, won: &mut Won) -> Result<(), AllocError> {
        let (ns, name) = (won.guard.ns.clone(), won.guard.name.clone());
        let patch = extend_patch(
            &won.uid,
            &won.guard.alloc_id,
            &fmt_ts(won.claimed_until),
            &fmt_ts(won.expires_at),
        );
        match self.backend.patch_pod(&ns, &name, patch).await {
            PatchOutcome::Applied(_) => Ok(()),
            // The leader retired the pod at the claim value, or it is gone.
            PatchOutcome::Lost => {
                won.guard.reason = "pod_lost";
                Err(AllocError::PodLost)
            }
            // Landed or not, `alloc` is still this id, so the guard's retire applies.
            PatchOutcome::Unknown => Err(AllocError::UpstreamError),
            PatchOutcome::Failed(error) => {
                error!(namespace = %ns, pod = %name, error, "commit patch");
                Err(AllocError::Internal)
            }
        }
    }

    /// Record the key on a won cached slot and return the cache result when the
    /// local layer applies, or `reused` when it does not and the slot is
    /// cached. `cold` when the pod was created for this request.
    fn local_cache(
        &self,
        r: &Req<'_>,
        pool: &BuildKitPool,
        plan: Option<&cache::Plan>,
        won: &Won,
        cold: bool,
    ) -> Option<&'static str> {
        if let Some((k, cap)) = &r.affinity
            && cache::cached(pool, &won.pod)
        {
            let keys = rnd::push_key(won.keys_before.as_deref(), k, *cap);
            cache::annotate_detached(
                Arc::clone(&self.backend),
                self.metrics.clone(),
                pool,
                &won.pod,
                keys,
            );
        }
        if let Some(scope) = &r.scope {
            cache::bind_detached(
                Arc::clone(&self.backend),
                self.metrics.clone(),
                pool,
                &won.pod,
                scope,
            );
        }
        let p = plan?;
        if !p.local {
            // The slot's state dir still applies without the layer.
            let reused = if won.wiped { "wiped" } else { "reused" };
            return cache::cached(pool, &won.pod).then_some(reused);
        }
        let (ns, name) = (&r.key.0, &r.key.1);
        let pods: Vec<Arc<Pod>> = self
            .pods
            .state()
            .into_iter()
            .filter(|q| of_pool(q, ns, name, &r.pool_uid))
            .collect();
        let before = won.keys_before.as_deref();
        Some(cache::result(
            pool,
            &won.pod,
            before,
            cold.then_some(won.wiped),
            r.scope.as_deref(),
            &p.key,
            &pods,
        ))
    }

    /// Warm, cold, then the queue. The queue is FIFO per pool and cache scope
    /// on this replica: a request that finds others of its scope waiting for
    /// the pool makes no attempt of its own and goes behind them. Its arrival
    /// wakes the head, which makes the attempt in its place, so a newcomer
    /// never takes a pod a waiter of its scope could have had. Waiters of
    /// different scopes do not wait for each other, since the pods one can use
    /// may be of no use to the other. Across replicas the order is best effort.
    async fn acquire(&self, r: &Req<'_>, qt: Duration) -> Result<Won, AllocError> {
        // Shutdown is checked only before an attempt, so a request is never
        // refused after its claim was sent.
        let snapshot = self.queues.version(&r.key);
        if self.stopping.is_cancelled() {
            return Err(AllocError::ShuttingDown);
        }
        // The wake version of the last attempt; `None` until this request made one.
        let mut tried = None;
        let scope = r.scope.as_deref();
        if self.queues.is_waiting(&r.key, scope) {
            self.queues.wake(&r.key);
        } else {
            if let Some(w) = self.attempt(r).await? {
                return Ok(w);
            }
            tried = Some(snapshot);
        }
        let exhausted = || AllocError::PoolExhausted(format!("{}/{}", r.key.0, r.key.1));
        if qt.is_zero() {
            return Err(exhausted());
        }
        let Some(mut ticket) = self.queues.enqueue(&r.key, scope) else {
            self.metrics.queue_wait(&r.key.0, &r.key.1, "full", 0.0);
            return Err(exhausted());
        };
        let mut rec = WaitRecord {
            metrics: &self.metrics,
            key: &r.key,
            since: Instant::now(),
            result: "cancelled",
        };
        let deadline = rec.since + qt;
        loop {
            if self.stopping.is_cancelled() {
                rec.result = "shutting_down";
                return Err(AllocError::ShuttingDown);
            }
            let v = ticket.seen();
            if tried != Some(v) && ticket.is_head() {
                tried = Some(v);
                match self.attempt(r).await {
                    Ok(Some(w)) => {
                        rec.result = "ok";
                        return Ok(w);
                    }
                    Ok(None) => continue,
                    Err(e) => {
                        rec.result = "error";
                        return Err(e);
                    }
                }
            }
            tokio::select! {
                r = tokio::time::timeout_at(deadline, ticket.changed()) => if r.is_err() {
                    rec.result = "timeout";
                    return Err(exhausted());
                },
                () = self.stopping.cancelled() => {}
            }
        }
    }

    /// Armed before the request is sent; see [`OrphanGuard::confirmed`].
    fn guard(&self, r: &Req<'_>, name: &str, uid: Option<&str>, grace: Option<u32>) -> OrphanGuard {
        OrphanGuard {
            backend: Arc::clone(&self.backend),
            metrics: self.metrics.clone(),
            ns: r.key.0.clone(),
            pool: r.key.1.clone(),
            name: name.to_owned(),
            uid: uid.map(str::to_owned),
            alloc_id: r.alloc_id.to_string(),
            grace,
            reason: "abandoned",
            armed: true,
            confirmed: false,
        }
    }

    /// One pass over the warm path, then the cold path; `Ok(None)` means queue.
    #[allow(clippy::too_many_lines, reason = "one pass over both paths")]
    async fn attempt(&self, r: &Req<'_>) -> Result<Option<Won>, AllocError> {
        let (ns, name) = (&r.key.0, &r.key.1);
        let pool = self
            .pools
            .get(&ObjectRef::new(name).within(ns))
            .ok_or(AllocError::PoolNotFound)?;
        // A pool deleted and recreated under the same name is another pool, and
        // its access rules may have changed since the caller was authorized.
        if pool.metadata.uid.as_deref() != Some(r.pool_uid.as_str())
            || !access_rules(&pool).is_some_and(|a| a.allows(r.who, ns))
        {
            info!(namespace = %ns, pool = %name, reason = "access_denied", "queued allocation refused");
            return Err(AllocError::PoolNotFound);
        }
        let pool_uid = r.pool_uid.as_str();
        // Re-read on every attempt, because a queued request may span a hard CA rotation.
        let ca = self.certs.bundle().ok_or(AllocError::CaUnavailable)?;
        let now = whole_seconds(OffsetDateTime::now_utc());
        let expires_at = now + r.ttl;
        let hash = template_hash(&pool).map_err(|e| {
            error!(error = %e, "template hash");
            AllocError::Internal
        })?;
        let fp = ca.internal_ca().fingerprint_hex();
        let want = Want {
            template_hash: &hash,
            internal_ca: fp.get(..16).unwrap_or_default(),
            cert_not_after: expires_at + CERT_MARGIN,
        };
        let pods: Vec<Arc<Pod>> = self
            .pods
            .state()
            .into_iter()
            .filter(|p| of_pool(p, ns, name, pool_uid))
            .collect();
        // A pod in a cached slot that serves another cache scope is never offered.
        let (mut cands, foreign): (Vec<&Arc<Pod>>, Vec<&Arc<Pod>>) = pods
            .iter()
            .filter(|p| is_candidate(p, &want))
            .partition(|p| cache::serves(&pool, p, r.scope.as_deref()));
        cache::order_warm(
            &pool,
            &mut cands,
            r.affinity.as_ref().map(|a| a.0.as_str()),
            r.local,
        );
        let (now_s, id) = (fmt_ts(now), r.alloc_id.as_str());
        // A claim lost to another writer, or with an unknown outcome.
        let mut contended = false;
        for pod in cands.into_iter().take(MAX_TRIES) {
            let (Some(pname), Some(uid)) = (&pod.metadata.name, &pod.metadata.uid) else {
                continue;
            };
            let before = rnd::pod_cache_keys(pod).map(str::to_owned);
            let claimed_until = claim_until(expires_at, time::Duration::ZERO);
            let mut patch = claim_patch(uid, id, &r.owner, &now_s, &fmt_ts(claimed_until));
            cache::claim_ops(
                &pool,
                pod,
                before.as_deref(),
                r.affinity.as_ref(),
                r.scope.as_deref(),
                &mut patch,
            );
            // Arm the guard first, because a cancelled send may still land.
            let mut guard = self.guard(r, pname, Some(uid), grace_of(pod));
            match self.backend.patch_pod(ns, pname, patch).await {
                PatchOutcome::Applied(p) => {
                    guard.confirmed = true;
                    // The cache can lag an eviction: the edge would refuse this pod at once.
                    if p.metadata.deletion_timestamp.is_some()
                        || p.status.as_ref().and_then(|s| s.phase.as_deref()) != Some("Running")
                    {
                        guard.reason = "pod_lost";
                        info!(namespace = %ns, pool = %name, pod = %pname, "claimed pod is terminating, retiring it");
                        drop(guard);
                        continue;
                    }
                    return Ok(Some(Won {
                        guard,
                        uid: uid.clone(),
                        ca: Arc::clone(&ca),
                        pod: *p,
                        cold: None,
                        expires_at,
                        claimed_until,
                        keys_before: before,
                        wiped: false,
                    }));
                }
                PatchOutcome::Lost => {
                    guard.armed = false;
                    self.metrics.conflict(ns, name);
                    contended = true;
                }
                // The outcome is unknown, so the dropped guard sends the undo (retire) patch.
                PatchOutcome::Unknown => {
                    drop(guard);
                    contended = true;
                }
                PatchOutcome::Failed(error) => {
                    guard.armed = false;
                    error!(namespace = %ns, pool = %name, pod = %pname, error, "claim patch");
                    return Err(AllocError::Internal);
                }
            }
        }
        let won = self.cold(r, &pool, &pods, &ca, now, expires_at).await?;
        if won.is_none() {
            self.recycle(r, &pool, &pods, &foreign).await;
            if contended {
                self.wake_later(&r.key);
            }
        }
        Ok(won)
    }

    async fn cold(
        &self,
        r: &Req<'_>,
        pool: &BuildKitPool,
        pods: &[Arc<Pod>],
        ca: &Arc<CaBundle>,
        now: OffsetDateTime,
        expires_at: OffsetDateTime,
    ) -> Result<Option<Won>, AllocError> {
        let (ns, name) = (&r.key.0, &r.key.1);
        let now_s = fmt_ts(now);
        let start = i64::try_from(seconds(&pool.spec.workers.start_timeout)?)
            .map(time::Duration::seconds)
            .map_err(|_| AllocError::Internal)?;
        let max = u32::try_from(pool.spec.scaling.max).unwrap_or(0);
        let mut slots = free_slots(
            pods.iter().filter_map(|p| p.metadata.name.as_deref()),
            name,
            max,
        );
        cache::order_cold(pool, &mut slots, r.local);
        let id = r.alloc_id.as_str();
        for slot in slots.into_iter().take(MAX_TRIES) {
            let have = match cache::ensure_volume(&*self.backend, &self.metrics, pool, slot).await?
            {
                Volume::Ready(have) => have,
                Volume::Skip => continue,
                Volume::Quota => return Ok(None),
            };
            let claimed_until = claim_until(expires_at, start);
            let claim = Claim {
                alloc_id: id.to_owned(),
                owner: r.owner.clone(),
                allocated_at: now_s.clone(),
                expires_at: fmt_ts(claimed_until),
            };
            // The pod is single-use and retired when the allocation ends, so its
            // cert only has to outlive the allocation.
            let validity = expires_at + CERT_MARGIN - now;
            let mut worker =
                spawn::prepare(pool, ca, slot, Some(claim), now, validity).map_err(|e| {
                    error!(error = %e, "issuing the worker cert");
                    AllocError::Internal
                })?;
            if worker.not_after < expires_at + CERT_MARGIN {
                // Waiting would not help: every slot gets a cert from the same CA.
                warn!(namespace = %ns, pool = %name, "the internal CA ends before the allocation would");
                return Err(AllocError::CaUnavailable);
            }
            // The pod serves the caller's scope. Its volume is kept only when that
            // scope owns it; otherwise its init container empties it.
            let (mut before, mut wiped) = (None, false);
            if rnd::is_cached_slot(pool, slot) {
                match rnd::bind_pod(&mut worker.pod, have.binding.as_ref(), r.scope.as_deref()) {
                    rnd::Bound::Kept => before = have.keys,
                    rnd::Bound::Wiped => wiped = true,
                    rnd::Bound::Fresh => {}
                }
            }
            if let Some((k, cap)) = &r.affinity
                && rnd::is_cached_slot(pool, slot)
            {
                rnd::set_pod_cache_keys(
                    &mut worker.pod,
                    &rnd::push_key(before.as_deref(), k, *cap),
                );
            }
            let pname = worker.pod.metadata.name.clone().unwrap_or_default();
            // Subscribe before the create so no event about the new pod is missed.
            let sub = (self.subscribe)();
            let rendered = Box::new(worker.pod.clone());
            let (mut guard, s) = self.create_detached(r, worker, &pname).await?;
            if let Err(e) = &s.secret {
                // The pod cannot start without it; the guard retires it now.
                error!(namespace = %ns, pool = %name, pod = %pname, error = %e, "creating the worker Secret");
                return Err(AllocError::Internal);
            }
            let pod = match (s.out, s.pod) {
                (CreateOutcome::Created(p), _) => *p,
                // The answer was lost, but the read-back found the pod this
                // create made (by its nonce), and its Secret was created.
                (CreateOutcome::Unknown, spawn::PodCreated::Yes { uid, .. }) => {
                    let mut p = *rendered;
                    p.metadata.uid = Some(uid);
                    guard.grace = grace_of(&p);
                    p
                }
                (CreateOutcome::Unknown, _) => return Err(AllocError::UpstreamError),
                (CreateOutcome::Exists, _) => continue,
                (CreateOutcome::Quota, _) => return Ok(None),
                (CreateOutcome::Failed(error), _) => {
                    error!(namespace = %ns, pool = %name, pod = %pname, error, "creating worker pod");
                    return Err(AllocError::Internal);
                }
            };
            let Some(uid) = pod.metadata.uid.clone() else {
                return Err(AllocError::Internal);
            };
            return Ok(Some(Won {
                guard,
                uid,
                ca: Arc::clone(ca),
                pod,
                cold: Some(sub),
                expires_at,
                claimed_until,
                keys_before: before,
                wiped,
            }));
        }
        Ok(None)
    }

    /// Wakes the queue of `key` after [`CONFLICT_RETRY`], so its head tries
    /// again with a fresher pod store.
    fn wake_later(&self, key: &PoolKey) {
        let (queues, key) = (Arc::clone(&self.queues), key.clone());
        tokio::spawn(async move {
            tokio::time::sleep(CONFLICT_RETRY).await;
            queues.wake(&key);
        });
    }

    /// When a request found no pod and no slot, retire one warm pod of a
    /// cached slot that serves another cache scope ([`cache::recycle_pick`]).
    /// Its volume is unbound first, so the slot's next pod serves any scope.
    /// Without this, warm pods of other scopes could hold every slot for good.
    async fn recycle(
        &self,
        r: &Req<'_>,
        pool: &BuildKitPool,
        pods: &[Arc<Pod>],
        foreign: &[&Arc<Pod>],
    ) {
        let (ns, name) = (&r.key.0, &r.key.1);
        let max = u32::try_from(pool.spec.scaling.max).unwrap_or(0);
        let free = free_slots(
            pods.iter().filter_map(|p| p.metadata.name.as_deref()),
            name,
            max,
        )
        .len();
        let Some(pod) = cache::recycle_pick(pool, pods, free, foreign) else {
            return;
        };
        let (Some(pname), Some(uid)) = (&pod.metadata.name, &pod.metadata.uid) else {
            return;
        };
        if !cache::unbind(&*self.backend, &self.metrics, pool, pod).await {
            return;
        }
        if retire(&*self.backend, ns, pname, Some(uid), FREE, grace_of(pod)).await == Retired::Yes {
            self.metrics.release(ns, name, "cache_scope");
            info!(namespace = %ns, pool = %name, pod = %pname, reason = "cache_scope", "retired a warm pod of another cache scope");
        }
    }

    /// Runs [`spawn::spawn`] (pod, then its Secret) in its own task, so a
    /// cancelled request cannot stop it between the two creates and leave a
    /// pod that cannot start. The task holds the orphan guard until the
    /// outcome is known and hands it back; if the request is gone by then,
    /// the guard drops in the task and retires the pod.
    async fn create_detached(
        &self,
        r: &Req<'_>,
        worker: spawn::Worker,
        pname: &str,
    ) -> Result<(OrphanGuard, spawn::Spawned<CreateOutcome>), AllocError> {
        // Armed first, by name (no uid yet), because the create may land
        // although its answer is lost.
        let mut guard = self.guard(r, pname, None, None);
        let backend = Arc::clone(&self.backend);
        let ns = r.key.0.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let s = spawn::spawn(&*backend, worker, |pod| backend.create_pod(&ns, pod)).await;
            if let spawn::PodCreated::Yes { uid, .. } = &s.pod {
                guard.uid = Some(uid.clone());
                guard.confirmed = true;
            }
            match &s.out {
                CreateOutcome::Created(p) => guard.grace = grace_of(p),
                // The pod may exist, so the undo stays armed.
                CreateOutcome::Unknown => {}
                CreateOutcome::Exists | CreateOutcome::Quota | CreateOutcome::Failed(_) => {
                    guard.armed = false;
                }
            }
            // A send to a dropped request returns the guard, which drops here.
            drop(tx.send((guard, s)));
        });
        rx.await.map_err(|_| {
            error!(pod = %pname, "the cold create task ended without an outcome");
            AllocError::Internal
        })
    }

    /// Wait for a cold pod to be Ready (store + broadcast, no polling), for
    /// `start`, but never past [`READY_MARGIN`] before the claim ends: the
    /// leader retires the pod at the claim value, so a later Ready could only
    /// end in `pod_lost`.
    async fn await_ready(
        &self,
        won: &mut Won,
        mut sub: PodSubscription,
        start: Duration,
    ) -> Result<(), AllocError> {
        let left = won.claimed_until - READY_MARGIN - OffsetDateTime::now_utc();
        let deadline = Instant::now() + start.min(left.try_into().unwrap_or(Duration::ZERO));
        let oref = ObjectRef::new(&won.guard.name).within(&won.guard.ns);
        let mut seen = false;
        loop {
            match self.pods.get(&oref) {
                Some(p) if p.metadata.uid.as_deref() == Some(won.uid.as_str()) => {
                    seen = true;
                    if p.metadata.deletion_timestamp.is_some()
                        || ann(&p, ANN_ALLOC) != Some(won.guard.alloc_id.as_str())
                    {
                        return Err(AllocError::PodLost);
                    }
                    if is_ready(&p) {
                        won.pod = (*p).clone();
                        return Ok(());
                    }
                }
                Some(_) => return Err(AllocError::PodLost),
                None if seen => return Err(AllocError::PodLost),
                None => {}
            }
            match tokio::time::timeout_at(deadline, sub.recv()).await {
                Err(_) => {
                    won.guard.reason = "start_timeout";
                    return Err(AllocError::StartTimeout);
                }
                Ok(None) => return Err(AllocError::Internal),
                Ok(Some(PodEvent::Delete(p)))
                    if p.metadata.uid.as_deref() == Some(won.uid.as_str()) =>
                {
                    return Err(AllocError::PodLost);
                }
                Ok(Some(_)) => {}
            }
        }
    }

    /// The pods claimed for `id`, among pods of a pool in the watch store: the
    /// managed-by label, the pool label and namespace, and a controller
    /// ownerReference to that pool's uid. The id is public, so a pod outside
    /// a pool must never match.
    fn find_alloc(&self, id: &str) -> Vec<Arc<Pod>> {
        self.pods
            .state()
            .into_iter()
            .filter(|p| ann(p, ANN_ALLOC) == Some(id) && self.in_known_pool(p))
            .collect()
    }

    fn in_known_pool(&self, pod: &Pod) -> bool {
        let labels = pod.metadata.labels.as_ref();
        let (Some(ns), Some(pool)) = (
            pod.metadata.namespace.as_deref(),
            labels.and_then(|l| l.get(LABEL_POOL)),
        ) else {
            return false;
        };
        labels
            .and_then(|l| l.get(LABEL_MANAGED_BY))
            .map(String::as_str)
            == Some(CONTROLLER)
            && self
                .pools
                .get(&ObjectRef::new(pool).within(ns))
                .and_then(|p| p.metadata.uid.clone())
                .is_some_and(|uid| of_pool(pod, ns, pool, &uid))
    }

    /// DELETE /v1/allocations/{id}: owner-only. A tenant may plant a
    /// look-alike pod in its own pool, so every matching pod the caller owns
    /// is retired. Each retire tests the uid and `alloc == id`, so retiring a
    /// decoy is harmless and the real pod is always among them.
    pub async fn release(&self, who: &Identity, id: &str) -> Result<(), AllocError> {
        if id.parse::<AllocId>().is_err() {
            return Err(AllocError::NotFound);
        }
        let mut sub = (self.subscribe)();
        let mut found = self.find_alloc(id);
        if found.is_empty() {
            let Ok(_permit) = self.release_waiters.try_acquire() else {
                warn!(
                    alloc_id = id,
                    result = "busy",
                    "release refused: too many waiting"
                );
                return Err(AllocError::ReleaseBusy);
            };
            let deadline = Instant::now() + RELEASE_WAIT;
            while found.is_empty() {
                match tokio::time::timeout_at(deadline, sub.recv()).await {
                    // Deadline, or the Pod watch closed (shutdown).
                    Err(_) | Ok(None) => return Err(AllocError::NotFound),
                    // The store already holds the event's pod; rescan only on a
                    // match, so every matching pod is still retired.
                    Ok(Some(PodEvent::Apply(p))) if ann(&p, ANN_ALLOC) == Some(id) => {
                        found = self.find_alloc(id);
                    }
                    Ok(Some(PodEvent::Resync)) => found = self.find_alloc(id),
                    Ok(Some(_)) => {}
                }
            }
        }
        let owner = who.owner();
        found.retain(|p| ann(p, ANN_OWNER).map(str::as_bytes) == Some(owner.as_bytes()));
        if found.is_empty() {
            warn!(alloc_id = id, result = "forbidden", "release refused");
            return Err(AllocError::Forbidden);
        }
        let mut worst = Retired::Lost;
        for pod in found {
            let ns = pod.metadata.namespace.clone().unwrap_or_default();
            let pool = pod
                .metadata
                .labels
                .as_ref()
                .and_then(|l| l.get(LABEL_POOL))
                .cloned()
                .unwrap_or_default();
            let name = pod.metadata.name.clone().unwrap_or_default();
            let Some(uid) = pod.metadata.uid.clone() else {
                continue;
            };
            match retire(&*self.backend, &ns, &name, Some(&uid), id, grace_of(&pod)).await {
                Retired::Yes => {
                    self.metrics.release(&ns, &pool, "released");
                    info!(namespace = %ns, pool = %pool, pod = %name, alloc_id = id, reason = "released", "release");
                    worst = Retired::Yes;
                }
                Retired::Lost => {}
                r @ Retired::Failed => worst = if worst == Retired::Yes { worst } else { r },
                r @ Retired::Unknown => {
                    if worst == Retired::Lost {
                        worst = r;
                    }
                }
            }
        }
        match worst {
            Retired::Yes => Ok(()),
            Retired::Lost => Err(AllocError::NotFound),
            Retired::Unknown => Err(AllocError::UpstreamError),
            Retired::Failed => Err(AllocError::Internal),
        }
    }
}

/// Records `buildkit_queue_wait_seconds` exactly once, also on cancellation.
struct WaitRecord<'a> {
    metrics: &'a AllocMetrics,
    key: &'a PoolKey,
    since: Instant,
    result: &'static str,
}

impl Drop for WaitRecord<'_> {
    fn drop(&mut self) {
        self.metrics.queue_wait(
            &self.key.0,
            &self.key.1,
            self.result,
            self.since.elapsed().as_secs_f64(),
        );
    }
}
