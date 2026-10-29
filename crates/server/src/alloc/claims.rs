//! Pure decisions about claims: who owns one, where it stands in its pool's
//! queue, the statuses the leader writes, and which pods have lost their claim.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use buildkit_controller_api::claim::{
    BuildKitClaim, BuildKitClaimStatus, CacheTier, ClaimPhase, secret_name,
};
use buildkit_controller_api::dto::{CacheResponse, ErrorCode};
use buildkit_controller_api::v1alpha1::BuildKitPool;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::{Pod, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, ObjectMeta, OwnerReference, Time};
use time::OffsetDateTime;

use super::pods::{ann, derived_alloc_id, parse_ts};
use crate::auth::Identity;
use crate::pki::AllocId;
use crate::render::{
    ANN_ALLOC, ANN_CLAIM, ANN_CLAIM_UID, ANN_EXPIRES_AT, ANN_OWNER, DRAINING, FREE,
};

/// `(namespace, name)` of a pool.
pub type PoolKey = (String, String);

/// The only condition type a claim carries.
pub const COND_BOUND: &str = "Bound";
/// How long after creation a claim waits for its pool to appear in the pool
/// store, or for a rule admitting it, before `PoolNotFound` is terminal.
pub const POOL_GRACE: Duration = Duration::from_secs(30);
/// How long after binding the pod store may lag before a missing pod counts as lost.
pub const LOST_GRACE: Duration = Duration::from_secs(30);
/// How long a finished claim of a CI caller stays before the leader deletes it.
pub const FINISHED_TTL: Duration = Duration::from_mins(10);
/// How long a CI claim whose bound answer was never delivered may go without
/// its caller asking for it before the leader deletes it: the client is gone.
/// It covers the longest gap the shipped clients leave between requests that
/// reach a replica (a 25 s poll, a 40 s client timeout on a poll lost on the
/// way, a 30 s `Retry-After`), plus [`SEEN_EVERY`], plus clock skew between
/// the replica that writes `seen-at` and the leader that reads it.
pub const ABANDON_AFTER: Duration = Duration::from_mins(3);
/// The API refreshes a claim's `seen-at` at most this often.
pub const SEEN_EVERY: Duration = Duration::from_secs(15);
/// Claim annotation: when the API last answered the claim's caller.
pub const ANN_SEEN_AT: &str = "buildkit.smrt-devops.net/seen-at";
/// Claim annotation `true` once the API has answered with the bound allocation.
pub const ANN_DELIVERED: &str = "buildkit.smrt-devops.net/delivered";

/// Condition reasons a claim can carry.
pub mod reason {
    /// Bound to a worker.
    pub const ALLOCATED: &str = "Allocated";
    /// Waiting in the pool's queue.
    pub const QUEUED: &str = "Queued";
    /// Still `Pending` at its queue timeout.
    pub const QUEUE_TIMEOUT: &str = "QueueTimeout";
    /// The pool is missing or its access rules do not admit the caller.
    pub const POOL_NOT_FOUND: &str = "PoolNotFound";
    /// The pool's `Ready` condition is not True.
    pub const POOL_NOT_READY: &str = "PoolNotReady";
    /// No valid CA is loaded.
    pub const CA_UNAVAILABLE: &str = "CAUnavailable";
    /// A TTL, key or cache choice the pool does not allow.
    pub const INVALID_REQUEST: &str = "InvalidRequest";
    /// The Secret name is taken by an object the claim does not control.
    pub const SECRET_CONFLICT: &str = "SecretConflict";
    /// The API server refused the Secret for good.
    pub const SECRET_REJECTED: &str = "SecretRejected";
    /// The claim's namespace is not in `claims.namespaces`.
    pub const NAMESPACE_NOT_SERVED: &str = "NamespaceNotServed";
    /// The worker pod went away before expiry.
    pub const WORKER_LOST: &str = "WorkerLost";
    /// The allocation TTL ran out.
    pub const EXPIRED: &str = "Expired";
}

