//! Allocation scenarios on a real API server. Run by `cargo xtask alloc-verify`
//! (which provides a kind cluster through `BK_TEST_KUBECONFIG`); ignored otherwise.
//!
//! Every allocation is a `BuildKitClaim`. The harness runs the allocation
//! component on the host as the leader (front door and allocator), and a second
//! replica as a follower that only answers reads. In namespace `bk-alloc` it
//! stands in for the reconciler: it creates warm pause pods with the labels,
//! annotations and ownerReference the reconciler would render. In namespace
//! `bk-alloc-cold` the real reconciler runs too, for a pool with no warm pod.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

#[path = "support/kind.rs"]
mod harness;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use buildkit_controller::alloc::{AllocError, AllocService, Outcome};
use buildkit_controller::auth::Identity;
use buildkit_controller::certs::Certs;
use buildkit_controller::config::{Cli, Config, Settings};
use buildkit_controller::election::LeaderState;
use buildkit_controller::pki::CaBundle;
use buildkit_controller::reconcile::Reconciler;
use buildkit_controller::render::{ANN_ALLOC, ANN_OWNER, PodInputs, worker_pod};
use buildkit_controller::runtime::AppContext;
use buildkit_controller::runtime::hooks::{Component, LeaderWork};
use buildkit_controller::runtime::leader::supervise;
use buildkit_controller::runtime::metrics::Metrics;
use buildkit_controller::runtime::readiness::Readiness;
use buildkit_controller_api::claim::{BuildKitClaim, ClaimPhase};
use buildkit_controller_api::dto::{AllocationResponse, CreateAllocationRequest};
use buildkit_controller_api::v1alpha1::BuildKitPool;
use clap::Parser;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::{Namespace, Pod, Secret};
use kube::ResourceExt;
use kube::api::{Api, ListParams, Patch, PatchParams, PostParams};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const NS: &str = "bk-alloc";
const COLD_NS: &str = "bk-alloc-cold";
const POOL: &str = "p";
const ROUNDS: usize = 20;
const CONCURRENT: usize = 100;
const WARM: usize = 30;

static KEY: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| rcgen::KeyPair::generate().unwrap().public_key_pem());

fn sa(ns: &str, name: &str) -> Identity {
    Identity::ServiceAccount {
        namespace: ns.into(),
        name: name.into(),
        uid: "u".into(),
    }
}

fn req(ns: &str, ttl: u64, qt: u64) -> CreateAllocationRequest {
    serde_json::from_value(json!({"namespace": ns, "pool": POOL, "ttlSeconds": ttl,
        "queueTimeoutSeconds": qt, "publicKey": *KEY}))
    .unwrap()
}

/// Poll a pending allocation, as a client does, until it is bound or fails.
async fn settle(
    svc: &AllocService,
    who: &Identity,
    mut out: Result<Outcome, AllocError>,
) -> Result<Outcome, AllocError> {
    while let Ok(Outcome::Pending(p)) = &out {
        out = svc.get(who, &p.id, Duration::from_secs(25)).await;
    }
    out
}

fn bound(o: Result<Outcome, AllocError>) -> AllocationResponse {
    match o {
        Ok(Outcome::Bound(r)) => *r,
        other => panic!("not bound: {other:?}"),
    }
}

/// One replica on the host: its context, allocation service and metrics.
struct Replica {
    ctx: AppContext,
    svc: AllocService,
    metrics: Arc<Metrics>,
}

struct H {
    pods: Api<Pod>,
    claims: Api<BuildKitClaim>,
    pool: BuildKitPool,
    fp: String,
    image: String,
    leader: Replica,
    follower: Replica,
}

