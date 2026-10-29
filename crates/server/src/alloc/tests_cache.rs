//! The cache part of allocation: scope, tier, flags and pod annotations.

#![allow(clippy::many_single_char_names)]

use buildkit_controller_api::claim::CacheTier;
use buildkit_controller_api::dto::CacheResponse;

use super::*;
use crate::alloc::cache;
use crate::render::{ANN_SCOPE, ANN_TIER, is_scope_id, scope_id};

/// A GitHub Actions token for `event` on the protected `main`.
fn oidc(event: &str) -> Identity {
    with_claims(oidc_unprotected(event), json!({"ref_protected": "true"}))
}

fn oidc_unprotected(event: &str) -> Identity {
    let claims = json!({"repository_id": 42, "repository": "o/app", "event_name": event});
    Identity::Oidc {
        issuer: "https://token.actions.githubusercontent.com".into(),
        subject: "repo:o/app:ref:refs/heads/main".into(),
        claims: claims.as_object().unwrap().clone(),
    }
}

fn claim_id(ns: &str) -> Identity {
    Identity::Claim {
        namespace: ns.into(),
        name: "build".into(),
        uid: "u".into(),
    }
}

/// The bound claim's cache and its pod's `P scope` and `P tier`.
fn bound(e: &Env, ns: &str, name: &str) -> (Option<CacheResponse>, Option<String>, Option<String>) {
    let s = e.fake.status(ns, name);
    assert_eq!(phase(&s), ClaimPhase::Bound, "{s:?}");
    let pod = e.fake.pod(s.pod.as_deref().unwrap());
    let a = |k| ann(&pod, k).map(str::to_owned);
    (s.cache, a(ANN_SCOPE), a(ANN_TIER))
}

#[test]
fn default_writers_trust_push_events_and_in_cluster_callers() {
    let p = pool();
    for e in cache::TRUSTED_EVENTS {
        assert_eq!(cache::tier(&p, &oidc(e)), CacheTier::Trusted, "{e}");
    }
    for e in ["pull_request", "pull_request_target", "issue_comment", ""] {
        assert_eq!(cache::tier(&p, &oidc(e)), CacheTier::Untrusted, "{e}");
    }
    assert_eq!(cache::tier(&p, &sa("x")), CacheTier::Trusted);
    assert_eq!(cache::tier(&p, &claim_id(CI)), CacheTier::Trusted);
}

#[test]
fn writers_replace_the_default() {
    let mut p = pool();
    p.spec.workers.cache.writers = Some(
        serde_json::from_value(json!([{"oidc": {
            "issuer": "https://token.actions.githubusercontent.com",
            "claims": {"event_name": "workflow_dispatch"}
        }}]))
        .unwrap(),
    );
    assert_eq!(
        cache::tier(&p, &oidc("workflow_dispatch")),
        CacheTier::Trusted
    );
    assert_eq!(cache::tier(&p, &oidc("push")), CacheTier::Untrusted);
    assert_eq!(cache::tier(&p, &sa("x")), CacheTier::Untrusted);
    p.spec.workers.cache.writers = Some(Vec::new());
    assert_eq!(cache::tier(&p, &claim_id(CI)), CacheTier::Untrusted);
}

/// Pull request builds, and the `workflow_run`, comment and review jobs that can
/// run their code, are never trusted, whatever the writers say.
#[test]
fn pull_requests_never_write_whatever_the_writers_say() {
    let mut p = pool();
    p.spec.workers.cache.writers = Some(
        serde_json::from_value(json!([
            {"oidc": {"issuer": "https://token.actions.githubusercontent.com", "claims": {"event_name": "pull_request*"}}},
            {"oidc": {"issuer": "https://token.actions.githubusercontent.com", "claims": {"repository": "o/app"}}}
        ]))
        .unwrap(),
    );
    // Jobs that run in the base repository's context but can check out fork code.
    for e in [
        "pull_request",
        "pull_request_target",
        "workflow_run",
        "issue_comment",
        "pull_request_review",
        "pull_request_review_comment",
    ] {
        assert_eq!(cache::tier(&p, &oidc(e)), CacheTier::Untrusted, "{e}");
    }
    // The same caller on a push is trusted by the repository rule.
    assert_eq!(cache::tier(&p, &oidc("push")), CacheTier::Trusted);
}

