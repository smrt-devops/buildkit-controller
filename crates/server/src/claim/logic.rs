//! Pure decisions of the claim controller.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use buildkit_controller_api::ApiError;
use buildkit_controller_api::claim::{BuildKitClaim, BuildKitClaimStatus, ClaimPhase, secret_name};
use buildkit_controller_api::dto::{AllocationResponse, CacheKey, CreateAllocationRequest};
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::{Pod, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, ObjectMeta, OwnerReference, Time};
use time::OffsetDateTime;

use crate::alloc::AllocError;
use crate::alloc::pods::{DRAINING, FREE, ann, of_pool, parse_ts};
use crate::auth::Identity;
use crate::render::{ANN_ALLOC, ANN_OWNER, CONTROLLER, LABEL_MANAGED_BY, LABEL_POOL};

/// The allocation request a claim stands for: its pool, `ttl`, `cacheKey` and `cache`.
///
/// # Errors
///
/// [`ApiError::Invalid`] when `spec.cacheKey` is not a valid cache key.
pub fn request(c: &BuildKitClaim, ttl: Option<u64>) -> Result<CreateAllocationRequest, ApiError> {
    Ok(CreateAllocationRequest {
        namespace: c.spec.pool_ref.namespace.clone(),
        pool: c.spec.pool_ref.name.clone(),
        ttl_seconds: ttl,
        queue_timeout_seconds: None,
        public_key: None,
        cache_key: c
            .spec
            .cache_key
            .clone()
            .map(CacheKey::try_from)
            .transpose()?,
        cache: c.spec.cache,
    })
}

/// Owner prefix of identities made by [`identity`].
pub const OWNER_PREFIX: &str = "claim:";
/// Secret annotation holding the allocation id.
pub const SECRET_ANN_ALLOC: &str = ANN_ALLOC;
/// Secret annotation holding the allocation expiry.
pub const SECRET_ANN_EXPIRES: &str = "buildkit.smrt-devops.net/expires-at";
/// The only condition type a claim carries.
pub const COND_BOUND: &str = "Bound";
/// First retry delay after a retryable failure.
pub const BACKOFF_MIN: Duration = Duration::from_secs(5);
/// Longest retry delay.
pub const BACKOFF_MAX: Duration = Duration::from_mins(1);

/// The claim's identity, from API-server-set metadata only.
#[must_use]
pub fn identity(c: &BuildKitClaim) -> Option<Identity> {
    Some(Identity::Claim {
        namespace: c.metadata.namespace.clone()?,
        name: c.metadata.name.clone()?,
        uid: c.metadata.uid.clone()?,
    })
}

/// Inverse of `Identity::owner` for claim owners (`claim:<ns>/<name>#<uid>`).
#[must_use]
pub fn parse_owner(s: &str) -> Option<Identity> {
    let (ns, rest) = s.strip_prefix(OWNER_PREFIX)?.split_once('/')?;
    let (name, uid) = rest.split_once('#')?;
    (!ns.is_empty() && !name.is_empty() && !uid.is_empty()).then(|| Identity::Claim {
        namespace: ns.to_owned(),
        name: name.to_owned(),
        uid: uid.to_owned(),
    })
}

