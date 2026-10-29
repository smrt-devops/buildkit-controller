#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use buildkit_controller_api::claim::{BuildKitClaimSpec, PoolRef};
use buildkit_controller_api::dto::ErrorCode;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;

use super::*;
use crate::alloc::pods::new_alloc_id;

const OWN: &str = "bk-system";

fn claim(ns: &str, name: &str, uid: &str, created: &str) -> BuildKitClaim {
    let mut c = BuildKitClaim::new(
        name,
        BuildKitClaimSpec {
            pool_ref: PoolRef {
                namespace: "pools".into(),
                name: "default".into(),
            },
            ..BuildKitClaimSpec::default()
        },
    );
    c.metadata.namespace = Some(ns.into());
    c.metadata.uid = Some(uid.into());
    c.metadata.creation_timestamp = Some(Time(created.parse().unwrap()));
    c
}

fn with_phase(mut c: BuildKitClaim, phase: ClaimPhase) -> BuildKitClaim {
    c.status = Some(BuildKitClaimStatus {
        phase: Some(phase),
        ..BuildKitClaimStatus::default()
    });
    c
}

fn served() -> HashSet<String> {
    HashSet::from([OWN.to_owned(), "ci".to_owned()])
}

fn pod(name: &str, claim: Option<&str>, alloc: &str) -> Pod {
    let mut p = Pod::default();
    p.metadata.name = Some(name.into());
    let mut a = BTreeMap::from([(ANN_ALLOC.to_owned(), alloc.to_owned())]);
    if let Some(c) = claim {
        a.insert(ANN_CLAIM.to_owned(), c.to_owned());
    }
    p.metadata.annotations = Some(a);
    p
}

fn binding(id: &str) -> Binding {
    Binding {
        alloc_id: id.into(),
        pod: "default-0".into(),
        endpoint: "tcp://bk:8444".into(),
        server_name: "bk".into(),
        expires_at: "2026-10-21T12:00:00Z".into(),
        certificate: "CERT".into(),
        cache: None,
    }
}

#[test]
fn a_written_owner_counts_only_in_the_controllers_namespace() {
    let mut ci = claim(OWN, "a", "u1", "2026-10-21T12:00:00Z");
    ci.spec.owner = Some("oidc:https://t#repo".into());
    ci.spec.cache_scope = Some("scope".into());
    let c = caller(&ci, OWN).unwrap();
    assert_eq!(c.owner, "oidc:https://t#repo");
    assert!(c.identity.is_none());
    assert_eq!(c.cache_scope.as_deref(), Some("scope"));
    // The same fields in a tenant namespace are ignored.
    let mut tenant = ci.clone();
    tenant.metadata.namespace = Some("ci".into());
    let c = caller(&tenant, OWN).unwrap();
    assert_eq!(c.owner, "claim:ci/a#u1");
    assert!(c.identity.is_some() && c.cache_scope.is_none());
    // In the controller's namespace without an owner, the claim owns itself.
    ci.spec.owner = None;
    assert_eq!(caller(&ci, OWN).unwrap().owner, "claim:bk-system/a#u1");
}

#[test]
fn allocation_ids_are_fixed_for_the_claim() {
    let id = new_alloc_id().unwrap();
    let ci = claim(OWN, id.as_str(), "u1", "2026-10-21T12:00:00Z");
    assert_eq!(alloc_id(&ci, OWN), Some(id.clone()));
    // A tenant claim of the same name takes its id from its uid instead.
    let tenant = claim("ci", id.as_str(), "u1", "2026-10-21T12:00:00Z");
    let derived = alloc_id(&tenant, OWN).unwrap();
    assert_ne!(derived, id);
    assert_eq!(alloc_id(&tenant, OWN), Some(derived.clone()), "stable");
    let other = claim("ci", id.as_str(), "u2", "2026-10-21T12:00:00Z");
    assert_ne!(
        alloc_id(&other, OWN),
        Some(derived),
        "a recreated claim is another allocation"
    );
    // A name that is no allocation id falls back to the uid.
    let named = claim(OWN, "build", "u1", "2026-10-21T12:00:00Z");
    assert_eq!(alloc_id(&named, OWN), alloc_id(&tenant, OWN));
}