/// Who a claim allocates for.
#[derive(Debug, Clone, PartialEq)]
pub struct Caller {
    /// The owner string written on the pod and checked on release.
    pub owner: String,
    /// The claim's own identity, matched against the pool's access rules.
    /// `None` for a claim the API wrote: the API checked its caller.
    pub identity: Option<Identity>,
    /// The cache scope id the API wrote, for a claim the API wrote.
    pub cache_scope: Option<String>,
    /// The cache tier the API wrote, for a claim the API wrote.
    pub cache_tier: Option<CacheTier>,
}

/// The claim's identity, from API-server-set metadata only.
#[must_use]
pub fn identity(c: &BuildKitClaim) -> Option<Identity> {
    Some(Identity::Claim {
        namespace: c.metadata.namespace.clone()?,
        name: c.metadata.name.clone()?,
        uid: c.metadata.uid.clone()?,
    })
}

/// The caller of `c`. A written `spec.owner` counts only in the controller's
/// namespace `own`, where tenants cannot create claims; anywhere else the
/// owner is the claim itself, so a tenant cannot pose as a CI caller.
#[must_use]
pub fn caller(c: &BuildKitClaim, own: &str) -> Option<Caller> {
    if c.metadata.namespace.as_deref() == Some(own)
        && let Some(owner) = &c.spec.owner
    {
        return Some(Caller {
            owner: owner.clone(),
            identity: None,
            cache_scope: c.spec.cache_scope.clone(),
            cache_tier: c.spec.cache_tier,
        });
    }
    let id = identity(c)?;
    Some(Caller {
        owner: id.owner(),
        identity: Some(id),
        cache_scope: None,
        cache_tier: None,
    })
}

/// The allocation id of `c`: its name for a claim the API created in the
/// controller's namespace `own` (the API names it after the id), else derived
/// from its uid. Either way it is fixed for the claim's life.
#[must_use]
pub fn alloc_id(c: &BuildKitClaim, own: &str) -> Option<AllocId> {
    if c.metadata.namespace.as_deref() == Some(own)
        && let Some(id) = c.metadata.name.as_deref().and_then(|n| n.parse().ok())
    {
        return Some(id);
    }
    derived_alloc_id(c.metadata.uid.as_deref()?)
}

/// `<namespace>/<name>`, the value of `P claim`.
#[must_use]
pub fn claim_ref(c: &BuildKitClaim) -> Option<String> {
    Some(format!(
        "{}/{}",
        c.metadata.namespace.as_deref()?,
        c.metadata.name.as_deref()?
    ))
}

/// The phase in status, `Pending` when unset.
#[must_use]
pub fn phase(c: &BuildKitClaim) -> ClaimPhase {
    c.status.as_ref().and_then(|s| s.phase).unwrap_or_default()
}

/// The condition reason currently in status.
#[must_use]
pub fn reason(c: &BuildKitClaim) -> Option<&str> {
    bound_condition(c).map(|x| x.reason.as_str())
}

fn bound_condition(c: &BuildKitClaim) -> Option<&Condition> {
    c.status
        .as_ref()?
        .conditions
        .iter()
        .find(|x| x.type_ == COND_BOUND)
}

/// The pool a claim asks for.
#[must_use]
pub fn pool_key(c: &BuildKitClaim) -> PoolKey {
    (
        c.spec.pool_ref.namespace.clone(),
        c.spec.pool_ref.name.clone(),
    )
}

/// True when the claim's namespace is served.
#[must_use]
pub fn served<S: std::hash::BuildHasher>(
    c: &BuildKitClaim,
    namespaces: &HashSet<String, S>,
) -> bool {
    c.metadata
        .namespace
        .as_ref()
        .is_some_and(|ns| namespaces.contains(ns))
}

/// A served claim that waits for a worker.
#[must_use]
pub fn waits<S: std::hash::BuildHasher>(c: &BuildKitClaim, served_ns: &HashSet<String, S>) -> bool {
    phase(c) == ClaimPhase::Pending
        && c.metadata.deletion_timestamp.is_none()
        && c.metadata.uid.is_some()
        && served(c, served_ns)
}

