//! The leader's allocator. One pass serves one pool's queue: its pending
//! claims, oldest first, each bound to a free warm pod. Binding is the pod
//! patch (its `test`s stop a deposed leader from booking a pod twice), then
//! the claim status: `Bound`, pod, expiry and certificate.
//!
//! Every step is idempotent. A claim's allocation id is fixed, so a pass that
//! finds a pod already carrying it finishes that bind instead of starting a
//! new one; a leader that dies between the pod patch and the status write
//! loses nothing.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use buildkit_controller_api::claim::{BuildKitClaim, CacheTier, ClaimPhase, secret_name};
use buildkit_controller_api::dto::CacheKey;
use buildkit_controller_api::v1alpha1::BuildKitPool;
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::reflector::{ObjectRef, Store};
use time::OffsetDateTime;
use tokio::time::Instant;
use tracing::{error, info, warn};

use super::backend::{Backend, PatchOutcome, Write};
use super::cache::{self, Plan};
use super::certs::CertSource;
use super::claims::{self, Binding, Caller, PoolKey, owner_hold_limit, reason};
use super::metrics::AllocMetrics;
use super::pods::{
    Bind, CERT_MARGIN, MAX_TRIES, Want, ann, claim_patch, fmt_ts, is_candidate, of_pool, parse_ts,
    retire_patch, shuffle, whole_seconds,
};
use super::pool::{MIN_TTL, access_rules, pool_ready, seconds};
use crate::pki::{AllocId, AllocationId, CaBundle, ClientPublicKey, LeafKey};
use crate::render::{self as rnd, ANN_ALLOC, ANN_EXPIRES_AT, template_hash};

/// How long the binder remembers a claim it bound or a pod it took, for
/// stores that have not caught up yet.
const RECENT: Duration = Duration::from_secs(30);
/// After a lost or unknown pod patch, the pool's next pass comes this soon:
/// the pod store lags, and no event may follow.
const RETRY: Duration = Duration::from_secs(1);

/// Static inputs from the config file.
#[derive(Debug, Clone)]
pub struct BindConfig {
    /// `edge.endpoint`.
    pub endpoint: String,
    /// `edge.serverName`.
    pub server_name: String,
    /// `edge.allocationDomain`, `None` when unset.
    pub allocation_domain: Option<String>,
    /// The controller's namespace, where the API creates claims.
    pub namespace: String,
    /// Namespaces whose claims are served.
    pub served: HashSet<String>,
    /// `registry.host`, `None` when the controller runs no cache registry.
    pub registry_host: Option<String>,
}

impl BindConfig {
    /// The endpoint of allocation `id`: per allocation with `allocationDomain`.
    #[must_use]
    pub fn endpoint(&self, id: &str) -> String {
        match &self.allocation_domain {
            Some(d) => {
                let port = self.endpoint.rsplit_once(':').map_or("", |(_, p)| p);
                format!("tcp://{id}.{d}:{port}")
            }
            None => self.endpoint.clone(),
        }
    }
}

/// What one claim asks of its pool, checked.
struct Ask {
    caller: Caller,
    alloc_id: AllocId,
    claim_ref: String,
    /// The claim's uid, written on the pod: a later claim of the same name is
    /// another claim and never takes this one's pod.
    claim_uid: String,
    ttl: time::Duration,
    /// A PEM key that parses.
    public_key: Option<String>,
    plan: Option<Plan>,
    deadline: Option<OffsetDateTime>,
}

/// A pod bound to a claim.
struct Won {
    pod: Pod,
}

/// What a pass carries from claim to claim.
#[derive(Default)]
struct Pass {
    /// The earliest queue deadline among the claims left waiting.
    until: Option<OffsetDateTime>,
    /// A pod patch was lost or its outcome is unknown, or the pod store lags one.
    retry: bool,
    /// Pods this pass took or saw lost; never offered twice.
    taken: HashSet<String>,
    /// Workers each owner holds in the pool, counting this pass's binds.
    held: HashMap<String, usize>,
}

