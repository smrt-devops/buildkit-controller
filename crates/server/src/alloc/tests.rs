#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use buildkit_controller_api::dto::{CreateAllocationRequest, ErrorCode};
use buildkit_controller_api::v1alpha1::BuildKitPool;
use futures::future::BoxFuture;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Secret};
use kube::runtime::reflector::store::Writer;
use kube::runtime::watcher;
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::{broadcast, watch};

use super::backend::{Backend, CreateOutcome, PatchOutcome, PvcCreate};
use super::certs::MaterialSource;
use super::engine::{COMMIT_WINDOW, Engine, EngineConfig};
use super::metrics::AllocMetrics;
use super::pods::*;
use super::queue::{Queues, WaitersWriter};
use super::{AllocError, drive};
use crate::auth::Identity;
use crate::certs::{CaCertRule, CertMaterial};
use crate::pki::CaBundle;
use crate::reconcile::spawn::WorkerSecrets;
use crate::render::{ANN_ALLOC, ANN_OWNER, PodInputs, fixtures, template_hash, worker_pod};
use crate::runtime::stores::{PodEvent, PodSubscription};

const NS: &str = fixtures::POOL_NAMESPACE;
const POOL: &str = "p";

struct World {
    pods: BTreeMap<String, Pod>,
    writer: Writer<Pod>,
    calls: Vec<String>,
    waiters: Vec<i32>,
    uid: u32,
    /// Apply claim patches but never answer (response lost in flight).
    hang_claims: bool,
    /// Refuse every pod create with a quota error, so the cold path never wins.
    quota: bool,
    /// Answer this many retire patches with an unknown outcome, unapplied.
    unknown_retires: u32,
    /// Fail every `status.waiters` apply with this error (after recording it).
    waiters_err: Option<String>,
    /// Every `status.waiters` apply takes this long.
    waiters_delay: Duration,
    pvcs: BTreeMap<String, PersistentVolumeClaim>,
    /// What every `create_pvc` answers.
    pvc_create: PvcCreate,
    /// Answer commit patches (the ones that replace `expires-at`) with this,
    /// unapplied, instead of applying them.
    extend: Option<PatchOutcome>,
    /// Every patch sent, in order.
    patches: Vec<Value>,
    /// Pod creates land at once but answer after this long.
    create_delay: Duration,
    /// Pod creates answer `Unknown`; `true` lands them anyway.
    lost_creates: Option<bool>,
    /// Every worker Secret create fails with this.
    secret_err: Option<String>,
}

struct Fake {
    w: Mutex<World>,
    tx: broadcast::Sender<PodEvent>,
}

impl Fake {
    fn put(&self, pod: Pod) {
        let mut w = self.w.lock().unwrap();
        let name = pod.metadata.name.clone().unwrap();
        w.writer
            .apply_watcher_event(&watcher::Event::Apply(pod.clone()));
        w.pods.insert(name, pod.clone());
        let _ = self.tx.send(PodEvent::Apply(Arc::new(pod)));
    }
    fn get(&self, name: &str) -> Option<Pod> {
        self.w.lock().unwrap().pods.get(name).cloned()
    }
    fn calls(&self) -> Vec<String> {
        self.w.lock().unwrap().calls.clone()
    }
    fn set_ready(&self, name: &str) {
        let mut p = self.get(name).unwrap();
        p.status = Some(ready_status());
        self.put(p);
    }
}

impl Backend for Fake {
    fn patch_pod(&self, _: &str, name: &str, patch: Value) -> BoxFuture<'static, PatchOutcome> {
        let first_replace = patch
            .as_array()
            .and_then(|a| a.iter().find(|o| o["op"] == "replace"))
            .map(|o| o["value"].as_str().unwrap_or_default().to_owned())
            .unwrap_or_default();
        let commit = patch.as_array().is_some_and(|a| {
            a.iter().any(|o| {
                o["op"] == "replace"
                    && o["path"]
                        .as_str()
                        .is_some_and(|p| p.ends_with("expires-at"))
            })
        });
        {
            let mut w = self.w.lock().unwrap();
            w.calls.push(format!("patch {name} {first_replace}"));
            w.patches.push(patch.clone());
            if let (true, Some(out)) = (commit, w.extend.clone()) {
                return Box::pin(async move { out });
            }
        }
        if first_replace == DRAINING {
            let mut w = self.w.lock().unwrap();
            if w.unknown_retires > 0 {
                w.unknown_retires -= 1;
                return Box::pin(async { PatchOutcome::Unknown });
            }
        }
        let out = match self.get(name) {
            None => PatchOutcome::Lost,
            Some(pod) => {
                let ops: json_patch::Patch = serde_json::from_value(patch).unwrap();
                let mut doc = serde_json::to_value(&pod).unwrap();
                if json_patch::patch(&mut doc, &ops.0).is_ok() {
                    let p: Pod = serde_json::from_value(doc).unwrap();
                    self.put(p.clone());
                    PatchOutcome::Applied(Box::new(p))
                } else {
                    PatchOutcome::Lost
                }
            }
        };
        let hang = self.w.lock().unwrap().hang_claims && first_replace != DRAINING;
        Box::pin(async move {
            if hang {
                std::future::pending::<()>().await;
            }
            out
        })
    }

    fn create_pod(&self, _: &str, mut pod: Pod) -> BoxFuture<'static, CreateOutcome> {
        let name = pod.metadata.name.clone().unwrap();
        self.w.lock().unwrap().calls.push(format!("create {name}"));
        let lost = self.w.lock().unwrap().lost_creates;
        let out = if self.get(&name).is_some() {
            CreateOutcome::Exists
        } else if lost == Some(false) {
            CreateOutcome::Unknown
        } else if self.w.lock().unwrap().quota {
            CreateOutcome::Quota
        } else {
            let n = {
                let mut w = self.w.lock().unwrap();
                w.uid += 1;
                w.uid
            };
            pod.metadata.uid = Some(format!("00000000-0000-4000-8000-{n:012}"));
            self.put(pod.clone());
            if lost.is_some() {
                CreateOutcome::Unknown
            } else {
                CreateOutcome::Created(Box::new(pod))
            }
        };
        let delay = self.w.lock().unwrap().create_delay;
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            out
        })
    }

    fn delete_pod(
        &self,
        _: &str,
        name: &str,
        _: &str,
        _: Option<u32>,
    ) -> BoxFuture<'static, Result<(), String>> {
        let mut w = self.w.lock().unwrap();
        w.calls.push(format!("delete {name}"));
        if let Some(p) = w.pods.remove(name) {
            w.writer
                .apply_watcher_event(&watcher::Event::Delete(p.clone()));
            let _ = self.tx.send(PodEvent::Delete(Arc::new(p)));
        }
        Box::pin(async { Ok(()) })
    }

    fn apply_waiters(
        &self,
        _: &str,
        _: &str,
        _: &str,
        count: i32,
        _: &str,
    ) -> BoxFuture<'static, Result<(), String>> {
        let mut w = self.w.lock().unwrap();
        w.waiters.push(count);
        let out = w.waiters_err.clone().map_or(Ok(()), Err);
        let delay = w.waiters_delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            out
        })
    }

    fn get_pvc(
        &self,
        _: &str,
        name: &str,
    ) -> BoxFuture<'static, Result<Option<PersistentVolumeClaim>, String>> {
        let mut w = self.w.lock().unwrap();
        w.calls.push(format!("get_pvc {name}"));
        let out = Ok(w.pvcs.get(name).cloned());
        Box::pin(async move { out })
    }

    fn create_pvc(&self, _: &str, mut pvc: PersistentVolumeClaim) -> BoxFuture<'static, PvcCreate> {
        let mut w = self.w.lock().unwrap();
        let name = pvc.metadata.name.clone().unwrap();
        w.calls.push(format!("create_pvc {name}"));
        let out = w.pvc_create.clone();
        if out == PvcCreate::Created {
            pvc.metadata.uid = Some(format!("pvc-{name}"));
            w.pvcs.entry(name).or_insert(pvc);
        }
        Box::pin(async move { out })
    }

    fn delete_pvc(&self, _: &str, name: &str, _: &str) -> BoxFuture<'static, Result<(), String>> {
        let mut w = self.w.lock().unwrap();
        w.calls.push(format!("delete_pvc {name}"));
        w.pvcs.remove(name);
        Box::pin(async { Ok(()) })
    }

    fn annotate_pvc(
        &self,
        _: &str,
        name: &str,
        key: &str,
        value: &str,
    ) -> BoxFuture<'static, Result<(), String>> {
        let mut w = self.w.lock().unwrap();
        w.calls.push(format!("annotate_pvc {name} {value}"));
        if let Some(p) = w.pvcs.get_mut(name) {
            p.metadata
                .annotations
                .get_or_insert_default()
                .insert(key.into(), value.into());
        }
        Box::pin(async { Ok(()) })
    }
}