#[test]
fn queues_are_fifo_by_creation_then_name_and_hold_only_waiting_served_claims() {
    let claims: Vec<Arc<BuildKitClaim>> = [
        claim("ci", "c", "u3", "2026-10-21T12:00:02Z"),
        claim("ci", "b", "u2", "2026-10-21T12:00:01Z"),
        claim(OWN, "a", "u1", "2026-10-21T12:00:01Z"),
        with_phase(
            claim("ci", "bound", "u4", "2026-10-21T11:00:00Z"),
            ClaimPhase::Bound,
        ),
        claim("elsewhere", "x", "u5", "2026-10-21T11:00:00Z"),
    ]
    .into_iter()
    .map(Arc::new)
    .collect();
    let mut deleting = claim("ci", "d", "u6", "2026-10-21T11:00:00Z");
    deleting.metadata.deletion_timestamp = Some(Time("2026-10-21T12:00:03Z".parse().unwrap()));
    let mut all = claims.clone();
    all.push(Arc::new(deleting));
    let q = queues(&all, &served());
    let names: Vec<&str> = q[&("pools".into(), "default".into())]
        .iter()
        .map(|c| c.metadata.name.as_deref().unwrap())
        .collect();
    assert_eq!(names, ["a", "b", "c"]);
    assert_eq!(position(&all, &served(), &claims[0]), Some(3));
    assert_eq!(position(&all, &served(), &claims[2]), Some(1));
    assert_eq!(position(&all, &served(), &claims[3]), None, "bound");
}

#[test]
fn deadlines_grace_and_expiry() {
    let c = claim("ci", "a", "u1", "2026-10-21T12:00:00Z");
    assert_eq!(
        queue_deadline(&c, 120),
        Some(parse_ts("2026-10-21T12:02:00Z").unwrap())
    );
    assert!(young(&c, parse_ts("2026-10-21T12:00:29Z").unwrap()));
    assert!(!young(&c, parse_ts("2026-10-21T12:00:30Z").unwrap()));
    let now = parse_ts("2026-10-21T12:00:00Z").unwrap();
    let bound = |exp: &str| {
        let mut c = c.clone();
        c.status = Some(BuildKitClaimStatus {
            phase: Some(ClaimPhase::Bound),
            expires_at: Some(exp.into()),
            ..BuildKitClaimStatus::default()
        });
        c
    };
    assert!(expired(&bound("2026-10-21T11:59:59Z"), now));
    assert!(expired(&bound("2026-10-21T12:00:00Z"), now));
    assert!(!expired(&bound("2026-10-21T12:00:01Z"), now));
}

#[test]
fn a_bound_claim_is_lost_when_its_worker_goes() {
    let mut c = claim("ci", "build", "u1", "2026-10-21T12:00:00Z");
    c.status = Some(bound_status(&c, &binding("a1"), true));
    let at = |secs: u64| OffsetDateTime::now_utc() + LOST_GRACE + Duration::from_secs(secs);
    let live = pod("default-0", Some("ci/build"), "a1");
    assert!(!lost(&c, [&live], at(1)), "the worker is running");
    assert!(lost(&c, [], at(1)), "the worker pod was deleted");
    assert!(lost(&c, [&pod("default-0", None, "a2")], at(1)));
    assert!(
        !lost(&c, [], OffsetDateTime::now_utc()),
        "inside the grace after binding"
    );
    let mut evicted = live.clone();
    evicted.status = Some(k8s_openapi::api::core::v1::PodStatus {
        phase: Some("Failed".into()),
        ..Default::default()
    });
    assert!(lost(&c, [&evicted], at(1)));
    let s = ended_status(&c, ClaimPhase::Lost);
    assert_eq!(
        (s.phase, s.secret_name.as_deref()),
        (Some(ClaimPhase::Lost), None)
    );
    assert_eq!(s.conditions[0].reason, reason::WORKER_LOST);
    c.status = Some(s);
    assert!(!lost(&c, [], at(1)), "a Lost claim is never judged again");
    assert!(!expired(&c, at(100_000)));
}