impl Pass {
    fn until(&mut self, t: OffsetDateTime) {
        self.until = Some(self.until.map_or(t, |u| u.min(t)));
    }
}

#[derive(Default)]
struct Memory {
    /// Claim uids bound recently.
    bound: HashMap<String, Instant>,
    /// Pod uids taken recently.
    taken: HashMap<String, Instant>,
}

/// The leader's allocator for every pool.
pub struct Binder {
    pub(super) backend: Arc<dyn Backend>,
    pools: Store<BuildKitPool>,
    pods: Store<Pod>,
    claims: Store<BuildKitClaim>,
    certs: Arc<dyn CertSource>,
    cfg: BindConfig,
    metrics: AllocMetrics,
    memory: Mutex<Memory>,
}

impl Binder {
    /// Assemble from the shared stores.
    #[must_use]
    pub fn new(
        backend: Arc<dyn Backend>,
        stores: (Store<BuildKitPool>, Store<Pod>, Store<BuildKitClaim>),
        certs: Arc<dyn CertSource>,
        cfg: BindConfig,
        metrics: AllocMetrics,
    ) -> Self {
        let (pools, pods, claims) = stores;
        Self {
            backend,
            pools,
            pods,
            claims,
            certs,
            cfg,
            metrics,
            memory: Mutex::default(),
        }
    }

    /// The configuration it binds with.
    #[must_use]
    pub fn config(&self) -> &BindConfig {
        &self.cfg
    }

    /// The backend it writes through.
    #[must_use]
    pub fn backend(&self) -> &Arc<dyn Backend> {
        &self.backend
    }

    /// The metrics it counts into.
    #[must_use]
    pub fn metrics(&self) -> &AllocMetrics {
        &self.metrics
    }

    fn memory(&self) -> std::sync::MutexGuard<'_, Memory> {
        self.memory.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The pools with waiting claims.
    #[must_use]
    pub fn pools_waiting(&self) -> Vec<PoolKey> {
        claims::pending(&self.claims.state(), &self.cfg.served)
            .into_keys()
            .collect()
    }

    /// One pass over the queue of pool `key`. Returns when the pool needs
    /// another pass without an event: at the next queue deadline, or soon
    /// after a contended patch.
    pub async fn pass(&self, key: &PoolKey) -> Option<Duration> {
        let all = self.claims.state();
        let mut queue = claims::pending(&all, &self.cfg.served)
            .remove(key)
            .unwrap_or_default();
        if queue.is_empty() {
            return None;
        }
        let now = OffsetDateTime::now_utc();
        {
            let mut m = self.memory();
            m.bound.retain(|_, t| t.elapsed() < RECENT);
            m.taken.retain(|_, t| t.elapsed() < RECENT);
            queue.retain(|c| {
                c.metadata
                    .uid
                    .as_ref()
                    .is_none_or(|u| !m.bound.contains_key(u))
            });
        }
        let pool = self
            .pools
            .get(&ObjectRef::new(&key.1).within(&key.0))
            .filter(|p| p.metadata.uid.is_some());
        let mut pass = Pass {
            held: self.held(&all, key),
            ..Pass::default()
        };
        // Fair share: an owner at the hold limit lets a claim of another
        // owner under it, waiting further back, go first. Its claim is served
        // after the rest of the queue, so a worker nobody else took is never
        // left idle. Once an owner yields, its later claims follow it, so
        // one owner's claims keep their order.
        let limit = pool.as_deref().map_or(usize::MAX, owner_hold_limit);
        let own = self.cfg.namespace.as_str();
        let owners: Vec<Option<String>> = queue
            .iter()
            .map(|c| claims::caller(c, own).map(|x| x.owner))
            .collect();
        let mut behind: HashMap<&str, usize> = HashMap::new();
        for o in owners.iter().flatten() {
            *behind.entry(o.as_str()).or_default() += 1;
        }
        let mut later = Vec::new();
        let mut yielded: HashSet<&str> = HashSet::new();
        for (c, owner) in queue.iter().zip(&owners) {
            let Some(o) = owner.as_deref() else {
                self.serve(c, pool.as_deref(), now, &mut pass).await;
                continue;
            };
            if let Some(n) = behind.get_mut(o) {
                *n -= 1;
            }
            let held = |x: &str| pass.held.get(x).copied().unwrap_or(0);
            let yields = yielded.contains(o)
                || (held(o) >= limit
                    && behind
                        .iter()
                        .any(|(x, n)| *n > 0 && *x != o && held(x) < limit));
            if yields {
                yielded.insert(o);
                later.push(c);
            } else {
                self.serve(c, pool.as_deref(), now, &mut pass).await;
            }
        }
        for c in later {
            self.serve(c, pool.as_deref(), now, &mut pass).await;
        }
        let at = pass
            .until
            .map(|t| (t - now).try_into().unwrap_or(Duration::ZERO) + Duration::from_secs(1));
        match (at, pass.retry) {
            (Some(a), true) => Some(a.min(RETRY)),
            (None, true) => Some(RETRY),
            (a, false) => a,
        }
    }

