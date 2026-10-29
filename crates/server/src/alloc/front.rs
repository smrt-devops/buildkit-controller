//! The front door every replica runs: authenticate (done by the HTTP layer),
//! check the request, create or read the caller's claim. It signs nothing and
//! keeps no queue; the claim is the allocation's whole state, so any replica
//! answers for any allocation.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use buildkit_controller_api::claim::{BuildKitClaim, BuildKitClaimSpec, ClaimPhase, PoolRef};
use buildkit_controller_api::dto::{
    AllocationResponse, AllocationState, CreateAllocationRequest, PendingAllocation,
};
use buildkit_controller_api::v1alpha1::{self as crd, BuildKitPool};
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::reflector::{ObjectRef, Store};
use time::OffsetDateTime;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::backend::{Backend, Write};
use super::bind::retire_pod;
use super::cache;
use super::certs::CertSource;
use super::claims;
use super::error::AllocError;
use super::metrics::AllocMetrics;
use super::pods::{ann, fmt_ts, key_alloc_id, of_pool};
use super::pool::{MIN_TTL, access_rules, pool_ready, seconds};
use crate::auth::Identity;
use crate::pki::{AllocId, ClientPublicKey, Namespace, PoolName};
use crate::render::{ANN_ALLOC, ANN_CLAIM, ANN_OWNER, LABEL_POOL};
use crate::runtime::stores::{ClaimEvent, ClaimSubscription};

/// How long `POST /v1/allocations` waits for the claim to bind.
pub const CREATE_WAIT: Duration = Duration::from_secs(20);
/// The longest `wait` of `GET /v1/allocations/{id}`.
pub const MAX_WAIT: Duration = Duration::from_secs(25);

/// What an allocation call found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Bound: the full response.
    Bound(Box<AllocationResponse>),
    /// Still waiting in the pool's queue.
    Pending(PendingAllocation),
}

/// The front door's inputs.
pub struct Front {
    backend: Arc<dyn Backend>,
    pools: Store<BuildKitPool>,
    claims: Store<BuildKitClaim>,
    subscribe: Box<dyn Fn() -> ClaimSubscription + Send + Sync>,
    certs: Arc<dyn CertSource>,
    /// The controller's namespace, where CI claims live.
    namespace: String,
    served: HashSet<String>,
    metrics: AllocMetrics,
    stopping: CancellationToken,
    /// Worker pods, for releasing allocations made before claims existed.
    /// v0.1.0 upgrade shim: remove in the release after v3.
    pods: Option<Store<Pod>>,
}

impl Front {
    /// Assemble; `subscribe` returns a fresh claim event subscription.
    #[allow(clippy::too_many_arguments, reason = "plain constructor")]
    #[must_use]
    pub fn new(
        backend: Arc<dyn Backend>,
        pools: Store<BuildKitPool>,
        claims: Store<BuildKitClaim>,
        subscribe: Box<dyn Fn() -> ClaimSubscription + Send + Sync>,
        certs: Arc<dyn CertSource>,
        namespace: String,
        served: HashSet<String>,
        metrics: AllocMetrics,
    ) -> Self {
        Self {
            backend,
            pools,
            claims,
            subscribe,
            certs,
            namespace,
            served,
            metrics,
            stopping: CancellationToken::new(),
            pods: None,
        }
    }

    /// Releases of allocations made before every allocation was a claim
    /// retire their pods from this store. v0.1.0 upgrade shim: remove in the
    /// release after v3.
    #[must_use]
    pub fn with_pods(mut self, pods: Store<Pod>) -> Self {
        self.pods = Some(pods);
        self
    }

    /// Waits end early once `stopping` fires: the caller polls another replica.
    #[must_use]
    pub fn with_stopping(mut self, stopping: CancellationToken) -> Self {
        self.stopping = stopping;
        self
    }