/// Queue order: oldest `creationTimestamp` first (the API server's clock, so
/// no replica can jump the queue), then `uid`, then name and namespace. The
/// API server picks the uid at random; the name of an API claim derives from
/// the client's key, so a client could grind keys to win every tie.
#[must_use]
pub fn queue_order(a: &BuildKitClaim, b: &BuildKitClaim) -> Ordering {
    let key = |c: &BuildKitClaim| {
        (
            c.metadata.creation_timestamp.as_ref().map(|t| t.0),
            c.metadata.uid.clone(),
            c.metadata.name.clone(),
            c.metadata.namespace.clone(),
        )
    };
    key(a).cmp(&key(b))
}

/// True once the leader has admitted waiting claim `c`: it writes a status
/// only after the pool's access rules admit the claim, and tenants cannot
/// write status. A claim naming a pool that refuses it never counts as a
/// waiter, so it can neither grow that pool nor push its queue back.
#[must_use]
pub fn admitted(c: &BuildKitClaim) -> bool {
    c.status.is_some()
}

fn by_pool<'a>(
    claims: impl Iterator<Item = &'a Arc<BuildKitClaim>>,
) -> HashMap<PoolKey, Vec<Arc<BuildKitClaim>>> {
    let mut out: HashMap<PoolKey, Vec<Arc<BuildKitClaim>>> = HashMap::new();
    for c in claims {
        out.entry(pool_key(c)).or_default().push(Arc::clone(c));
    }
    for q in out.values_mut() {
        q.sort_by(|a, b| queue_order(a, b));
    }
    out
}

/// Every waiting claim of every pool, admitted or not, each queue in
/// [`queue_order`]: what the leader serves, so it can bind or refuse each.
#[must_use]
pub fn pending<S: std::hash::BuildHasher>(
    claims: &[Arc<BuildKitClaim>],
    served_ns: &HashSet<String, S>,
) -> HashMap<PoolKey, Vec<Arc<BuildKitClaim>>> {
    by_pool(claims.iter().filter(|c| waits(c, served_ns)))
}

/// The admitted waiting claims of every pool ([`admitted`]), each queue in
/// [`queue_order`]: the demand a pool scales for.
#[must_use]
pub fn queues<S: std::hash::BuildHasher>(
    claims: &[Arc<BuildKitClaim>],
    served_ns: &HashSet<String, S>,
) -> HashMap<PoolKey, Vec<Arc<BuildKitClaim>>> {
    by_pool(claims.iter().filter(|c| waits(c, served_ns) && admitted(c)))
}

/// The 1-based place of `c` among the waiting claims of its pool. Only
/// [`admitted`] claims, and claims of the controller's namespace `own` (the
/// API admitted their callers), count as ahead of it.
#[must_use]
pub fn position<S: std::hash::BuildHasher>(
    claims: &[Arc<BuildKitClaim>],
    served_ns: &HashSet<String, S>,
    own: &str,
    c: &BuildKitClaim,
) -> Option<u32> {
    if !waits(c, served_ns) {
        return None;
    }
    let key = pool_key(c);
    let ahead = claims
        .iter()
        .filter(|o| waits(o, served_ns) && pool_key(o) == key)
        .filter(|o| admitted(o) || o.metadata.namespace.as_deref() == Some(own))
        .filter(|o| queue_order(o, c) == Ordering::Less)
        .count();
    u32::try_from(ahead + 1).ok()
}

/// Workers one owner (an OIDC run, a service account or pod, a claim) may
/// hold in `pool` while another owner waits for one: half of `scaling.max`,
/// at least 1. With nobody else waiting an owner may take the whole pool, so
/// a single tenant or one CI matrix is not capped at half. The leader
/// enforces it when binding.
pub(super) fn owner_hold_limit(pool: &BuildKitPool) -> usize {
    (usize::try_from(pool.spec.scaling.max).unwrap_or(0) / 2).max(1)
}

