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
use crate::render::{self, ANN_BRANCH_SCOPE, ANN_SCOPE, ANN_TIER};

/// GitHub Actions events whose builds may write the cache by default, when
/// the token's `ref_protected` claim is also true. Not `merge_group`: its
/// queue ref is never protected.
pub const TRUSTED_EVENTS: [&str; 3] = ["push", "schedule", "workflow_dispatch"];
/// GitHub Actions events whose builds never write the cache, whatever
/// `cache.writers` says: they can run code from forks or from unreviewed
/// refs, and `workflow_run` and the comment and review events do so from the
/// base repository's own ref. A `deployment` can target any commit,
/// including a pull request's head.
pub const READ_ONLY_EVENTS: [&str; 8] = [
    "pull_request",
    "pull_request_target",
    "workflow_run",
    "issue_comment",
    "pull_request_review",
    "pull_request_review_comment",
    "deployment",
    "deployment_status",
];
/// GitLab CI `pipeline_source` values whose builds never write the cache,
/// whatever `cache.writers` says: merge request pipelines, which can run a
/// fork's code in the parent project, and child pipelines, whose token does
/// not say what started the parent.
pub const READ_ONLY_PIPELINE_SOURCES: [&str; 3] = [
    "merge_request_event",
    "external_pull_request_event",
    "parent_pipeline",
];

/// The cache of one allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Plan {
    /// `<host>/<pool-namespace>/<pool>`.
    pub base: String,
    /// The tag: the request's `cacheKey`, else the pool name.
    pub key: String,
    /// The caller's scope id: every allocation pulls it, a trusted one pushes it.
    pub scope: String,
    /// Whether the allocation may push its scope.
    pub tier: CacheTier,
    /// The untrusted caller's own branch scope id, which it pulls first and
    /// pushes. Never set for a trusted allocation.
    pub branch: Option<String>,
}

/// The cache of a claim on `pool`, or `None` when the controller runs no
/// cache registry (`host`) or the request asked for none. `branch` is kept
/// only for an untrusted allocation and only when it is a well-formed scope
/// id other than `scope`.
pub(super) fn plan(
    host: Option<&str>,
    pool: &BuildKitPool,
    key: Option<&CacheKey>,
    choice: Option<CacheChoice>,
    scope: String,
    tier: CacheTier,
    branch: Option<String>,
) -> Option<Plan> {
    let host = host?;
    if choice == Some(CacheChoice::None) {
        return None;
    }
    let name = pool.name_any();
    let branch =
        branch.filter(|b| tier == CacheTier::Untrusted && render::is_scope_id(b) && *b != scope);
    Some(Plan {
        base: format!("{host}/{}/{name}", pool.namespace().unwrap_or_default()),
        key: key.map_or_else(|| name.clone(), |k| k.as_str().to_owned()),
        scope,
        tier,
        branch,
    })
}

impl Plan {
    /// The `201` response's `cache`. Trusted: `--cache-to` and `--cache-from`
    /// name the scope. Untrusted with a branch scope: `--cache-to` names the
    /// branch, `--cache-from` the branch then the scope, so the branch's own
    /// layers win. Untrusted without one: `--cache-from` the scope only. A
    /// failed export never fails the build.
    pub(super) fn response(&self) -> CacheResponse {
        let r = |scope: &str| format!("{}/{scope}:{}", self.base, self.key);
        let to = |r: String| {
            format!("type=registry,ref={r},mode=max,ignore-error=true,oci-mediatypes=true")
        };
        let from = |r: String| format!("type=registry,ref={r}");
        let (to, from) = match (self.tier, &self.branch) {
            (CacheTier::Trusted, _) => (Some(to(r(&self.scope))), vec![from(r(&self.scope))]),
            (CacheTier::Untrusted, Some(b)) => {
                (Some(to(r(b))), vec![from(r(b)), from(r(&self.scope))])
            }
            (CacheTier::Untrusted, None) => (None, vec![from(r(&self.scope))]),
        };
        CacheResponse {
            key: self.key.clone(),
            registry: Some(RegistryCacheFlags { to, from }),
        }
    }