    /// `POST /v1/allocations` after authentication: check the request, create
    /// the claim, wait up to [`CREATE_WAIT`] for it to bind.
    ///
    /// The claim is named after the request's public key, so the call is
    /// idempotent per key: a repeated `POST` from the same caller finds the
    /// claim the first one made and answers for it the same way, and never
    /// makes a second one. A `POST` the client drops leaves its claim for the
    /// retry to find, in its place in the queue. A client that never comes
    /// back leaves a pending claim that fails at its queue timeout, or a bound
    /// one that ends at its TTL, as it would had it crashed after the answer.
    pub async fn allocate(
        &self,
        who: &Identity,
        req: &CreateAllocationRequest,
    ) -> Result<Outcome, AllocError> {
        let started = Instant::now();
        let claim = match self.claim_for(who, req) {
            Ok(c) => c,
            Err(e) => {
                // Unknown pools are counted with empty labels, so request
                // strings never become label values (bounded cardinality).
                let (ns, pool) = match &e {
                    AllocError::PoolNotFound => ("", ""),
                    _ => (req.namespace.as_str(), req.pool.as_str()),
                };
                self.metrics.allocation(ns, pool, e.code().as_str());
                info!(
                    namespace = ns,
                    pool,
                    result = e.code().as_str(),
                    "allocation refused"
                );
                return Err(e);
            }
        };
        let id = claim.metadata.name.clone().unwrap_or_default();
        // Subscribed before the create, so the bind cannot slip past the wait.
        let mut sub = (self.subscribe)();
        let found = self.create_or_find(who, claim).await;
        let repeat = matches!(found, Ok(Some(_)));
        if let Ok(Some(c)) = &found {
            self.touch(c).await;
        }
        let r = match found {
            Err(e) => Err(e),
            Ok(Some(c)) if claims::phase(&c) != ClaimPhase::Pending => {
                self.answer(&id, Some(c)).await
            }
            Ok(c) => {
                let at = Instant::now() + CREATE_WAIT;
                let settled = self.settle(&id, at, &mut sub).await.or(c);
                self.answer(&id, settled).await
            }
        };
        info!(namespace = %req.namespace, pool = %req.pool, alloc_id = %id, owner = %who.owner(),
            repeat, state = state_of(&r), duration_ms = started.elapsed().as_millis(), "allocation");
        r
    }

    /// Create `claim`, or find the claim of the same name (the same key) that
    /// an earlier `POST` created: `Ok(None)` when created. Another caller's
    /// claim is [`AllocError::Forbidden`] and reveals nothing about it.
    async fn create_or_find(
        &self,
        who: &Identity,
        claim: BuildKitClaim,
    ) -> Result<Option<Arc<BuildKitClaim>>, AllocError> {
        let id = claim.metadata.name.clone().unwrap_or_default();
        let pool = claim.spec.pool_ref.clone();
        match self.backend.create_claim(claim).await {
            Write::Done => Ok(None),
            Write::Exists => {
                let c = match self.owned(who, &id).await {
                    // Deleted since the create: the client's next try makes it anew.
                    Err(AllocError::NotFound) => return Err(AllocError::UpstreamError),
                    r => r?,
                };
                if c.spec.pool_ref != pool {
                    return Err(AllocError::InvalidRequest(
                        "publicKey belongs to an allocation from another pool".into(),
                    ));
                }
                Ok(Some(c))
            }
            Write::Unknown => Err(AllocError::UpstreamError),
            w => {
                error!(claim = %id, outcome = ?w, "creating claim");
                Err(AllocError::Internal)
            }
        }
    }

    /// The answer to a `POST` for claim `id` as found: pending while it is not
    /// seen yet, or bound but this replica cannot name the CA yet, so the
    /// client polls for the response rather than lose the id.
    async fn answer(&self, id: &str, c: Option<Arc<BuildKitClaim>>) -> Result<Outcome, AllocError> {
        let Some(c) = c else {
            return Ok(pending(id, None));
        };
        match self.outcome(&c) {
            Err(AllocError::CaUnavailable) => Ok(pending(id, None)),
            r => self.deliver(&c, r).await,
        }
    }

    /// Mark `c` delivered before its bound answer `r` goes out, so the leader
    /// never takes it for abandoned while its client holds the certificate. A
    /// mark that fails turns the answer into a 503 that the client retries.
    async fn deliver(
        &self,
        c: &BuildKitClaim,
        r: Result<Outcome, AllocError>,
    ) -> Result<Outcome, AllocError> {
        if !matches!(r, Ok(Outcome::Bound(_))) || claims::delivered(c) {
            return r;
        }
        self.annotate(c, claims::ANN_DELIVERED, "true".into())
            .await?;
        r
    }