/// When a waiting claim's queue timeout ends, given its timeout in seconds.
#[must_use]
pub fn queue_deadline(c: &BuildKitClaim, timeout_secs: u64) -> Option<OffsetDateTime> {
    let created = c.metadata.creation_timestamp.as_ref()?.0.as_second();
    let t = created.checked_add(i64::try_from(timeout_secs).ok()?)?;
    OffsetDateTime::from_unix_timestamp(t).ok()
}

/// True while the claim is younger than [`POOL_GRACE`] at `now`: a pool
/// applied with the claim, or an access rule added just after it, may not be
/// in the store yet.
#[must_use]
pub fn young(c: &BuildKitClaim, now: OffsetDateTime) -> bool {
    let grace = i64::try_from(POOL_GRACE.as_secs()).unwrap_or(i64::MAX);
    c.metadata
        .creation_timestamp
        .as_ref()
        .is_some_and(|t| now.unix_timestamp() < t.0.as_second().saturating_add(grace))
}

/// True when a `Bound` claim's allocation has run out at `now`.
#[must_use]
pub fn expired(c: &BuildKitClaim, now: OffsetDateTime) -> bool {
    phase(c) == ClaimPhase::Bound
        && c.status
            .as_ref()
            .and_then(|s| s.expires_at.as_deref())
            .and_then(parse_ts)
            .is_some_and(|t| t <= now)
}

/// True when a `Bound` claim's worker is gone before expiry.
///
/// The worker counts as present while some pod carries the claim's
/// allocation id, is not being deleted, and has not failed or exited. The
/// claim is not judged in its first [`LOST_GRACE`] after binding, so a pod
/// store that has not yet seen the bind does not fail a fresh claim.
#[must_use]
pub fn lost<'a>(
    c: &BuildKitClaim,
    pods: impl IntoIterator<Item = &'a Pod>,
    now: OffsetDateTime,
) -> bool {
    if phase(c) != ClaimPhase::Bound {
        return false;
    }
    let Some(id) = c.status.as_ref().and_then(|s| s.allocation_id.as_deref()) else {
        return false;
    };
    let grace = i64::try_from(LOST_GRACE.as_secs()).unwrap_or(i64::MAX);
    let bound_at = bound_condition(c).map(|x| x.last_transition_time.0.as_second());
    if bound_at.is_none_or(|t| now.unix_timestamp() < t.saturating_add(grace)) {
        return false;
    }
    !pods.into_iter().any(|p| serves(p, id))
}

/// True when `p` carries allocation `id`, is not being deleted, and has not
/// failed or exited.
#[must_use]
pub fn serves(p: &Pod, id: &str) -> bool {
    let ended = p
        .status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .is_some_and(|ph| ph == "Failed" || ph == "Succeeded");
    ann(p, ANN_ALLOC) == Some(id) && p.metadata.deletion_timestamp.is_none() && !ended
}

/// True when a finished claim of the controller's namespace `own` is due for
/// deletion at `now`, [`FINISHED_TTL`] after it ended.
#[must_use]
pub fn collectable(c: &BuildKitClaim, own: &str, now: OffsetDateTime) -> bool {
    let ttl = i64::try_from(FINISHED_TTL.as_secs()).unwrap_or(i64::MAX);
    c.metadata.namespace.as_deref() == Some(own)
        && c.metadata.deletion_timestamp.is_none()
        && phase(c).is_terminal()
        && bound_condition(c).is_none_or(|x| {
            now.unix_timestamp() >= x.last_transition_time.0.as_second().saturating_add(ttl)
        })
}

fn claim_ann<'a>(c: &'a BuildKitClaim, key: &str) -> Option<&'a str> {
    c.metadata
        .annotations
        .as_ref()?
        .get(key)
        .map(String::as_str)
}