fn ready_status() -> k8s_openapi::api::core::v1::PodStatus {
    serde_json::from_value(json!({
        "phase": "Running", "podIP": "10.0.0.9",
        "conditions": [{"type": "Ready", "status": "True"}]
    }))
    .unwrap()
}

struct Env {
    metrics: AllocMetrics,
    engine: Arc<Engine>,
    stopping: tokio_util::sync::CancellationToken,
    fake: Arc<Fake>,
    pool: BuildKitPool,
    ca: Arc<CaBundle>,
    pools: Writer<BuildKitPool>,
    ca_tx: watch::Sender<CertMaterial>,
}

fn pool(max: i32) -> BuildKitPool {
    pool_with(max, false)
}

fn pool_with(max: i32, cache: bool) -> BuildKitPool {
    let mut spec = json!({
        "template": {"spec": {"containers": [{"name": "buildkitd", "image": "moby/buildkit:v0.26.3",
            "resources": {"limits": {"memory": "1Gi", "ephemeral-storage": "1Gi"}}}]}},
        "scaling": {"min": 0, "max": max},
        "access": {"rules": [{"serviceAccount": {}}]},
    });
    if cache {
        spec["workers"] = json!({"cache": {"local": {"maxVolumes": 2}}});
    }
    let mut p = fixtures::pool(POOL, spec).unwrap();
    let status = json!({"conditions": [{"type": "Ready", "status": "True", "reason": "Ready",
        "message": "", "lastTransitionTime": "2026-01-01T00:00:00Z"}]});
    p.status = Some(serde_json::from_value(status).unwrap());
    p
}

fn env_with(max: i32, cold: bool, queue_cap: usize) -> Env {
    env_full(max, cold, queue_cap, false)
}

fn env_full(max: i32, cold: bool, queue_cap: usize, cache: bool) -> Env {
    let ca = Arc::new(CaBundle::generate(OffsetDateTime::now_utc()).unwrap());
    let pool = pool_with(max, cache);
    let mut pools = Writer::<BuildKitPool>::default();
    pools.apply_watcher_event(&watcher::Event::Apply(pool.clone()));
    let pod_writer = Writer::<Pod>::default();
    let pods = pod_writer.as_reader();
    let (tx, _) = broadcast::channel(1024);
    let fake = Arc::new(Fake {
        w: Mutex::new(World {
            pods: BTreeMap::new(),
            writer: pod_writer,
            calls: Vec::new(),
            waiters: Vec::new(),
            uid: 0,
            hang_claims: false,
            quota: !cold,
            unknown_retires: 0,
            waiters_err: None,
            waiters_delay: Duration::ZERO,
            pvcs: BTreeMap::new(),
            pvc_create: PvcCreate::Created,
            extend: None,
            patches: Vec::new(),
            create_delay: Duration::ZERO,
            lost_creates: None,
            secret_err: None,
        }),
        tx: tx.clone(),
    });
    let (ca_tx, ca_rx) = watch::channel(CertMaterial::from_bundle(
        Arc::clone(&ca),
        CaCertRule::ClientCa,
    ));
    let stopping = tokio_util::sync::CancellationToken::new();
    let metrics = AllocMetrics::default();
    let engine = Arc::new(
        Engine::new(
            Arc::clone(&fake) as Arc<dyn Backend>,
            pools.as_reader(),
            pods,
            Box::new(move || PodSubscription::new(tx.subscribe())),
            Arc::new(MaterialSource::new(ca_rx)),
            EngineConfig {
                endpoint: "tcp://bk.example.com:8444".into(),
                server_name: "bk.example.com".into(),
                allocation_domain: Some("alloc.bk.example.com".into()),
            },
            metrics.clone(),
            Arc::new(Queues::new(queue_cap, metrics.clone())),
        )
        .with_stopping(stopping.clone()),
    );
    Env {
        metrics,
        engine,
        stopping,
        fake,
        pool,
        ca,
        pools,
        ca_tx,
    }
}

impl Env {
    fn warm(&self, slot: u32) -> String {
        self.warm_for(&self.pool, &self.ca, slot)
    }

    fn warm_for(&self, pool: &BuildKitPool, ca: &CaBundle, slot: u32) -> String {
        let mut pod = worker_pod(
            pool,
            &PodInputs {
                slot,
                internal_ca_fingerprint: ca.internal_ca().fingerprint_hex(),
                worker_cert_not_after: "2099-01-01T00:00:00Z".into(),
                nonce: "00000000000000aa".into(),
                claim: None,
            },
        )
        .unwrap();
        pod.metadata.uid = Some(format!("11111111-0000-4000-8000-{slot:012}"));
        pod.status = Some(ready_status());
        let name = pod.metadata.name.clone().unwrap();
        self.fake.put(pod);
        name
    }

    fn spawn_drive(&self) -> tokio_util::sync::CancellationToken {
        let stop = tokio_util::sync::CancellationToken::new();
        let (e, s) = (Arc::clone(&self.engine), stop.clone());
        tokio::spawn(async move { drive(&e, "r1".into(), s).await });
        stop
    }
}

fn sa(name: &str) -> Identity {
    Identity::ServiceAccount {
        namespace: NS.into(),
        name: name.into(),
        uid: "u".into(),
    }
}