    /// Note that the caller of `c` asked for it, at most every
    /// [`claims::SEEN_EVERY`]: the leader deletes an undelivered claim nobody
    /// asks for. A failed write is retried on the next request.
    async fn touch(&self, c: &BuildKitClaim) {
        let now = OffsetDateTime::now_utc();
        if !claims::seen_due(c, now) {
            return;
        }
        if let Err(e) = self.annotate(c, claims::ANN_SEEN_AT, fmt_ts(now)).await {
            warn!(claim = ?c.metadata.name, error = %e, "recording that a claim was asked for");
        }
    }

    async fn annotate(
        &self,
        c: &BuildKitClaim,
        key: &str,
        value: String,
    ) -> Result<(), AllocError> {
        let (Some(name), Some(uid)) = (c.metadata.name.as_deref(), c.metadata.uid.as_deref())
        else {
            return Err(AllocError::Internal);
        };
        let set = BTreeMap::from([(key.to_owned(), value)]);
        match self
            .backend
            .annotate_claim(&self.namespace, name, uid, set)
            .await
        {
            Write::Done => Ok(()),
            Write::Gone => Err(AllocError::NotFound),
            Write::Unknown => Err(AllocError::UpstreamError),
            w => {
                error!(claim = name, outcome = ?w, "annotating claim");
                Err(AllocError::Internal)
            }
        }
    }

    /// `GET /v1/allocations/{id}`: the allocation, after waiting up to `wait`
    /// (at most [`MAX_WAIT`]) while it is pending.
    pub async fn get(
        &self,
        who: &Identity,
        id: &str,
        wait: Duration,
    ) -> Result<Outcome, AllocError> {
        let mut sub = (self.subscribe)();
        let c = self.owned(who, id).await?;
        self.touch(&c).await;
        let c = if claims::phase(&c) == ClaimPhase::Pending && !wait.is_zero() {
            // A claim the store does not hold yet answers as last read.
            let at = Instant::now() + wait.min(MAX_WAIT);
            self.settle(id, at, &mut sub).await.unwrap_or(c)
        } else {
            c
        };
        let r = self.outcome(&c);
        self.deliver(&c, r).await
    }

    /// `DELETE /v1/allocations/{id}`: deletes the claim; the leader retires its pod.
    pub async fn release(&self, who: &Identity, id: &str) -> Result<(), AllocError> {
        let c = match self.owned(who, id).await {
            Err(AllocError::NotFound) => return self.release_unclaimed(who, id).await,
            r => r?,
        };
        let uid = c.metadata.uid.as_deref().unwrap_or_default();
        match self.backend.delete_claim(&self.namespace, id, uid).await {
            Write::Done => {
                info!(alloc_id = id, result = "released", "release");
                Ok(())
            }
            Write::Gone => Err(AllocError::NotFound),
            Write::Unknown => Err(AllocError::UpstreamError),
            w => {
                error!(alloc_id = id, outcome = ?w, "deleting claim");
                Err(AllocError::Internal)
            }
        }
    }

    /// Release an allocation made before every allocation was a claim: its
    /// pods carry the id and the caller's owner but no `P claim`. Each retire
    /// tests the uid and `alloc == id`, so a look-alike pod is harmless.
    /// v0.1.0 upgrade shim: remove in the release after v3.
    async fn release_unclaimed(&self, who: &Identity, id: &str) -> Result<(), AllocError> {
        let Some(pods) = &self.pods else {
            return Err(AllocError::NotFound);
        };
        let found: Vec<Arc<Pod>> = pods
            .state()
            .into_iter()
            .filter(|p| ann(p, ANN_ALLOC) == Some(id) && ann(p, ANN_CLAIM).is_none())
            .filter(|p| p.metadata.deletion_timestamp.is_none() && self.in_known_pool(p))
            .collect();
        if found.is_empty() {
            return Err(AllocError::NotFound);
        }
        let owner = who.owner();
        let mine: Vec<&Arc<Pod>> = found
            .iter()
            .filter(|p| ann(p, ANN_OWNER).map(str::as_bytes) == Some(owner.as_bytes()))
            .collect();
        if mine.is_empty() {
            warn!(alloc_id = id, result = "forbidden", "release refused");
            return Err(AllocError::Forbidden);
        }
        let mut released = false;
        for p in mine {
            let ns = p.metadata.namespace.as_deref().unwrap_or_default();
            let pool = p
                .metadata
                .labels
                .as_ref()
                .and_then(|l| l.get(LABEL_POOL))
                .map_or("", String::as_str);
            released |=
                retire_pod(&*self.backend, &self.metrics, ns, pool, p, id, "released").await;
        }
        if released {
            info!(
                alloc_id = id,
                result = "released",
                "release of an allocation without a claim"
            );
            Ok(())
        } else {
            Err(AllocError::NotFound)
        }
    }