impl H {
    /// Warm pause pods named `p-w<tag>-<i>`, waited for in the leader's store.
    async fn warm(&self, tag: &str, n: usize) -> Vec<String> {
        let mut names = Vec::new();
        for i in 0..n {
            let mut pod = worker_pod(
                &self.pool,
                &PodInputs {
                    slot: 0,
                    internal_ca_fingerprint: self.fp.clone(),
                    worker_cert_not_after: "2099-01-01T00:00:00Z".into(),
                    nonce: "00000000000000aa".into(),
                },
            )
            .unwrap();
            let name = format!("{POOL}-w{tag}-{i}");
            pod.metadata.name = Some(name.clone());
            pod.spec = Some(
                serde_json::from_value(json!({
                    "terminationGracePeriodSeconds": 0,
                    "containers": [{"name": "pause", "image": self.image, "imagePullPolicy": "Never"}]
                }))
                .unwrap(),
            );
            self.pods
                .create(&PostParams::default(), &pod)
                .await
                .unwrap();
            names.push(name);
        }
        let store = self.leader.ctx.pods();
        wait("warm pods ready in store", Duration::from_mins(2), || {
            names.iter().all(|n| {
                store
                    .get(&kube::runtime::reflector::ObjectRef::new(n).within(NS))
                    .is_some_and(|p| {
                        p.status.as_ref().is_some_and(|s| {
                            s.pod_ip.is_some()
                                && s.conditions
                                    .iter()
                                    .flatten()
                                    .any(|c| c.type_ == "Ready" && c.status == "True")
                        })
                    })
            })
        })
        .await;
        names
    }

    async fn gone(&self, names: &[String]) {
        let store = self.leader.ctx.pods();
        wait("released pods gone", Duration::from_mins(2), || {
            names.iter().all(|n| {
                store
                    .get(&kube::runtime::reflector::ObjectRef::new(n).within(NS))
                    .is_none()
            })
        })
        .await;
    }

    /// Pods of the namespace that carry an allocation.
    async fn allocated(&self) -> Vec<Pod> {
        let live = self.pods.list(&ListParams::default()).await.unwrap();
        live.items
            .into_iter()
            .filter(|p| {
                p.annotations()
                    .get(ANN_ALLOC)
                    .is_some_and(|a| a != "free" && a != "draining")
            })
            .collect()
    }
}

