//! The cache part of an allocation: the caller's scope and trust tier, the
//! buildx flags for the built-in registry, and the pod annotations the
//! registry authorizes from.

use buildkit_controller_api::claim::CacheTier;
use buildkit_controller_api::dto::{CacheChoice, CacheKey, CacheResponse, RegistryCacheFlags};
use buildkit_controller_api::v1alpha1::{BuildKitPool, CacheScope};
use kube::ResourceExt;
use serde_json::{Value, json};

use super::pods::ptr;
use super::pool::rules;
use crate::auth::Identity;
use crate::render::{self, ANN_SCOPE, ANN_TIER};

/// GitHub Actions events whose builds may write the cache by default.
pub const TRUSTED_EVENTS: [&str; 4] = ["push", "schedule", "workflow_dispatch", "merge_group"];
/// GitHub Actions events whose builds never write the cache, whatever
/// `cache.writers` says: they can run code from forks, and `workflow_run` and
/// the comment and review events do so from the base repository's own ref.
pub const READ_ONLY_EVENTS: [&str; 6] = [
    "pull_request",
    "pull_request_target",
    "workflow_run",
    "issue_comment",
    "pull_request_review",
    "pull_request_review_comment",
];

/// The cache of one allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Plan {
    /// `<host>/<pool-namespace>/<pool>/<scope>`.
    pub repository: String,
    /// The tag: the request's `cacheKey`, else the pool name.
    pub key: String,
    /// The caller's scope id.
    pub scope: String,
    /// Whether the allocation may push.
    pub tier: CacheTier,
}

/// The cache of a claim on `pool`, or `None` when the controller runs no
/// cache registry (`host`) or the request asked for none.
pub(super) fn plan(
    host: Option<&str>,
    pool: &BuildKitPool,
    key: Option<&CacheKey>,
    choice: Option<CacheChoice>,
    scope: String,
    tier: CacheTier,
) -> Option<Plan> {
    let host = host?;
    if choice == Some(CacheChoice::None) {
        return None;
    }
    let name = pool.name_any();
    Some(Plan {
        repository: format!(
            "{host}/{}/{name}/{scope}",
            pool.namespace().unwrap_or_default()
        ),
        key: key.map_or_else(|| name.clone(), |k| k.as_str().to_owned()),
        scope,
        tier,
    })
}

impl Plan {
    /// The `201` response's `cache`: `--cache-from` always, `--cache-to` only
    /// for a trusted allocation. A failed export never fails the build.
    pub(super) fn response(&self) -> CacheResponse {
        let r = format!("{}:{}", self.repository, self.key);
        CacheResponse {
            key: self.key.clone(),
            registry: Some(RegistryCacheFlags {
                to: (self.tier == CacheTier::Trusted).then(|| {
                    format!("type=registry,ref={r},mode=max,ignore-error=true,oci-mediatypes=true")
                }),
                from: vec![format!("type=registry,ref={r}")],
            }),
        }
    }

    /// Add `P scope` and `P tier` to a claim patch: the registry reads the
    /// allocation's rights from the pod.
    pub(super) fn claim_ops(&self, patch: &mut Value) {
        if let Value::Array(ops) = patch {
            ops.push(
                json!({"op": "add", "path": ptr("annotations", ANN_SCOPE), "value": self.scope}),
            );
            ops.push(json!({"op": "add", "path": ptr("annotations", ANN_TIER), "value": self.tier.as_str()}));
        }
    }
}

/// The cache scope name of `who` under `scope`: the caller's trust boundary.
/// A GitHub Actions token (or any OIDC token with the claim) is scoped to
/// `repository_id`, else `repository`; any other OIDC token to its `sub`.
/// The issuer is always part of it, so another issuer cannot claim the same
/// scope. A `ServiceAccount` is scoped to its namespace and name, a
/// `BuildKitClaim` to its namespace (every claim of a namespace is one tenant).
#[must_use]
pub fn scope_name(scope: CacheScope, who: &Identity) -> String {
    let name = match (scope, who) {
        (CacheScope::Pool, _) => json!(["pool"]),
        (
            CacheScope::Identity,
            Identity::Oidc {
                issuer,
                subject,
                claims,
            },
        ) => {
            let (k, v) = ["repository_id", "repository"]
                .into_iter()
                .find_map(|k| claim(claims, k).map(|v| (k, v)))
                .unwrap_or(("sub", subject.clone()));
            json!(["oidc", issuer, k, v])
        }
        (
            CacheScope::Identity,
            Identity::ServiceAccount {
                namespace, name, ..
            },
        ) => json!(["sa", namespace, name]),
        (CacheScope::Identity, Identity::Claim { namespace, .. }) => json!(["claim", namespace]),
    };
    name.to_string()
}

fn claim(claims: &serde_json::Map<String, Value>, k: &str) -> Option<String> {
    match claims.get(k) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// The scope id of `who` in `pool`.
#[must_use]
pub fn scope(pool: &BuildKitPool, who: &Identity) -> String {
    render::scope_id(&scope_name(pool.spec.workers.cache.scope, who))
}

/// Whether `who` may write the cache of `pool`. An OIDC caller whose
/// `event_name` is one of [`READ_ONLY_EVENTS`] never may. Otherwise, with
/// `cache.writers` set, a caller one of them matches; a malformed list trusts
/// nobody. Unset, an OIDC caller whose `event_name` is one of
/// [`TRUSTED_EVENTS`] and every in-cluster caller.
#[must_use]
pub fn tier(pool: &BuildKitPool, who: &Identity) -> CacheTier {
    let pull_request = matches!(who, Identity::Oidc { claims, .. }
        if claim(claims, "event_name").is_some_and(|e| READ_ONLY_EVENTS.contains(&e.as_str())));
    let trusted = !pull_request
        && match &pool.spec.workers.cache.writers {
            Some(w) => {
                rules(w).is_some_and(|r| r.allows(who, &pool.namespace().unwrap_or_default()))
            }
            None => match who {
                Identity::Oidc { claims, .. } => claim(claims, "event_name")
                    .is_some_and(|e| TRUSTED_EVENTS.contains(&e.as_str())),
                Identity::ServiceAccount { .. } | Identity::Claim { .. } => true,
            },
        };
    if trusted {
        CacheTier::Trusted
    } else {
        CacheTier::Untrusted
    }
}