    async fn serve(
        &self,
        c: &BuildKitClaim,
        pool: Option<&BuildKitPool>,
        now: OffsetDateTime,
        pass: &mut Pass,
    ) {
        let own = self.cfg.namespace.as_str();
        let (Some(caller), Some(alloc_id), Some(claim_ref), Some(claim_uid)) = (
            claims::caller(c, own),
            claims::alloc_id(c, own),
            claims::claim_ref(c),
            c.metadata.uid.clone(),
        ) else {
            return;
        };
        let ns = c.metadata.namespace.as_deref().unwrap_or_default();
        let admitted = pool.filter(|p| {
            caller.identity.as_ref().is_none_or(|id| {
                access_rules(p).is_some_and(|r| r.allows(id, &c.spec.pool_ref.namespace))
            })
        });
        let Some(pool) = admitted else {
            if pool.is_some() {
                info!(namespace = ns, claim = ?c.metadata.name, reason = "access_denied", "claim refused");
            }
            return self.no_pool(c, now, pass).await;
        };
        let host = self.cfg.registry_host.as_deref();
        let ask = match ask(c, pool, own, host, caller, alloc_id, (claim_ref, claim_uid)) {
            Ok(a) => a,
            Err(message) => {
                return self
                    .fail(c, reason::INVALID_REQUEST, &message, "invalid_request")
                    .await;
            }
        };
        let Some(ca) = self.certs.bundle() else {
            return self
                .wait(
                    c,
                    &ask,
                    reason::CA_UNAVAILABLE,
                    "no valid CA loaded",
                    now,
                    pass,
                )
                .await;
        };
        if let Some((pod, expires_at)) = self.bound_pod(pool, &ask) {
            let won = Won { pod };
            return self
                .finish(c, pool, &ask, &won, expires_at, &ca, pass)
                .await;
        }
        if !pool_ready(pool) {
            return self
                .wait(
                    c,
                    &ask,
                    reason::POOL_NOT_READY,
                    "the pool is not ready",
                    now,
                    pass,
                )
                .await;
        }
        let expires_at = whole_seconds(now) + ask.ttl;
        match self.claim_pod(pool, &ask, &ca, now, expires_at, pass).await {
            Some(won) => {
                self.finish(c, pool, &ask, &won, expires_at, &ca, pass)
                    .await;
            }
            None => {
                self.wait(c, &ask, reason::QUEUED, "waiting for a worker", now, pass)
                    .await;
            }
        }
    }