/// True once the API has answered with the bound allocation of `c`.
#[must_use]
pub fn delivered(c: &BuildKitClaim) -> bool {
    claim_ann(c, ANN_DELIVERED) == Some("true")
}

/// Seconds since the API last answered the caller of `c`; `None` for a claim
/// without `seen-at` (an in-cluster claim, or one made before it existed).
fn unseen_for(c: &BuildKitClaim, now: OffsetDateTime) -> Option<u64> {
    let at = parse_ts(claim_ann(c, ANN_SEEN_AT)?)?;
    u64::try_from(now.unix_timestamp() - at.unix_timestamp()).ok()
}

/// True when the API should refresh `seen-at` of `c` at `now`.
#[must_use]
pub fn seen_due(c: &BuildKitClaim, now: OffsetDateTime) -> bool {
    !delivered(c) && unseen_for(c, now).is_some_and(|s| s >= SEEN_EVERY.as_secs())
}

/// True when CI claim `c` of the controller's namespace `own` is pending, or
/// bound with its answer never delivered, and its caller has not asked for it
/// for [`ABANDON_AFTER`]: the client is gone, and the claim is deleted so it
/// holds no place and no worker. A delivered claim lives to its TTL.
#[must_use]
pub fn abandoned(c: &BuildKitClaim, own: &str, now: OffsetDateTime) -> bool {
    c.metadata.namespace.as_deref() == Some(own)
        && c.metadata.deletion_timestamp.is_none()
        && !phase(c).is_terminal()
        && !delivered(c)
        && unseen_for(c, now).is_some_and(|s| s >= ABANDON_AFTER.as_secs())
}

/// Pods whose allocation no longer has a claim to serve: the claim named by
/// `(namespace, name)` is gone, is another claim of that name (another uid),
/// has ended, or is bound to another pod (a bind a deposed leader made). Pods
/// bound before every allocation was
/// a claim carry no `P claim`: the ones an in-cluster claim owned count when
/// that claim (by uid) is gone, has ended, or names another allocation id.
/// Only served namespaces count; claims nobody watches, and pre-claim
/// allocations of API callers (released by `DELETE`), are left to their expiry.
#[must_use]
pub fn strays<'a, S: std::hash::BuildHasher>(
    pods: impl IntoIterator<Item = &'a Pod>,
    claims: &[Arc<BuildKitClaim>],
    served_ns: &HashSet<String, S>,
    own: &str,
) -> Vec<&'a Pod> {
    let by_ref: HashMap<String, &BuildKitClaim> = claims
        .iter()
        .filter_map(|c| Some((claim_ref(c)?, c.as_ref())))
        .collect();
    let by_uid: HashMap<&str, &BuildKitClaim> = claims
        .iter()
        .filter_map(|c| Some((c.metadata.uid.as_deref()?, c.as_ref())))
        .collect();
    pods.into_iter()
        .filter(|p| p.metadata.deletion_timestamp.is_none())
        .filter(|p| {
            let Some(id) = ann(p, ANN_ALLOC).filter(|a| *a != FREE && *a != DRAINING) else {
                return false;
            };
            let Some(cref) = ann(p, ANN_CLAIM) else {
                return unclaimed_stray(p, id, &by_uid, served_ns);
            };
            let ns = cref.split_once('/').map_or("", |(ns, _)| ns);
            if !served_ns.contains(ns) {
                return false;
            }
            let Some(c) = by_ref.get(cref) else {
                return true;
            };
            // A claim of the same name made after the pod's claim went: the pod
            // was released, and never serves the new one.
            if ann(p, ANN_CLAIM_UID) != c.metadata.uid.as_deref() {
                return true;
            }
            if alloc_id(c, own).is_none_or(|a| a.as_str() != id) {
                return true;
            }
            match phase(c) {
                ClaimPhase::Pending => false,
                ClaimPhase::Bound => {
                    c.status.as_ref().and_then(|s| s.pod.as_deref()) != p.metadata.name.as_deref()
                }
                ClaimPhase::Expired | ClaimPhase::Lost | ClaimPhase::Failed => true,
            }
        })
        .collect()
}