#[test]
fn scopes_are_per_repository_unless_the_pool_shares_one() {
    let p = pool();
    let a = cache::scope(&p, &oidc("push"));
    assert!(is_scope_id(&a));
    // The event does not matter: a fork build reads the repository's cache.
    assert_eq!(a, cache::scope(&p, &oidc("pull_request")));
    assert_ne!(a, cache::scope(&p, &sa("x")));
    assert_ne!(cache::scope(&p, &sa("x")), cache::scope(&p, &sa("y")));
    assert_eq!(
        cache::scope(&p, &claim_id(CI)),
        scope_id(&json!(["claim", CI]).to_string())
    );
    let mut shared = pool();
    shared.spec.workers.cache.scope = buildkit_controller_api::v1alpha1::CacheScope::Pool;
    assert_eq!(
        cache::scope(&shared, &sa("x")),
        cache::scope(&shared, &oidc("x"))
    );
}

/// The API writes scope and tier into the claim; the bind puts them on the
/// pod and the flags name `<host>/<ns>/<pool>/<scope>:<key>`.
#[tokio::test]
async fn a_trusted_api_allocation_gets_both_flags() {
    let e = Arc::new(env());
    e.warm(0);
    let req: CreateAllocationRequest = serde_json::from_value(json!({
        "namespace": POOL_NS, "pool": POOL, "publicKey": *KEY, "cacheKey": "app"
    }))
    .unwrap();
    let leader = {
        let e = Arc::clone(&e);
        let mut sub = ClaimSubscription::new(e.fake.tx.subscribe());
        tokio::spawn(async move {
            while sub.recv().await.is_some() {
                e.binder.pass(&key()).await;
            }
        })
    };
    let out = e.front.allocate(&sa("alice"), &req).await.unwrap();
    leader.abort();
    let Outcome::Bound(r) = out else {
        panic!("{out:?}")
    };
    let c = e.fake.claim(OWN, &r.id).unwrap();
    let scope = c.spec.cache_scope.clone().unwrap();
    assert_eq!(scope, cache::scope(&e.pool, &sa("alice")));
    assert_eq!(c.spec.cache_tier, Some(CacheTier::Trusted));
    let refr = format!("{CACHE_HOST}/{POOL_NS}/{POOL}/{scope}:app");
    let flags = r.cache.as_ref().unwrap().registry.as_ref().unwrap();
    assert_eq!(
        flags.to.as_deref(),
        Some(
            format!("type=registry,ref={refr},mode=max,ignore-error=true,oci-mediatypes=true")
                .as_str()
        )
    );
    assert_eq!(flags.from, [format!("type=registry,ref={refr}")]);
    let (cached, s, t) = bound(&e, OWN, &r.id);
    assert_eq!(cached, r.cache);
    assert_eq!(
        (s.as_deref(), t.as_deref()),
        (Some(scope.as_str()), Some("trusted"))
    );
}

/// An untrusted allocation reads the same cache and gets no `--cache-to`; a
/// claim the API wrote without a tier is untrusted.
#[tokio::test]
async fn untrusted_allocations_only_read() {
    let e = env();
    e.warm(0);
    e.warm(1);
    let scope = cache::scope(&e.pool, &oidc("pull_request"));
    let spec = |tier: Option<&str>| {
        let mut s = json!({"owner": "oidc:x", "publicKey": *KEY, "cacheScope": scope});
        if let Some(t) = tier {
            s["cacheTier"] = json!(t);
        }
        s
    };
    let a = new_alloc_id().unwrap().to_string();
    e.claim(OWN, &a, 1, spec(Some("untrusted")));
    let b = new_alloc_id().unwrap().to_string();
    e.claim(OWN, &b, 0, spec(None));
    e.binder.pass(&key()).await;
    for id in [&a, &b] {
        let (cached, s, t) = bound(&e, OWN, id);
        let c = cached.unwrap();
        let flags = c.registry.unwrap();
        assert_eq!(flags.to, None, "{id}");
        assert_eq!(
            flags.from,
            [format!(
                "type=registry,ref={CACHE_HOST}/{POOL_NS}/{POOL}/{scope}:{POOL}"
            )]
        );
        assert_eq!(c.key, POOL);
        assert_eq!(
            (s.as_deref(), t.as_deref()),
            (Some(scope.as_str()), Some("untrusted"))
        );
    }
}

/// No cache: `cache: none`, or a malformed scope the API never writes. The pod then carries no scope, so the
/// registry grants it nothing.
#[tokio::test]
async fn without_a_cache_the_pod_gets_no_scope() {
    let e = env();
    for slot in 0..3 {
        e.warm(slot);
    }
    let none = new_alloc_id().unwrap().to_string();
    let scope = cache::scope(&e.pool, &sa("a"));
    e.claim(OWN, &none, 2, json!({"owner": "sa:x/a", "publicKey": *KEY, "cacheScope": scope, "cacheTier": "trusted", "cache": "none"}));
    let bad = new_alloc_id().unwrap().to_string();
    e.claim(OWN, &bad, 1, json!({"owner": "sa:x/a", "publicKey": *KEY, "cacheScope": "../../other", "cacheTier": "trusted"}));
    e.binder.pass(&key()).await;
    for id in [&none, &bad] {
        assert_eq!(bound(&e, OWN, id), (None, None, None), "{id}");
    }
}