async fn wait(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let end = tokio::time::Instant::now() + limit;
    while !f() {
        assert!(tokio::time::Instant::now() < end, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn namespace(client: &kube::Client, ns: &str) {
    Api::<Namespace>::all(client.clone())
        .create(
            &PostParams::default(),
            &serde_json::from_value(json!({"metadata": {"name": ns}})).unwrap(),
        )
        .await
        .unwrap();
}

/// The CA Secret of the controller in `ns`.
async fn ca_secret(client: &kube::Client, ns: &str, config: &Config) -> CaBundle {
    let ca = CaBundle::generate(OffsetDateTime::now_utc()).unwrap();
    let mut secret = Secret::default();
    secret.metadata.name = Some(config.pki.ca_secret_name.clone());
    secret.data = Some(
        ca.to_secret_data()
            .into_iter()
            .map(|(k, v)| (k, ByteString(v)))
            .collect(),
    );
    Api::<Secret>::namespaced(client.clone(), ns)
        .create(&PostParams::default(), &secret)
        .await
        .unwrap();
    ca
}

fn config(ns: &str) -> String {
    format!(
        "watchNamespaces: [{ns}]\nedge:\n  endpoint: tcp://bk.example.com:8444\n  serverName: bk.example.com\n  allocationDomain: alloc.bk.example.com\n"
    )
}

/// Start a replica named `name` in controller namespace `ns`; the leader also
/// runs the reconciler when `reconcile` is set.
async fn replica(
    client: &kube::Client,
    ns: &str,
    name: &str,
    leader: bool,
    reconcile: bool,
    stop: &CancellationToken,
) -> Replica {
    let work = std::path::PathBuf::from(std::env::var("ALLOC_VERIFY_WORK").unwrap());
    let yaml = config(ns);
    let path = work.join(format!("config-{ns}-{name}.yaml"));
    std::fs::write(&path, &yaml).unwrap();
    let mut cli = Cli::parse_from(["buildkit-controller", "--config", path.to_str().unwrap()]);
    cli.pod_name = name.into();
    cli.pod_namespace = ns.into();
    let config = Config::from_yaml(&yaml).unwrap();
    let metrics = Arc::new(Metrics::new());
    let state = if leader {
        LeaderState::Leader { term: 1 }
    } else {
        LeaderState::Follower
    };
    let (leader_tx, leader_rx) = watch::channel(state);
    std::mem::forget(leader_tx);
    let ctx = AppContext::build(
        client.clone(),
        Settings { cli, config },
        Arc::clone(&metrics),
        Arc::new(Readiness::with_spec_checks()),
        leader_rx,
        stop,
    );
    let certs = Certs::new();
    let svc = AllocService::new(certs.subscribe());
    {
        let (ctx, stop) = (ctx.clone(), stop.clone());
        tokio::spawn(async move { certs.run(ctx, stop).await });
    }
    // The front door on every replica; the allocator acts only while leading.
    let components: [Arc<dyn Component>; 2] = [Arc::new(svc.clone()), Arc::new(svc.allocator())];
    for c in components {
        let (ctx, stop) = (ctx.clone(), stop.clone());
        tokio::spawn(async move { c.run(ctx, stop).await });
    }
    if reconcile {
        let work: Arc<dyn LeaderWork> = Arc::new(Reconciler::new(&ctx));
        let (c, stop) = (ctx.clone(), stop.clone());
        tokio::spawn(async move {
            let synced = c.clone();
            supervise(
                c.leader(),
                stop,
                move || {
                    let s = synced.clone();
                    Box::pin(async move { s.wait_synced().await })
                },
                work,
            )
            .await
        });
    }
    wait("alloc running", Duration::from_mins(1), || svc.is_running()).await;
    Replica { ctx, svc, metrics }
}

fn pool_json(ns: &str, image: &str, max: i32) -> Value {
    json!({
        "apiVersion": "buildkit.smrt-devops.net/v1alpha1", "kind": "BuildKitPool",
        "metadata": {"name": POOL, "namespace": ns},
        "spec": {
            "template": {"spec": {"containers": [{"name": "buildkitd", "image": image,
                "imagePullPolicy": "IfNotPresent",
                "resources": {"limits": {"memory": "1Gi", "ephemeral-storage": "1Gi"}}}]}},
            "scaling": {"min": 0, "max": max},
            "access": {"rules": [{"serviceAccount": {}}]},
            "allocation": {"maxQueueTimeout": "5m"}
        }
    })
}

async fn setup(stop: &CancellationToken) -> H {
    let client = harness::client().await;
    let image = std::env::var("ALLOC_VERIFY_PAUSE_IMAGE").unwrap();
    namespace(&client, NS).await;
    let yaml = config(NS);
    let ca = ca_secret(&client, NS, &Config::from_yaml(&yaml).unwrap()).await;
    let fp = ca.internal_ca().fingerprint_hex();
    let pools: Api<BuildKitPool> = Api::namespaced(client.clone(), NS);
    let pool: BuildKitPool =
        serde_json::from_value(pool_json(NS, "moby/buildkit:v0.26.3", 1)).unwrap();
    let pool = pools.create(&PostParams::default(), &pool).await.unwrap();
    // No reconciler here: the harness marks the pool Ready itself.
    pools
        .patch_status(
            POOL,
            &PatchParams::default(),
            &Patch::Merge(
                json!({"status": {"conditions": [{"type": "Ready", "status": "True",
                "reason": "Ready", "message": "", "lastTransitionTime": "2026-01-01T00:00:00Z"}]}}),
            ),
        )
        .await
        .unwrap();
    let leader = replica(&client, NS, "r1", true, false, stop).await;
    let follower = replica(&client, NS, "r2", false, false, stop).await;
    wait("pool status in store", Duration::from_secs(30), || {
        leader
            .ctx
            .pools()
            .state()
            .iter()
            .any(|p| p.status.is_some())
    })
    .await;
    H {
        pods: Api::namespaced(client.clone(), NS),
        claims: Api::namespaced(client, NS),
        pool,
        fp,
        image,
        leader,
        follower,
    }
}

fn metric(m: &Metrics, needle: &str) -> u64 {
    m.render()
        .lines()
        .filter(|l| !l.starts_with('#') && l.contains(needle))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .map(|v| v as u64)
        .sum()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "needs a kind cluster: cargo xtask alloc-verify"]
async fn alloc_verify() {
    let stop = CancellationToken::new();
    let h = setup(&stop).await;
    let svc = h.leader.svc.clone();

    // 1. Exclusive binds under contention: each bound claim owns exactly one pod.
    let (mut total_ok, mut total_exhausted) = (0, 0);
    for round in 0..ROUNDS {
        let names = h.warm(&format!("r{round}"), WARM).await;
        let mut set = tokio::task::JoinSet::new();
        for i in 0..CONCURRENT {
            let svc = svc.clone();
            set.spawn(async move {
                let who = sa(NS, &format!("c{i}"));
                (who.clone(), svc.allocate(&who, &req(NS, 600, 0)).await)
            });
        }
        let mut ok = Vec::new();
        let mut exhausted = 0;
        while let Some(r) = set.join_next().await {
            match r.unwrap() {
                (who, Ok(Outcome::Bound(a))) => ok.push((who, *a)),
                (_, Err(AllocError::PoolExhausted(_))) => exhausted += 1,
                (_, other) => panic!("round {round}: unexpected {other:?}"),
            }
        }
        let ids: BTreeSet<_> = ok.iter().map(|(_, a)| a.id.clone()).collect();
        let claimed: Vec<Pod> = h
            .allocated()
            .await
            .into_iter()
            .filter(|p| names.contains(&p.name_any()))
            .collect();
        let claimed_ids: BTreeSet<_> = claimed
            .iter()
            .map(|p| p.annotations()[ANN_ALLOC].clone())
            .collect();
        assert_eq!(
            ids.len(),
            ok.len(),
            "round {round}: duplicate allocation ids"
        );
        assert_eq!(
            claimed.len(),
            ok.len(),
            "round {round}: claimed pods != successes"
        );
        assert_eq!(
            claimed_ids, ids,
            "round {round}: each success owns exactly one pod"
        );
        assert_eq!(
            ok.len(),
            WARM.min(CONCURRENT),
            "round {round}: every warm pod is used"
        );
        for p in &claimed {
            let id = &p.annotations()[ANN_ALLOC];
            let (_, a) = ok.iter().find(|(_, a)| &a.id == id).unwrap();
            assert_eq!(p.annotations().get(ANN_OWNER), Some(&a.owner));
            let c = h.claims.get(id).await.unwrap();
            assert_eq!(
                c.status.unwrap().pod.as_deref(),
                Some(p.name_any().as_str())
            );
        }
        println!(
            "round {round:2}: {CONCURRENT} concurrent, {WARM} warm -> bound {} (distinct pods {}), queue timeout {exhausted}",
            ok.len(),
            claimed.len()
        );
        total_ok += ok.len();
        total_exhausted += exhausted;
        let mut rel = tokio::task::JoinSet::new();
        for (who, a) in ok {
            let svc = svc.clone();
            rel.spawn(async move { svc.release(&who, &a.id).await });
        }
        while let Some(r) = rel.join_next().await {
            r.unwrap().unwrap();
        }
        h.gone(&names).await;
    }
    println!(
        "SCENARIO exclusive: {ROUNDS} rounds, bound {total_ok}, queue timeout {total_exhausted}, shared pods 0, conflicts {}",
        metric(&h.leader.metrics, "buildkit_claim_conflicts_total{")
    );

    // 2. A waiting allocation binds after a release, in the same POST.
    let first = h.warm("q", 1).await;
    let holder = sa(NS, "holder");
    let held = bound(svc.allocate(&holder, &req(NS, 600, 0)).await);
    let queued = {
        let svc = svc.clone();
        tokio::spawn(async move {
            let who = sa(NS, "queued");
            let out = svc.allocate(&who, &req(NS, 600, 90)).await;
            settle(&svc, &who, out).await
        })
    };
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !queued.is_finished(),
        "must wait while the pool is exhausted"
    );
    svc.release(&holder, &held.id).await.unwrap();
    h.gone(&first).await;
    assert!(!queued.is_finished(), "no free pod yet");
    let next = h.warm("q2", 1).await; // the reconciler's replacement warm pod
    let got = bound(
        tokio::time::timeout(Duration::from_mins(1), queued)
            .await
            .unwrap()
            .unwrap(),
    );
    let pod = h.pods.get(&next[0]).await.unwrap();
    assert_eq!(pod.annotations()[ANN_ALLOC], got.id);
    println!(
        "SCENARIO queued-after-release: ok, waiter got {} after release of {}",
        next[0], first[0]
    );
    svc.release(&sa(NS, "queued"), &got.id).await.unwrap();
    h.gone(&next).await;

    // 3. A claim still pending at its queue timeout fails: pool_exhausted.
    let t = tokio::time::Instant::now();
    let r = svc.allocate(&sa(NS, "late"), &req(NS, 600, 3)).await;
    let waited = t.elapsed();
    assert!(matches!(r, Err(AllocError::PoolExhausted(_))), "{r:?}");
    assert!(
        waited >= Duration::from_secs(3) && waited < Duration::from_secs(8),
        "{waited:?}"
    );
    println!(
        "SCENARIO queue-timeout: pool_exhausted after {:.2}s (queueTimeout 3s)",
        waited.as_secs_f64()
    );

    // 4. Cancelled requests leave no claim and no allocated pod behind.
    let names = h.warm("x", 40).await;
    let canceller = sa(NS, "canceller");
    let mut finished = 0;
    for i in 0..40u64 {
        let r = req(NS, 600, 0);
        let fut = svc.allocate(&canceller, &r);
        // Completed: the client has the id and releases it.
        if let Ok(Ok(Outcome::Bound(a))) = tokio::time::timeout(Duration::from_millis(i), fut).await
        {
            finished += 1;
            svc.release(&canceller, &a.id).await.unwrap();
        }
    }
    let leftovers = || async {
        let claims = h.claims.list(&ListParams::default()).await.unwrap();
        let mine: Vec<String> = claims
            .items
            .iter()
            .filter(|c| c.spec.owner.as_deref() == Some("sa:bk-alloc/canceller"))
            .filter(|c| c.status.as_ref().and_then(|s| s.phase) != Some(ClaimPhase::Failed))
            .map(ResourceExt::name_any)
            .collect();
        (mine, h.allocated().await.len())
    };
    let end = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (claims, pods) = leftovers().await;
        if claims.is_empty() && pods == 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < end,
            "left behind: claims {claims:?}, allocated pods {pods}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let released = metric(&h.leader.metrics, "reason=\"released\"");
    println!(
        "SCENARIO cancelled: 40 cancelled requests ({finished} completed and released), claims left 0, allocated pods left 0, pods released {released}"
    );
    for n in &names {
        let _ = h.pods.delete(n, &kube::api::DeleteParams::default()).await;
    }
    h.gone(&names).await;

    // 5. Any replica answers; only the owner reads or releases; the cert.
    let names = h.warm("o", 1).await;
    let owner = sa(NS, "owner");
    let r = bound(svc.allocate(&owner, &req(NS, 900, 0)).await);
    let read = bound(h.follower.svc.get(&owner, &r.id, Duration::ZERO).await);
    assert_eq!(read, r, "the follower answers from the claim");
    let intruder = sa(NS, "intruder");
    assert_eq!(
        h.follower.svc.get(&intruder, &r.id, Duration::ZERO).await,
        Err(AllocError::Forbidden)
    );
    assert_eq!(
        svc.release(&intruder, &r.id).await,
        Err(AllocError::Forbidden)
    );
    let claim = h.claims.get(&r.id).await.unwrap();
    let pod_name = claim.status.as_ref().unwrap().pod.clone().unwrap();
    let pod = h.pods.get(&pod_name).await.unwrap();
    assert_eq!(
        pod.annotations()[ANN_ALLOC],
        r.id,
        "non-owner release must not touch the pod"
    );
    println!(
        "SCENARIO owner-only: follower answered, forbidden for others, pod {pod_name} still allocated"
    );
    let pod_uid = pod.metadata.uid.clone().unwrap();
    let der = pem::parse(&r.client_cert).unwrap();
    let (_, cert) = x509_parser::parse_x509_certificate(der.contents()).unwrap();
    let san = cert.subject_alternative_name().unwrap().unwrap();
    let sans: Vec<String> = san
        .value
        .general_names
        .iter()
        .map(ToString::to_string)
        .collect();
    let want = format!("buildkit://{NS}/{POOL}/{pod_uid}/{}", r.id);
    assert_eq!(sans, vec![format!("URI({want})")]);
    let na = cert.validity().not_after.timestamp();
    let expires = pod.annotations()[buildkit_controller::render::ANN_EXPIRES_AT].clone();
    let allocated = pod.annotations()[buildkit_controller::render::ANN_ALLOCATED_AT].clone();
    assert_eq!(r.expires_at, expires);
    let ts = |s: &str| {
        buildkit_controller::alloc::pods::parse_ts(s)
            .unwrap()
            .unix_timestamp()
    };
    assert_eq!(na, ts(&expires), "notAfter = expires-at");
    assert_eq!(
        na - ts(&allocated),
        900,
        "lifetime = TTL, no provisional expiry"
    );
    assert_eq!(
        r.endpoint,
        format!("tcp://{}.alloc.bk.example.com:8444", r.id)
    );
    println!("SCENARIO cert: SAN {want}, notAfter {expires} = allocated-at + 900s");
    svc.release(&owner, &r.id).await.unwrap();
    h.gone(&names).await;
    println!(
        "SCENARIO metrics: allocations ok {}, pool_exhausted {}",
        metric(
            &h.leader.metrics,
            "buildkit_allocations_total{namespace=\"bk-alloc\",pool=\"p\",result=\"ok\"}"
        ),
        metric(
            &h.leader.metrics,
            "buildkit_allocations_total{namespace=\"bk-alloc\",pool=\"p\",result=\"pool_exhausted\"}"
        )
    );

    upgrade(&h).await;
    cold(&stop).await;
    stop.cancel();
}

/// Scenario 6, the cold path: a pool with `min: 0` and no warm pod. The claim waits,
/// the leader's reconciler creates a pod for it (with its own serving Secret,
/// owned by the pod), the claim binds, expires, and the pod and its Secret go.
async fn cold(stop: &CancellationToken) {
    let client = harness::client().await;
    let image = std::env::var("ALLOC_VERIFY_WORKER_IMAGE").unwrap();
    namespace(&client, COLD_NS).await;
    ca_secret(
        &client,
        COLD_NS,
        &Config::from_yaml(&config(COLD_NS)).unwrap(),
    )
    .await;
    let pools: Api<BuildKitPool> = Api::namespaced(client.clone(), COLD_NS);
    let pool: BuildKitPool = serde_json::from_value(pool_json(COLD_NS, &image, 1)).unwrap();
    pools.create(&PostParams::default(), &pool).await.unwrap();
    let r = replica(&client, COLD_NS, "c1", true, true, stop).await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), COLD_NS);
    let claims: Api<BuildKitClaim> = Api::namespaced(client.clone(), COLD_NS);
    let store = r.ctx.pools();
    wait("cold pool Ready", Duration::from_mins(1), || {
        store.state().iter().any(|p| {
            p.status
                .iter()
                .flat_map(|s| &s.conditions)
                .any(|c| c.type_ == "Ready" && c.status == "True")
        })
    })
    .await;
    assert!(
        pods.list(&ListParams::default())
            .await
            .unwrap()
            .items
            .is_empty(),
        "no warm pod"
    );

    let who = sa(COLD_NS, "cold");
    let started = tokio::time::Instant::now();
    let svc = r.svc.clone();
    let post = tokio::spawn({
        let who = who.clone();
        async move { svc.allocate(&who, &req(COLD_NS, 60, 240)).await }
    });
    // The claim waits in the queue, and the pool counts it.
    let claim_store = r.ctx.claims();
    wait("claim pending", Duration::from_secs(20), || {
        claim_store.state().iter().any(|c| {
            c.status.as_ref().is_some_and(|s| {
                s.phase == Some(ClaimPhase::Pending) && s.queue_position == Some(1)
            })
        })
    })
    .await;
    let waiting = pools
        .get(POOL)
        .await
        .unwrap()
        .status
        .and_then(|s| s.waiting);
    println!("SCENARIO cold: claim Pending at place 1, pool waiting {waiting:?}");
    let a = bound(settle(&r.svc, &who, post.await.unwrap()).await);
    let took = started.elapsed();
    let c = claims.get(&a.id).await.unwrap();
    let st = c.status.unwrap();
    let pod = pods.get(st.pod.as_deref().unwrap()).await.unwrap();
    assert_eq!(pod.annotations()[ANN_ALLOC], a.id);
    let pod_uid = pod.metadata.uid.clone().unwrap();
    let nonce = pod.annotations()[buildkit_controller::render::ANN_WORKER_NONCE].clone();
    let secret_name = format!("{}-{nonce}", pod.name_any());
    let secret = Api::<Secret>::namespaced(client.clone(), COLD_NS)
        .get(&secret_name)
        .await
        .unwrap();
    let owner = &secret.metadata.owner_references.unwrap()[0];
    assert_eq!(
        (owner.kind.as_str(), owner.uid.as_str()),
        ("Pod", pod_uid.as_str())
    );
    println!(
        "SCENARIO cold: bound to {} in {:.1}s; its Secret {secret_name} is owned by the pod",
        pod.name_any(),
        took.as_secs_f64()
    );
    // At expiry the claim ends, the pod goes, and its Secret is collected.
    let deadline = Duration::from_mins(3);
    let secrets = Api::<Secret>::namespaced(client.clone(), COLD_NS);
    let end = tokio::time::Instant::now() + deadline;
    loop {
        let phase = claims
            .get(&a.id)
            .await
            .unwrap()
            .status
            .and_then(|s| s.phase);
        let pod_gone = pods.get_opt(&pod.name_any()).await.unwrap().is_none();
        let secret_gone = secrets.get_opt(&secret_name).await.unwrap().is_none();
        if phase == Some(ClaimPhase::Expired) && pod_gone && secret_gone {
            break;
        }
        assert!(
            tokio::time::Instant::now() < end,
            "after expiry: phase {phase:?}, pod gone {pod_gone}, secret gone {secret_gone}"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    println!(
        "SCENARIO cold: claim Expired at {}; pod and Secret collected",
        a.expires_at
    );
}

/// Scenario 7, an upgrade: pods allocated before every allocation was a claim
/// carry no `P claim`. An API caller's is released by its owner's DELETE, and
/// an in-cluster claim's whose claim is gone is retired by the leader's sweep.
async fn upgrade(h: &H) {
    let names = h.warm("u", 2).await;
    let (api_id, claim_id) = (
        "aaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        "bbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
    );
    for (name, id, owner) in [
        (&names[0], &api_id, "sa:bk-alloc/legacy".to_owned()),
        (
            &names[1],
            &claim_id,
            format!("claim:{NS}/old#00000000-0000-4000-8000-00000000dead"),
        ),
    ] {
        let patch = json!({"metadata": {
            "labels": {"buildkit.smrt-devops.net/state": "allocated"},
            "annotations": {ANN_ALLOC: id, ANN_OWNER: owner,
                "buildkit.smrt-devops.net/expires-at": "2099-01-01T00:00:00Z"}}});
        h.pods
            .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
            .unwrap();
    }
    let svc = &h.leader.svc;
    assert_eq!(
        svc.release(&sa(NS, "intruder"), &api_id).await,
        Err(AllocError::Forbidden)
    );
    // The store may lag the patch by a moment.
    let end = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match svc.release(&sa(NS, "legacy"), &api_id).await {
            Ok(()) => break,
            Err(AllocError::NotFound) if tokio::time::Instant::now() < end => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            r => panic!("release of a pre-claim allocation: {r:?}"),
        }
    }
    h.gone(&names).await;
    println!(
        "SCENARIO upgrade: a pre-claim allocation released by its owner's DELETE (others 403); a pre-claim pod of a deleted in-cluster claim retired by the sweep"
    );
}
