//! Idle delivered allocations: the edge's `edge-seen-at` stamps and the
//! leader's release of a claim nobody uses.

use super::*;
use crate::alloc::EdgeActivity;
use crate::alloc::leader::ALLOCATION_IDLE;

fn secs(n: i64) -> time::Duration {
    time::Duration::seconds(n)
}

/// A claim of `who` bound and delivered (its client polled and got the
/// answer): its id and pod.
async fn delivered(e: &Env, who: &str, req: &CreateAllocationRequest) -> (String, String) {
    let Outcome::Pending(p) = e.front.allocate(&sa(who), req).await.unwrap() else {
        panic!("bound without a pod")
    };
    e.binder.pass(&key()).await;
    let got = e.front.get(&sa(who), &p.id, Duration::ZERO).await.unwrap();
    assert!(matches!(got, Outcome::Bound(_)), "{got:?}");
    let pod = e.fake.status(OWN, &p.id).pod.unwrap();
    (p.id, pod)
}

fn activity(e: &Env) -> EdgeActivity {
    EdgeActivity::new(
        Arc::clone(&e.fake) as Arc<dyn Backend>,
        e.fake.claims.lock().unwrap().as_reader(),
        OWN.into(),
    )
}

/// `edge-seen-at` writes to claim `id` since the calls were cleared.
fn stamps(e: &Env, id: &str) -> usize {
    let call = format!("annotate_claim {id}");
    e.fake.calls().iter().filter(|c| **c == call).count()
}

fn edge_seen(e: &Env, id: &str) -> Option<OffsetDateTime> {
    let c = e.fake.claim(OWN, id).unwrap();
    parse_ts(
        c.metadata
            .annotations
            .as_ref()?
            .get(claims::ANN_EDGE_SEEN_AT)?,
    )
}

fn none() -> std::iter::Empty<(String, OffsetDateTime)> {
    std::iter::empty()
}

/// A client that got its allocation and left without a `DELETE` or any edge
/// connection: the claim is kept for the pool's `scaling.idleTimeout` (10m)
/// after delivery, then ends `Expired/Idle`, its worker is retired as
/// `allocation_idle`, and the API answers `not_found`.
#[tokio::test(start_paused = true)]
async fn an_idle_delivered_claim_is_released_after_the_idle_timeout() {
    let e = env();
    e.warm(0);
    let (id, pod) = delivered(&e, "alice", &post_req(&KEY)).await;
    let at = OffsetDateTime::now_utc();
    tidy_at(&e, at + secs(590)).await;
    assert_eq!(
        phase(&e.fake.status(OWN, &id)),
        ClaimPhase::Bound,
        "recent delivery: kept"
    );
    tidy_at(&e, at + secs(601)).await;
    let s = e.fake.status(OWN, &id);
    assert_eq!(
        (phase(&s), why(&s).as_str()),
        (ClaimPhase::Expired, reason::IDLE)
    );
    tidy_at(&e, at + secs(602)).await;
    let calls = e.fake.calls();
    assert!(
        calls.contains(&format!("patch {pod} draining")),
        "{calls:?}"
    );
    assert_eq!(e.metrics.released(POOL_NS, POOL, ALLOCATION_IDLE), 1);
    assert_eq!(e.metrics.released(POOL_NS, POOL, "released"), 0);
    assert_eq!(
        e.front.get(&sa("alice"), &id, Duration::ZERO).await,
        Err(AllocError::NotFound)
    );
}

/// A build holding one edge connection for two hours, with a 10 minute
/// `idleTimeout`, keeps its claim: the replica stamps it every 15 s while the
/// connection is open. Once it closes, the close is stamped, and the claim
/// goes an `idleTimeout` after that.
#[tokio::test(start_paused = true)]
async fn an_open_connection_keeps_the_claim_through_a_long_build() {
    let mut e = env();
    let mut pool = e.pool.clone();
    pool.spec.allocation.max_ttl = buildkit_controller_api::v1alpha1::Duration::new("3h");
    e.pools.apply_watcher_event(&watcher::Event::Apply(pool));
    e.warm(0);
    let mut req = post_req(&KEY);
    req.ttl_seconds = Some(3 * 3600);
    let (id, _) = delivered(&e, "alice", &req).await;
    let act = activity(&e);
    e.fake.w.lock().unwrap().calls.clear();
    let live = HashSet::from([id.clone()]);
    let start = OffsetDateTime::now_utc();
    for s in (0..=7200).step_by(5) {
        let now = start + secs(s);
        act.stamp(&live, none(), now).await;
        if s % 60 == 0 {
            tidy_at(&e, now).await;
            assert_eq!(
                phase(&e.fake.status(OWN, &id)),
                ClaimPhase::Bound,
                "at {s} s"
            );
        }
    }
    // Throttled like seen-at: one write per 15 s, not one per round.
    let n = stamps(&e, &id);
    assert!((7200 / 15..=7200 / 15 + 2).contains(&n), "{n} stamps");
    // The connection closes at 7203 s, 3 s after the last stamp. The
    // throttle holds the close back until the stamp is 15 s old.
    let closed = start + secs(7203);
    act.stamp(&HashSet::new(), [(id.clone(), closed)], start + secs(7205))
        .await;
    assert_eq!(stamps(&e, &id), n, "throttled");
    for s in [7210, 7215, 7220, 7225] {
        act.stamp(&HashSet::new(), none(), start + secs(s)).await;
    }
    assert_eq!(stamps(&e, &id), n + 1, "the close is stamped once");
    let last = edge_seen(&e, &id).unwrap();
    assert!(last >= closed, "{last} < {closed}");
    tidy_at(&e, last + secs(590)).await;
    assert_eq!(phase(&e.fake.status(OWN, &id)), ClaimPhase::Bound);
    tidy_at(&e, last + secs(600)).await;
    assert_eq!(why(&e.fake.status(OWN, &id)).as_str(), reason::IDLE);
}