/// A tenant claim's scope and tier come from the claim itself, never from
/// fields it wrote.
#[tokio::test]
async fn a_tenant_claim_is_scoped_to_its_namespace() {
    let e = env();
    e.warm(0);
    let forged = cache::scope(&e.pool, &sa("victim"));
    e.claim(
        CI,
        "build",
        0,
        json!({"cacheScope": forged, "cacheTier": "trusted"}),
    );
    e.binder.pass(&key()).await;
    let (cached, s, t) = bound(&e, CI, "build");
    let want = cache::scope(&e.pool, &claim_id(CI));
    assert_eq!(
        (s.as_deref(), t.as_deref()),
        (Some(want.as_str()), Some("trusted"))
    );
    assert!(cached.unwrap().registry.unwrap().from[0].contains(&want));
}

/// `who` with `extra` claims added or replaced.
fn with_claims(who: Identity, extra: serde_json::Value) -> Identity {
    let Identity::Oidc {
        issuer,
        subject,
        mut claims,
    } = who
    else {
        unreachable!("an OIDC identity")
    };
    claims.extend(extra.as_object().unwrap().clone());
    Identity::Oidc {
        issuer,
        subject,
        claims,
    }
}

/// Every branch shares the repository's scope, so by default only a build of
/// a protected ref may write it: a branch anyone with push access can write
/// must not feed the protected branch's builds.
#[test]
fn by_default_only_protected_refs_write() {
    let p = pool();
    for e in cache::TRUSTED_EVENTS {
        assert_eq!(
            cache::tier(&p, &oidc_unprotected(e)),
            CacheTier::Untrusted,
            "{e}"
        );
        for v in [json!("false"), json!(""), json!(null), json!(1)] {
            let who = with_claims(oidc(e), json!({ "ref_protected": v }));
            assert_eq!(cache::tier(&p, &who), CacheTier::Untrusted, "{e} {v}");
        }
        let who = with_claims(oidc(e), json!({"ref_protected": true}));
        assert_eq!(cache::tier(&p, &who), CacheTier::Trusted, "{e}");
    }
    // A writers rule decides on its own claims.
    let mut p = pool();
    p.spec.workers.cache.writers = Some(
        serde_json::from_value(json!([{"oidc": {
            "issuer": "https://token.actions.githubusercontent.com",
            "claims": {"repository": "o/app"}
        }}]))
        .unwrap(),
    );
    assert_eq!(
        cache::tier(&p, &oidc_unprotected("push")),
        CacheTier::Trusted
    );
}

/// Deployments can target any commit, and GitLab merge request pipelines can
/// run a fork's code in the parent project: neither writes, whatever the
/// writers say.
#[test]
fn deployments_and_merge_request_pipelines_never_write() {
    let mut p = pool();
    p.spec.workers.cache.writers = Some(
        serde_json::from_value(json!([
            {"oidc": {"issuer": "https://token.actions.githubusercontent.com", "claims": {"repository": "o/app"}}},
            {"oidc": {"issuer": "https://gitlab.com", "claims": {"project_path": "g/app"}}}
        ]))
        .unwrap(),
    );
    for e in ["deployment", "deployment_status"] {
        assert_eq!(cache::tier(&p, &oidc(e)), CacheTier::Untrusted, "{e}");
    }
    let gitlab = |source: &str| Identity::Oidc {
        issuer: "https://gitlab.com".into(),
        subject: "project_path:g/app:ref_type:branch:ref:main".into(),
        claims:
            json!({"project_path": "g/app", "pipeline_source": source, "ref_protected": "true"})
                .as_object()
                .unwrap()
                .clone(),
    };
    for s in cache::READ_ONLY_PIPELINE_SOURCES {
        assert_eq!(cache::tier(&p, &gitlab(s)), CacheTier::Untrusted, "{s}");
    }
    assert_eq!(cache::tier(&p, &gitlab("push")), CacheTier::Trusted);
    // GitLab tokens carry no `event_name`, so without writers they never write.
    p.spec.workers.cache.writers = None;
    assert_eq!(cache::tier(&p, &gitlab("push")), CacheTier::Untrusted);
}