    /// Workers each owner holds in pool `key`: its `Bound` claims, and the
    /// claims this leader bound in the last [`RECENT`] that the store still
    /// shows `Pending`.
    fn held(&self, claims: &[Arc<BuildKitClaim>], key: &PoolKey) -> HashMap<String, usize> {
        let own = self.cfg.namespace.as_str();
        let m = self.memory();
        let mut out: HashMap<String, usize> = HashMap::new();
        for c in claims {
            let holds = match claims::phase(c) {
                ClaimPhase::Bound => true,
                ClaimPhase::Pending => c
                    .metadata
                    .uid
                    .as_ref()
                    .is_some_and(|u| m.bound.contains_key(u)),
                _ => false,
            };
            if holds
                && claims::pool_key(c) == *key
                && c.metadata.deletion_timestamp.is_none()
                && let Some(x) = claims::caller(c, own)
            {
                *out.entry(x.owner).or_default() += 1;
            }
        }
        out
    }

    /// A pod of `pool` that already carries this claim's allocation: a bind
    /// that landed while its status write did not. With its expiry.
    fn bound_pod(&self, pool: &BuildKitPool, ask: &Ask) -> Option<(Pod, OffsetDateTime)> {
        let (ns, name, uid) = pool_id(pool)?;
        self.pods.state().into_iter().find_map(|p| {
            let ok = of_pool(&p, ns, name, uid)
                && ann(&p, ANN_ALLOC) == Some(ask.alloc_id.as_str())
                && ann(&p, rnd::ANN_CLAIM) == Some(ask.claim_ref.as_str())
                && ann(&p, rnd::ANN_CLAIM_UID) == Some(ask.claim_uid.as_str())
                && claims::serves(&p, ask.alloc_id.as_str());
            let expires = ann(&p, ANN_EXPIRES_AT).and_then(parse_ts)?;
            ok.then(|| ((*p).clone(), expires))
        })
    }

    /// Claim a free warm pod for `ask`, at most [`MAX_TRIES`] candidates.
    async fn claim_pod(
        &self,
        pool: &BuildKitPool,
        ask: &Ask,
        ca: &CaBundle,
        now: OffsetDateTime,
        expires_at: OffsetDateTime,
        pass: &mut Pass,
    ) -> Option<Won> {
        let (ns, name, pool_uid) = pool_id(pool)?;
        let hash = template_hash(pool)
            .map_err(|e| error!(error = %e, "template hash"))
            .ok()?;
        let fp = ca.internal_ca().fingerprint_hex();
        let want = Want {
            template_hash: &hash,
            internal_ca: fp.get(..16).unwrap_or_default(),
            cert_not_after: expires_at + CERT_MARGIN,
        };
        let recent: HashSet<String> = self.memory().taken.keys().cloned().collect();
        let mut cands = Vec::new();
        for p in self.pods.state() {
            let Some(uid) = p.metadata.uid.as_ref() else {
                continue;
            };
            if pass.taken.contains(uid)
                || !of_pool(&p, ns, name, pool_uid)
                || !is_candidate(&p, &want)
            {
                continue;
            }
            if recent.contains(uid) {
                // The store has not caught up with a patch of ours, and no
                // pod event will wake the pool when it does.
                pass.retry = true;
            } else {
                cands.push(p);
            }
        }
        shuffle(&mut cands);
        let now_s = fmt_ts(now);
        let expires = fmt_ts(expires_at);
        let bind = Bind {
            alloc_id: ask.alloc_id.as_str(),
            owner: &ask.caller.owner,
            claim: &ask.claim_ref,
            claim_uid: &ask.claim_uid,
            now: &now_s,
            expires: &expires,
        };
        for pod in cands.iter().take(MAX_TRIES) {
            let (Some(pname), Some(uid)) = (&pod.metadata.name, &pod.metadata.uid) else {
                continue;
            };
            pass.taken.insert(uid.clone());
            let mut patch = claim_patch(uid, &bind);
            if let Some(p) = &ask.plan {
                p.claim_ops(&mut patch);
            }
            let outcome = self.backend.patch_pod(ns, pname, patch).await;
            // An unknown outcome is not remembered: the next pass adopts the
            // pod if the patch landed, and offers it again if it did not.
            if !matches!(outcome, PatchOutcome::Unknown) {
                self.memory().taken.insert(uid.clone(), Instant::now());
            }
            match outcome {
                PatchOutcome::Applied(p) => {
                    // The cache can lag an eviction: the edge would refuse this pod at once.
                    if p.metadata.deletion_timestamp.is_some()
                        || p.status.as_ref().and_then(|s| s.phase.as_deref()) != Some("Running")
                    {
                        info!(namespace = %ns, pool = %name, pod = %pname, "claimed pod is terminating, retiring it");
                        self.retire(ns, name, &p, ask.alloc_id.as_str(), "pod_lost")
                            .await;
                        continue;
                    }
                    return Some(Won { pod: *p });
                }
                PatchOutcome::Lost => {
                    self.metrics.conflict(ns, name);
                    pass.retry = true;
                }
                // Landed or not: the next pass finds the pod if it did.
                PatchOutcome::Unknown => pass.retry = true,
                PatchOutcome::Failed(error) => {
                    error!(namespace = %ns, pool = %name, pod = %pname, error, "claim patch");
                    return None;
                }
            }
        }
        None
    }