/// What to do with an allocation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// Terminal: the claim goes to `Failed` with this reason.
    Fail(&'static str),
    /// Stay `Pending` with this reason and retry after backoff.
    Retry(&'static str),
}

/// Map an engine error onto the claim lifecycle.
#[must_use]
pub fn classify(e: &AllocError) -> Disposition {
    match e {
        AllocError::PoolNotFound => Disposition::Fail("PoolNotFound"),
        AllocError::InvalidRequest(_) => Disposition::Fail("InvalidRequest"),
        AllocError::PoolExhausted(_) | AllocError::ReleaseBusy => Disposition::Retry("Queued"),
        AllocError::NotReady => Disposition::Retry("PoolNotReady"),
        AllocError::CaUnavailable => Disposition::Retry("CAUnavailable"),
        AllocError::StartTimeout => Disposition::Retry("StartTimeout"),
        AllocError::PodLost => Disposition::Retry("PodLost"),
        AllocError::ShuttingDown => Disposition::Retry("ShuttingDown"),
        AllocError::UpstreamError
        | AllocError::Internal
        | AllocError::NotFound
        | AllocError::Forbidden => Disposition::Retry("Error"),
    }
}

/// How long after creation a claim waits for its pool to appear in the pool
/// store, or for a rule admitting it, before `PoolNotFound` is terminal.
pub const POOL_GRACE: Duration = Duration::from_mins(1);

/// [`classify`] for this claim at `now`: `PoolNotFound` is retried in the
/// claim's first [`POOL_GRACE`], since a pool applied with the claim, or an
/// access rule added just after it, may not be in the leader's store yet.
#[must_use]
pub fn dispose(c: &BuildKitClaim, e: &AllocError, now: OffsetDateTime) -> Disposition {
    let created = c
        .metadata
        .creation_timestamp
        .as_ref()
        .map(|t| t.0.as_second());
    let grace = i64::try_from(POOL_GRACE.as_secs()).unwrap_or(i64::MAX);
    let young = created.is_some_and(|t| now.unix_timestamp() < t.saturating_add(grace));
    match classify(e) {
        Disposition::Fail(reason @ "PoolNotFound") if young => Disposition::Retry(reason),
        d => d,
    }
}

/// The next retry delay after `prev` (doubling, [`BACKOFF_MIN`] to [`BACKOFF_MAX`]).
#[must_use]
pub fn next_backoff(prev: Option<Duration>) -> Duration {
    prev.map_or(BACKOFF_MIN, |d| (d * 2).min(BACKOFF_MAX))
}

/// The phase in status, `Pending` when unset.
#[must_use]
pub fn phase(c: &BuildKitClaim) -> ClaimPhase {
    c.status.as_ref().and_then(|s| s.phase).unwrap_or_default()
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

/// The condition reason currently in status.
#[must_use]
pub fn reason(c: &BuildKitClaim) -> Option<&str> {
    c.status
        .as_ref()?
        .conditions
        .iter()
        .find(|x| x.type_ == COND_BOUND)
        .map(|x| x.reason.as_str())
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

/// Status for a bound claim.
#[must_use]
pub fn bound_status(c: &BuildKitClaim, b: &Binding) -> BuildKitClaimStatus {
    BuildKitClaimStatus {
        phase: Some(ClaimPhase::Bound),
        allocation_id: Some(b.alloc_id.clone()),
        endpoint: Some(b.endpoint.clone()),
        server_name: Some(b.server_name.clone()),
        secret_name: c.metadata.name.as_deref().map(secret_name),
        expires_at: Some(b.expires_at.clone()),
        observed_generation: c.metadata.generation,
        conditions: vec![condition(true, "Allocated", "", c.metadata.generation)],
    }
}

/// Status for an expired claim.
#[must_use]
pub fn expired_status(c: &BuildKitClaim) -> BuildKitClaimStatus {
    let mut s = c.status.clone().unwrap_or_default();
    s.phase = Some(ClaimPhase::Expired);
    s.secret_name = None;
    s.conditions = vec![condition(
        false,
        "Expired",
        "the allocation TTL ran out",
        c.metadata.generation,
    )];
    s
}

/// True when the claim's namespace is in `claims.namespaces`.
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

/// How long after binding the pod store may lag before a missing pod counts as lost.
pub const LOST_GRACE: Duration = Duration::from_secs(30);

/// True when a `Bound` claim's worker is gone before expiry.
///
/// The worker counts as present while some pod carries the claim's
/// allocation id, is not being deleted, and has not failed or exited. The
/// claim is not judged in its first [`LOST_GRACE`] after binding, so a pod
/// store that has not yet seen the allocation patch does not fail a fresh
/// claim.
#[must_use]
pub fn lost<'a>(
    c: &BuildKitClaim,
    pods: impl IntoIterator<Item = &'a Pod>,
    now: OffsetDateTime,
) -> bool {
    if phase(c) != ClaimPhase::Bound {
        return false;
    }
    let Some(s) = c.status.as_ref() else {
        return false;
    };
    let Some(id) = s.allocation_id.as_deref() else {
        return false;
    };
    let bound_at = s
        .conditions
        .iter()
        .find(|x| x.type_ == COND_BOUND)
        .map(|x| x.last_transition_time.0.as_second());
    let grace = i64::try_from(LOST_GRACE.as_secs()).unwrap_or(i64::MAX);
    if bound_at.is_none_or(|t| now.unix_timestamp() < t.saturating_add(grace)) {
        return false;
    }
    !pods.into_iter().any(|p| serves(p, id))
}

/// True when `p` carries allocation `id`, is not being deleted, and has not
/// failed or exited.
fn serves(p: &Pod, id: &str) -> bool {
    let ended = p
        .status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .is_some_and(|ph| ph == "Failed" || ph == "Succeeded");
    ann(p, ANN_ALLOC) == Some(id) && p.metadata.deletion_timestamp.is_none() && !ended
}

/// True when an adopted Secret's allocation still has a worker owned by
/// `owner`. The Secret can outlive its allocation: a create the server
/// applied while the client saw an error, or a task aborted at that await,
/// retires the pod and leaves the Secret behind.
#[must_use]
pub fn worker_serves<'a>(
    pods: impl IntoIterator<Item = &'a Pod>,
    b: &Binding,
    owner: &str,
) -> bool {
    pods.into_iter()
        .any(|p| serves(p, &b.alloc_id) && ann(p, ANN_OWNER) == Some(owner))
}

/// The API message when the Secret create was refused for good: a 4xx other
/// than 401, 409 and 429, such as a quota (403) or an admission policy (422).
/// Retrying would allocate a worker and retire it again on every attempt.
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

/// Status for a claim whose worker went away. Rebinding would need a new
/// certificate in the same Secret while the client may still hold the old
/// one for a pod that no longer exists, so the claim stays `Lost` and the
/// pipeline creates a new claim.
#[must_use]
pub fn lost_status(c: &BuildKitClaim) -> BuildKitClaimStatus {
    let mut s = c.status.clone().unwrap_or_default();
    s.phase = Some(ClaimPhase::Lost);
    s.secret_name = None;
    s.conditions = vec![condition(
        false,
        "WorkerLost",
        "the worker pod was deleted, evicted or failed before the allocation expired",
        c.metadata.generation,
    )];
    s
}

/// What status records about a bound allocation, from the engine or an adopted Secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// Allocation id.
    pub alloc_id: String,
    /// Edge endpoint.
    pub endpoint: String,
    /// TLS server name.
    pub server_name: String,
    /// RFC 3339 expiry.
    pub expires_at: String,
}