/// A pod bound for an in-cluster claim before every allocation was a claim
/// (owner `claim:<ns>/<name>#<uid>`, no `P claim`) whose claim is gone, has
/// ended, or records another allocation id. v0.1.0 upgrade shim: remove in
/// the release after 0.2.0.
fn unclaimed_stray<S: std::hash::BuildHasher>(
    p: &Pod,
    id: &str,
    by_uid: &HashMap<&str, &BuildKitClaim>,
    served_ns: &HashSet<String, S>,
) -> bool {
    let Some((ns, uid)) = ann(p, ANN_OWNER).and_then(claim_owner) else {
        return false;
    };
    if !served_ns.contains(ns) {
        return false;
    }
    by_uid.get(uid).is_none_or(|c| {
        phase(c).is_terminal()
            || c.status.as_ref().and_then(|s| s.allocation_id.as_deref()) != Some(id)
    })
}

/// `(namespace, uid)` of a claim owner string `claim:<ns>/<name>#<uid>`.
pub(crate) fn claim_owner(s: &str) -> Option<(&str, &str)> {
    let (ns, rest) = s.strip_prefix("claim:")?.split_once('/')?;
    let (name, uid) = rest.split_once('#')?;
    (!ns.is_empty() && !name.is_empty() && !uid.is_empty()).then_some((ns, uid))
}

fn condition(status: bool, reason: &str, message: &str, generation: Option<i64>) -> Condition {
    Condition {
        type_: COND_BOUND.into(),
        status: if status { "True" } else { "False" }.into(),
        reason: reason.into(),
        message: message.into(),
        observed_generation: generation,
        last_transition_time: Time(k8s_openapi::jiff::Timestamp::now()),
    }
}

/// Status for a claim that is not bound (`Pending` or `Failed`).
#[must_use]
pub fn unbound_status(
    c: &BuildKitClaim,
    phase: ClaimPhase,
    reason: &str,
    message: &str,
) -> BuildKitClaimStatus {
    BuildKitClaimStatus {
        phase: Some(phase),
        observed_generation: c.metadata.generation,
        conditions: vec![condition(false, reason, message, c.metadata.generation)],
        ..BuildKitClaimStatus::default()
    }
}

/// What status records about a bound allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// Allocation id.
    pub alloc_id: String,
    /// The worker pod.
    pub pod: String,
    /// Edge endpoint.
    pub endpoint: String,
    /// TLS server name.
    pub server_name: String,
    /// RFC 3339 expiry.
    pub expires_at: String,
    /// The client certificate (PEM).
    pub certificate: String,
    /// Cache flags and outcome.
    pub cache: Option<CacheResponse>,
}

/// Status for a bound claim; `secret` when the claim got a Secret.
#[must_use]
pub fn bound_status(c: &BuildKitClaim, b: &Binding, secret: bool) -> BuildKitClaimStatus {
    BuildKitClaimStatus {
        phase: Some(ClaimPhase::Bound),
        allocation_id: Some(b.alloc_id.clone()),
        endpoint: Some(b.endpoint.clone()),
        server_name: Some(b.server_name.clone()),
        secret_name: c
            .metadata
            .name
            .as_deref()
            .filter(|_| secret)
            .map(secret_name),
        pod: Some(b.pod.clone()),
        certificate: Some(b.certificate.clone()),
        expires_at: Some(b.expires_at.clone()),
        cache: b.cache.clone(),
        observed_generation: c.metadata.generation,
        conditions: vec![condition(
            true,
            reason::ALLOCATED,
            "",
            c.metadata.generation,
        )],
    }
}

/// Status for a claim that ended: `Expired` or `Lost`.
#[must_use]
pub fn ended_status(c: &BuildKitClaim, phase: ClaimPhase) -> BuildKitClaimStatus {
    let (reason, message) = if phase == ClaimPhase::Lost {
        (
            reason::WORKER_LOST,
            "the worker pod was deleted, evicted or failed before the allocation expired",
        )
    } else {
        (reason::EXPIRED, "the allocation TTL ran out")
    };
    let mut s = c.status.clone().unwrap_or_default();
    s.phase = Some(phase);
    s.secret_name = None;
    s.conditions = vec![condition(false, reason, message, c.metadata.generation)];
    s
}