    /// Sign the certificate and write the bind into the claim (and its Secret).
    #[allow(clippy::too_many_arguments, reason = "one bind")]
    async fn finish(
        &self,
        c: &BuildKitClaim,
        pool: &BuildKitPool,
        ask: &Ask,
        won: &Won,
        expires_at: OffsetDateTime,
        ca: &CaBundle,
        pass: &mut Pass,
    ) {
        let pod = &won.pod;
        let (Some((pns, pname, _)), Some(pod_uid), Some(pod_name)) = (
            pool_id(pool),
            pod.metadata.uid.as_deref(),
            pod.metadata.name.as_deref(),
        ) else {
            return;
        };
        let (ns, name) = claim_names(c);
        let id = match (pns.parse(), pname.parse(), pod_uid.parse()) {
            (Ok(namespace), Ok(pool), Ok(pod_uid)) => AllocationId {
                namespace,
                pool,
                pod_uid,
                alloc_id: ask.alloc_id.clone(),
            },
            _ => return,
        };
        let key = match &ask.public_key {
            Some(pem) => match ClientPublicKey::from_pem(pem) {
                Ok(k) => LeafKey::Client(k),
                Err(_) => return,
            },
            None => LeafKey::Generate,
        };
        let cert = match ca.issue_allocation(&id, key, OffsetDateTime::now_utc(), expires_at) {
            Ok(c) => c,
            Err(e) => {
                error!(namespace = ns, claim = name, error = %e, "issuing allocation cert");
                pass.retry = true;
                return;
            }
        };
        let binding = Binding {
            alloc_id: ask.alloc_id.to_string(),
            pod: pod_name.to_owned(),
            endpoint: self.cfg.endpoint(ask.alloc_id.as_str()),
            server_name: self.cfg.server_name.clone(),
            expires_at: fmt_ts(expires_at),
            certificate: cert.cert_pem().to_owned(),
            cache: ask.plan.as_ref().map(Plan::response),
        };
        let secret = match cert.private_key_pem() {
            None => false,
            Some(key_pem) => {
                let ca_pem = self.certs.ca_cert_pem(ca);
                match self
                    .write_secret(c, &binding, &key_pem, ca_pem.as_deref())
                    .await
                {
                    Ok(()) => true,
                    Err(Some((r, message))) => {
                        // The sweep retires the pod of a failed claim.
                        return self.fail(c, r, &message, "internal").await;
                    }
                    Err(None) => {
                        pass.retry = true;
                        return;
                    }
                }
            }
        };
        let status = claims::bound_status(c, &binding, secret);
        match self
            .backend
            .patch_claim_status(ns, name, rv(c), &status)
            .await
        {
            Write::Done => {}
            // Deleted meanwhile: the sweep retires the pod.
            Write::Gone => return,
            w => {
                warn!(namespace = ns, claim = name, outcome = ?w, "writing bound claim status");
                pass.retry = true;
                return;
            }
        }
        *pass.held.entry(ask.caller.owner.clone()).or_default() += 1;
        if let Some(uid) = &c.metadata.uid {
            self.memory().bound.insert(uid.clone(), Instant::now());
        }
        let (pool_ns, pool_name) = (&c.spec.pool_ref.namespace, &c.spec.pool_ref.name);
        let waited = waited(c);
        // A claim the leader never had to mark `Pending` found a warm pod at once.
        let path = if c.status.is_none() { "warm" } else { "cold" };
        self.metrics.allocation(pool_ns, pool_name, "ok");
        self.metrics.duration(pool_ns, pool_name, path, waited);
        self.metrics.queue_wait(pool_ns, pool_name, "ok", waited);
        if let Some(p) = &ask.plan {
            self.metrics.cache(pool_ns, pool_name, p.tier.as_str());
        }
        info!(namespace = ns, claim = name, pool = %pool_name, pod = pod_name,
            alloc_id = %ask.alloc_id, owner = %ask.caller.owner, path, waited_s = waited, "claim bound");
    }

