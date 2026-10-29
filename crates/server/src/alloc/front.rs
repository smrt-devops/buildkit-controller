//! The front door every replica runs: authenticate (done by the HTTP layer),
//! check the request, create or read the caller's claim. It signs nothing and
//! keeps no queue; the claim is the allocation's whole state, so any replica
//! answers for any allocation.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use buildkit_controller_api::claim::{BuildKitClaim, BuildKitClaimSpec, ClaimPhase, PoolRef};
use buildkit_controller_api::dto::{
    AllocationResponse, AllocationState, CreateAllocationRequest, PendingAllocation,
};
use buildkit_controller_api::v1alpha1::{self as crd, BuildKitPool};
use kube::runtime::reflector::{ObjectRef, Store};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::backend::{Backend, Write};
use super::bind::{MIN_TTL, access_rules, pool_ready, seconds};
use super::cache;
use super::certs::CertSource;
use super::error::AllocError;
use super::metrics::AllocMetrics;
use super::pods::new_alloc_id;
use crate::auth::Identity;
use crate::claim::logic;
use crate::pki::{AllocId, ClientPublicKey, Namespace, PoolName};
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
        }
    }

    /// Waits end early once `stopping` fires: the caller polls another replica.
    #[must_use]
    pub fn with_stopping(mut self, stopping: CancellationToken) -> Self {
        self.stopping = stopping;
        self
    }

    /// `POST /v1/allocations` after authentication: check the request, create
    /// the claim, wait up to [`CREATE_WAIT`] for it to bind.
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
        let mut guard = Abandon {
            backend: Arc::clone(&self.backend),
            namespace: self.namespace.clone(),
            name: id.clone(),
            armed: true,
        };
        match self.backend.create_claim(claim).await {
            Write::Done => {}
            Write::Unknown => return Err(AllocError::UpstreamError),
            w => {
                guard.armed = false;
                error!(claim = %id, outcome = ?w, "creating claim");
                return Err(AllocError::Internal);
            }
        }
        let found = self
            .settle(&id, Instant::now() + CREATE_WAIT, &mut sub)
            .await;
        guard.armed = false;
        let r = match found {
            Some(c) => self.outcome(&c),
            // Created, but not seen yet.
            None => Ok(pending(&id, None)),
        };
        info!(namespace = %req.namespace, pool = %req.pool, alloc_id = %id, owner = %who.owner(),
            state = state_of(&r), duration_ms = started.elapsed().as_millis(), "allocation");
        r
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
        let c = if logic::phase(&c) == ClaimPhase::Pending && !wait.is_zero() {
            // A claim the store does not hold yet answers as last read.
            let at = Instant::now() + wait.min(MAX_WAIT);
            self.settle(id, at, &mut sub).await.unwrap_or(c)
        } else {
            c
        };
        self.outcome(&c)
    }

    /// `DELETE /v1/allocations/{id}`: deletes the claim; the leader retires its pod.
    pub async fn release(&self, who: &Identity, id: &str) -> Result<(), AllocError> {
        let c = self.owned(who, id).await?;
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
        let owner = logic::caller(&c, &self.namespace).map(|c| c.owner);
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
                .is_some_and(|c| logic::phase(c) != ClaimPhase::Pending)
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
        if let Some((code, message)) = logic::api_error(c) {
            return Err(AllocError::of_claim(code, message));
        }
        let id = c.metadata.name.clone().unwrap_or_default();
        let s = c
            .status
            .as_ref()
            .filter(|_| logic::phase(c) == ClaimPhase::Bound);
        let Some(s) = s else {
            let all = self.claims.state();
            return Ok(pending(&id, logic::position(&all, &self.served, c)));
        };
        let bundle = self.certs.bundle().ok_or(AllocError::CaUnavailable)?;
        let field = |v: &Option<String>| v.clone().ok_or(AllocError::Internal);
        Ok(Outcome::Bound(Box::new(AllocationResponse {
            id: field(&s.allocation_id)?,
            namespace: c.spec.pool_ref.namespace.clone(),
            pool: c.spec.pool_ref.name.clone(),
            owner: logic::caller(c, &self.namespace)
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
        ClientPublicKey::from_pem(&pem).map_err(|_| {
            AllocError::InvalidRequest("publicKey must be a PEM SPKI ECDSA P-256 key".into())
        })?;
        let id = new_alloc_id().ok_or(AllocError::Internal)?;
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

/// Deletes a claim whose creator went away before it could learn the id: the
/// POST was cancelled (client gone) between sending the create and returning.
struct Abandon {
    backend: Arc<dyn Backend>,
    namespace: String,
    name: String,
    armed: bool,
}

impl Drop for Abandon {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (backend, ns, name) = (
            Arc::clone(&self.backend),
            self.namespace.clone(),
            self.name.clone(),
        );
        rt.spawn(async move {
            // The create may still be in flight; one late retry covers it.
            for gap in [0, 2] {
                tokio::time::sleep(Duration::from_secs(gap)).await;
                let Ok(Some(c)) = backend.get_claim(&ns, &name).await else {
                    continue;
                };
                let uid = c.metadata.uid.unwrap_or_default();
                if matches!(backend.delete_claim(&ns, &name, &uid).await, Write::Done) {
                    info!(alloc_id = %name, "deleted the claim of an abandoned allocation");
                    return;
                }
            }
        });
    }
}