#[test]
fn finished_ci_claims_are_collected_after_ten_minutes() {
    let mut c = claim(OWN, "a", "u1", "2026-10-21T12:00:00Z");
    c.status = Some(unbound_status(
        &c,
        ClaimPhase::Failed,
        reason::QUEUE_TIMEOUT,
        "",
        None,
    ));
    let ended = c.status.as_ref().unwrap().conditions[0]
        .last_transition_time
        .0
        .as_second();
    let at = |s: i64| OffsetDateTime::from_unix_timestamp(ended + s).unwrap();
    assert!(!collectable(&c, OWN, at(599)));
    assert!(collectable(&c, OWN, at(600)));
    let mut tenant = c.clone();
    tenant.metadata.namespace = Some("ci".into());
    assert!(
        !collectable(&tenant, OWN, at(6000)),
        "in-cluster claims are their owner's"
    );
    let pending = claim(OWN, "b", "u2", "2026-10-21T12:00:00Z");
    assert!(!collectable(&pending, OWN, at(6000)));
}

#[test]
fn strays_are_pods_whose_claim_is_gone_ended_or_bound_elsewhere() {
    let id = new_alloc_id().unwrap().to_string();
    let mut bound = claim(OWN, &id, "u1", "2026-10-21T12:00:00Z");
    bound.status = Some(bound_status(&bound, &binding(&id), false));
    let mut pending = claim("ci", "p", "u2", "2026-10-21T12:00:00Z");
    pending.status = None;
    let pending_id = alloc_id(&pending, OWN).unwrap().to_string();
    let ended = with_phase(
        claim("ci", "e", "u3", "2026-10-21T12:00:00Z"),
        ClaimPhase::Expired,
    );
    let ended_id = alloc_id(&ended, OWN).unwrap().to_string();
    let claims: Vec<Arc<BuildKitClaim>> =
        [bound, pending, ended].into_iter().map(Arc::new).collect();
    let cref = format!("{OWN}/{id}");
    let pods = vec![
        pod("default-0", Some(&cref), &id),
        pod("default-1", Some(&cref), &id),
        pod("default-2", Some("ci/p"), &pending_id),
        pod("default-3", Some("ci/e"), &ended_id),
        pod("default-4", Some("ci/gone"), "zzzzzzzzzzzzzzzzzzzzzzzzzz"),
        pod("default-5", Some("ci/p"), "yyyyyyyyyyyyyyyyyyyyyyyyyy"),
        pod("default-6", None, "xxxxxxxxxxxxxxxxxxxxxxxxxx"),
        pod(
            "default-7",
            Some("unwatched/q"),
            "wwwwwwwwwwwwwwwwwwwwwwwwww",
        ),
        pod("default-8", Some("ci/gone"), FREE),
    ];
    let got: Vec<&str> = strays(&pods, &claims, &served(), OWN)
        .into_iter()
        .map(|p| p.metadata.name.as_deref().unwrap())
        .collect();
    assert_eq!(
        got,
        ["default-1", "default-3", "default-4", "default-5"],
        "bound elsewhere, ended, claim gone, replaced; kept: the bound pod, a pending bind, \
         pre-v3 pods, unwatched namespaces and free pods"
    );
}

