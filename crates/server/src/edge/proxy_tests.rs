#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::runtime::{reflector, watcher};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;

use super::super::handler::Refusal;
use super::super::metrics::{Direction, EdgeMetrics, Revocation};
use super::super::pods::{PodView, revocation};
use super::super::revoke::Registry;
use super::{End, pump};
use crate::pki::AllocationId;
use crate::runtime::metrics::Metrics;
use crate::runtime::stores::{PodEvent, PodSubscription, StoreFreshness};
use buildkit_controller_api::v1alpha1::BuildKitPool;

const UID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
const ALLOC: &str = "abcdefghijklmnopqrstuvwxyz";
const POOL_UID: &str = "7d9f3c1e-0000-4000-8000-000000000001";
const NONCE: &str = "0123456789abcdef";

fn id() -> AllocationId {
    format!("buildkit://team-a/pool-1/{UID}/{ALLOC}")
        .parse()
        .unwrap()
}

fn future(secs: i64) -> String {
    let t = OffsetDateTime::now_utc() + time::Duration::seconds(secs);
    t.replace_nanosecond(0)
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

/// A Running worker pod for [`id`]; `edit` patches its JSON first.
fn pod(edit: impl FnOnce(&mut Value)) -> Pod {
    let mut v = json!({
        "metadata": {
            "name": "pool-1-0", "namespace": "team-a", "uid": UID,
            "labels": {
                "app.kubernetes.io/managed-by": "buildkit-controller",
                "buildkit.smrt-devops.net/pool": "pool-1",
            },
            "annotations": {
                "buildkit.smrt-devops.net/alloc": ALLOC,
                "buildkit.smrt-devops.net/expires-at": future(3600),
                "buildkit.smrt-devops.net/worker-nonce": NONCE,
            },
            "ownerReferences": [{
                "apiVersion": "buildkit.smrt-devops.net/v1alpha1", "kind": "BuildKitPool",
                "name": "pool-1", "uid": POOL_UID, "controller": true,
            }],
        },
        "status": { "phase": "Running", "podIP": "10.0.0.7" },
    });
    edit(&mut v);
    serde_json::from_value(v).unwrap()
}

fn view(pods: Vec<Pod>) -> PodView {
    view_with(pods, StoreFreshness::fixed(true))
}

fn view_with(pods: Vec<Pod>, fresh: StoreFreshness) -> PodView {
    let (reader, mut writer) = reflector::store::<Pod>();
    for p in pods {
        writer.apply_watcher_event(&watcher::Event::Apply(p));
    }
    let (pools, mut pw) = reflector::store::<BuildKitPool>();
    let mut pool = BuildKitPool::new("pool-1", serde_json::from_value(json!({})).unwrap());
    pool.metadata.namespace = Some("team-a".into());
    pool.metadata.uid = Some(POOL_UID.into());
    pw.apply_watcher_event(&watcher::Event::Apply(pool));
    PodView {
        pods: reader,
        pools,
        fresh,
    }
}

fn check(edit: impl FnOnce(&mut Value)) -> Result<std::net::IpAddr, Refusal> {
    view(vec![pod(edit)])
        .resolve(&id(), OffsetDateTime::now_utc())
        .map(|t| t.ip)
}

#[test]
fn admits_a_running_claimed_pod() {
    assert_eq!(check(|_| {}).unwrap().to_string(), "10.0.0.7");
}

#[test]
fn resolves_the_pod_incarnation_the_upstream_cert_must_name() {
    let t = view(vec![pod(|_| {})])
        .resolve(&id(), OffsetDateTime::now_utc())
        .unwrap();
    assert_eq!(
        t.worker.dns_name(),
        format!("{NONCE}.pool-1-0.pool-1.team-a.worker.buildkit.internal")
    );
}

#[test]
fn refuses_a_pod_without_a_well_formed_nonce() {
    let ann = |n: Value| {
        move |v: &mut Value| {
            let anns = v["metadata"]["annotations"].as_object_mut().unwrap();
            match n {
                Value::Null => drop(anns.remove("buildkit.smrt-devops.net/worker-nonce")),
                n => drop(anns.insert("buildkit.smrt-devops.net/worker-nonce".into(), n)),
            }
        }
    };
    for bad in [
        Value::Null,
        json!(""),
        json!("0123456789ABCDEF"),
        json!("a.b"),
        json!("0123456789abcde"),
    ] {
        assert_eq!(check(ann(bad.clone())), Err(Refusal::PodNotReady), "{bad}");
    }
}

#[test]
fn check3_pod_not_found() {
    assert_eq!(
        view(vec![]).resolve(&id(), OffsetDateTime::now_utc()),
        Err(Refusal::PodNotFound)
    );
    for edit in [
        (|v: &mut Value| v["metadata"]["namespace"] = json!("team-b")) as fn(&mut Value),
        |v| v["metadata"]["labels"]["buildkit.smrt-devops.net/pool"] = json!("pool-2"),
        |v| v["metadata"]["labels"]["app.kubernetes.io/managed-by"] = json!("someone"),
        |v| v["metadata"]["ownerReferences"][0]["uid"] = json!("other-uid"),
        |v| v["metadata"]["ownerReferences"][0]["controller"] = json!(false),
    ] {
        assert_eq!(check(edit), Err(Refusal::PodNotFound));
    }
}

#[test]
fn a_stale_store_admits_nothing() {
    let v = view_with(vec![pod(|_| {})], StoreFreshness::fixed(false));
    assert_eq!(
        v.resolve(&id(), OffsetDateTime::now_utc()),
        Err(Refusal::StoreStale)
    );
    assert_eq!(Refusal::StoreStale.result_label(), "store_stale");
}

#[test]
fn check6_refuses_an_ip_another_running_pod_holds() {
    let successor = pod(|v| {
        v["metadata"]["name"] = json!("pool-1-def");
        v["metadata"]["uid"] = json!("1f8fad5b-d9cb-469f-a165-70867728950e");
        v["metadata"]["annotations"]["buildkit.smrt-devops.net/alloc"] =
            json!("bbbbbbbbbbbbbbbbbbbbbbbbbb");
    });
    let v = view(vec![pod(|_| {}), successor.clone()]);
    assert_eq!(
        v.resolve(&id(), OffsetDateTime::now_utc()),
        Err(Refusal::PodNotReady)
    );
    // A finished pod that still reports the IP does not hold it.
    let mut done = successor.clone();
    done.status.as_mut().unwrap().phase = Some("Succeeded".into());
    let v = view(vec![pod(|_| {}), done]);
    assert!(v.resolve(&id(), OffsetDateTime::now_utc()).is_ok());
    // Nor does a terminating pod that still reports it.
    let mut leaving = successor;
    leaving.metadata.deletion_timestamp = Some(
        k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(jiff::Timestamp::now()),
    );
    let v = view(vec![pod(|_| {}), leaving]);
    assert!(v.resolve(&id(), OffsetDateTime::now_utc()).is_ok());
}

#[test]
fn check4_alloc_mismatch() {
    for alloc in ["free", "draining", "bbbbbbbbbbbbbbbbbbbbbbbbbb"] {
        let r = check(|v| {
            v["metadata"]["annotations"]["buildkit.smrt-devops.net/alloc"] = json!(alloc);
        });
        assert_eq!(r, Err(Refusal::AllocMismatch), "{alloc}");
    }
}

#[test]
fn check5_expired_or_deleting() {
    let past = check(|v| {
        v["metadata"]["annotations"]["buildkit.smrt-devops.net/expires-at"] = json!(future(-5));
    });
    assert_eq!(past, Err(Refusal::Expired));
    let garbage = check(|v| {
        v["metadata"]["annotations"]["buildkit.smrt-devops.net/expires-at"] = json!("soon");
    });
    assert_eq!(garbage, Err(Refusal::Expired));
    let deleting = check(|v| v["metadata"]["deletionTimestamp"] = json!(future(0)));
    assert_eq!(deleting, Err(Refusal::Expired));
}

#[test]
fn check6_pod_not_ready() {
    assert_eq!(
        check(|v| v["status"]["phase"] = json!("Pending")),
        Err(Refusal::PodNotReady)
    );
    assert_eq!(
        check(|v| v["status"]["podIP"] = Value::Null),
        Err(Refusal::PodNotReady)
    );
}

#[test]
fn revocation_rules() {
    let alloc = id().alloc_id;
    assert_eq!(revocation(None, &alloc), Some(Revocation::PodGone));
    assert_eq!(revocation(Some(&pod(|_| {})), &alloc), None);
    let deleting = pod(|v| v["metadata"]["deletionTimestamp"] = json!(future(0)));
    assert_eq!(
        revocation(Some(&deleting), &alloc),
        Some(Revocation::PodGone)
    );
    for a in ["draining", "free", "bbbbbbbbbbbbbbbbbbbbbbbbbb"] {
        let p = pod(|v| v["metadata"]["annotations"]["buildkit.smrt-devops.net/alloc"] = json!(a));
        assert_eq!(
            revocation(Some(&p), &alloc),
            Some(Revocation::Released),
            "{a}"
        );
    }
}

/// Registry fed from a broadcast like the runtime's root watch.
async fn revoked_by(ev: PodEvent, store: Vec<Pod>) -> Option<Revocation> {
    let registry = Arc::new(Registry::default());
    let (reg, rx) = registry.register(&id()).unwrap();
    let (tx, sub) = broadcast::channel(8);
    let stop = CancellationToken::new();
    let r = Arc::clone(&registry);
    let task = tokio::spawn({
        let (view, stop) = (view(store), stop.clone());
        async move { r.follow(view, PodSubscription::new(sub), stop).await }
    });
    tx.send(ev).unwrap();
    let got = tokio::time::timeout(Duration::from_secs(1), rx)
        .await
        .ok()
        .and_then(Result::ok);
    stop.cancel();
    task.await.unwrap();
    drop(reg);
    assert_eq!(registry.len(), 0);
    got
}

#[tokio::test]
async fn pod_events_revoke_within_a_second() {
    let gone = PodEvent::Delete(Arc::new(pod(|_| {})));
    assert_eq!(revoked_by(gone, vec![]).await, Some(Revocation::PodGone));
    for a in ["draining", "bbbbbbbbbbbbbbbbbbbbbbbbbb"] {
        let p = pod(|v| v["metadata"]["annotations"]["buildkit.smrt-devops.net/alloc"] = json!(a));
        let ev = PodEvent::Apply(Arc::new(p));
        assert_eq!(
            revoked_by(ev, vec![]).await,
            Some(Revocation::Released),
            "{a}"
        );
    }
    // An unrelated update leaves the connection alone.
    let same = PodEvent::Apply(Arc::new(pod(|_| {})));
    assert_eq!(revoked_by(same, vec![]).await, None);
    // Resync rescans the store, and a pod gone from it counts as deleted.
    assert_eq!(
        revoked_by(PodEvent::Resync, vec![]).await,
        Some(Revocation::PodGone)
    );
    assert_eq!(revoked_by(PodEvent::Resync, vec![pod(|_| {})]).await, None);
}

fn metrics() -> EdgeMetrics {
    EdgeMetrics::register(&Metrics::new())
}

#[tokio::test(start_paused = true)]
async fn cert_not_after_closes_at_the_deadline() {
    let (mut client, _c) = tokio::io::duplex(1024);
    let (mut up, _u) = tokio::io::duplex(1024);
    let (_tx, rx) = oneshot::channel();
    let m = metrics();
    let start = tokio::time::Instant::now();
    let end = pump(
        &mut client,
        &mut up,
        &m,
        Some(start + Duration::from_secs(90)),
        rx,
    )
    .await;
    assert!(matches!(end, End::Revoked(Revocation::Expired)));
    assert_eq!(start.elapsed(), Duration::from_secs(90));
}

// The deadline is fixed at accept, so a slow upstream dial must not push the
// cut past the cert's notAfter.
#[tokio::test(start_paused = true)]
async fn slow_dial_does_not_stretch_the_not_after_deadline() {
    let (mut client, _c) = tokio::io::duplex(1024);
    let (mut up, _u) = tokio::io::duplex(1024);
    let (_tx, rx) = oneshot::channel();
    let m = metrics();
    let accepted = tokio::time::Instant::now();
    let not_after = accepted + Duration::from_secs(90);
    tokio::time::sleep(Duration::from_mins(1)).await; // the dial
    let end = pump(&mut client, &mut up, &m, Some(not_after), rx).await;
    assert!(matches!(end, End::Revoked(Revocation::Expired)));
    assert_eq!(accepted.elapsed(), Duration::from_secs(90));
}

#[tokio::test(start_paused = true)]
async fn quiet_sessions_have_no_idle_timeout() {
    let (mut client, _c) = tokio::io::duplex(1024);
    let (mut up, _u) = tokio::io::duplex(1024);
    let (_tx, rx) = oneshot::channel();
    let m = metrics();
    let quiet = pump(&mut client, &mut up, &m, None, rx);
    assert!(
        tokio::time::timeout(Duration::from_hours(24), quiet)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn revocation_cuts_a_live_pump_and_bytes_are_counted() {
    let (mut client, mut c) = tokio::io::duplex(1 << 16);
    let (mut up, mut u) = tokio::io::duplex(1 << 16);
    let (tx, rx) = oneshot::channel();
    let m = metrics();
    let task = tokio::spawn(async move { pump(&mut client, &mut up, &m, None, rx).await });
    c.write_all(&[7u8; 1000]).await.unwrap();
    let mut buf = vec![0u8; 1000];
    u.read_exact(&mut buf).await.unwrap();
    u.write_all(&[9u8; 300]).await.unwrap();
    c.read_exact(&mut buf[..300]).await.unwrap();
    tx.send(Revocation::Released).unwrap();
    let end = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(end, End::Revoked(Revocation::Released)));
}

#[tokio::test]
async fn bytes_are_counted_per_direction_and_half_close_is_kept() {
    let (mut client, mut c) = tokio::io::duplex(1 << 16);
    let (mut up, mut u) = tokio::io::duplex(1 << 16);
    let (_tx, rx) = oneshot::channel();
    let m = metrics();
    let mc = m.clone();
    let task = tokio::spawn(async move { pump(&mut client, &mut up, &mc, None, rx).await });
    c.write_all(&[1u8; 5000]).await.unwrap();
    c.shutdown().await.unwrap();
    let mut got = Vec::new();
    u.read_to_end(&mut got).await.unwrap();
    assert_eq!(got.len(), 5000, "client EOF reaches the worker");
    u.write_all(&[2u8; 700]).await.unwrap();
    u.shutdown().await.unwrap();
    let mut back = Vec::new();
    c.read_to_end(&mut back).await.unwrap();
    assert_eq!(
        back.len(),
        700,
        "worker still answers after the client half-closed"
    );
    let end = task.await.unwrap();
    assert!(matches!(end, End::Closed(Ok((5000, 700)))));
    assert_eq!(m.bytes(Direction::ClientToWorker).get(), 5000);
    assert_eq!(m.bytes(Direction::WorkerToClient).get(), 700);
}

#[test]
fn each_allocation_is_capped_and_a_closed_connection_frees_its_slot() {
    let registry = Arc::new(Registry::new(2));
    let a = registry.register(&id()).unwrap();
    let _b = registry.register(&id()).unwrap();
    assert!(registry.register(&id()).is_none(), "third on one pod");
    let other: AllocationId =
        format!("buildkit://team-a/pool-1/1f8fad5b-d9cb-469f-a165-70867728950e/{ALLOC}")
            .parse()
            .unwrap();
    assert!(
        registry.register(&other).is_some(),
        "another pod has its own"
    );
    drop(a);
    assert!(registry.register(&id()).is_some());
    let unlimited = Arc::new(Registry::new(0));
    let held: Vec<_> = (0..100)
        .map(|_| unlimited.register(&id()).unwrap())
        .collect();
    assert_eq!(held.len(), 100);
}

/// The proxy's back half over `pods`, with no TLS generation loaded.
fn inner(pods: Vec<Pod>, max_per_pod: usize) -> super::Inner {
    super::Inner {
        view: view(pods),
        tls: tokio::sync::watch::channel(None).1,
        metrics: metrics(),
        options: super::ProxyOptions {
            upstream_connect_timeout: Duration::from_secs(1),
            upstream_port: 1,
        },
        live: Arc::new(Registry::new(max_per_pod)),
    }
}

#[tokio::test]
async fn an_allocation_at_its_cap_is_refused_and_counted() {
    let p = inner(vec![pod(|_| {})], 1);
    let (_held, _rx) = p.live.register(&id()).unwrap();
    let (client, mut c) = tokio::io::duplex(1024);
    p.forward(client, &id(), None).await;
    assert_eq!(p.metrics.outcome_count(Some(&id()), "allocation_limit"), 1);
    assert_eq!(p.metrics.outcome_count(Some(&id()), "upstream_error"), 0);
    let mut b = Vec::new();
    c.read_to_end(&mut b).await.unwrap();
    assert!(b.is_empty(), "closed without a byte");
    assert_eq!(p.live.len(), 1);
}

#[tokio::test]
async fn without_an_edge_client_cert_the_connection_is_an_upstream_error() {
    let p = inner(vec![pod(|_| {})], 64);
    let (client, _c) = tokio::io::duplex(1024);
    p.forward(client, &id(), None).await;
    assert_eq!(p.metrics.outcome_count(Some(&id()), "upstream_error"), 1);
    assert_eq!(p.metrics.outcome_count(Some(&id()), "ok"), 0);
    assert_eq!(p.live.len(), 0, "unregistered once closed");
}

#[tokio::test(start_paused = true)]
async fn a_revocation_sent_before_the_pump_cuts_at_once() {
    let (mut client, _c) = tokio::io::duplex(1024);
    let (mut up, _u) = tokio::io::duplex(1024);
    let (tx, rx) = oneshot::channel();
    // Fired while the upstream was being dialled.
    tx.send(Revocation::PodGone).unwrap();
    let m = metrics();
    let start = tokio::time::Instant::now();
    let end = pump(&mut client, &mut up, &m, None, rx).await;
    assert!(matches!(end, End::Revoked(Revocation::PodGone)));
    assert_eq!(start.elapsed(), Duration::ZERO);
}

#[tokio::test]
async fn a_relay_dropped_mid_copy_counts_drain_and_leaves_the_gauge_at_zero() {
    let m = metrics();
    let (mut client, mut c) = tokio::io::duplex(1 << 16);
    let (mut up, mut u) = tokio::io::duplex(1 << 16);
    let (_tx, rx) = oneshot::channel();
    let task = tokio::spawn({
        let m = m.clone();
        async move { super::relay(&m, &id(), &mut client, &mut up, None, rx).await }
    });
    c.write_all(b"ping").await.unwrap();
    let mut b = [0u8; 4];
    u.read_exact(&mut b).await.unwrap();
    assert_eq!(m.active(&id()).get(), 1);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(m.revocation_count(Revocation::Drain), 1);
    assert_eq!(m.active(&id()).get(), 0);
}

#[tokio::test]
async fn a_revoked_relay_is_counted_once_under_its_reason() {
    let m = metrics();
    let (mut client, _c) = tokio::io::duplex(1024);
    let (mut up, _u) = tokio::io::duplex(1024);
    let (tx, rx) = oneshot::channel();
    tx.send(Revocation::Released).unwrap();
    super::relay(&m, &id(), &mut client, &mut up, None, rx).await;
    assert_eq!(m.revocation_count(Revocation::Released), 1);
    assert_eq!(m.revocation_count(Revocation::Drain), 0);
    assert_eq!(m.active(&id()).get(), 0);
}