    /// The claim's Secret, fresh: a stale one the claim controls is replaced.
    /// `Err(Some(reason, message))` fails the claim, `Err(None)` retries it.
    async fn write_secret(
        &self,
        c: &BuildKitClaim,
        b: &Binding,
        key_pem: &str,
        ca: Option<&str>,
    ) -> Result<(), Option<(&'static str, String)>> {
        let (ns, name) = claim_names(c);
        let uid = c.metadata.uid.as_deref().unwrap_or_default();
        let sname = secret_name(name);
        match self.backend.get_secret(ns, &sname).await {
            Ok(Some(s)) if !claims::controlled_by(&s, uid) => {
                return Err(Some((
                    reason::SECRET_CONFLICT,
                    format!("secret {sname} exists and this claim does not own it"),
                )));
            }
            Ok(Some(s)) => {
                let suid = s.metadata.uid.unwrap_or_default();
                if !matches!(
                    self.backend.delete_secret(ns, &sname, &suid).await,
                    Write::Done | Write::Gone
                ) {
                    return Err(None);
                }
            }
            Ok(None) => {}
            Err(w) => {
                warn!(namespace = ns, secret = %sname, outcome = ?w, "reading claim secret");
                return Err(None);
            }
        }
        let secret = claims::secret(c, b, key_pem, ca).ok_or(None)?;
        match self.backend.create_secret(secret).await {
            Write::Done => Ok(()),
            Write::Refused(m) => Err(Some((
                reason::SECRET_REJECTED,
                format!("secret create refused: {m}"),
            ))),
            w => {
                warn!(namespace = ns, secret = %sname, outcome = ?w, "creating claim secret");
                Err(None)
            }
        }
    }

    /// The claim keeps waiting, unless its queue timeout has passed.
    #[allow(clippy::too_many_arguments, reason = "one status")]
    async fn wait(
        &self,
        c: &BuildKitClaim,
        ask: &Ask,
        why: &'static str,
        message: &str,
        now: OffsetDateTime,
        pass: &mut Pass,
    ) {
        if let Some(d) = ask.deadline {
            if d <= now {
                return self
                    .fail(
                        c,
                        reason::QUEUE_TIMEOUT,
                        "no worker became free within the queue timeout",
                        "pool_exhausted",
                    )
                    .await;
            }
            pass.until(d);
        }
        // No place in the queue is stored: the API works it out from the
        // store when asked. Only a new reason is written.
        if c.status.is_some() && claims::reason(c) == Some(why) {
            return;
        }
        let (ns, name) = claim_names(c);
        let status = claims::unbound_status(c, ClaimPhase::Pending, why, message);
        if !matches!(
            self.backend
                .patch_claim_status(ns, name, rv(c), &status)
                .await,
            Write::Done | Write::Gone
        ) {
            warn!(namespace = ns, claim = name, "writing pending claim status");
        }
    }