#[test]
fn finished_claims_answer_with_their_api_error() {
    let c = claim(OWN, "a", "u1", "2026-10-21T12:00:00Z");
    let failed = |r: &str| {
        let mut x = c.clone();
        x.status = Some(unbound_status(&c, ClaimPhase::Failed, r, "msg", None));
        api_error(&x).unwrap().0
    };
    assert_eq!(failed(reason::QUEUE_TIMEOUT), ErrorCode::PoolExhausted);
    assert_eq!(failed(reason::POOL_NOT_FOUND), ErrorCode::PoolNotFound);
    assert_eq!(failed(reason::INVALID_REQUEST), ErrorCode::InvalidRequest);
    assert_eq!(failed(reason::SECRET_CONFLICT), ErrorCode::Internal);
    assert_eq!(
        api_error(&with_phase(c.clone(), ClaimPhase::Lost))
            .unwrap()
            .0,
        ErrorCode::PodLost
    );
    assert_eq!(
        api_error(&with_phase(c.clone(), ClaimPhase::Expired))
            .unwrap()
            .0,
        ErrorCode::NotFound
    );
    assert_eq!(api_error(&c), None, "pending");
    assert_eq!(api_error(&with_phase(c, ClaimPhase::Bound)), None);
}

#[test]
fn statuses_and_the_secret() {
    let c = claim("ci", "build", "u1", "2026-10-21T12:00:00Z");
    let b = bound_status(&c, &binding("a1"), true);
    assert_eq!(b.phase, Some(ClaimPhase::Bound));
    assert_eq!(b.secret_name.as_deref(), Some("build-buildkit"));
    assert_eq!(
        (b.pod.as_deref(), b.certificate.as_deref()),
        (Some("default-0"), Some("CERT"))
    );
    assert_eq!(bound_status(&c, &binding("a1"), false).secret_name, None);
    let p = unbound_status(&c, ClaimPhase::Pending, reason::QUEUED, "waiting", Some(4));
    assert_eq!(
        (p.queue_position, p.conditions[0].status.as_str()),
        (Some(4), "False")
    );
    let s = secret(&c, &binding("a1"), "KEY", Some("CA")).unwrap();
    assert_eq!(s.metadata.name.as_deref(), Some("build-buildkit"));
    let keys: Vec<_> = s.data.as_ref().unwrap().keys().cloned().collect();
    assert_eq!(
        keys,
        ["ca.crt", "endpoint", "server-name", "tls.crt", "tls.key"]
    );
    assert!(controlled_by(&s, "u1") && !controlled_by(&s, "u2"));
    let no_ca = secret(&c, &binding("a1"), "KEY", None).unwrap();
    assert!(!no_ca.data.unwrap().contains_key("ca.crt"));
}

/// Pods bound for in-cluster claims before every allocation was a claim have
/// no `P claim`: they are strays once their claim (by uid) is gone, has ended
/// or names another allocation; API callers' pods are left to `DELETE`.
#[test]
fn pre_claim_pods_of_ended_claims_are_strays() {
    let owned = |name: &str, owner: &str, alloc: &str| {
        let mut p = pod(name, None, alloc);
        p.metadata
            .annotations
            .as_mut()
            .unwrap()
            .insert(ANN_OWNER.to_owned(), owner.to_owned());
        p
    };
    let mut live = claim("ci", "live", "u1", "2026-10-21T12:00:00Z");
    live.status = Some(bound_status(&live, &binding("aaaa"), true));
    let ended = with_phase(
        claim("ci", "ended", "u2", "2026-10-21T12:00:00Z"),
        ClaimPhase::Expired,
    );
    let claims: Vec<Arc<BuildKitClaim>> = [live, ended].into_iter().map(Arc::new).collect();
    let pods = vec![
        owned("default-0", "claim:ci/live#u1", "aaaa"),
        owned("default-1", "claim:ci/live#u1", "bbbb"),
        owned("default-2", "claim:ci/ended#u2", "cccc"),
        owned("default-3", "claim:ci/gone#u9", "dddd"),
        owned("default-4", "claim:unwatched/x#u8", "eeee"),
        owned("default-5", "sa:ci/builder", "ffff"),
        owned("default-6", "claim:ci/gone#u9", FREE),
    ];
    let got: Vec<&str> = strays(&pods, &claims, &served(), OWN)
        .into_iter()
        .map(|p| p.metadata.name.as_deref().unwrap())
        .collect();
    assert_eq!(got, ["default-1", "default-2", "default-3"]);
}