fn req(qt: u64) -> CreateAllocationRequest {
    serde_json::from_value(
        json!({"namespace": NS, "pool": POOL, "ttlSeconds": 600, "queueTimeoutSeconds": qt}),
    )
    .unwrap()
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[test]
fn every_error_maps_to_one_distinct_api_code() {
    let all = [
        AllocError::InvalidRequest(String::new()),
        AllocError::Forbidden,
        AllocError::PoolNotFound,
        AllocError::NotFound,
        AllocError::Internal,
        AllocError::PoolExhausted(String::new()),
        AllocError::StartTimeout,
        AllocError::PodLost,
        AllocError::NotReady,
        AllocError::CaUnavailable,
        AllocError::UpstreamError,
        AllocError::ShuttingDown,
    ];
    let codes: std::collections::BTreeSet<&str> = all.iter().map(|e| e.code().as_str()).collect();
    assert_eq!(codes.len(), all.len());
    let http_only = [
        ErrorCode::Unauthenticated,
        ErrorCode::PayloadTooLarge,
        ErrorCode::UnsupportedMediaType,
    ];
    for c in ErrorCode::ALL {
        assert!(
            codes.contains(c.as_str()) || http_only.contains(&c),
            "{c} unmapped"
        );
    }
    assert_eq!(
        AllocError::PoolExhausted(String::new())
            .code()
            .http_status(),
        503
    );
    assert_eq!(AllocError::Forbidden.code().http_status(), 403);
    assert_eq!(
        AllocError::StartTimeout.code().retry_after_seconds(),
        Some(5)
    );
}

#[test]
fn candidate_selection_rules() {
    let e = env_with(4, false, 10);
    let hash = template_hash(&e.pool).unwrap();
    let fp = e.ca.internal_ca().fingerprint_hex();
    let want = Want {
        template_hash: &hash,
        internal_ca: &fp[..16],
        cert_not_after: parse_ts("2098-12-31T23:55:00Z").unwrap(),
    };
    let base = e.fake.get(&e.warm(0)).unwrap();
    assert!(is_candidate(&base, &want));
    let mutate = |f: &dyn Fn(&mut Pod)| {
        let mut p = base.clone();
        f(&mut p);
        is_candidate(&p, &want)
    };
    let set_ann = |k: &'static str, v: &'static str| {
        move |p: &mut Pod| {
            p.metadata
                .annotations
                .get_or_insert_default()
                .insert(k.into(), v.into());
        }
    };
    assert!(!mutate(&set_ann(ANN_ALLOC, "draining")));
    assert!(!mutate(&set_ann(
        crate::render::ANN_INTERNAL_CA,
        "0000000000000000"
    )));
    assert!(!mutate(&set_ann(
        crate::render::ANN_WORKER_CERT_NOT_AFTER,
        "2098-12-31T23:54:59Z"
    )));
    assert!(!mutate(&|p| p.status = None));
    assert!(!mutate(&|p| {
        if let Some(s) = p.status.as_mut() {
            s.pod_ip = None;
        }
    }));
    assert!(!mutate(&|p| {
        p.metadata.deletion_timestamp =
            Some(serde_json::from_value(json!("2026-01-01T00:00:00Z")).unwrap());
    }));
    assert!(!mutate(&|p| {
        p.metadata
            .labels
            .get_or_insert_default()
            .insert(crate::render::LABEL_TEMPLATE_HASH.into(), "stale".into());
    }));
    let uid = e.pool.metadata.uid.clone().unwrap();
    assert!(of_pool(&base, NS, POOL, &uid));
    assert!(!of_pool(&base, NS, POOL, "other-uid"));
}

#[test]
fn slots_ids_patches_and_timestamps() {
    assert_eq!(slot_of("p-0", "p"), Some(0));
    assert_eq!(slot_of("p-07", "p"), None);
    assert_eq!(slot_of("p-x", "p"), None);
    assert_eq!(
        free_slots(["p-0", "p-2", "q-1"].into_iter(), "p", 4),
        vec![1, 3]
    );
    let id = new_alloc_id().unwrap();
    assert_eq!(id.as_str().len(), 26);
    assert_ne!(new_alloc_id().unwrap(), id);
    let c = claim_patch("U", "ID", "sa:a/b", "N", "E");
    assert_eq!(
        c[0],
        json!({"op": "test", "path": "/metadata/uid", "value": "U"})
    );
    assert_eq!(
        c[1]["path"],
        "/metadata/annotations/buildkit.smrt-devops.net~1alloc"
    );
    assert_eq!(c[1]["value"], "free");
    assert_eq!(retire_patch(None, "ID").as_array().unwrap().len(), 3);
    let t = parse_ts("2026-10-19T12:00:00Z").unwrap();
    assert_eq!(fmt_ts(t), "2026-10-19T12:00:00Z");
    assert_eq!(parse_ts("2026-10-19T12:00:00.5Z"), Some(t));
    assert_eq!(parse_ts("2026-10-19 12:00:00Z"), None);
}

#[tokio::test(start_paused = true)]
async fn warm_claim_issues_cert_with_uri_san_and_ttl_lifetime() {
    let e = env_with(4, false, 10);
    let name = e.warm(0);
    let a = e.engine.allocate(&sa("builder"), &req(0)).await.unwrap();
    let (pname, uid) = (a.pod().0.to_owned(), a.pod().1.to_owned());
    assert_eq!(pname, name);
    let r = a.commit();
    assert_eq!(r.owner, format!("sa:{NS}/builder"));
    assert_eq!(
        r.endpoint,
        format!("tcp://{}.alloc.bk.example.com:8444", r.id)
    );
    assert!(r.client_key.is_some());
    let pod = e.fake.get(&name).unwrap();
    assert_eq!(ann(&pod, ANN_ALLOC), Some(r.id.as_str()));
    let der = pem::parse(&r.client_cert).unwrap();
    let (_, cert) = x509_parser::parse_x509_certificate(der.contents()).unwrap();
    let san = cert.subject_alternative_name().unwrap().unwrap();
    let uris: Vec<String> = san
        .value
        .general_names
        .iter()
        .map(|g| format!("{g}"))
        .collect();
    assert_eq!(
        uris,
        vec![format!("URI(buildkit://{NS}/{POOL}/{uid}/{})", r.id)]
    );
    let not_after = cert.validity().not_after.timestamp();
    assert_eq!(
        fmt_ts(OffsetDateTime::from_unix_timestamp(not_after).unwrap()),
        r.expires_at
    );
    let allocated = parse_ts(ann(&pod, crate::render::ANN_ALLOCATED_AT).unwrap()).unwrap();
    assert_eq!(not_after - allocated.unix_timestamp(), 600);
    settle().await;
    assert!(!e.fake.calls().iter().any(|c| c.contains("draining")));
}

#[tokio::test(start_paused = true)]
async fn bad_public_key_is_rejected_before_any_claim() {
    let e = env_with(4, true, 10);
    e.warm(0);
    let mut r = req(0);
    r.public_key = Some("-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n".into());
    let err = e.engine.allocate(&sa("b"), &r).await.err().unwrap();
    assert_eq!(err.code(), ErrorCode::InvalidRequest);
    assert!(e.fake.calls().is_empty(), "{:?}", e.fake.calls());
    r.public_key = None;
    r.ttl_seconds = Some(59);
    assert!(matches!(
        e.engine.allocate(&sa("b"), &r).await,
        Err(AllocError::InvalidRequest(_))
    ));
    assert!(e.fake.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn access_denied_is_pool_not_found() {
    let e = env_with(4, false, 10);
    e.warm(0);
    let other = Identity::ServiceAccount {
        namespace: "elsewhere".into(),
        name: "b".into(),
        uid: "u".into(),
    };
    let err = e.engine.allocate(&other, &req(0)).await.err().unwrap();
    assert_eq!(err, AllocError::PoolNotFound);
    assert!(e.fake.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn uncommitted_allocation_is_retired_on_drop() {
    let e = env_with(4, false, 10);
    let name = e.warm(0);
    let a = e.engine.allocate(&sa("b"), &req(0)).await.unwrap();
    drop(a);
    settle().await;
    let calls = e.fake.calls();
    assert!(
        calls.contains(&format!("patch {name} draining")),
        "{calls:?}"
    );
    assert!(calls.contains(&format!("delete {name}")), "{calls:?}");
}

#[tokio::test(start_paused = true)]
async fn cancelled_cold_request_leaves_no_orphaned_claim() {
    let e = env_with(1, true, 10);
    let (who, r) = (sa("b"), req(0));
    let fut = e.engine.allocate(&who, &r);
    // The cold pod is created pre-claimed and never becomes Ready; the client goes away.
    assert!(
        tokio::time::timeout(Duration::from_secs(3), fut)
            .await
            .is_err()
    );
    settle().await;
    let calls = e.fake.calls();
    // The cold path creates the pod, then its Secret owned by the pod.
    assert_eq!(calls[0], "create p-0", "{calls:?}");
    assert!(calls[1].starts_with("secret create p-0-"), "{calls:?}");
    assert!(calls[1].ends_with(" Pod/p-0"), "{calls:?}");
    assert!(
        calls.contains(&"patch p-0 draining".to_owned()),
        "{calls:?}"
    );
    assert!(calls.contains(&"delete p-0".to_owned()), "{calls:?}");
    assert!(e.fake.get("p-0").is_none());
}

#[tokio::test(start_paused = true)]
async fn cold_pod_start_timeout_and_success() {
    let e = env_with(2, true, 10);
    let err = e.engine.allocate(&sa("b"), &req(0)).await.err().unwrap();
    assert_eq!(err, AllocError::StartTimeout);
    settle().await;
    assert!(e.fake.calls().iter().any(|c| c.ends_with("draining")));

    let engine = Arc::clone(&e.engine);
    let h = tokio::spawn(async move { engine.allocate(&sa("b"), &req(0)).await });
    tokio::time::sleep(Duration::from_secs(1)).await;
    let created = e
        .fake
        .calls()
        .iter()
        .rev()
        .find_map(|c| c.strip_prefix("create ").map(str::to_owned))
        .unwrap();
    e.fake.set_ready(&created);
    let a = h.await.unwrap().unwrap();
    assert_eq!(a.pod().0, created);
    let pod = e.fake.get(&created).unwrap();
    assert_eq!(ann(&pod, ANN_OWNER), Some(format!("sa:{NS}/b").as_str()));
    let _ = a.commit();
}

#[tokio::test(start_paused = true)]
async fn queue_timeout_returns_pool_exhausted() {
    let e = env_with(4, false, 10);
    let t0 = tokio::time::Instant::now();
    let err = e.engine.allocate(&sa("b"), &req(5)).await.err().unwrap();
    assert!(matches!(err, AllocError::PoolExhausted(_)));
    assert_eq!(t0.elapsed().as_secs(), 5);
    let t1 = tokio::time::Instant::now();
    assert!(matches!(
        e.engine.allocate(&sa("b"), &req(0)).await,
        Err(AllocError::PoolExhausted(_))
    ));
    assert_eq!(t1.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn queue_full_is_immediate_pool_exhausted() {
    let e = env_with(4, false, 1);
    let engine = Arc::clone(&e.engine);
    let _h = tokio::spawn(async move { engine.allocate(&sa("b"), &req(60)).await });
    tokio::time::sleep(Duration::from_millis(10)).await;
    let t = tokio::time::Instant::now();
    assert!(matches!(
        e.engine.allocate(&sa("b"), &req(60)).await,
        Err(AllocError::PoolExhausted(_))
    ));
    assert_eq!(t.elapsed(), Duration::ZERO);
}

/// Shutdown wakes every queued request with `shutting_down` at once, claims
/// nothing for it, and empties the queue.
#[tokio::test(start_paused = true)]
async fn shutdown_refuses_queued_requests_before_any_claim() {
    let e = env_with(4, false, 10);
    let _stop = e.spawn_drive();
    let mut handles = Vec::new();
    for i in 0..3 {
        let engine = Arc::clone(&e.engine);
        handles.push(tokio::spawn(async move {
            engine
                .allocate(&sa(&format!("w{i}")), &req(600))
                .await
                .err()
        }));
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
    let key = (NS.to_owned(), POOL.to_owned());
    assert_eq!(e.engine.queues.lengths(), vec![(key.clone(), 3)]);
    let t = tokio::time::Instant::now();
    e.stopping.cancel();
    for h in handles {
        assert_eq!(h.await.unwrap(), Some(AllocError::ShuttingDown));
    }
    assert_eq!(t.elapsed(), Duration::ZERO);
    assert_eq!(e.engine.queues.lengths(), vec![(key, 0)]);
    assert!(
        !e.fake.calls().iter().any(|c| c.starts_with("patch")),
        "{:?}",
        e.fake.calls()
    );
    assert_eq!(
        AllocError::ShuttingDown.code().retry_after_seconds(),
        Some(1)
    );
}

/// After shutdown a new request is refused even when a warm pod is free, and
/// that pod stays free.
#[tokio::test(start_paused = true)]
async fn shutdown_refuses_new_requests_and_leaves_warm_pods_free() {
    let e = env_with(4, false, 10);
    let name = e.warm(0);
    e.stopping.cancel();
    assert!(matches!(
        e.engine.allocate(&sa("late"), &req(60)).await,
        Err(AllocError::ShuttingDown)
    ));
    assert_eq!(ann(&e.fake.get(&name).unwrap(), ANN_ALLOC), Some(FREE));
    assert!(e.fake.calls().is_empty(), "{:?}", e.fake.calls());
}

#[tokio::test(start_paused = true)]
async fn queue_is_fifo_and_woken_by_pod_events() {
    let e = env_with(8, false, 10);
    let _stop = e.spawn_drive();
    let mut handles = Vec::new();
    for i in 0..3 {
        let engine = Arc::clone(&e.engine);
        handles.push(tokio::spawn(async move {
            let who = sa(&format!("w{i}"));
            engine
                .allocate(&who, &req(60))
                .await
                .map(super::Allocation::commit)
        }));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for slot in 0..3 {
        let name = e.warm(slot);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let pod = e.fake.get(&name).unwrap();
        assert_eq!(
            ann(&pod, ANN_OWNER),
            Some(format!("sa:{NS}/w{slot}").as_str()),
            "waiter {slot} must get the pod {slot}"
        );
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }
}

/// A request that finds a waiter goes behind it, so a pod freed while
/// someone is queued goes to the waiter even when the newcomer runs first.
#[tokio::test(start_paused = true)]
async fn a_newcomer_never_takes_a_pod_from_a_queued_request() {
    let e = env_with(4, false, 10);
    let _stop = e.spawn_drive();
    let engine = Arc::clone(&e.engine);
    let first = tokio::spawn(async move {
        engine
            .allocate(&sa("first"), &req(60))
            .await
            .map(super::Allocation::commit)
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    let key = (NS.to_owned(), POOL.to_owned());
    assert_eq!(e.engine.queues.lengths(), vec![(key, 1)]);
    // The newcomer runs before the pod event wakes the queue head.
    let p0 = e.warm(0);
    let t = tokio::time::Instant::now();
    assert!(matches!(
        e.engine.allocate(&sa("late"), &req(0)).await,
        Err(AllocError::PoolExhausted(_))
    ));
    assert_eq!(t.elapsed(), Duration::ZERO);
    let engine = Arc::clone(&e.engine);
    let second = tokio::spawn(async move {
        engine
            .allocate(&sa("second"), &req(60))
            .await
            .map(super::Allocation::commit)
    });
    let p1 = e.warm(1);
    assert_eq!(
        first.await.unwrap().unwrap().owner,
        format!("sa:{NS}/first")
    );
    assert_eq!(
        second.await.unwrap().unwrap().owner,
        format!("sa:{NS}/second")
    );
    // Either waiter may claim either pod; the newcomer got neither.
    let mut owners: Vec<String> = [p0, p1]
        .iter()
        .map(|p| ann(&e.fake.get(p).unwrap(), ANN_OWNER).unwrap().to_owned())
        .collect();
    owners.sort();
    assert_eq!(
        owners,
        [format!("sa:{NS}/first"), format!("sa:{NS}/second")]
    );
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(128))]

    /// FIFO on one replica: whatever the interleaving of arrivals and freed
    /// pods, the requests that got a pod are exactly the earliest arrivals.
    #[test]
    fn requests_are_served_in_arrival_order(
        events in proptest::collection::vec((proptest::bool::ANY, 0..4_usize), 1..20),
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        let (served, want) = rt.block_on(async {
            let e = env_with(64, false, 64);
            let _stop = e.spawn_drive();
            tokio::time::sleep(Duration::from_millis(1)).await;
            let (mut handles, mut freed) = (Vec::new(), 0_u32);
            for (arrive, yields) in events {
                if arrive {
                    let engine = Arc::clone(&e.engine);
                    let who = sa(&format!("a{}", handles.len()));
                    handles.push(tokio::spawn(async move {
                        engine
                            .allocate(&who, &req(600))
                            .await
                            .map(super::Allocation::commit)
                    }));
                } else {
                    e.warm(freed);
                    freed += 1;
                }
                for _ in 0..yields {
                    tokio::task::yield_now().await;
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            let want: Vec<usize> = (0..handles.len().min(freed as usize)).collect();
            let mut served = Vec::new();
            for (i, h) in handles.iter().enumerate() {
                if h.is_finished() {
                    served.push(i);
                }
            }
            for h in handles.into_iter().filter(tokio::task::JoinHandle::is_finished) {
                assert!(h.await.unwrap().is_ok());
            }
            (served, want)
        });
        proptest::prop_assert_eq!(served, want);
    }
}

#[tokio::test(start_paused = true)]
async fn release_is_owner_only() {
    let e = env_with(4, false, 10);
    let name = e.warm(0);
    let id = e
        .engine
        .allocate(&sa("owner"), &req(0))
        .await
        .unwrap()
        .commit()
        .id;
    assert_eq!(
        e.engine.release(&sa("thief"), &id).await,
        Err(AllocError::Forbidden)
    );
    assert_eq!(
        ann(&e.fake.get(&name).unwrap(), ANN_ALLOC),
        Some(id.as_str())
    );
    e.engine.release(&sa("owner"), &id).await.unwrap();
    assert!(e.fake.get(&name).is_none());
    let t = tokio::time::Instant::now();
    assert_eq!(
        e.engine.release(&sa("owner"), &id).await,
        Err(AllocError::NotFound)
    );
    assert_eq!(t.elapsed().as_secs(), 2);
    assert_eq!(
        e.engine.release(&sa("owner"), "nope").await,
        Err(AllocError::NotFound)
    );
}

#[tokio::test(start_paused = true)]
async fn waiters_status_written_on_change_heartbeat_and_zero() {
    let e = env_with(4, false, 10);
    let mut writer = WaitersWriter::new("r1".into(), e.engine.pools.clone());
    let key = (NS.to_owned(), POOL.to_owned());
    let ticket = e.engine.queues.enqueue(&key).unwrap();
    writer.tick(&e.engine.queues, &*e.fake).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    writer.tick(&e.engine.queues, &*e.fake).await;
    tokio::time::sleep(Duration::from_secs(21)).await;
    writer.tick(&e.engine.queues, &*e.fake).await;
    drop(ticket);
    tokio::time::sleep(Duration::from_secs(1)).await;
    writer.tick(&e.engine.queues, &*e.fake).await;
    writer.tick(&e.engine.queues, &*e.fake).await;
    assert_eq!(e.fake.w.lock().unwrap().waiters, vec![1, 1, 0]);
}

/// On shutdown the queued requests are refused and `drive` returns only after
/// this replica's `status.waiters` entry is back at 0, so the leader does not
/// count them twice while they retry elsewhere.
#[tokio::test(start_paused = true)]
async fn shutdown_sets_status_waiters_back_to_zero_before_drive_returns() {
    let e = env_with(4, false, 10);
    let stop = e.stopping.clone();
    let engine = Arc::clone(&e.engine);
    let drive = tokio::spawn(async move { drive(&engine, "r1".into(), stop).await });
    let engine = Arc::clone(&e.engine);
    let queued = tokio::spawn(async move { engine.allocate(&sa("w"), &req(600)).await.err() });
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(e.fake.w.lock().unwrap().waiters, vec![1]);
    e.stopping.cancel();
    drive.await.unwrap();
    assert_eq!(e.fake.w.lock().unwrap().waiters, vec![1, 0]);
    assert_eq!(queued.await.unwrap(), Some(AllocError::ShuttingDown));
}

#[tokio::test(start_paused = true)]
async fn claim_landing_after_cancellation_is_retired() {
    let e = env_with(4, false, 10);
    let name = e.warm(0);
    e.fake.w.lock().unwrap().hang_claims = true;
    let (who, r) = (sa("b"), req(0));
    let fut = e.engine.allocate(&who, &r);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), fut)
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    settle().await;
    assert!(e.fake.get(&name).is_none(), "{:?}", e.fake.calls());
    assert!(e.fake.calls().contains(&format!("delete {name}")));
}

#[tokio::test(start_paused = true)]
async fn queued_request_spanning_ca_rotation_uses_the_new_ca() {
    let e = env_with(4, false, 10);
    let _stop = e.spawn_drive();
    let engine = Arc::clone(&e.engine);
    let h = tokio::spawn(async move {
        engine
            .allocate(&sa("b"), &req(60))
            .await
            .map(super::Allocation::commit)
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    // Hard rotation while queued; the only warm pod carries the new fingerprint.
    let new_ca = Arc::new(CaBundle::generate(OffsetDateTime::now_utc()).unwrap());
    e.ca_tx
        .send(CertMaterial::from_bundle(
            Arc::clone(&new_ca),
            CaCertRule::ClientCa,
        ))
        .unwrap();
    let name = e.warm_for(&e.pool, &new_ca, 0);
    let r = h.await.unwrap().unwrap();
    assert_eq!(
        ann(&e.fake.get(&name).unwrap(), ANN_ALLOC),
        Some(r.id.as_str())
    );
    let new_pem = new_ca.client_ca().cert_pem().to_owned();
    assert_eq!(r.ca_cert.as_deref(), Some(new_pem.as_str()));
    let issuer = pem::parse(&new_pem).unwrap();
    let (_, issuer) = x509_parser::parse_x509_certificate(issuer.contents()).unwrap();
    let leaf = pem::parse(&r.client_cert).unwrap();
    let (_, leaf) = x509_parser::parse_x509_certificate(leaf.contents()).unwrap();
    let aki = leaf
        .extensions()
        .iter()
        .find_map(|x| match x.parsed_extension() {
            x509_parser::extensions::ParsedExtension::AuthorityKeyIdentifier(a) => {
                a.key_identifier.as_ref().map(|k| k.0.to_vec())
            }
            _ => None,
        });
    let ski = issuer
        .extensions()
        .iter()
        .find_map(|x| match x.parsed_extension() {
            x509_parser::extensions::ParsedExtension::SubjectKeyIdentifier(k) => Some(k.0.to_vec()),
            _ => None,
        });
    assert!(ski.is_some());
    assert_eq!(aki, ski, "signed by the rotated client CA");
}

#[tokio::test(start_paused = true)]
async fn waiter_cannot_claim_in_a_recreated_or_reaccessed_pool() {
    for recreate in [true, false] {
        let mut e = env_with(4, false, 10);
        let _stop = e.spawn_drive();
        let engine = Arc::clone(&e.engine);
        let h = tokio::spawn(async move { engine.allocate(&sa("b"), &req(60)).await.map(|_| ()) });
        tokio::time::sleep(Duration::from_secs(1)).await;
        let mut next = e.pool.clone();
        if recreate {
            // Same name, new uid; its rules still admit the caller.
            next.metadata.uid = Some("9f6c1f4e-3b0a-4c43-9a55-5d4f0c7b1a99".into());
        } else {
            // Same pool, access narrowed to another namespace.
            next.metadata.generation = Some(2);
            next.spec.access = serde_json::from_value(
                json!({"rules": [{"serviceAccount": {"namespace": "elsewhere"}}]}),
            )
            .unwrap();
        }
        e.pools
            .apply_watcher_event(&watcher::Event::Apply(next.clone()));
        let name = e.warm_for(&next, &e.ca, 0);
        let r = h.await.unwrap();
        assert_eq!(r, Err(AllocError::PoolNotFound), "recreate={recreate}");
        assert_eq!(ann(&e.fake.get(&name).unwrap(), ANN_ALLOC), Some(FREE));
        assert!(e.fake.calls().iter().all(|c| !c.starts_with("patch")));
    }
}

/// A worker-shaped pod named `name` in `ns`, claimed for `id` by `owner`.
fn decoy(e: &Env, ns: &str, name: &str, id: &str, owner: &str) -> Pod {
    let mut pod = worker_pod(
        &e.pool,
        &PodInputs {
            slot: 0,
            internal_ca_fingerprint: e.ca.internal_ca().fingerprint_hex(),
            worker_cert_not_after: "2099-01-01T00:00:00Z".into(),
            nonce: "00000000000000aa".into(),
            claim: None,
        },
    )
    .unwrap();
    pod.metadata.namespace = Some(ns.into());
    pod.metadata.name = Some(name.into());
    pod.metadata.uid = Some(format!("dddddddd-0000-4000-8000-{:012}", name.len()));
    pod.status = Some(ready_status());
    let a = pod.metadata.annotations.get_or_insert_with(BTreeMap::new);
    a.insert(ANN_ALLOC.into(), id.into());
    a.insert(ANN_OWNER.into(), owner.into());
    pod
}

#[tokio::test(start_paused = true)]
async fn release_ignores_pods_outside_the_resolved_pool() {
    let e = env_with(4, false, 10);
    let id = new_alloc_id().unwrap();
    let owner = sa("owner").owner();
    // Same labels and claim in a namespace with no pool.
    e.fake
        .put(decoy(&e, "evil", "decoy-a", id.as_str(), &owner));
    // In the pool's namespace, but with no controller ownerReference to the pool.
    let mut orphan = decoy(&e, NS, "decoy-b", id.as_str(), &owner);
    orphan.metadata.owner_references = None;
    e.fake.put(orphan);
    assert_eq!(
        e.engine.release(&sa("owner"), id.as_str()).await,
        Err(AllocError::NotFound)
    );
    assert!(e.fake.calls().is_empty(), "{:?}", e.fake.calls());
    // A decoy cannot block the real owner either.
    e.fake.put(decoy(
        &e,
        "evil",
        "decoy-c",
        id.as_str(),
        &sa("thief").owner(),
    ));
    let name = e.warm(0);
    let real = e
        .engine
        .allocate(&sa("owner"), &req(0))
        .await
        .unwrap()
        .commit()
        .id;
    let mut d = decoy(&e, "evil", "decoy-d", &real, &sa("thief").owner());
    d.metadata.uid = Some("eeeeeeee-0000-4000-8000-000000000001".into());
    e.fake.put(d);
    for i in 0..8 {
        e.fake.put(decoy(
            &e,
            "evil",
            &format!("decoy-x{i}"),
            &real,
            &sa("thief").owner(),
        ));
    }
    e.engine.release(&sa("owner"), &real).await.unwrap();
    assert!(e.fake.get(&name).is_none());
    assert!(e.fake.get("decoy-d").is_some());
}

#[tokio::test(start_paused = true)]
async fn claim_on_a_terminating_pod_is_retired_not_returned() {
    let e = env_with(1, false, 10);
    let name = e.warm(0);
    // The API server is already deleting the pod; the cache has not seen it yet.
    e.fake
        .w
        .lock()
        .unwrap()
        .pods
        .get_mut(&name)
        .unwrap()
        .metadata
        .deletion_timestamp = Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
        jiff::Timestamp::now(),
    ));
    let got = e.engine.allocate(&sa("b"), &req(0)).await;
    assert!(got.is_err(), "a terminating pod was handed out");
    settle().await;
    let calls = e.fake.calls();
    assert!(
        calls.contains(&format!("patch {name} draining")),
        "{calls:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn confirmed_orphan_retire_retries_until_it_lands() {
    let e = env_with(4, false, 10);
    let name = e.warm(0);
    let a = e.engine.allocate(&sa("b"), &req(0)).await.unwrap();
    // The claim landed; the first five retires time out.
    e.fake.w.lock().unwrap().unknown_retires = 5;
    drop(a);
    tokio::time::sleep(Duration::from_mins(2)).await;
    settle().await;
    let calls = e.fake.calls();
    let retires = calls
        .iter()
        .filter(|c| **c == format!("patch {name} draining"))
        .count();
    assert_eq!(retires, 6, "{calls:?}");
    assert!(e.fake.get(&name).is_none(), "{calls:?}");
}

#[tokio::test(start_paused = true)]
async fn waiters_of_a_deleted_pool_are_not_written_again() {
    let mut e = env_with(4, false, 10);
    let mut writer = WaitersWriter::new("r1".into(), e.engine.pools.clone());
    let key = (NS.to_owned(), POOL.to_owned());
    let ticket = e.engine.queues.enqueue(&key).unwrap();
    writer.tick(&e.engine.queues, &*e.fake).await;
    drop(ticket);
    e.pools
        .apply_watcher_event(&watcher::Event::Delete(e.pool.clone()));
    e.fake.w.lock().unwrap().waiters_err = Some("404 NotFound".into());
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        writer.tick(&e.engine.queues, &*e.fake).await;
    }
    assert_eq!(e.fake.w.lock().unwrap().waiters, vec![1]);
    assert!(e.engine.queues.lengths().is_empty());
}

#[tokio::test(start_paused = true)]
async fn slow_waiters_apply_does_not_delay_queue_wakes() {
    let e = env_with(8, false, 10);
    e.fake.w.lock().unwrap().waiters_delay = Duration::from_secs(10);
    let _stop = e.spawn_drive();
    let engine = Arc::clone(&e.engine);
    let waiter = tokio::spawn(async move {
        engine
            .allocate(&sa("w"), &req(60))
            .await
            .map(super::Allocation::commit)
    });
    // The writer's first apply (count 1) starts at the 1 s tick and hangs.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let t = tokio::time::Instant::now();
    e.warm(0);
    waiter.await.unwrap().unwrap();
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "woken after {:?}",
        t.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn release_retires_every_owned_match_including_a_decoy_in_another_pool() {
    let mut e = env_with(4, false, 10);
    let name = e.warm(0);
    let owner = sa("owner");
    let id = e
        .engine
        .allocate(&owner, &req(0))
        .await
        .unwrap()
        .commit()
        .id;
    // A tenant's own cached pool, holding look-alikes with the victim's id and owner.
    let mut q = pool(32);
    q.metadata.name = Some("q".into());
    q.metadata.uid = Some("3f6c1f4e-3b0a-4c43-9a55-5d4f0c7b1a10".into());
    e.pools
        .apply_watcher_event(&watcher::Event::Apply(q.clone()));
    for slot in 0..20 {
        let mut d = worker_pod(
            &q,
            &PodInputs {
                slot,
                internal_ca_fingerprint: e.ca.internal_ca().fingerprint_hex(),
                worker_cert_not_after: "2099-01-01T00:00:00Z".into(),
                nonce: "00000000000000aa".into(),
                claim: None,
            },
        )
        .unwrap();
        d.metadata.uid = Some(format!("cccccccc-0000-4000-8000-{slot:012}"));
        d.status = Some(ready_status());
        let a = d.metadata.annotations.get_or_insert_with(BTreeMap::new);
        a.insert(ANN_ALLOC.into(), id.clone());
        a.insert(ANN_OWNER.into(), owner.owner());
        e.fake.put(d);
    }
    e.engine.release(&owner, &id).await.unwrap();
    assert!(e.fake.get(&name).is_none(), "the real pod stayed allocated");
}

/// Regression (auth-api review P3): at shutdown the Pod broadcast closes and
/// `recv` returns `None` at once, so a release of an unknown id spun until its
/// deadline. It now returns `NotFound` immediately.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_returns_when_the_pod_watch_closes() {
    let e = env_with(4, false, 10);
    let engine = Engine::new(
        Arc::clone(&e.fake) as Arc<dyn Backend>,
        e.pools.as_reader(),
        e.engine.pods.clone(),
        Box::new(|| {
            let (tx, rx) = broadcast::channel(1);
            drop(tx);
            PodSubscription::new(rx)
        }),
        Arc::clone(&e.engine.certs),
        e.engine.cfg.clone(),
        AllocMetrics::default(),
        Arc::new(Queues::new(1, AllocMetrics::default())),
    );
    let id = crate::alloc::pods::new_alloc_id().unwrap().to_string();
    // A regression spins on the closed watch and never yields, so run the
    // release on its own worker and bound it by wall clock.
    let task = tokio::spawn(async move { engine.release(&sa("owner"), &id).await });
    let done = tokio::time::timeout(Duration::from_millis(500), task).await;
    let joined = done.expect("release must return when the watch closes");
    assert_eq!(joined.unwrap(), Err(AllocError::NotFound));
}

/// Regression (auth-api review P3): every release of an unknown id waited
/// `RELEASE_WAIT` holding a broadcast receiver, with no limit. Past
/// `RELEASE_WAITERS` a release gets 503 with `Retry-After` instead.
#[tokio::test(start_paused = true)]
async fn release_waiters_are_bounded() {
    use super::engine::RELEASE_WAITERS;
    let e = env_with(4, false, 10);
    let mut waiting = Vec::new();
    for _ in 0..RELEASE_WAITERS {
        let engine = Arc::clone(&e.engine);
        let id = crate::alloc::pods::new_alloc_id().unwrap().to_string();
        waiting.push(tokio::spawn(async move {
            engine.release(&sa("owner"), &id).await
        }));
    }
    tokio::task::yield_now().await;
    while e.engine.release_waiters.available_permits() > 0 {
        tokio::task::yield_now().await;
    }
    let id = crate::alloc::pods::new_alloc_id().unwrap().to_string();
    let busy = e.engine.release(&sa("owner"), &id).await;
    assert_eq!(busy, Err(AllocError::ReleaseBusy));
    assert_eq!(busy.unwrap_err().code().retry_after_seconds(), Some(5));
    for h in waiting {
        assert_eq!(h.await.unwrap(), Err(AllocError::NotFound));
    }
    // The permits come back.
    assert_eq!(
        e.engine.release_waiters.available_permits(),
        RELEASE_WAITERS
    );
}

impl WorkerSecrets for Fake {
    fn create_worker_secret(&self, s: &Secret) -> BoxFuture<'static, Result<(), String>> {
        let name = s.metadata.name.clone().unwrap_or_default();
        let owners: Vec<String> = (s.metadata.owner_references.iter().flatten())
            .map(|o| format!("{}/{}", o.kind, o.name))
            .collect();
        let mut w = self.w.lock().unwrap();
        w.calls
            .push(format!("secret create {name} {}", owners.join(",")));
        let out = w.secret_err.clone().map_or(Ok(()), Err);
        Box::pin(async move { out })
    }
    fn get_worker_pod(
        &self,
        _: &str,
        name: &str,
    ) -> BoxFuture<'static, Result<Option<Pod>, String>> {
        self.w.lock().unwrap().calls.push(format!("get {name}"));
        let pod = self.get(name);
        Box::pin(async move { Ok(pod) })
    }
}

fn keyed_req(key: &str) -> CreateAllocationRequest {
    serde_json::from_value(json!({
        "namespace": NS, "pool": POOL, "ttlSeconds": 600, "queueTimeoutSeconds": 0, "cacheKey": key
    }))
    .unwrap()
}

impl Env {
    fn set_keys(&self, name: &str, keys: &str) {
        let mut p = self.fake.get(name).unwrap();
        crate::render::set_pod_cache_keys(&mut p, keys);
        self.fake.put(p);
    }
}

#[tokio::test(start_paused = true)]
#[allow(clippy::many_single_char_names)]
async fn same_key_concurrent_allocations_hit_then_spill() {
    // maxVolumes 2: slots 0 and 1 have volumes, slot 2 does not.
    let e = env_full(4, false, 10, true);
    let (p0, p1, _p2) = (e.warm(0), e.warm(1), e.warm(2));
    e.set_keys(&p0, "other,app-main");
    e.set_keys(&p1, "other");
    let a = e
        .engine
        .allocate(&sa("b"), &keyed_req("app-main"))
        .await
        .unwrap();
    assert_eq!(a.pod().0, p0);
    let first = a.commit();
    let c = first.cache.clone().unwrap();
    assert_eq!(
        (c.key.as_str(), c.local.unwrap().result.as_str()),
        ("app-main", "hit")
    );
    assert!(c.registry.is_none());
    // The claim moved the key to the front of the pod's list.
    let pod = e.fake.get(&p0).unwrap();
    assert_eq!(crate::render::pod_cache_keys(&pod), Some("app-main,other"));
    settle().await;
    let pvc0 = format!("{POOL}-cache-0");
    assert!(
        e.fake
            .calls()
            .contains(&format!("annotate_pvc {pvc0} app-main,other"))
    );

    // The matching volume is busy: the second build of the key spills to slot 1.
    let b = e
        .engine
        .allocate(&sa("b"), &keyed_req("app-main"))
        .await
        .unwrap();
    assert_eq!(b.pod().0, p1);
    let second = b.commit();
    assert_eq!(second.cache.unwrap().local.unwrap().result, "spill");
    let pod = e.fake.get(&p1).unwrap();
    assert_eq!(crate::render::pod_cache_keys(&pod), Some("app-main,other"));

    // A third build lands in the slot without a volume and its keys stay untouched.
    let c = e
        .engine
        .allocate(&sa("b"), &keyed_req("app-main"))
        .await
        .unwrap()
        .commit();
    assert_eq!(c.cache.unwrap().local.unwrap().result, "uncached");
    let mut reg = prometheus_client::registry::Registry::default();
    e.metrics.register(&mut reg);
    let mut m = String::new();
    prometheus_client::encoding::text::encode(&mut m, &reg).unwrap();
    for r in ["hit", "spill", "uncached"] {
        assert!(
            m.contains(&format!("result=\"{r}\"")),
            "{r} missing from {m}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn unknown_key_is_a_miss_and_a_keyless_request_uses_the_pool_name() {
    let e = env_full(4, false, 10, true);
    let p0 = e.warm(0);
    e.set_keys(&p0, "x");
    let r = e
        .engine
        .allocate(&sa("b"), &keyed_req("new"))
        .await
        .unwrap()
        .commit();
    assert_eq!(r.cache.unwrap().local.unwrap().result, "miss");
    e.warm(1);
    let r = e.engine.allocate(&sa("b"), &req(0)).await.unwrap().commit();
    assert_eq!(r.cache.unwrap().key, POOL);
}

#[test]
fn cache_plan_resolves_layers_and_rejects_missing_ones() {
    use buildkit_controller_api::dto::CacheChoice;

    use super::cache::plan;
    let mk = |cache: Value| {
        let mut p = fixtures::pool(POOL, json!({"workers": {"cache": cache}})).unwrap();
        p.metadata.uid = Some("u".into());
        p
    };
    let key = |s: &str| buildkit_controller_api::dto::CacheKey::try_from(s.to_owned()).unwrap();
    let both = mk(json!({
        "registry": {"repository": "r.example/cache", "importFrom": ["trunk"]},
        "local": {"keysPerVolume": 3}
    }));
    let p = plan(&both, Some(&key("k1")), None).unwrap().unwrap();
    assert_eq!((p.key.as_str(), p.local, p.soft), ("k1", true, Some(3)));
    let r = p.registry.unwrap();
    assert_eq!(
        r.from,
        [
            "type=registry,ref=r.example/cache:k1",
            "type=registry,ref=r.example/cache:trunk"
        ]
    );
    let p = plan(&both, None, Some(CacheChoice::Registry))
        .unwrap()
        .unwrap();
    assert_eq!((p.key.as_str(), p.local, p.soft), (POOL, false, None));
    let p = plan(&both, None, Some(CacheChoice::Local))
        .unwrap()
        .unwrap();
    assert!(p.registry.is_none() && p.local);
    let p = plan(&both, Some(&key("k")), Some(CacheChoice::None))
        .unwrap()
        .unwrap();
    assert!(p.registry.is_none() && !p.local && p.soft.is_none());
    assert_eq!(p.response(None), None);

    let local_off = mk(json!({"local": {"affinity": "off"}}));
    let p = plan(&local_off, None, None).unwrap().unwrap();
    assert_eq!((p.local, p.soft), (true, None));
    assert!(matches!(
        plan(&local_off, None, Some(CacheChoice::Registry)),
        Err(AllocError::InvalidRequest(_))
    ));
    let disabled = mk(json!({"registry": {"repository": "r.example/c", "enabled": false}}));
    assert_eq!(plan(&disabled, Some(&key("k")), None).unwrap(), None);
    assert!(matches!(
        plan(&disabled, None, Some(CacheChoice::Local)),
        Err(AllocError::InvalidRequest(_))
    ));
}

fn choice_req(cache: &str) -> CreateAllocationRequest {
    serde_json::from_value(json!({
        "namespace": NS, "pool": POOL, "ttlSeconds": 600, "queueTimeoutSeconds": 0, "cache": cache
    }))
    .unwrap()
}

#[test]
fn without_the_local_layer_uncached_slots_come_first() {
    use super::cache::{order_cold, order_warm};
    // maxVolumes 2 of max 4: slots 0 and 1 are cached.
    let pool = pool_with(4, true);
    let pods: Vec<Arc<Pod>> = (0..4)
        .map(|slot| {
            worker_pod(
                &pool,
                &PodInputs {
                    slot,
                    internal_ca_fingerprint: "00".into(),
                    worker_cert_not_after: "2099-01-01T00:00:00Z".into(),
                    nonce: "00000000000000aa".into(),
                    claim: None,
                },
            )
            .unwrap()
            .into()
        })
        .collect();
    let cached = |p: &&Arc<Pod>| super::cache::cached(&pool, p);
    for _ in 0..20 {
        let mut c: Vec<&Arc<Pod>> = pods.iter().collect();
        order_warm(&pool, &mut c, None, false);
        assert!(!c.iter().take(2).any(cached) && c.iter().skip(2).all(cached));
        order_warm(&pool, &mut c, None, true);
        assert!(c.iter().take(2).all(cached));
        let mut s = vec![0, 1, 2, 3];
        order_cold(&pool, &mut s, false);
        assert!(s[..2].iter().all(|&n| n >= 2), "{s:?}");
        order_cold(&pool, &mut s, true);
        assert!(s[..2].iter().all(|&n| n < 2), "{s:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn cache_none_prefers_an_uncached_slot_and_reports_reuse_otherwise() {
    let e = env_full(4, false, 10, true);
    let (p0, p2) = (e.warm(0), e.warm(2));
    e.set_keys(&p0, "x");
    // Both kinds are free: the uncached slot wins and nothing is reported.
    let a = e
        .engine
        .allocate(&sa("b"), &choice_req("none"))
        .await
        .unwrap();
    assert_eq!(a.pod().0, p2);
    assert_eq!(a.commit().cache, None);
    // Only a cached slot is free: allocate there rather than queue, and say so.
    let b = e
        .engine
        .allocate(&sa("b"), &choice_req("none"))
        .await
        .unwrap();
    assert_eq!(b.pod().0, p0);
    let cache = b.commit().cache.unwrap();
    assert!(cache.registry.is_none());
    assert_eq!(cache.local.unwrap().result, "reused");
    // The request left the slot's keys alone.
    assert_eq!(
        crate::render::pod_cache_keys(&e.fake.get(&p0).unwrap()),
        Some("x")
    );
    settle().await;
    assert!(!e.fake.calls().iter().any(|c| c.starts_with("annotate_pvc")));
    let mut reg = prometheus_client::registry::Registry::default();
    e.metrics.register(&mut reg);
    let mut m = String::new();
    prometheus_client::encoding::text::encode(&mut m, &reg).unwrap();
    assert!(!m.contains("result=\"reused\""), "{m}");
}

fn expires_ann(p: &Pod) -> OffsetDateTime {
    parse_ts(ann(p, crate::render::ANN_EXPIRES_AT).unwrap()).unwrap()
}

fn allocated_ann(p: &Pod) -> OffsetDateTime {
    parse_ts(ann(p, crate::render::ANN_ALLOCATED_AT).unwrap()).unwrap()
}

/// The `expires-at` value a patch writes.
fn written_expiry(patch: &Value) -> String {
    let op = patch
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["op"] != "test" && o["path"].as_str().unwrap().ends_with("expires-at"));
    op.unwrap()["value"].as_str().unwrap().to_owned()
}

#[tokio::test(start_paused = true)]
async fn warm_claim_is_short_until_the_commit_extends_it() {
    let e = env_with(4, false, 10);
    let name = e.warm(0);
    let a = e.engine.allocate(&sa("b"), &req(0)).await.unwrap();
    let patches = e.fake.w.lock().unwrap().patches.clone();
    assert_eq!(patches.len(), 2, "{patches:?}");
    // The claim holds the pod only for the commit window.
    let claimed = written_expiry(&patches[0]);
    let pod = e.fake.get(&name).unwrap();
    let window = parse_ts(&claimed).unwrap() - allocated_ann(&pod);
    assert!(
        (COMMIT_WINDOW..=COMMIT_WINDOW + time::Duration::SECOND).contains(&window),
        "{window}"
    );
    // The commit moves it to the real expiry, tested against the claim value,
    // before the response exists.
    let (uid, id) = (a.pod().1.to_owned(), a.response().id.clone());
    let expires = a.response().expires_at.clone();
    assert_eq!(patches[1], extend_patch(&uid, &id, &claimed, &expires));
    assert_eq!(
        ann(&pod, crate::render::ANN_EXPIRES_AT),
        Some(expires.as_str())
    );
    let _ = a.commit();
}

#[tokio::test(start_paused = true)]
async fn cold_claim_covers_the_start_timeout_then_the_commit_extends_it() {
    let e = env_with(2, true, 10);
    let engine = Arc::clone(&e.engine);
    let h = tokio::spawn(async move { engine.allocate(&sa("b"), &req(0)).await });
    tokio::time::sleep(Duration::from_secs(1)).await;
    let created = e
        .fake
        .calls()
        .iter()
        .find_map(|c| c.strip_prefix("create ").map(str::to_owned))
        .unwrap();
    let pod = e.fake.get(&created).unwrap();
    // `workers.startTimeout` defaults to 2m.
    let start = time::Duration::minutes(2) + COMMIT_WINDOW;
    let window = expires_ann(&pod) - allocated_ann(&pod);
    assert!(
        (start..=start + time::Duration::SECOND).contains(&window),
        "{window}"
    );
    e.fake.set_ready(&created);
    let a = h.await.unwrap().unwrap();
    let pod = e.fake.get(&created).unwrap();
    assert_eq!(
        ann(&pod, crate::render::ANN_EXPIRES_AT),
        Some(a.response().expires_at.as_str())
    );
    assert_eq!(
        expires_ann(&pod) - allocated_ann(&pod),
        time::Duration::minutes(10)
    );
    let _ = a.commit();
}

#[tokio::test(start_paused = true)]
async fn a_failed_commit_fails_the_request_and_retires_the_pod() {
    let cases = [
        (PatchOutcome::Lost, AllocError::PodLost),
        (PatchOutcome::Unknown, AllocError::UpstreamError),
        (PatchOutcome::Failed("boom".into()), AllocError::Internal),
    ];
    for (out, want) in cases {
        let e = env_with(4, false, 10);
        let name = e.warm(0);
        e.fake.w.lock().unwrap().extend = Some(out);
        let got = e.engine.allocate(&sa("b"), &req(0)).await.err();
        assert_eq!(got, Some(want.clone()));
        settle().await;
        let calls = e.fake.calls();
        assert!(
            calls.contains(&format!("patch {name} draining")),
            "{want:?}: {calls:?}"
        );
        assert!(e.fake.get(&name).is_none(), "{want:?}: {calls:?}");
    }
}

fn applies(doc: &mut Value, patch: &Value) -> bool {
    let p: json_patch::Patch = serde_json::from_value(patch.clone()).unwrap();
    json_patch::patch(doc, &p.0).is_ok()
}

proptest::proptest! {
    /// The leader's expiry retire and the engine's commit race on one pod.
    /// Whatever the order, exactly one lands, and a landed commit always
    /// survives.
    #[test]
    fn expiry_retire_and_commit_never_both_land(
        claim in 0i64..100_000,
        extra in 1i64..100_000,
        commit_first: bool,
    ) {
        use crate::reconcile::ops::retire_patch as leader_retire;
        let t = |s: i64| fmt_ts(OffsetDateTime::from_unix_timestamp(1_800_000_000 + s).unwrap());
        let (claimed, real) = (t(claim), t(claim + extra));
        let mut doc = json!({"metadata": {
            "uid": "U",
            "annotations": {ANN_ALLOC: "ID", crate::render::ANN_EXPIRES_AT: claimed},
            "labels": {crate::render::LABEL_STATE: "allocated"},
        }});
        let commit = extend_patch("U", "ID", &claimed, &real);
        let retire = leader_retire("U", "ID", Some(&claimed));
        let (a, b) = if commit_first { (&commit, &retire) } else { (&retire, &commit) };
        proptest::prop_assert!(applies(&mut doc, a));
        proptest::prop_assert!(!applies(&mut doc, b));
        let alloc = doc["metadata"]["annotations"][ANN_ALLOC].as_str().unwrap();
        proptest::prop_assert_eq!(alloc, if commit_first { "ID" } else { DRAINING });
    }
}

#[test]
fn an_untested_expiry_retire_would_reap_a_committed_allocation() {
    use crate::reconcile::ops::retire_patch as leader_retire;
    let mut doc = json!({"metadata": {
        "uid": "U",
        "annotations": {ANN_ALLOC: "ID", crate::render::ANN_EXPIRES_AT: "2027-01-01T00:01:00Z"},
        "labels": {crate::render::LABEL_STATE: "allocated"},
    }});
    let commit = extend_patch("U", "ID", "2027-01-01T00:01:00Z", "2027-01-02T00:00:00Z");
    assert!(applies(&mut doc, &commit));
    // Why the expiry retire carries the value: without it this lands.
    assert!(applies(&mut doc, &leader_retire("U", "ID", None)));
}

#[tokio::test(start_paused = true)]
async fn a_request_cancelled_mid_create_still_finishes_the_create() {
    let e = env_with(1, true, 10);
    // The create lands at once, but its answer takes 5 s.
    e.fake.w.lock().unwrap().create_delay = Duration::from_secs(5);
    let (who, r) = (sa("b"), req(0));
    // The client goes away while the pod create is in flight.
    assert!(
        tokio::time::timeout(Duration::from_secs(1), e.engine.allocate(&who, &r))
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_secs(10)).await;
    settle().await;
    let calls = e.fake.calls();
    assert_eq!(calls[0], "create p-0", "{calls:?}");
    // The sequence ran to the end: the Secret is the pod's, so the garbage
    // collector deletes it with the pod the guard then retires.
    assert!(
        calls[1].starts_with("secret create p-0-") && calls[1].ends_with(" Pod/p-0"),
        "{calls:?}"
    );
    assert!(
        calls.contains(&"patch p-0 draining".to_owned()),
        "{calls:?}"
    );
    assert!(calls.contains(&"delete p-0".to_owned()), "{calls:?}");
    assert!(e.fake.get("p-0").is_none());
}

#[tokio::test(start_paused = true)]
async fn a_refused_secret_fails_the_request_and_retires_the_pod_at_once() {
    let e = env_with(1, true, 10);
    e.fake.w.lock().unwrap().secret_err = Some("forbidden".into());
    let got = e.engine.allocate(&sa("b"), &req(0)).await.err();
    assert_eq!(got, Some(AllocError::Internal));
    settle().await;
    let calls = e.fake.calls();
    // No wait for startTimeout: the guard knows the pod's uid and retires it.
    assert!(
        calls.contains(&"patch p-0 draining".to_owned()),
        "{calls:?}"
    );
    assert!(e.fake.get("p-0").is_none(), "{calls:?}");
}

#[tokio::test(start_paused = true)]
async fn a_lost_create_answer_is_read_back_before_any_secret() {
    for landed in [true, false] {
        let e = env_with(1, true, 10);
        e.fake.w.lock().unwrap().lost_creates = Some(landed);
        let got = e.engine.allocate(&sa("b"), &req(0)).await.err();
        assert_eq!(got, Some(AllocError::UpstreamError), "landed {landed}");
        tokio::time::sleep(Duration::from_secs(15)).await;
        settle().await;
        let calls = e.fake.calls();
        assert_eq!(calls[..2], ["create p-0", "get p-0"], "{calls:?}");
        let secret = calls.iter().any(|c| c.starts_with("secret create"));
        // A pod that landed gets its Secret (owned by it) and is retired;
        // a pod that did not land gets no Secret at all.
        assert_eq!(secret, landed, "{calls:?}");
        assert!(e.fake.get("p-0").is_none(), "{calls:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn every_commit_moves_expires_at_even_for_the_shortest_ttl() {
    // TTL 60 s is shorter than the commit window: the claim still ends a
    // second early, so the commit always changes `expires-at` and every
    // leader retire that tested the claim value loses to it.
    let e = env_with(4, false, 10);
    let name = e.warm(0);
    let mut r = req(0);
    r.ttl_seconds = Some(60);
    let a = e.engine.allocate(&sa("b"), &r).await.unwrap();
    let patches = e.fake.w.lock().unwrap().patches.clone();
    assert_eq!(patches.len(), 2, "{patches:?}");
    let claimed = parse_ts(&written_expiry(&patches[0])).unwrap();
    let pod = e.fake.get(&name).unwrap();
    assert_eq!(claimed - allocated_ann(&pod), time::Duration::seconds(59));
    assert_eq!(
        expires_ann(&pod) - allocated_ann(&pod),
        time::Duration::seconds(60)
    );
    let _ = a.commit();
}