    /// No pool, or no access rule admits the claim: `PoolNotFound`, after a
    /// grace for a pool applied with the claim.
    async fn no_pool(&self, c: &BuildKitClaim, now: OffsetDateTime, pass: &mut Pass) {
        if claims::young(c, now) {
            if let Some(t) = c
                .metadata
                .creation_timestamp
                .as_ref()
                .and_then(|t| OffsetDateTime::from_unix_timestamp(t.0.as_second()).ok())
            {
                pass.until(t + claims::POOL_GRACE);
            }
            return;
        }
        self.fail(
            c,
            reason::POOL_NOT_FOUND,
            "pool not found",
            "pool_not_found",
        )
        .await;
    }

    /// The claim fails for good.
    async fn fail(&self, c: &BuildKitClaim, why: &'static str, message: &str, result: &str) {
        let (ns, name) = claim_names(c);
        let status = claims::unbound_status(c, ClaimPhase::Failed, why, message);
        if let Some(uid) = &c.metadata.uid {
            self.memory().bound.insert(uid.clone(), Instant::now());
        }
        match self
            .backend
            .patch_claim_status(ns, name, rv(c), &status)
            .await
        {
            Write::Done | Write::Gone => {}
            w => {
                warn!(namespace = ns, claim = name, outcome = ?w, "writing failed claim status");
                if let Some(uid) = &c.metadata.uid {
                    self.memory().bound.remove(uid);
                }
                return;
            }
        }
        // Unknown pools are counted with empty labels, so claim strings never
        // become label values (bounded cardinality).
        let (pns, pname) = if why == reason::POOL_NOT_FOUND {
            ("", "")
        } else {
            (
                c.spec.pool_ref.namespace.as_str(),
                c.spec.pool_ref.name.as_str(),
            )
        };
        self.metrics.allocation(pns, pname, result);
        if why == reason::QUEUE_TIMEOUT {
            self.metrics.queue_wait(pns, pname, "timeout", waited(c));
        }
        info!(namespace = ns, claim = name, reason = why, "claim failed");
    }

    /// Retire a pod: the retire patch, then delete on success.
    pub async fn retire(&self, ns: &str, pool: &str, pod: &Pod, expected: &str, why: &str) -> bool {
        retire_pod(&*self.backend, &self.metrics, ns, pool, pod, expected, why).await
    }
}

