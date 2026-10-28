//! Allocation scenarios on a real API server. Run by `cargo xtask alloc-verify`
//! (which provides a kind cluster through `BK_TEST_KUBECONFIG`); ignored otherwise.
//!
//! The harness stands in for the reconciler. It creates warm pause pods with
//! the labels, annotations and ownerReference the reconciler would render.
//! A draining pod holds the pool's only slot, so the cold path always queues.

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

use buildkit_controller::alloc::{AllocError, AllocService};
use buildkit_controller::auth::Identity;
use buildkit_controller::certs::Certs;
use buildkit_controller::config::{Cli, Config, Settings};
use buildkit_controller::election::LeaderState;
use buildkit_controller::pki::CaBundle;
use buildkit_controller::render::{ANN_ALLOC, ANN_OWNER, PodInputs, worker_pod};
use buildkit_controller::runtime::AppContext;
use buildkit_controller::runtime::hooks::Component;
use buildkit_controller::runtime::metrics::Metrics;
use buildkit_controller::runtime::readiness::Readiness;
use buildkit_controller_api::dto::CreateAllocationRequest;
use buildkit_controller_api::v1alpha1::BuildKitPool;
use clap::Parser;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::{Namespace, Pod, Secret};
use kube::ResourceExt;
use kube::api::{Api, ListParams, Patch, PatchParams, PostParams};
use serde_json::json;
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const NS: &str = "bk-alloc";
const POOL: &str = "p";
const ROUNDS: usize = 20;
const CONCURRENT: usize = 100;
const WARM: usize = 30;

fn sa(name: &str) -> Identity {
    Identity::ServiceAccount {
        namespace: NS.into(),
        name: name.into(),
        uid: "u".into(),
    }
}

fn req(ttl: u64, qt: u64) -> CreateAllocationRequest {
    serde_json::from_value(
        json!({"namespace": NS, "pool": POOL, "ttlSeconds": ttl, "queueTimeoutSeconds": qt}),
    )
    .unwrap()
}

struct H {
    pods: Api<Pod>,
    pool: BuildKitPool,
    fp: String,
    image: String,
    ctx: AppContext,
}

impl H {
    /// Holds the pool's only slot with a draining pod `p-0`, as the reconciler
    /// would while it retires one. The cold path then finds no free slot and
    /// queues, which is what these scenarios exercise.
    async fn hold_slot(&self) {
        let mut pod = worker_pod(
            &self.pool,
            &PodInputs {
                slot: 0,
                internal_ca_fingerprint: self.fp.clone(),
                worker_cert_not_after: "2099-01-01T00:00:00Z".into(),
                nonce: "00000000000000aa".into(),
                claim: None,
            },
        )
        .unwrap();
        pod.annotations_mut()
            .insert(ANN_ALLOC.into(), "draining".into());
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
    }

    /// Warm pause pods named `p-w<tag>-<i>`, waited for in the controller's store.
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
                    claim: None,
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
        let store = self.ctx.pods();
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
        let store = self.ctx.pods();
        wait("released pods gone", Duration::from_mins(2), || {
            names.iter().all(|n| {
                store
                    .get(&kube::runtime::reflector::ObjectRef::new(n).within(NS))
                    .is_none()
            })
        })
        .await;
    }
}