impl From<&AllocationResponse> for Binding {
    fn from(r: &AllocationResponse) -> Self {
        Self {
            alloc_id: r.id.clone(),
            endpoint: r.endpoint.clone(),
            server_name: r.server_name.clone(),
            expires_at: r.expires_at.clone(),
        }
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

/// The binding recorded in a Secret this claim controls, for adoption after a crash.
#[must_use]
pub fn adopt(s: &Secret, uid: &str) -> Option<Binding> {
    if !controlled_by(s, uid) {
        return None;
    }
    let a = s.metadata.annotations.as_ref()?;
    let d = s.data.as_ref()?;
    let text = |k: &str| d.get(k).and_then(|b| String::from_utf8(b.0.clone()).ok());
    Some(Binding {
        alloc_id: a.get(SECRET_ANN_ALLOC)?.clone(),
        expires_at: a.get(SECRET_ANN_EXPIRES)?.clone(),
        endpoint: text("endpoint")?,
        server_name: text("server-name")?,
    })
}

/// The Secret for a fresh allocation, controlled by the claim.
#[must_use]
pub fn secret(c: &BuildKitClaim, r: &AllocationResponse) -> Option<Secret> {
    let name = c.metadata.name.clone()?;
    let b = |s: &str| ByteString(s.as_bytes().to_vec());
    let mut data = BTreeMap::from([
        ("tls.crt".to_owned(), b(&r.client_cert)),
        ("tls.key".to_owned(), b(r.client_key.as_deref()?)),
        ("endpoint".to_owned(), b(&r.endpoint)),
        ("server-name".to_owned(), b(&r.server_name)),
    ]);
    if let Some(ca) = &r.ca_cert {
        data.insert("ca.crt".to_owned(), b(ca));
    }
    Some(Secret {
        metadata: ObjectMeta {
            name: Some(secret_name(&name)),
            namespace: c.metadata.namespace.clone(),
            annotations: Some(BTreeMap::from([
                (SECRET_ANN_ALLOC.to_owned(), r.id.clone()),
                (SECRET_ANN_EXPIRES.to_owned(), r.expires_at.clone()),
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

/// Allocated pods whose owning claim (by uid) is gone: `(owner, alloc id)` to release.
///
/// Only pods the engine would release count: managed by this controller and
/// controlled by a pool in the pool store (`pool_uid` maps namespace and pool
/// name to the cached pool's uid). Any other labelled pod would fail release
/// after a wait on every sweep. The owner's claim namespace must be served:
/// claims from other namespaces never bind, and with `watchNamespaces` set the
/// claim store does not hold them, so their absence proves nothing.
#[must_use]
pub fn orphans<'a, S: std::hash::BuildHasher, T: std::hash::BuildHasher>(
    pods: impl IntoIterator<Item = &'a Pod>,
    live_uids: &HashSet<String, S>,
    served: &HashSet<String, T>,
    pool_uid: impl Fn(&str, &str) -> Option<String>,
) -> Vec<(Identity, String)> {
    pods.into_iter()
        .filter(|p| in_pool(p, &pool_uid))
        .filter_map(|p| {
            let who = parse_owner(ann(p, ANN_OWNER)?)?;
            let id = ann(p, ANN_ALLOC).filter(|a| *a != FREE && *a != DRAINING && !a.is_empty())?;
            let Identity::Claim { namespace, uid, .. } = &who else {
                return None;
            };
            (served.contains(namespace) && !live_uids.contains(uid))
                .then(|| (who.clone(), id.to_owned()))
        })
        .collect()
}

/// The engine's pool membership test: the managed-by label, and the pool
/// label, namespace and controller ownerReference of a cached pool.
fn in_pool(p: &Pod, pool_uid: impl Fn(&str, &str) -> Option<String>) -> bool {
    let labels = p.metadata.labels.as_ref();
    let (Some(ns), Some(pool)) = (
        p.metadata.namespace.as_deref(),
        labels.and_then(|l| l.get(LABEL_POOL)),
    ) else {
        return false;
    };
    labels
        .and_then(|l| l.get(LABEL_MANAGED_BY))
        .map(String::as_str)
        == Some(CONTROLLER)
        && pool_uid(ns, pool).is_some_and(|uid| of_pool(p, ns, pool, &uid))
}

#[cfg(test)]
mod tests {
    use buildkit_controller_api::claim::{BuildKitClaimSpec, PoolRef};

    use super::*;

    fn claim(phase: Option<ClaimPhase>, expires: Option<&str>) -> BuildKitClaim {
        let mut c = BuildKitClaim::new(
            "build",
            BuildKitClaimSpec {
                pool_ref: PoolRef {
                    namespace: "pools".into(),
                    name: "default".into(),
                },
                ..BuildKitClaimSpec::default()
            },
        );
        c.metadata.namespace = Some("ci".into());
        c.metadata.uid = Some("u1".into());
        c.status = Some(BuildKitClaimStatus {
            phase,
            expires_at: expires.map(str::to_owned),
            ..BuildKitClaimStatus::default()
        });
        c
    }

    #[test]
    fn request_carries_the_cache_key_and_mode() {
        use buildkit_controller_api::dto::CacheChoice;
        let mut c = claim(None, None);
        let r = request(&c, Some(600)).unwrap();
        assert_eq!(
            (r.cache_key, r.cache, r.ttl_seconds),
            (None, None, Some(600))
        );
        c.spec.cache_key = Some("app-main".into());
        c.spec.cache = Some(CacheChoice::Registry);
        let r = request(&c, None).unwrap();
        assert_eq!(
            r.cache_key
                .as_ref()
                .map(buildkit_controller_api::dto::CacheKey::as_str),
            Some("app-main")
        );
        assert_eq!(r.cache, Some(CacheChoice::Registry));
        assert_eq!(
            (r.namespace.as_str(), r.pool.as_str()),
            ("pools", "default")
        );
        c.spec.cache_key = Some("Bad Key".into());
        assert!(request(&c, None).is_err());
    }

    fn resp(ca: Option<&str>) -> AllocationResponse {
        AllocationResponse {
            id: "a1".into(),
            namespace: "pools".into(),
            pool: "default".into(),
            owner: "claim:ci/build#u1".into(),
            endpoint: "tcp://bk:8444".into(),
            server_name: "bk".into(),
            ca_cert: ca.map(str::to_owned),
            client_cert: "CERT".into(),
            client_key: Some("KEY".into()),
            expires_at: "2026-10-21T12:00:00Z".into(),
            cache: None,
        }
    }

    fn pod(owner: &str, alloc: &str) -> Pod {
        let mut p = Pod::default();
        p.metadata.annotations = Some(BTreeMap::from([
            (ANN_OWNER.to_owned(), owner.to_owned()),
            (ANN_ALLOC.to_owned(), alloc.to_owned()),
        ]));
        // A worker of pool `pools/default` (uid `pu`).
        p.metadata.namespace = Some("pools".into());
        p.metadata.labels = Some(BTreeMap::from([
            (LABEL_MANAGED_BY.to_owned(), CONTROLLER.to_owned()),
            (LABEL_POOL.to_owned(), "default".to_owned()),
        ]));
        p.metadata.owner_references = Some(vec![OwnerReference {
            uid: "pu".into(),
            controller: Some(true),
            ..OwnerReference::default()
        }]);
        p
    }

    /// The pool store of the tests: only `pools/default`, uid `pu`.
    fn pool_uid(ns: &str, name: &str) -> Option<String> {
        (ns == "pools" && name == "default").then(|| "pu".to_owned())
    }

    #[test]
    fn owner_round_trips_and_rejects_others() {
        let c = claim(None, None);
        let who = identity(&c).unwrap();
        assert_eq!(parse_owner(&who.owner()), Some(who));
        for bad in [
            "sa:ci/x",
            "claim:ci/x",
            "claim:/x#u",
            "claim:ci/#u",
            "claim:ci/x#",
        ] {
            assert_eq!(parse_owner(bad), None, "{bad}");
        }
    }

    #[test]
    fn identity_needs_server_metadata() {
        let mut c = claim(None, None);
        c.metadata.uid = None;
        assert!(identity(&c).is_none());
    }

    #[test]
    fn classify_terminal_and_retryable() {
        assert_eq!(
            classify(&AllocError::PoolNotFound),
            Disposition::Fail("PoolNotFound")
        );
        assert_eq!(
            classify(&AllocError::InvalidRequest("ttl".into())),
            Disposition::Fail("InvalidRequest")
        );
        assert_eq!(
            classify(&AllocError::PoolExhausted("p".into())),
            Disposition::Retry("Queued")
        );
        assert_eq!(
            classify(&AllocError::UpstreamError),
            Disposition::Retry("Error")
        );
    }

    #[test]
    fn backoff_doubles_to_cap() {
        assert_eq!(next_backoff(None), BACKOFF_MIN);
        assert_eq!(next_backoff(Some(BACKOFF_MIN)), Duration::from_secs(10));
        assert_eq!(next_backoff(Some(Duration::from_secs(50))), BACKOFF_MAX);
    }

    #[test]
    fn expiry_only_for_bound() {
        let now = parse_ts("2026-10-21T12:00:00Z").unwrap();
        assert!(expired(
            &claim(Some(ClaimPhase::Bound), Some("2026-10-21T11:59:59Z")),
            now
        ));
        assert!(expired(
            &claim(Some(ClaimPhase::Bound), Some("2026-10-21T12:00:00Z")),
            now
        ));
        assert!(!expired(
            &claim(Some(ClaimPhase::Bound), Some("2026-10-21T12:00:01Z")),
            now
        ));
        assert!(!expired(
            &claim(Some(ClaimPhase::Pending), Some("2026-10-21T11:00:00Z")),
            now
        ));
        assert!(!expired(&claim(Some(ClaimPhase::Bound), None), now));
    }

    #[test]
    fn secret_is_controlled_and_adoptable() {
        let c = claim(None, None);
        let s = secret(&c, &resp(Some("CA"))).unwrap();
        assert_eq!(s.metadata.name.as_deref(), Some("build-buildkit"));
        let keys: Vec<_> = s.data.as_ref().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            ["ca.crt", "endpoint", "server-name", "tls.crt", "tls.key"]
        );
        assert!(controlled_by(&s, "u1") && !controlled_by(&s, "u2"));
        assert_eq!(adopt(&s, "u1"), Some(Binding::from(&resp(None))));
        // A Secret from an earlier claim of the same name is not adopted.
        assert_eq!(adopt(&s, "u2"), None);
        let no_ca = secret(&c, &resp(None)).unwrap();
        assert!(!no_ca.data.unwrap().contains_key("ca.crt"));
    }

    #[test]
    fn orphans_are_claim_pods_without_a_live_claim() {
        let pods = [
            pod("claim:ci/a#u1", "a1"),
            pod("claim:ci/b#u2", "a2"),
            pod("claim:ci/c#u3", FREE),
            pod("sa:ci/x", "a4"),
        ];
        let live = HashSet::from(["u1".to_owned()]);
        let served = HashSet::from(["ci".to_owned()]);
        let got = orphans(&pods, &live, &served, pool_uid);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, "a2");
        assert_eq!(got[0].0.owner(), "claim:ci/b#u2");
    }

    /// Only pods the engine would release are swept. A labelled pod anywhere
    /// else would make every sweep wait for a release that cannot succeed.
    #[test]
    fn orphans_are_workers_of_cached_pools_for_served_namespaces() {
        let orphan = || pod("claim:ci/b#u2", "a2");
        let served = HashSet::from(["ci".to_owned()]);
        let live = HashSet::new();
        let swept = |p: Pod| !orphans([&p], &live, &served, pool_uid).is_empty();
        assert!(swept(orphan()));

        let mut planted = orphan();
        planted.metadata.owner_references = None;
        assert!(!swept(planted), "no controller ownerReference");
        let mut other_uid = orphan();
        other_uid.metadata.owner_references.as_mut().unwrap()[0].uid = "old".into();
        assert!(!swept(other_uid), "owned by a deleted or recreated pool");
        let mut other_ns = orphan();
        other_ns.metadata.namespace = Some("tenant".into());
        assert!(!swept(other_ns), "pool not in the pool store");
        let mut unmanaged = orphan();
        unmanaged
            .metadata
            .labels
            .as_mut()
            .unwrap()
            .remove(LABEL_MANAGED_BY);
        assert!(!swept(unmanaged), "not managed by this controller");
        assert!(
            !swept(pod("claim:apps/b#u2", "a2")),
            "claim namespace not served, so its claims may be unwatched"
        );
    }

    #[test]
    fn pool_not_found_is_retried_while_the_claim_is_young() {
        let now = parse_ts("2026-10-21T12:00:00Z").unwrap();
        let mut c = claim(None, None);
        let created = |c: &mut BuildKitClaim, ts: &str| {
            c.metadata.creation_timestamp = Some(Time(ts.parse().unwrap()));
        };
        created(&mut c, "2026-10-21T11:59:30Z");
        assert_eq!(
            dispose(&c, &AllocError::PoolNotFound, now),
            Disposition::Retry("PoolNotFound")
        );
        assert_eq!(
            dispose(&c, &AllocError::InvalidRequest("x".into()), now),
            Disposition::Fail("InvalidRequest"),
            "only PoolNotFound gets the grace"
        );
        assert_eq!(
            dispose(&c, &AllocError::NotReady, now),
            Disposition::Retry("PoolNotReady")
        );
        created(&mut c, "2026-10-21T11:59:00Z");
        assert_eq!(
            dispose(&c, &AllocError::PoolNotFound, now),
            Disposition::Fail("PoolNotFound"),
            "past the grace"
        );
        c.metadata.creation_timestamp = None;
        assert_eq!(
            dispose(&c, &AllocError::PoolNotFound, now),
            Disposition::Fail("PoolNotFound")
        );
    }

    /// A Secret is adopted only while its allocation still has a live worker
    /// owned by the claim.
    #[test]
    fn adoption_needs_a_serving_worker() {
        let b = Binding::from(&resp(None));
        let owner = "claim:ci/build#u1";
        let live = pod(owner, "a1");
        assert!(worker_serves([&live], &b, owner));
        assert!(!worker_serves([], &b, owner), "pod gone");
        assert!(
            !worker_serves([&pod(owner, DRAINING)], &b, owner),
            "retired by the orphan guard"
        );
        assert!(
            !worker_serves([&pod("claim:ci/build#u0", "a1")], &b, owner),
            "another owner"
        );
        let mut deleting = live.clone();
        deleting.metadata.deletion_timestamp = Some(Time(k8s_openapi::jiff::Timestamp::now()));
        assert!(!worker_serves([&deleting], &b, owner), "being deleted");
        let mut failed = live;
        failed.status = Some(k8s_openapi::api::core::v1::PodStatus {
            phase: Some("Failed".into()),
            ..Default::default()
        });
        assert!(!worker_serves([&failed], &b, owner), "failed");
    }

    #[test]
    fn secret_create_refusals_fail_the_claim_only_when_final() {
        let api = |code: u16| {
            kube::Error::Api(
                kube::core::Status::failure("exceeded quota: secrets", "Forbidden")
                    .with_code(code)
                    .boxed(),
            )
        };
        for code in [400, 403, 404, 422] {
            let msg = secret_rejected(&api(code)).unwrap_or_default();
            assert!(msg.contains(&code.to_string()), "{code}: {msg}");
            assert!(msg.contains("exceeded quota"), "{msg}");
        }
        for code in [401, 409, 429, 500, 503] {
            assert_eq!(secret_rejected(&api(code)), None, "{code}");
        }
        assert_eq!(
            secret_rejected(&kube::Error::LinesCodecMaxLineLengthExceeded),
            None
        );
    }

    #[test]
    fn statuses() {
        let c = claim(None, None);
        let bound = bound_status(&c, &Binding::from(&resp(None)));
        assert_eq!(bound.phase, Some(ClaimPhase::Bound));
        assert_eq!(bound.secret_name.as_deref(), Some("build-buildkit"));
        let f = unbound_status(&c, ClaimPhase::Failed, "PoolNotFound", "pool not found");
        assert_eq!(f.conditions[0].status, "False");
        let mut b = c.clone();
        b.status = Some(bound);
        let e = expired_status(&b);
        assert_eq!((e.phase, e.secret_name), (Some(ClaimPhase::Expired), None));
        assert_eq!(reason(&b), Some("Allocated"));
    }

    #[test]
    fn bound_claim_is_lost_when_its_worker_goes() {
        let mut c = claim(Some(ClaimPhase::Bound), Some("2026-10-21T12:00:00Z"));
        c.status = Some(bound_status(&c, &Binding::from(&resp(None))));
        let at = |secs: u64| OffsetDateTime::now_utc() + LOST_GRACE + Duration::from_secs(secs);
        let live = pod("claim:ci/build#u1", "a1");
        assert!(!lost(&c, [&live], at(1)), "the worker is running");
        assert!(lost(&c, [], at(1)), "the worker pod was deleted");
        assert!(
            lost(&c, [&pod("claim:ci/build#u1", "a2")], at(1)),
            "the pod now serves another allocation"
        );
        assert!(
            !lost(&c, [], OffsetDateTime::now_utc()),
            "inside the grace after binding"
        );

        let mut evicted = live.clone();
        evicted.status = Some(k8s_openapi::api::core::v1::PodStatus {
            phase: Some("Failed".into()),
            ..Default::default()
        });
        assert!(lost(&c, [&evicted], at(1)), "the pod was evicted or failed");
        let mut deleting = live.clone();
        deleting.metadata.deletion_timestamp = Some(Time(k8s_openapi::jiff::Timestamp::now()));
        assert!(lost(&c, [&deleting], at(1)), "the pod is being deleted");

        let s = lost_status(&c);
        assert_eq!(s.phase, Some(ClaimPhase::Lost));
        assert!(s.secret_name.is_none() && ClaimPhase::Lost.is_terminal());
        assert_eq!(s.conditions[0].reason, "WorkerLost");
        c.status = Some(s);
        assert!(!lost(&c, [], at(1)), "a Lost claim is never judged again");
        assert!(!expired(&c, at(100_000)), "nor expired");
    }

    #[test]
    fn only_listed_namespaces_are_served() {
        let c = claim(None, None);
        assert!(served(&c, &HashSet::from(["ci".to_owned()])));
        assert!(!served(&c, &HashSet::from(["other".to_owned()])));
        let mut no_ns = c;
        no_ns.metadata.namespace = None;
        assert!(!served(&no_ns, &HashSet::from(["ci".to_owned()])));
    }
}