/// Check what claim `c` asks of `pool`; the message says what is wrong.
fn ask(
    c: &BuildKitClaim,
    pool: &BuildKitPool,
    own: &str,
    registry_host: Option<&str>,
    caller: Caller,
    alloc_id: AllocId,
    (claim_ref, claim_uid): (String, String),
) -> Result<Ask, String> {
    let a = &pool.spec.allocation;
    let max_ttl = seconds(&a.max_ttl)?;
    let ttl = match &c.spec.ttl {
        Some(t) => seconds(t)?,
        None => seconds(&a.default_ttl)?,
    };
    if !(MIN_TTL..=max_ttl).contains(&ttl) {
        return Err(format!(
            "ttl must be between {MIN_TTL}s and {}",
            a.max_ttl.as_str()
        ));
    }
    let qt = match &c.spec.queue_timeout {
        Some(t) => seconds(t)?,
        None => seconds(&a.queue_timeout)?,
    };
    if qt > seconds(&a.max_queue_timeout)? {
        return Err(format!(
            "queueTimeout must be at most {}",
            a.max_queue_timeout.as_str()
        ));
    }
    // The controller holds no Secret rights in its own namespace.
    let public_key = match &c.spec.public_key {
        Some(pem) => {
            ClientPublicKey::from_pem(pem)
                .map_err(|_| "publicKey must be a PEM SPKI ECDSA P-256 key".to_owned())?;
            Some(pem.clone())
        }
        None if c.metadata.namespace.as_deref() == Some(own) => {
            return Err("a claim in the controller's namespace must carry publicKey".into());
        }
        None => None,
    };
    let key = c
        .spec
        .cache_key
        .clone()
        .map(CacheKey::try_from)
        .transpose()
        .map_err(|e| e.to_string())?;
    // A claim the API wrote carries the scope and tier the API derived from
    // its caller; without a well-formed scope it gets no cache.
    let who = match (&caller.identity, &caller.cache_scope) {
        (Some(id), _) => Some((cache::scope(pool, id), cache::tier(pool, id))),
        (None, Some(s)) if rnd::is_scope_id(s) => {
            Some((s.clone(), caller.cache_tier.unwrap_or(CacheTier::Untrusted)))
        }
        (None, _) => None,
    };
    let plan = who.and_then(|(scope, tier)| {
        cache::plan(registry_host, pool, key.as_ref(), c.spec.cache, scope, tier)
    });
    Ok(Ask {
        deadline: claims::queue_deadline(c, qt),
        caller,
        alloc_id,
        claim_ref,
        claim_uid,
        ttl: time::Duration::seconds(i64::try_from(ttl).map_err(|e| e.to_string())?),
        public_key,
        plan,
    })
}

fn pool_id(pool: &BuildKitPool) -> Option<(&str, &str, &str)> {
    Some((
        pool.metadata.namespace.as_deref()?,
        pool.metadata.name.as_deref()?,
        pool.metadata.uid.as_deref()?,
    ))
}

fn claim_names(c: &BuildKitClaim) -> (&str, &str) {
    (
        c.metadata.namespace.as_deref().unwrap_or_default(),
        c.metadata.name.as_deref().unwrap_or_default(),
    )
}

/// Seconds from the claim's creation to now.
fn waited(c: &BuildKitClaim) -> f64 {
    let created = c
        .metadata
        .creation_timestamp
        .as_ref()
        .map_or(0, |t| t.0.as_millisecond());
    let now = OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
    #[allow(clippy::cast_precision_loss, reason = "a wait in milliseconds fits")]
    let ms = (now - i128::from(created)).max(0) as f64;
    ms / 1000.0
}

/// The claim's `resourceVersion`, the precondition of its status writes.
fn rv(c: &BuildKitClaim) -> &str {
    c.metadata.resource_version.as_deref().unwrap_or_default()
}

/// Retire `pod` of `pool`: the retire patch (uid and `alloc == expected`
/// tested), then delete it on success. False when the patch lost or failed.
pub(super) async fn retire_pod(
    backend: &dyn Backend,
    metrics: &AllocMetrics,
    ns: &str,
    pool: &str,
    pod: &Pod,
    expected: &str,
    why: &str,
) -> bool {
    let (Some(name), Some(uid)) = (&pod.metadata.name, &pod.metadata.uid) else {
        return false;
    };
    let patch = retire_patch(uid, expected);
    match backend.patch_pod(ns, name, patch).await {
        PatchOutcome::Applied(_) => {
            let grace = pod
                .spec
                .as_ref()
                .and_then(|s| s.termination_grace_period_seconds)
                .and_then(|g| u32::try_from(g).ok());
            if let Err(error) = backend.delete_pod(ns, name, uid, grace).await {
                warn!(namespace = %ns, pod = %name, error, "deleting retired pod");
            }
            metrics.release(ns, pool, why);
            true
        }
        PatchOutcome::Lost => false,
        PatchOutcome::Unknown | PatchOutcome::Failed(_) => {
            warn!(namespace = %ns, pod = %name, "retire patch failed");
            false
        }
    }
}