    /// A pod of a pool in the store: pool label, namespace and a controller
    /// ownerReference to that pool's uid. The id is public, so a pod outside a
    /// pool must never match.
    fn in_known_pool(&self, pod: &Pod) -> bool {
        let (Some(ns), Some(pool)) = (
            pod.metadata.namespace.as_deref(),
            pod.metadata.labels.as_ref().and_then(|l| l.get(LABEL_POOL)),
        ) else {
            return false;
        };
        self.pools
            .get(&ObjectRef::new(pool).within(ns))
            .and_then(|p| p.metadata.uid.clone())
            .is_some_and(|uid| of_pool(pod, ns, pool, &uid))
    }

    /// The claim of allocation `id`, if `who` owns it. A claim created on
    /// another replica a moment ago may not be in the store yet, so a miss
    /// asks the API server.
    async fn owned(&self, who: &Identity, id: &str) -> Result<Arc<BuildKitClaim>, AllocError> {
        if id.parse::<AllocId>().is_err() {
            return Err(AllocError::NotFound);
        }
        let c = match self.stored(id) {
            Some(c) => c,
            None => match self.backend.get_claim(&self.namespace, id).await {
                Ok(Some(c)) => Arc::new(c),
                Ok(None) => return Err(AllocError::NotFound),
                Err(Write::Unknown) => return Err(AllocError::UpstreamError),
                Err(w) => {
                    error!(alloc_id = id, outcome = ?w, "reading claim");
                    return Err(AllocError::Internal);
                }
            },
        };
        let owner = claims::caller(&c, &self.namespace).map(|c| c.owner);
        if owner.as_deref().map(str::as_bytes) != Some(who.owner().as_bytes()) {
            warn!(
                alloc_id = id,
                result = "forbidden",
                "allocation refused to another identity"
            );
            return Err(AllocError::Forbidden);
        }
        Ok(c)
    }

    fn stored(&self, id: &str) -> Option<Arc<BuildKitClaim>> {
        self.claims.get(&ObjectRef::new(id).within(&self.namespace))
    }