/// The API error a finished claim answers with: `QueueTimeout` is
/// `pool_exhausted`, a lost worker `pod_lost`, an expired allocation `not_found`.
#[must_use]
pub fn api_error(c: &BuildKitClaim) -> Option<(ErrorCode, String)> {
    let message = bound_condition(c).map_or_else(String::new, |x| x.message.clone());
    let code = match (phase(c), reason(c)) {
        (ClaimPhase::Pending | ClaimPhase::Bound, _) => return None,
        (ClaimPhase::Expired, _) => ErrorCode::NotFound,
        (ClaimPhase::Lost, _) => ErrorCode::PodLost,
        (ClaimPhase::Failed, Some(reason::QUEUE_TIMEOUT)) => ErrorCode::PoolExhausted,
        (ClaimPhase::Failed, Some(reason::POOL_NOT_FOUND)) => ErrorCode::PoolNotFound,
        (ClaimPhase::Failed, Some(reason::INVALID_REQUEST)) => ErrorCode::InvalidRequest,
        (ClaimPhase::Failed, _) => ErrorCode::Internal,
    };
    Some((code, message))
}

/// The API message when the Secret create was refused for good: a 4xx other
/// than 401, 409 and 429, such as a quota (403) or an admission policy (422).
/// Retrying would bind a worker and retire it again on every attempt.
#[must_use]
pub fn secret_rejected(e: &kube::Error) -> Option<String> {
    match e {
        kube::Error::Api(s)
            if (400..500).contains(&s.code) && !matches!(s.code, 401 | 409 | 429) =>
        {
            Some(format!("secret create refused ({}): {}", s.code, s.message))
        }
        _ => None,
    }
}

/// True when `s` is controlled by the claim with `uid`.
#[must_use]
pub fn controlled_by(s: &Secret, uid: &str) -> bool {
    s.metadata
        .owner_references
        .iter()
        .flatten()
        .any(|o| o.uid == uid && o.controller == Some(true))
}

/// The Secret of an in-cluster claim, controlled by the claim.
#[must_use]
pub fn secret(c: &BuildKitClaim, b: &Binding, key_pem: &str, ca: Option<&str>) -> Option<Secret> {
    let name = c.metadata.name.clone()?;
    let bytes = |s: &str| ByteString(s.as_bytes().to_vec());
    let mut data = BTreeMap::from([
        ("tls.crt".to_owned(), bytes(&b.certificate)),
        ("tls.key".to_owned(), bytes(key_pem)),
        ("endpoint".to_owned(), bytes(&b.endpoint)),
        ("server-name".to_owned(), bytes(&b.server_name)),
    ]);
    if let Some(ca) = ca {
        data.insert("ca.crt".to_owned(), bytes(ca));
    }
    Some(Secret {
        metadata: ObjectMeta {
            name: Some(secret_name(&name)),
            namespace: c.metadata.namespace.clone(),
            annotations: Some(BTreeMap::from([
                (ANN_ALLOC.to_owned(), b.alloc_id.clone()),
                (ANN_EXPIRES_AT.to_owned(), b.expires_at.clone()),
            ])),
            owner_references: Some(vec![OwnerReference {
                api_version: "buildkit.smrt-devops.net/v1alpha1".into(),
                kind: "BuildKitClaim".into(),
                name,
                uid: c.metadata.uid.clone()?,
                controller: Some(true),
                block_owner_deletion: Some(false),
            }]),
            ..ObjectMeta::default()
        },
        type_: Some("Opaque".into()),
        data: Some(data),
        ..Secret::default()
    })
}

#[cfg(test)]
#[path = "claims_tests.rs"]
mod tests;