async fn wait(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let end = tokio::time::Instant::now() + limit;
    while !f() {
        assert!(tokio::time::Instant::now() < end, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn setup() -> (H, AllocService, CancellationToken, Arc<Metrics>) {
    let client = harness::client().await;
    let image = std::env::var("ALLOC_VERIFY_PAUSE_IMAGE").unwrap();
    let work = std::path::PathBuf::from(std::env::var("ALLOC_VERIFY_WORK").unwrap());
    Api::<Namespace>::all(client.clone())
        .create(
            &PostParams::default(),
            &serde_json::from_value(json!({"metadata": {"name": NS}})).unwrap(),
        )
        .await
        .unwrap();

    let yaml = format!(
        "watchNamespace: {NS}\nedge:\n  endpoint: tcp://bk.example.com:8444\n  serverName: bk.example.com\n  allocationDomain: alloc.bk.example.com\n"
    );
    let path = work.join("config.yaml");
    std::fs::write(&path, &yaml).unwrap();
    let mut cli = Cli::parse_from(["buildkit-controller", "--config", path.to_str().unwrap()]);
    cli.pod_name = "r1".into();
    cli.pod_namespace = NS.into();
    let config = Config::from_yaml(&yaml).unwrap();

    let ca = CaBundle::generate(OffsetDateTime::now_utc()).unwrap();
    let fp = ca.internal_ca().fingerprint_hex();
    let mut secret = Secret::default();
    secret.metadata.name = Some(config.pki.ca_secret_name.clone());
    secret.data = Some(
        ca.to_secret_data()
            .into_iter()
            .map(|(k, v)| (k, ByteString(v)))
            .collect(),
    );
    Api::<Secret>::namespaced(client.clone(), NS)
        .create(&PostParams::default(), &secret)
        .await
        .unwrap();

    let pools: Api<BuildKitPool> = Api::namespaced(client.clone(), NS);
    let pool: BuildKitPool = serde_json::from_value(json!({
        "apiVersion": "buildkit.smrt-devops.net/v1alpha1", "kind": "BuildKitPool",
        "metadata": {"name": POOL, "namespace": NS},
        "spec": {
            "template": {"spec": {"containers": [{"name": "buildkitd", "image": "moby/buildkit:v0.26.3",
                "resources": {"limits": {"memory": "1Gi", "ephemeral-storage": "1Gi"}}}]}},
            "scaling": {"min": 0, "max": 1},
            "access": {"rules": [{"serviceAccount": {}}]},
            "allocation": {"maxQueueTimeout": "2m"}
        }
    }))
    .unwrap();
    let pool = pools.create(&PostParams::default(), &pool).await.unwrap();
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

    let metrics = Arc::new(Metrics::new());
    let (leader_tx, leader_rx) = watch::channel(LeaderState::Follower);
    std::mem::forget(leader_tx);
    let stop = CancellationToken::new();
    let ctx = AppContext::build(
        client.clone(),
        Settings { cli, config },
        Arc::clone(&metrics),
        Arc::new(Readiness::with_spec_checks()),
        leader_rx,
        &stop,
    );
    let certs = Certs::new();
    let svc = AllocService::new(certs.subscribe());
    {
        let (ctx, stop) = (ctx.clone(), stop.clone());
        tokio::spawn(async move { certs.run(ctx, stop).await });
    }
    {
        let (svc, ctx, stop) = (svc.clone(), ctx.clone(), stop.clone());
        tokio::spawn(async move { svc.run(ctx, stop).await });
    }
    wait("alloc running", Duration::from_mins(1), || svc.is_running()).await;
    wait("pool status in store", Duration::from_secs(30), || {
        ctx.pools().state().iter().any(|p| p.status.is_some())
    })
    .await;
    let h = H {
        pods: Api::namespaced(client, NS),
        pool,
        fp,
        image,
        ctx,
    };
    (h, svc, stop, metrics)
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
    let (h, svc, stop, metrics) = setup().await;
    h.hold_slot().await;

    // 1. Exclusive claims under contention.
    let (mut total_ok, mut total_exhausted) = (0, 0);
    for round in 0..ROUNDS {
        let names = h.warm(&format!("r{round}"), WARM).await;
        let mut set = tokio::task::JoinSet::new();
        for i in 0..CONCURRENT {
            let svc = svc.clone();
            set.spawn(async move {
                svc.allocate(&sa(&format!("c{i}")), &req(600, 0))
                    .await
                    .map(buildkit_controller::alloc::Allocation::commit)
            });
        }
        let mut ok = Vec::new();
        let mut exhausted = 0;
        while let Some(r) = set.join_next().await {
            match r.unwrap() {
                Ok(a) => ok.push(a),
                Err(AllocError::PoolExhausted(_)) => exhausted += 1,
                Err(e) => panic!("round {round}: unexpected {e:?}"),
            }
        }
        let ids: BTreeSet<_> = ok.iter().map(|a| a.id.clone()).collect();
        let live = h.pods.list(&ListParams::default()).await.unwrap();
        let claimed: Vec<&Pod> = live
            .items
            .iter()
            .filter(|p| names.contains(&p.name_any()))
            .filter(|p| p.annotations().get(ANN_ALLOC).is_some_and(|a| a != "free"))
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
        for p in &claimed {
            let id = &p.annotations()[ANN_ALLOC];
            let a = ok.iter().find(|a| &a.id == id).unwrap();
            assert_eq!(p.annotations().get(ANN_OWNER), Some(&a.owner));
        }
        println!(
            "round {round:2}: {CONCURRENT} concurrent, {WARM} warm -> ok {} (distinct pods {}), pool_exhausted {exhausted}",
            ok.len(),
            claimed.len()
        );
        total_ok += ok.len();
        total_exhausted += exhausted;
        let mut rel = tokio::task::JoinSet::new();
        for a in ok {
            let svc = svc.clone();
            rel.spawn(async move {
                svc.release(&sa(&a.owner["sa:bk-alloc/".len()..]), &a.id)
                    .await
            });
        }
        while let Some(r) = rel.join_next().await {
            r.unwrap().unwrap();
        }
        h.gone(&names).await;
    }
    println!(
        "SCENARIO exclusive: {ROUNDS} rounds, ok {total_ok}, pool_exhausted {total_exhausted}, shared pods 0, conflicts {}",
        metric(&metrics, "buildkit_claim_conflicts_total{")
    );

    // 2. A queued allocation succeeds after a release.
    let first = h.warm("q", 1).await;
    let held = svc
        .allocate(&sa("holder"), &req(600, 0))
        .await
        .unwrap()
        .commit();
    let queued = {
        let svc = svc.clone();
        tokio::spawn(async move {
            svc.allocate(&sa("queued"), &req(600, 90))
                .await
                .map(buildkit_controller::alloc::Allocation::commit)
        })
    };
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !queued.is_finished(),
        "must be queued while the pool is exhausted"
    );
    svc.release(&sa("holder"), &held.id).await.unwrap();
    h.gone(&first).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!queued.is_finished(), "no free pod yet");
    let next = h.warm("q2", 1).await; // the reconciler's replacement warm pod
    let got = tokio::time::timeout(Duration::from_secs(30), queued)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let pod = h.pods.get(&next[0]).await.unwrap();
    assert_eq!(pod.annotations()[ANN_ALLOC], got.id);
    println!(
        "SCENARIO queued-after-release: ok, waiter got {} after release of {}",
        next[0], first[0]
    );
    svc.release(&sa("queued"), &got.id).await.unwrap();
    h.gone(&next).await;

    // 3. Queue timeout gives pool_exhausted.
    let t = tokio::time::Instant::now();
    let r = svc.allocate(&sa("late"), &req(600, 3)).await;
    let waited = t.elapsed();
    assert!(
        matches!(r, Err(AllocError::PoolExhausted(_))),
        "{:?}",
        r.err()
    );
    assert!(
        waited >= Duration::from_secs(3) && waited < Duration::from_secs(6),
        "{waited:?}"
    );
    println!(
        "SCENARIO queue-timeout: pool_exhausted after {:.2}s (queueTimeout 3s)",
        waited.as_secs_f64()
    );

    // 4. Cancelled requests leave no orphaned claim.
    let names = h.warm("x", 40).await;
    let abandoned_before = metric(&metrics, "reason=\"abandoned\"");
    let mut finished = 0;
    let (canceller, cancel_req) = (sa("canceller"), req(600, 0));
    for i in 0..40u64 {
        let fut = svc.allocate(&canceller, &cancel_req);
        // Cancel at staggered points; a completed-but-uncommitted Allocation is dropped too.
        if let Ok(r) = tokio::time::timeout(Duration::from_micros(500 * i), fut).await {
            finished += usize::from(r.is_ok());
        }
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    let live = h.pods.list(&ListParams::default()).await.unwrap();
    let orphans: Vec<String> = live
        .items
        .iter()
        .filter(|p| {
            p.annotations()
                .get(ANN_ALLOC)
                .is_some_and(|a| a != "free" && a != "draining")
        })
        .map(ResourceExt::name_any)
        .collect();
    let retired = metric(&metrics, "reason=\"abandoned\"") - abandoned_before;
    let still_free = live
        .items
        .iter()
        .filter(|p| {
            names.contains(&p.name_any())
                && p.annotations().get(ANN_ALLOC).is_some_and(|a| a == "free")
        })
        .count();
    assert!(orphans.is_empty(), "orphaned claims: {orphans:?}");
    assert!(retired > 0, "no cancellation landed after a claim");
    println!(
        "SCENARIO cancelled: 40 cancelled requests, {retired} claimed then retired by the guard ({finished} completed uncommitted), {still_free} pods never claimed, orphaned claims 0"
    );

    // 5. Non-owner release is rejected; 6. the cert.
    let a = svc.allocate(&sa("owner"), &req(900, 0)).await.unwrap();
    let (pod_name, pod_uid) = (a.pod().0.to_owned(), a.pod().1.to_owned());
    let r = a.commit();
    assert_eq!(
        svc.release(&sa("intruder"), &r.id).await,
        Err(AllocError::Forbidden)
    );
    let pod = h.pods.get(&pod_name).await.unwrap();
    assert_eq!(
        pod.annotations()[ANN_ALLOC],
        r.id,
        "non-owner release must not touch the pod"
    );
    println!("SCENARIO non-owner-release: forbidden, pod {pod_name} still allocated");

    let der = pem::parse(&r.client_cert).unwrap();
    let (_, cert) = x509_parser::parse_x509_certificate(der.contents()).unwrap();
    let san = cert.subject_alternative_name().unwrap().unwrap();
    let names: Vec<String> = san
        .value
        .general_names
        .iter()
        .map(ToString::to_string)
        .collect();
    let want = format!("buildkit://{NS}/{POOL}/{pod_uid}/{}", r.id);
    assert_eq!(names, vec![format!("URI({want})")]);
    assert_eq!(pod.metadata.uid.as_deref(), Some(pod_uid.as_str()));
    let nb = cert.validity().not_before.timestamp();
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
    assert_eq!(na - ts(&allocated), 900, "lifetime = TTL");
    // Both are whole seconds, and the cert is issued just after the claim,
    // possibly in the next second.
    assert!(
        (299..=302).contains(&(ts(&allocated) - nb)),
        "backdated 5m: allocated-at {allocated}, notBefore {nb}"
    );
    assert_eq!(
        r.endpoint,
        format!("tcp://{}.alloc.bk.example.com:8444", r.id)
    );
    println!("SCENARIO cert: SAN {want}, notAfter {expires} = allocated-at + 900s");
    svc.release(&sa("owner"), &r.id).await.unwrap();
    stop.cancel();
}