    /// The claim once it is no longer pending, or as it stands at `deadline`
    /// (or when the replica starts stopping). `None` when it is gone.
    async fn settle(
        &self,
        id: &str,
        deadline: Instant,
        sub: &mut ClaimSubscription,
    ) -> Option<Arc<BuildKitClaim>> {
        loop {
            let c = self.stored(id);
            if c.as_ref()
                .is_some_and(|c| claims::phase(c) != ClaimPhase::Pending)
            {
                return c;
            }
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => return c,
                () = self.stopping.cancelled() => return c,
                ev = sub.recv() => match ev {
                    None => return c,
                    Some(ClaimEvent::Apply(x) | ClaimEvent::Delete(x)) if x.metadata.name.as_deref() != Some(id) => {}
                    Some(ClaimEvent::Delete(_)) => return None,
                    Some(_) => {}
                },
            }
        }
    }

    /// The API answer for a claim.
    fn outcome(&self, c: &BuildKitClaim) -> Result<Outcome, AllocError> {
        if let Some((code, message)) = claims::api_error(c) {
            return Err(AllocError::of_claim(code, message));
        }
        let id = c.metadata.name.clone().unwrap_or_default();
        let s = c
            .status
            .as_ref()
            .filter(|_| claims::phase(c) == ClaimPhase::Bound);
        let Some(s) = s else {
            let all = self.claims.state();
            return Ok(pending(&id, claims::position(&all, &self.served, c)));
        };
        let bundle = self.certs.bundle().ok_or(AllocError::CaUnavailable)?;
        let field = |v: &Option<String>| v.clone().ok_or(AllocError::Internal);
        Ok(Outcome::Bound(Box::new(AllocationResponse {
            id: field(&s.allocation_id)?,
            namespace: c.spec.pool_ref.namespace.clone(),
            pool: c.spec.pool_ref.name.clone(),
            owner: claims::caller(c, &self.namespace)
                .map(|c| c.owner)
                .unwrap_or_default(),
            endpoint: field(&s.endpoint)?,
            server_name: field(&s.server_name)?,
            ca_cert: self.certs.ca_cert_pem(&bundle),
            client_cert: field(&s.certificate)?,
            client_key: None,
            expires_at: field(&s.expires_at)?,
            cache: s.cache.clone(),
        })))
    }

    /// Everything that can fail on the caller's input happens here, before
    /// the claim exists: the pool and its access rules, readiness, TTL, queue
    /// timeout and key.
    fn claim_for(
        &self,
        who: &Identity,
        req: &CreateAllocationRequest,
    ) -> Result<BuildKitClaim, AllocError> {
        let pool = self.lookup(who, &req.namespace, &req.pool)?;
        self.certs.bundle().ok_or(AllocError::CaUnavailable)?;
        if !pool_ready(&pool) {
            return Err(AllocError::NotReady);
        }
        let a = &pool.spec.allocation;
        let internal = |e: String| {
            error!(error = %e, "invalid duration in pool spec");
            AllocError::Internal
        };
        let ttl = match req.ttl_seconds {
            Some(t) => t,
            None => seconds(&a.default_ttl).map_err(internal)?,
        };
        if ttl < MIN_TTL || ttl > seconds(&a.max_ttl).map_err(internal)? {
            return Err(AllocError::InvalidRequest(format!(
                "ttlSeconds must be between {MIN_TTL} and {}",
                a.max_ttl.as_str()
            )));
        }
        let qt = match req.queue_timeout_seconds {
            Some(t) => t,
            None => seconds(&a.queue_timeout).map_err(internal)?,
        };
        if qt > seconds(&a.max_queue_timeout).map_err(internal)? {
            return Err(AllocError::InvalidRequest(format!(
                "queueTimeoutSeconds must be at most {}",
                a.max_queue_timeout.as_str()
            )));
        }
        let pem = req.public_key.clone().unwrap_or_default();
        let key = ClientPublicKey::from_pem(&pem).map_err(|_| {
            AllocError::InvalidRequest("publicKey must be a PEM SPKI ECDSA P-256 key".into())
        })?;
        let id = key_alloc_id(&key).ok_or(AllocError::Internal)?;
        let mut c = BuildKitClaim::new(
            id.as_str(),
            BuildKitClaimSpec {
                pool_ref: PoolRef {
                    namespace: req.namespace.clone(),
                    name: req.pool.clone(),
                },
                ttl: Some(crd::Duration::new(&format!("{ttl}s"))),
                queue_timeout: Some(crd::Duration::new(&format!("{qt}s"))),
                cache_key: req.cache_key.as_ref().map(|k| k.as_str().to_owned()),
                cache: req.cache,
                public_key: Some(pem),
                owner: Some(who.owner()),
                cache_scope: Some(cache::scope(&pool, who)),
                cache_tier: Some(cache::tier(&pool, who)),
            },
        );
        c.metadata.namespace = Some(self.namespace.clone());
        c.metadata.annotations = Some(BTreeMap::from([(
            claims::ANN_SEEN_AT.to_owned(),
            fmt_ts(OffsetDateTime::now_utc()),
        )]));
        Ok(c)
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
}

fn pending(id: &str, position: Option<u32>) -> Outcome {
    Outcome::Pending(PendingAllocation {
        id: id.to_owned(),
        state: AllocationState::Pending,
        position,
    })
}

fn state_of(r: &Result<Outcome, AllocError>) -> &'static str {
    match r {
        Ok(Outcome::Bound(_)) => "bound",
        Ok(Outcome::Pending(_)) => "pending",
        Err(e) => e.code().as_str(),
    }
}