/// Stamps follow the seen-at throttle: the first at once, then at most every
/// 15 s. Claims that are not delivered API claims are never stamped.
#[tokio::test(start_paused = true)]
async fn edge_stamps_are_throttled_and_only_for_delivered_claims() {
    let e = env();
    e.warm(0);
    e.warm(1);
    let (id, _) = delivered(&e, "alice", &post_req(&KEY)).await;
    // Bound but never delivered: its client never got the certificate.
    let Outcome::Pending(quiet) = e
        .front
        .allocate(&sa("bob"), &post_req(&fresh_key()))
        .await
        .unwrap()
    else {
        panic!("bound without a pod")
    };
    e.binder.pass(&key()).await;
    assert_eq!(phase(&e.fake.status(OWN, &quiet.id)), ClaimPhase::Bound);
    let act = activity(&e);
    e.fake.w.lock().unwrap().calls.clear();
    let live = HashSet::from([
        id.clone(),
        quiet.id.clone(),
        "nosuchclaim0000000000000".into(),
    ]);
    let t0 = OffsetDateTime::now_utc();
    act.stamp(&live, none(), t0).await;
    assert_eq!(stamps(&e, &id), 1);
    for s in [5, 10, 14] {
        act.stamp(&live, none(), t0 + secs(s)).await;
    }
    assert_eq!(stamps(&e, &id), 1, "within 15 s of the last stamp");
    act.stamp(&live, none(), t0 + secs(15)).await;
    assert_eq!(stamps(&e, &id), 2);
    assert_eq!(stamps(&e, &quiet.id), 0, "undelivered: never stamped");
    // A failed write is retried next round.
    e.fake
        .w
        .lock()
        .unwrap()
        .annotate_out
        .push_back(Write::Unknown);
    act.stamp(
        &HashSet::new(),
        [(id.clone(), t0 + secs(31))],
        t0 + secs(31),
    )
    .await;
    assert_eq!(stamps(&e, &id), 3);
    assert!(
        edge_seen(&e, &id).unwrap() < t0 + secs(31),
        "the write failed"
    );
    act.stamp(&HashSet::new(), none(), t0 + secs(36)).await;
    assert_eq!(stamps(&e, &id), 4);
    assert_eq!(
        edge_seen(&e, &id).map(OffsetDateTime::unix_timestamp),
        Some((t0 + secs(36)).unix_timestamp())
    );
    act.stamp(&HashSet::new(), none(), t0 + secs(60)).await;
    assert_eq!(stamps(&e, &id), 4, "the close is recorded: no more writes");
}

/// The idle rule leaves undelivered claims to the abandon rule and in-cluster
/// claims to their TTL: a bound claim with no delivery is never ended as idle.
#[tokio::test(start_paused = true)]
async fn the_idle_rule_skips_undelivered_claims() {
    let e = env();
    e.warm(0);
    let ci = e.ci_claim(0, "sa:x/pipeline");
    e.binder.pass(&key()).await;
    assert_eq!(phase(&e.fake.status(OWN, &ci)), ClaimPhase::Bound);
    let before = e.fake.calls().len();
    tidy_at(&e, OffsetDateTime::now_utc() + secs(1800)).await;
    let s = e.fake.status(OWN, &ci);
    assert_eq!(
        (phase(&s), why(&s).as_str()),
        (ClaimPhase::Bound, reason::ALLOCATED)
    );
    let calls = e.fake.calls();
    assert!(
        !calls[before..].iter().any(|c| c.starts_with("status ")),
        "{calls:?}"
    );
}

/// The leader decides from its store. A stamp written since moved the
/// resourceVersion, so the status write is refused and the claim stays.
#[tokio::test(start_paused = true)]
async fn an_idle_release_decided_on_a_stale_store_is_refused() {
    let e = env();
    let pod = e.warm(0);
    let (id, _) = delivered(&e, "alice", &post_req(&KEY)).await;
    let stale = e.fake.claims.lock().unwrap().as_reader().state();
    let late = OffsetDateTime::now_utc() + secs(601);
    activity(&e)
        .stamp(&HashSet::from([id.clone()]), none(), late)
        .await;
    let pods = e.fake.pods.lock().unwrap().as_reader().state();
    crate::alloc::leader::tidy_with(&e.binder, &stale, &pods, late).await;
    assert_eq!(phase(&e.fake.status(OWN, &id)), ClaimPhase::Bound);
    tidy_at(&e, late + secs(1)).await;
    assert_eq!(
        phase(&e.fake.status(OWN, &id)),
        ClaimPhase::Bound,
        "the stamp is recent"
    );
    assert!(!e.fake.calls().contains(&format!("patch {pod} draining")));
}