    /// Add `P scope`, `P tier` and, with a branch scope, `P branch-scope` to a
    /// claim patch: the registry reads the allocation's rights from the pod.
    pub(super) fn claim_ops(&self, patch: &mut Value) {
        if let Value::Array(ops) = patch {
            ops.push(
                json!({"op": "add", "path": ptr("annotations", ANN_SCOPE), "value": self.scope}),
            );
            ops.push(json!({"op": "add", "path": ptr("annotations", ANN_TIER), "value": self.tier.as_str()}));
            if let Some(b) = &self.branch {
                ops.push(
                    json!({"op": "add", "path": ptr("annotations", ANN_BRANCH_SCOPE), "value": b}),
                );
            }
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

/// Whether the claim `k` is true: GitHub and GitLab send `"true"`.
fn is_true(claims: &serde_json::Map<String, Value>, k: &str) -> bool {
    match claims.get(k) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => false,
    }
}

/// The scope id of `who` in `pool`.
#[must_use]
pub fn scope(pool: &BuildKitPool, who: &Identity) -> String {
    render::scope_id(&scope_name(pool.spec.workers.cache.scope, who))
}

/// The branch scope id of `who` in `pool`: only for an untrusted OIDC caller
/// with a `ref` claim (`refs/heads/<branch>`, `refs/pull/<n>/merge`, a GitLab
/// ref), from the pool's scope name, the caller's repository identity (its
/// scope name under `scope: identity`, so a shared pool scope still keeps
/// repositories apart) and the ref. Every other caller has none.
#[must_use]
pub fn branch_scope(pool: &BuildKitPool, who: &Identity) -> Option<String> {
    let Identity::Oidc { claims, .. } = who else {
        return None;
    };
    if tier(pool, who) == CacheTier::Trusted {
        return None;
    }
    let git_ref = match claims.get("ref") {
        Some(Value::String(s)) if !s.is_empty() => s,
        _ => return None,
    };
    Some(render::branch_scope_id(
        &scope_name(pool.spec.workers.cache.scope, who),
        &scope_name(CacheScope::Identity, who),
        git_ref,
    ))
}

/// Whether `who` may write the cache of `pool`. An OIDC caller whose
/// `event_name` is one of [`READ_ONLY_EVENTS`], or whose `pipeline_source` is
/// one of [`READ_ONLY_PIPELINE_SOURCES`], never may. Otherwise, with
/// `cache.writers` set, a caller one of them matches; a malformed list trusts
/// nobody. Unset, every in-cluster caller, and an OIDC caller whose
/// `event_name` is one of [`TRUSTED_EVENTS`] and whose `ref_protected` is
/// true: every branch shares the repository's scope, so a build of a branch
/// anyone with push access can write would otherwise feed the protected
/// branch's builds.
#[must_use]
pub fn tier(pool: &BuildKitPool, who: &Identity) -> CacheTier {
    let read_only = matches!(who, Identity::Oidc { claims, .. }
        if claim(claims, "event_name").is_some_and(|e| READ_ONLY_EVENTS.contains(&e.as_str()))
            || claim(claims, "pipeline_source")
                .is_some_and(|s| READ_ONLY_PIPELINE_SOURCES.contains(&s.as_str())));
    let trusted = !read_only
        && match &pool.spec.workers.cache.writers {
            Some(w) => {
                rules(w).is_some_and(|r| r.allows(who, &pool.namespace().unwrap_or_default()))
            }
            None => match who {
                Identity::Oidc { claims, .. } => {
                    claim(claims, "event_name")
                        .is_some_and(|e| TRUSTED_EVENTS.contains(&e.as_str()))
                        && is_true(claims, "ref_protected")
                }
                Identity::ServiceAccount { .. } | Identity::Claim { .. } => true,
            },
        };
    if trusted {
        CacheTier::Trusted
    } else {
        CacheTier::Untrusted
    }
}
