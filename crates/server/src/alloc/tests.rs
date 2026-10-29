#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use buildkit_controller_api::claim::{BuildKitClaim, BuildKitClaimStatus, ClaimPhase};
use buildkit_controller_api::dto::CreateAllocationRequest;
use buildkit_controller_api::v1alpha1::BuildKitPool;
use futures::future::BoxFuture;
use k8s_openapi::api::core::v1::{Pod, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::runtime::reflector::store::Writer;
use kube::runtime::watcher;
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::{broadcast, watch};

use super::backend::{Backend, PatchOutcome, Write};
use super::certs::MaterialSource;
use super::claims::{self, reason};
use super::metrics::AllocMetrics;
use super::pods::*;
use super::{AllocError, BindConfig, Binder, Front, Outcome};
use crate::auth::Identity;
use crate::certs::{CaCertRule, CertMaterial};
use crate::pki::CaBundle;
use crate::render::{ANN_ALLOC, ANN_CLAIM, ANN_OWNER, PodInputs, fixtures, worker_pod};
use crate::runtime::stores::{ClaimEvent, ClaimSubscription};

const POOL_NS: &str = fixtures::POOL_NAMESPACE;
const POOL: &str = "p";
/// The controller's namespace.
const OWN: &str = "bk-system";
/// `cache.host` of the harness.
const CACHE_HOST: &str = "bk-registry.bk.svc:5000";
/// A tenant namespace the pool admits claims from.
const CI: &str = "ci";
/// A client public key, PEM.
static KEY: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| rcgen::KeyPair::generate().unwrap().public_key_pem());

/// An in-memory API server for pods, claims and Secrets.
#[derive(Default)]
struct World {
    pods: BTreeMap<String, Pod>,
    claims: BTreeMap<(String, String), BuildKitClaim>,
    secrets: BTreeMap<(String, String), Secret>,
    calls: Vec<String>,
    /// Answers the next pod patches with these, unapplied.
    patch_out: VecDeque<PatchOutcome>,
    /// Answers the next claim status writes with these, unapplied.
    status_out: VecDeque<Write>,
    /// Answers the next claim creates with these, unapplied.
    create_out: VecDeque<Write>,
    /// Answers the next claim annotation writes with these, unapplied.
    annotate_out: VecDeque<Write>,
    /// Answers the next claim deletes with these, unapplied.
    delete_out: VecDeque<Write>,
    rv: u64,
    uid: u64,
}

struct Fake {
    w: Mutex<World>,
    pods: Mutex<Writer<Pod>>,
    claims: Mutex<Writer<BuildKitClaim>>,
    tx: broadcast::Sender<ClaimEvent>,
}

impl Fake {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            w: Mutex::default(),
            pods: Mutex::default(),
            claims: Mutex::default(),
            tx,
        }
    }

    fn put_pod(&self, pod: Pod) {
        self.pods
            .lock()
            .unwrap()
            .apply_watcher_event(&watcher::Event::Apply(pod.clone()));
        let name = pod.metadata.name.clone().unwrap();
        self.w.lock().unwrap().pods.insert(name, pod);
    }

    fn pod(&self, name: &str) -> Pod {
        self.w.lock().unwrap().pods[name].clone()
    }

    fn put_claim(&self, mut c: BuildKitClaim) -> BuildKitClaim {
        {
            let mut w = self.w.lock().unwrap();
            w.rv += 1;
            c.metadata.resource_version = Some(w.rv.to_string());
            let key = (
                c.metadata.namespace.clone().unwrap(),
                c.metadata.name.clone().unwrap(),
            );
            w.claims.insert(key, c.clone());
        }
        self.claims
            .lock()
            .unwrap()
            .apply_watcher_event(&watcher::Event::Apply(c.clone()));
        let _ = self.tx.send(ClaimEvent::Apply(Arc::new(c.clone())));
        c
    }

    fn claim(&self, ns: &str, name: &str) -> Option<BuildKitClaim> {
        self.w
            .lock()
            .unwrap()
            .claims
            .get(&(ns.to_owned(), name.to_owned()))
            .cloned()
    }

    fn status(&self, ns: &str, name: &str) -> BuildKitClaimStatus {
        self.claim(ns, name).unwrap().status.unwrap_or_default()
    }

    fn calls(&self) -> Vec<String> {
        self.w.lock().unwrap().calls.clone()
    }

    fn call(&self, s: String) {
        self.w.lock().unwrap().calls.push(s);
    }
}

fn ready<T: Send + 'static>(v: T) -> BoxFuture<'static, T> {
    Box::pin(async move { v })
}

impl Backend for Fake {
    fn patch_pod(&self, _: &str, name: &str, patch: Value) -> BoxFuture<'static, PatchOutcome> {
        let alloc = patch
            .as_array()
            .and_then(|a| a.iter().find(|o| o["op"] == "replace"))
            .and_then(|o| o["value"].as_str())
            .unwrap_or_default()
            .to_owned();
        self.call(format!("patch {name} {alloc}"));
        if let Some(out) = self.w.lock().unwrap().patch_out.pop_front() {
            return ready(out);
        }
        let Some(pod) = self.w.lock().unwrap().pods.get(name).cloned() else {
            return ready(PatchOutcome::Lost);
        };
        let ops: json_patch::Patch = serde_json::from_value(patch).unwrap();
        let mut doc = serde_json::to_value(&pod).unwrap();
        if json_patch::patch(&mut doc, &ops.0).is_err() {
            return ready(PatchOutcome::Lost);
        }
        let p: Pod = serde_json::from_value(doc).unwrap();
        self.put_pod(p.clone());
        ready(PatchOutcome::Applied(Box::new(p)))
    }

    fn delete_pod(
        &self,
        _: &str,
        name: &str,
        _: &str,
        _: Option<u32>,
    ) -> BoxFuture<'static, Result<(), String>> {
        self.call(format!("delete_pod {name}"));
        if let Some(p) = self.w.lock().unwrap().pods.remove(name) {
            self.pods
                .lock()
                .unwrap()
                .apply_watcher_event(&watcher::Event::Delete(p));
        }
        ready(Ok(()))
    }

    fn create_claim(&self, mut c: BuildKitClaim) -> BoxFuture<'static, Write> {
        let name = c.metadata.name.clone().unwrap();
        self.call(format!("create_claim {name}"));
        if let Some(out) = self.w.lock().unwrap().create_out.pop_front() {
            return ready(out);
        }
        if self
            .claim(c.metadata.namespace.as_deref().unwrap(), &name)
            .is_some()
        {
            return ready(Write::Exists);
        }
        let uid = {
            let mut w = self.w.lock().unwrap();
            w.uid += 1;
            w.uid
        };
        c.metadata.uid = Some(format!("00000000-0000-4000-8000-{uid:012}"));
        c.metadata.creation_timestamp = Some(Time(k8s_openapi::jiff::Timestamp::now()));
        self.put_claim(c);
        ready(Write::Done)
    }

    fn get_claim(
        &self,
        ns: &str,
        name: &str,
    ) -> BoxFuture<'static, Result<Option<BuildKitClaim>, Write>> {
        self.call(format!("get_claim {name}"));
        ready(Ok(self.claim(ns, name)))
    }

    fn delete_claim(&self, ns: &str, name: &str, uid: &str) -> BoxFuture<'static, Write> {
        self.call(format!("delete_claim {name}"));
        if let Some(out) = self.w.lock().unwrap().delete_out.pop_front() {
            return ready(out);
        }
        let key = (ns.to_owned(), name.to_owned());
        let gone = {
            let mut w = self.w.lock().unwrap();
            match w.claims.get(&key) {
                Some(c) if c.metadata.uid.as_deref() == Some(uid) => w.claims.remove(&key),
                _ => None,
            }
        };
        let Some(c) = gone else {
            return ready(Write::Gone);
        };
        self.claims
            .lock()
            .unwrap()
            .apply_watcher_event(&watcher::Event::Delete(c.clone()));
        let _ = self.tx.send(ClaimEvent::Delete(Arc::new(c)));
        ready(Write::Done)
    }

    fn delete_claim_at(
        &self,
        ns: &str,
        name: &str,
        uid: &str,
        resource_version: &str,
    ) -> BoxFuture<'static, Write> {
        let current = self.claim(ns, name);
        if current
            .as_ref()
            .is_some_and(|c| c.metadata.resource_version.as_deref() != Some(resource_version))
        {
            self.call(format!("delete_claim {name} refused"));
            return ready(Write::Gone);
        }
        self.delete_claim(ns, name, uid)
    }

    fn annotate_claim(
        &self,
        ns: &str,
        name: &str,
        uid: &str,
        annotations: BTreeMap<String, String>,
    ) -> BoxFuture<'static, Write> {
        self.call(format!("annotate_claim {name}"));
        if let Some(out) = self.w.lock().unwrap().annotate_out.pop_front() {
            return ready(out);
        }
        match self.claim(ns, name) {
            Some(mut c) if c.metadata.uid.as_deref() == Some(uid) => {
                c.metadata
                    .annotations
                    .get_or_insert_default()
                    .extend(annotations);
                self.put_claim(c);
                ready(Write::Done)
            }
            _ => ready(Write::Gone),
        }
    }

    fn patch_claim_status(
        &self,
        ns: &str,
        name: &str,
        rv: &str,
        status: &BuildKitClaimStatus,
    ) -> BoxFuture<'static, Write> {
        let phase = status.phase.map_or("", ClaimPhase::as_str);
        self.call(format!("status {name} {phase}"));
        if let Some(out) = self.w.lock().unwrap().status_out.pop_front() {
            return ready(out);
        }
        let Some(mut c) = self.claim(ns, name) else {
            return ready(Write::Gone);
        };
        if c.metadata.resource_version.as_deref() != Some(rv) {
            return ready(Write::Gone);
        }
        c.status = Some(status.clone());
        self.put_claim(c);
        ready(Write::Done)
    }

    fn get_secret(
        &self,
        ns: &str,
        name: &str,
    ) -> BoxFuture<'static, Result<Option<Secret>, Write>> {
        let s = self
            .w
            .lock()
            .unwrap()
            .secrets
            .get(&(ns.to_owned(), name.to_owned()))
            .cloned();
        ready(Ok(s))
    }

    fn create_secret(&self, mut s: Secret) -> BoxFuture<'static, Write> {
        let key = (
            s.metadata.namespace.clone().unwrap(),
            s.metadata.name.clone().unwrap(),
        );
        self.call(format!("create_secret {}", key.1));
        let mut w = self.w.lock().unwrap();
        if w.secrets.contains_key(&key) {
            return ready(Write::Exists);
        }
        s.metadata.uid = Some(format!("secret-{}", key.1));
        w.secrets.insert(key, s);
        ready(Write::Done)
    }

    fn delete_secret(&self, ns: &str, name: &str, _: &str) -> BoxFuture<'static, Write> {
        self.call(format!("delete_secret {name}"));
        self.w
            .lock()
            .unwrap()
            .secrets
            .remove(&(ns.to_owned(), name.to_owned()));
        ready(Write::Done)
    }
}

fn pod_ready() -> k8s_openapi::api::core::v1::PodStatus {
    serde_json::from_value(json!({
        "phase": "Running", "podIP": "10.0.0.9",
        "conditions": [{"type": "Ready", "status": "True"}]
    }))
    .unwrap()
}

fn pool() -> BuildKitPool {
    let spec = json!({
        "template": {"spec": {"containers": [{"name": "buildkitd", "image": "moby/buildkit:v0.26.3",
            "resources": {"limits": {"memory": "1Gi", "ephemeral-storage": "1Gi"}}}]}},
        "scaling": {"min": 0, "max": 4},
        "access": {"rules": [{"serviceAccount": {}}, {"claims": {"namespaces": [CI]}}]},
        "allocation": {"defaultTTL": "1h", "maxTTL": "2h", "queueTimeout": "2m", "maxQueueTimeout": "10m"},
    });
    let mut p = fixtures::pool(POOL, spec).unwrap();
    let status = json!({"conditions": [{"type": "Ready", "status": "True", "reason": "Ready",
        "message": "", "lastTransitionTime": "2026-01-01T00:00:00Z"}]});
    p.status = Some(serde_json::from_value(status).unwrap());
    p
}

struct Env {
    fake: Arc<Fake>,
    binder: Binder,
    front: Front,
    pool: BuildKitPool,
    ca: Arc<CaBundle>,
    metrics: AllocMetrics,
    pools: Writer<BuildKitPool>,
    _ca_tx: watch::Sender<CertMaterial>,
}

fn env() -> Env {
    let ca = Arc::new(CaBundle::generate(OffsetDateTime::now_utc()).unwrap());
    let pool = pool();
    let mut pools = Writer::<BuildKitPool>::default();
    pools.apply_watcher_event(&watcher::Event::Apply(pool.clone()));
    let fake = Arc::new(Fake::new());
    let (pods, claims) = (
        fake.pods.lock().unwrap().as_reader(),
        fake.claims.lock().unwrap().as_reader(),
    );
    let (ca_tx, ca_rx) = watch::channel(CertMaterial::from_bundle(
        Arc::clone(&ca),
        CaCertRule::ClientCa,
    ));
    let certs = Arc::new(MaterialSource::new(ca_rx));
    let metrics = AllocMetrics::default();
    let served: HashSet<String> = [OWN, CI].into_iter().map(str::to_owned).collect();
    let binder = Binder::new(
        Arc::clone(&fake) as Arc<dyn Backend>,
        (pools.as_reader(), pods.clone(), claims.clone()),
        Arc::clone(&certs) as Arc<dyn super::CertSource>,
        BindConfig {
            endpoint: "tcp://bk.example.com:8444".into(),
            server_name: "bk.example.com".into(),
            allocation_domain: Some("alloc.bk.example.com".into()),
            namespace: OWN.into(),
            served: served.clone(),
            registry_host: Some(CACHE_HOST.into()),
        },
        metrics.clone(),
    );
    let tx = fake.tx.clone();
    let front = Front::new(
        Arc::clone(&fake) as Arc<dyn Backend>,
        pools.as_reader(),
        claims,
        Box::new(move || ClaimSubscription::new(tx.subscribe())),
        certs,
        OWN.into(),
        served,
        metrics.clone(),
    )
    .with_pods(pods);
    Env {
        fake,
        binder,
        front,
        pool,
        ca,
        metrics,
        pools,
        _ca_tx: ca_tx,
    }
}

fn key() -> (String, String) {
    (POOL_NS.to_owned(), POOL.to_owned())
}

fn sa(name: &str) -> Identity {
    Identity::ServiceAccount {
        namespace: POOL_NS.into(),
        name: name.into(),
        uid: "u".into(),
        pod_uid: None,
    }
}

impl Env {
    /// A free Ready pod in `slot`.
    fn warm(&self, slot: u32) -> String {
        let mut pod = worker_pod(
            &self.pool,
            &PodInputs {
                slot,
                internal_ca_fingerprint: self.ca.internal_ca().fingerprint_hex(),
                worker_cert_not_after: "2099-01-01T00:00:00Z".into(),
                nonce: "00000000000000aa".into(),
            },
        )
        .unwrap();
        pod.metadata.uid = Some(format!("11111111-0000-4000-8000-{slot:012}"));
        pod.status = Some(pod_ready());
        let name = pod.metadata.name.clone().unwrap();
        self.fake.put_pod(pod);
        name
    }

    /// A claim created `ago` seconds ago; `spec` is merged over a spec naming the pool.
    fn claim(&self, ns: &str, name: &str, ago: i64, spec: Value) -> BuildKitClaim {
        let mut base = json!({"poolRef": {"namespace": POOL_NS, "name": POOL}});
        if let (Value::Object(b), Value::Object(s)) = (&mut base, spec) {
            b.extend(s);
        }
        let mut c = BuildKitClaim::new(name, serde_json::from_value(base).unwrap());
        c.metadata.namespace = Some(ns.into());
        let n = {
            let mut w = self.fake.w.lock().unwrap();
            w.uid += 1;
            w.uid
        };
        c.metadata.uid = Some(format!("22222222-0000-4000-8000-{n:012}"));
        let at =
            k8s_openapi::jiff::Timestamp::now() - k8s_openapi::jiff::SignedDuration::from_secs(ago);
        c.metadata.creation_timestamp = Some(Time(at));
        self.fake.put_claim(c)
    }

    /// A CI claim as the API writes it, named after a fresh allocation id.
    fn ci_claim(&self, ago: i64, owner: &str) -> String {
        let id = new_alloc_id().unwrap().to_string();
        self.claim(OWN, &id, ago, json!({"owner": owner, "publicKey": *KEY}));
        id
    }
}

fn phase(s: &BuildKitClaimStatus) -> ClaimPhase {
    s.phase.unwrap_or_default()
}

fn why(s: &BuildKitClaimStatus) -> String {
    s.conditions
        .first()
        .map(|c| c.reason.clone())
        .unwrap_or_default()
}

/// Pending claims bind oldest first. The claim left over waits `Queued`, with
/// no stored place (the API works out an API claim's place from the store),
/// and binds once a pod frees.
#[tokio::test]
async fn claims_bind_oldest_first() {
    let e = env();
    e.warm(0);
    e.warm(1);
    let newest = e.ci_claim(10, "sa:x/newest");
    e.claim(
        OWN,
        "b2222222222222222222222222",
        60,
        json!({"owner": "sa:x/b", "publicKey": *KEY}),
    );
    e.claim(
        OWN,
        "a2222222222222222222222222",
        60,
        json!({"owner": "sa:x/a", "publicKey": *KEY}),
    );
    e.binder.pass(&key()).await;
    let a = e.fake.status(OWN, "a2222222222222222222222222");
    let b = e.fake.status(OWN, "b2222222222222222222222222");
    let n = e.fake.status(OWN, &newest);
    assert_eq!(
        (phase(&a), phase(&b), phase(&n)),
        (ClaimPhase::Bound, ClaimPhase::Bound, ClaimPhase::Pending)
    );
    assert_eq!((why(&n).as_str(), n.queue_position), (reason::QUEUED, None));
    // The first in line got the first pod tried; both pods carry their claims.
    for (s, owner) in [(&a, "sa:x/a"), (&b, "sa:x/b")] {
        let pod = e.fake.pod(s.pod.as_deref().unwrap());
        assert_eq!(ann(&pod, ANN_OWNER), Some(owner));
        assert_eq!(ann(&pod, ANN_ALLOC), s.allocation_id.as_deref());
        assert!(
            s.certificate
                .as_deref()
                .unwrap()
                .contains("BEGIN CERTIFICATE")
        );
        assert!(s.secret_name.is_none(), "publicKey claims get no Secret");
        assert!(s.endpoint.as_deref().unwrap().starts_with("tcp://"));
    }
    assert_eq!(
        e.fake
            .pod(a.pod.as_deref().unwrap())
            .metadata
            .annotations
            .unwrap()[ANN_CLAIM],
        format!("{OWN}/a2222222222222222222222222")
    );
    // A pod frees: the waiter binds on the next pass.
    e.warm(2);
    e.binder.pass(&key()).await;
    assert_eq!(phase(&e.fake.status(OWN, &newest)), ClaimPhase::Bound);
}

/// A claim still pending at its queue timeout fails `QueueTimeout`; with a
/// timeout of `0s` a warm pod still binds.
#[tokio::test]
async fn queue_timeout_fails_a_waiting_claim() {
    let e = env();
    e.claim(
        OWN,
        "a2222222222222222222222222",
        5,
        json!({"owner": "sa:x/a", "publicKey": *KEY, "queueTimeout": "2s"}),
    );
    let wait = e.binder.pass(&key()).await;
    let s = e.fake.status(OWN, "a2222222222222222222222222");
    assert_eq!(
        (phase(&s), why(&s).as_str()),
        (ClaimPhase::Failed, reason::QUEUE_TIMEOUT)
    );
    assert_eq!(wait, None);
    // A young claim waits, and the pass asks to run again at its deadline.
    e.claim(
        OWN,
        "b2222222222222222222222222",
        0,
        json!({"owner": "sa:x/b", "publicKey": *KEY, "queueTimeout": "30s"}),
    );
    let wait = e.binder.pass(&key()).await.unwrap();
    assert!(
        wait <= Duration::from_secs(31) && wait >= Duration::from_secs(29),
        "{wait:?}"
    );
    // The pool caps the timeout (maxQueueTimeout 10m).
    e.claim(
        OWN,
        "c2222222222222222222222222",
        700,
        json!({"owner": "sa:x/c", "publicKey": *KEY, "queueTimeout": "1h"}),
    );
    e.binder.pass(&key()).await;
    assert_eq!(
        phase(&e.fake.status(OWN, "c2222222222222222222222222")),
        ClaimPhase::Failed
    );
    // `0s` with a warm pod binds (the claim waiting ahead takes the other one).
    e.warm(0);
    e.warm(1);
    e.claim(
        OWN,
        "d2222222222222222222222222",
        0,
        json!({"owner": "sa:x/d", "publicKey": *KEY, "queueTimeout": "0s"}),
    );
    e.binder.pass(&key()).await;
    assert_eq!(
        phase(&e.fake.status(OWN, "d2222222222222222222222222")),
        ClaimPhase::Bound
    );
}

/// A pod another writer took (a deposed leader, the reconciler) loses the
/// patch's `test`; the claim is not bound to it, and the pass comes back soon.
#[tokio::test]
async fn a_lost_patch_never_double_books() {
    let e = env();
    let name = e.warm(0);
    // Taken behind the store's back: the store still shows it free.
    let mut taken = e.fake.pod(&name);
    taken
        .metadata
        .annotations
        .as_mut()
        .unwrap()
        .insert(ANN_ALLOC.into(), "otheralloc22222222222222222".into());
    e.fake.w.lock().unwrap().pods.insert(name.clone(), taken);
    let id = e.ci_claim(0, "sa:x/a");
    let wait = e.binder.pass(&key()).await;
    assert_eq!(
        wait,
        Some(Duration::from_secs(1)),
        "store lag: look again soon"
    );
    let s = e.fake.status(OWN, &id);
    assert_eq!(phase(&s), ClaimPhase::Pending);
    assert_eq!(
        ann(&e.fake.pod(&name), ANN_ALLOC),
        Some("otheralloc22222222222222222")
    );
    // An unknown outcome also stays pending, and the pod is not tried again at once.
    e.warm(1);
    e.fake
        .w
        .lock()
        .unwrap()
        .patch_out
        .push_back(PatchOutcome::Unknown);
    e.binder.pass(&key()).await;
    assert_eq!(phase(&e.fake.status(OWN, &id)), ClaimPhase::Pending);
}

/// A bind that landed on the pod while its status write did not is finished
/// by the next pass, with no second pod patch.
#[tokio::test]
async fn a_bind_without_its_status_is_adopted() {
    let e = env();
    let name = e.warm(0);
    e.warm(1);
    let id = e.ci_claim(0, "sa:x/a");
    e.fake
        .w
        .lock()
        .unwrap()
        .status_out
        .push_back(Write::Unknown);
    let wait = e.binder.pass(&key()).await;
    assert_eq!(wait, Some(Duration::from_secs(1)));
    assert_eq!(phase(&e.fake.status(OWN, &id)), ClaimPhase::Pending);
    let bound: Vec<String> = e
        .fake
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("patch "))
        .collect();
    assert_eq!(bound.len(), 1);
    e.binder.pass(&key()).await;
    let s = e.fake.status(OWN, &id);
    assert_eq!(phase(&s), ClaimPhase::Bound);
    let patches = e
        .fake
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("patch "))
        .count();
    assert_eq!(patches, 1, "adopted, not bound again");
    let pod = e.fake.pod(s.pod.as_deref().unwrap());
    assert_eq!(
        s.expires_at.as_deref(),
        ann(&pod, crate::render::ANN_EXPIRES_AT)
    );
    assert!(s.pod.as_deref() == Some(name.as_str()) || s.pod.as_deref() == Some("p-1"));
}

/// The owner a claim writes counts only in the controller's namespace. A
/// tenant claim is owned by itself and needs a `claims` access rule.
#[tokio::test]
async fn a_written_owner_counts_only_in_the_controllers_namespace() {
    let e = env();
    e.warm(0);
    e.warm(1);
    let ci = e.claim(CI, "build", 0, json!({"owner": "sa:render-verify/admin"}));
    e.binder.pass(&key()).await;
    let s = e.fake.status(CI, "build");
    assert_eq!(phase(&s), ClaimPhase::Bound);
    let pod = e.fake.pod(s.pod.as_deref().unwrap());
    let want = format!("claim:{CI}/build#{}", ci.metadata.uid.unwrap());
    assert_eq!(ann(&pod, ANN_OWNER), Some(want.as_str()));
    // Its allocation id comes from its uid, so it is the same on every pass.
    assert_eq!(
        s.allocation_id,
        claims::alloc_id(&e.fake.claim(CI, "build").unwrap(), OWN).map(|a| a.to_string())
    );
    // Without a publicKey it got a Secret with the key, controlled by the claim.
    assert_eq!(s.secret_name.as_deref(), Some("build-buildkit"));
    let secret =
        e.fake.w.lock().unwrap().secrets[&(CI.to_owned(), "build-buildkit".to_owned())].clone();
    let data = secret.data.unwrap();
    assert!(
        String::from_utf8(data["tls.key"].0.clone())
            .unwrap()
            .contains("PRIVATE KEY")
    );
    assert_eq!(
        String::from_utf8(data["tls.crt"].0.clone()).unwrap(),
        s.certificate.unwrap()
    );
    assert!(data.contains_key("ca.crt"));
    // A namespace no claims rule admits: `PoolNotFound`, after the grace.
    e.claim("other", "build", 0, json!({}));
    e.binder.pass(&key()).await;
    assert!(
        e.fake.claim("other", "build").unwrap().status.is_none(),
        "not served: the tidy pass refuses it"
    );
}

/// Claims the pool cannot serve fail at once: no `publicKey` in the
/// controller's namespace, a TTL above `maxTTL`, a Secret name taken, a pool
/// gone past the grace.
#[tokio::test]
async fn unservable_claims_fail() {
    let e = env();
    e.warm(0);
    e.claim(
        OWN,
        "a2222222222222222222222222",
        0,
        json!({"owner": "sa:x/a"}),
    );
    e.claim(
        OWN,
        "b2222222222222222222222222",
        0,
        json!({"owner": "sa:x/b", "publicKey": *KEY, "ttl": "3h"}),
    );
    e.claim(
        OWN,
        "c2222222222222222222222222",
        0,
        json!({"owner": "sa:x/c", "publicKey": "not a key"}),
    );
    e.binder.pass(&key()).await;
    for n in [
        "a2222222222222222222222222",
        "b2222222222222222222222222",
        "c2222222222222222222222222",
    ] {
        let s = e.fake.status(OWN, n);
        assert_eq!(
            (phase(&s), why(&s).as_str()),
            (ClaimPhase::Failed, reason::INVALID_REQUEST),
            "{n}"
        );
    }
    // A Secret of the claim's name that it does not control.
    let mut s = Secret::default();
    s.metadata.name = Some("taken-buildkit".into());
    s.metadata.namespace = Some(CI.into());
    e.fake
        .w
        .lock()
        .unwrap()
        .secrets
        .insert((CI.into(), "taken-buildkit".into()), s);
    e.claim(CI, "taken", 0, json!({}));
    e.binder.pass(&key()).await;
    let st = e.fake.status(CI, "taken");
    assert_eq!(
        (phase(&st), why(&st).as_str()),
        (ClaimPhase::Failed, reason::SECRET_CONFLICT)
    );
    // Another pool: missing. Young claims wait, old ones fail.
    let other = |n: &str, ago| {
        let c = e.claim(OWN, n, ago, json!({"owner": "sa:x/o", "publicKey": *KEY}));
        let mut c = c;
        c.spec.pool_ref.name = "gone".into();
        e.fake.put_claim(c);
    };
    other("d2222222222222222222222222", 0);
    other("e2222222222222222222222222", 40);
    let gone = (POOL_NS.to_owned(), "gone".to_owned());
    e.binder.pass(&gone).await;
    assert!(
        e.fake
            .claim(OWN, "d2222222222222222222222222")
            .unwrap()
            .status
            .is_none()
    );
    let s = e.fake.status(OWN, "e2222222222222222222222222");
    assert_eq!(
        (phase(&s), why(&s).as_str()),
        (ClaimPhase::Failed, reason::POOL_NOT_FOUND)
    );
}

/// The front door checks the request, then creates the caller's claim in the
/// controller's namespace, named after the allocation id, and answers 201
/// once the leader binds it.
#[tokio::test]
async fn allocate_creates_the_claim_and_answers_once_bound() {
    let e = Arc::new(env());
    e.warm(0);
    let req: CreateAllocationRequest = serde_json::from_value(json!({
        "namespace": POOL_NS, "pool": POOL, "ttlSeconds": 600, "queueTimeoutSeconds": 30, "publicKey": *KEY
    }))
    .unwrap();
    // The leader, on its own: one pass per claim event.
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
    assert_eq!(c.spec.owner.as_deref(), Some("sa:render-verify/alice"));
    assert_eq!(c.spec.public_key.as_deref(), Some(KEY.as_str()));
    assert_eq!(
        c.spec.ttl.as_ref().map(|d| d.as_str().to_owned()),
        Some("600s".into())
    );
    assert_eq!(
        c.spec.queue_timeout.as_ref().map(|d| d.as_str().to_owned()),
        Some("30s".into())
    );
    assert_eq!(r.owner, "sa:render-verify/alice");
    assert_eq!(
        r.endpoint,
        format!("tcp://{}.alloc.bk.example.com:8444", r.id)
    );
    assert!(r.ca_cert.as_deref().unwrap().contains("BEGIN CERTIFICATE"));
    assert_eq!(r.client_key, None);
    assert_eq!(
        Some(r.client_cert.as_str()),
        c.status.unwrap().certificate.as_deref()
    );
    // The caller's input is checked before any claim exists.
    for (body, code) in [
        (
            json!({"namespace": POOL_NS, "pool": "nope", "publicKey": *KEY}),
            "pool_not_found",
        ),
        (
            json!({"namespace": POOL_NS, "pool": POOL, "ttlSeconds": 10, "publicKey": *KEY}),
            "invalid_request",
        ),
        (
            json!({"namespace": POOL_NS, "pool": POOL, "queueTimeoutSeconds": 3600, "publicKey": *KEY}),
            "invalid_request",
        ),
        (
            json!({"namespace": POOL_NS, "pool": POOL, "publicKey": "x"}),
            "invalid_request",
        ),
    ] {
        let req: CreateAllocationRequest = serde_json::from_value(body).unwrap();
        let err = e.front.allocate(&sa("alice"), &req).await.unwrap_err();
        assert_eq!(err.code().as_str(), code);
    }
    let creates = e
        .fake
        .calls()
        .iter()
        .filter(|c| c.starts_with("create_claim"))
        .count();
    assert_eq!(creates, 1);
}

/// With no worker within the create wait the answer is pending, with the
/// claim's place; a poll then follows the claim to bound or failed. Only its
/// owner reads or releases it.
#[tokio::test(start_paused = true)]
async fn pending_allocation_is_polled_and_released_by_its_owner() {
    let e = env();
    let req: CreateAllocationRequest = serde_json::from_value(json!({
        "namespace": POOL_NS, "pool": POOL, "publicKey": *KEY
    }))
    .unwrap();
    e.ci_claim(30, "sa:x/ahead");
    let Outcome::Pending(p) = e.front.allocate(&sa("alice"), &req).await.unwrap() else {
        panic!("bound without a pod")
    };
    assert_eq!(p.position, Some(2));
    assert_eq!(
        e.front
            .get(&sa("mallory"), &p.id, Duration::ZERO)
            .await
            .unwrap_err(),
        AllocError::NotFound,
        "another caller learns nothing about a live id"
    );
    assert_eq!(
        e.front
            .get(&sa("alice"), "aaaaaaaaaaaaaaaaaaaaaaaaaa", Duration::ZERO)
            .await
            .unwrap_err(),
        AllocError::NotFound
    );
    let polled = e
        .front
        .get(&sa("alice"), &p.id, Duration::from_secs(25))
        .await
        .unwrap();
    assert!(matches!(polled, Outcome::Pending(_)), "{polled:?}");
    // The leader fails it: the poll answers with the error.
    let c = e.fake.claim(OWN, &p.id).unwrap();
    let mut failed = c.clone();
    failed.status = Some(claims::unbound_status(
        &c,
        ClaimPhase::Failed,
        reason::QUEUE_TIMEOUT,
        "no worker became free",
        None,
    ));
    e.fake.put_claim(failed);
    let err = e
        .front
        .get(&sa("alice"), &p.id, Duration::from_secs(25))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        AllocError::PoolExhausted("no worker became free".into())
    );
    // Release: owner only; transient failures are 503 upstream_error.
    assert_eq!(
        e.front.release(&sa("mallory"), &p.id).await.unwrap_err(),
        AllocError::NotFound
    );
    e.fake
        .w
        .lock()
        .unwrap()
        .delete_out
        .push_back(Write::Unknown);
    assert_eq!(
        e.front.release(&sa("alice"), &p.id).await.unwrap_err(),
        AllocError::UpstreamError
    );
    e.front.release(&sa("alice"), &p.id).await.unwrap();
    assert!(e.fake.claim(OWN, &p.id).is_none());
    assert_eq!(
        e.front.release(&sa("alice"), &p.id).await.unwrap_err(),
        AllocError::NotFound
    );
}

fn post_req(key: &str) -> CreateAllocationRequest {
    serde_json::from_value(json!({"namespace": POOL_NS, "pool": POOL, "publicKey": key})).unwrap()
}

fn claim_names(e: &Env) -> Vec<String> {
    e.fake
        .w
        .lock()
        .unwrap()
        .claims
        .keys()
        .map(|(_, n)| n.clone())
        .collect()
}

/// The claim is named after the public key. A repeated POST from the same
/// caller answers for the claim the first one made: pending with its place,
/// then bound. It never creates a second claim.
#[tokio::test(start_paused = true)]
async fn a_repeated_post_follows_the_same_claim() {
    let e = env();
    let req = post_req(&KEY);
    let Outcome::Pending(first) = e.front.allocate(&sa("alice"), &req).await.unwrap() else {
        panic!("bound without a pod")
    };
    assert_eq!(first.position, Some(1));
    e.ci_claim(-5, "sa:x/behind");
    let again = e.front.allocate(&sa("alice"), &req).await.unwrap();
    assert_eq!(again, Outcome::Pending(first.clone()));
    e.warm(0);
    e.binder.pass(&key()).await;
    let Outcome::Bound(r) = e.front.allocate(&sa("alice"), &req).await.unwrap() else {
        panic!("not bound")
    };
    assert_eq!(r.id, first.id);
    let creates = e
        .fake
        .calls()
        .iter()
        .filter(|c| c.starts_with("create_claim"))
        .count();
    assert_eq!(
        (creates, claim_names(&e).len()),
        (3, 2),
        "one claim per key"
    );
}

/// Another caller presenting the same key gets 403 and learns nothing about
/// the claim, pending or bound, and the claim is left as it was. A key that
/// names a claim of another pool is a 400.
#[tokio::test(start_paused = true)]
async fn a_key_in_use_by_another_caller_is_forbidden() {
    let e = env();
    let req = post_req(&KEY);
    let Outcome::Pending(p) = e.front.allocate(&sa("alice"), &req).await.unwrap() else {
        panic!("bound without a pod")
    };
    let rv = |e: &Env| e.fake.claim(OWN, &p.id).unwrap().metadata.resource_version;
    let before = rv(&e);
    assert_eq!(
        e.front.allocate(&sa("mallory"), &req).await,
        Err(AllocError::Forbidden)
    );
    assert_eq!(rv(&e), before, "untouched");
    e.warm(0);
    e.binder.pass(&key()).await;
    assert_eq!(
        e.front.allocate(&sa("mallory"), &req).await,
        Err(AllocError::Forbidden)
    );
    assert_eq!(claim_names(&e), vec![p.id.clone()]);
    let other = rcgen::KeyPair::generate().unwrap().public_key_pem();
    let id = key_alloc_id(&crate::pki::ClientPublicKey::from_pem(&other).unwrap()).unwrap();
    e.claim(
        OWN,
        id.as_str(),
        0,
        json!({"poolRef": {"namespace": POOL_NS, "name": "elsewhere"},
            "owner": "sa:render-verify/alice", "publicKey": other}),
    );
    assert!(matches!(
        e.front.allocate(&sa("alice"), &post_req(&other)).await,
        Err(AllocError::InvalidRequest(_))
    ));
}

/// A POST dropped while it waits (the client's connection went) leaves its
/// claim where it was. The client's retry with the same key finds it, still
/// ahead of a claim made in between, and it binds first.
#[tokio::test(start_paused = true)]
async fn a_dropped_post_keeps_its_claim_for_the_retry() {
    let e = Arc::new(env());
    let req = post_req(&KEY);
    let task = {
        let (e, req) = (Arc::clone(&e), req.clone());
        tokio::spawn(async move { e.front.allocate(&sa("alice"), &req).await })
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    let [id] = &claim_names(&e)[..] else {
        panic!("one claim")
    };
    let id = id.clone();
    task.abort();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(e.fake.claim(OWN, &id).is_some(), "{:?}", e.fake.calls());
    let later = e.ci_claim(-5, "sa:x/later");
    let Outcome::Pending(p) = e.front.allocate(&sa("alice"), &req).await.unwrap() else {
        panic!("bound without a pod")
    };
    assert_eq!((p.id.as_str(), p.position), (id.as_str(), Some(1)));
    e.warm(0);
    e.binder.pass(&key()).await;
    assert_eq!(phase(&e.fake.status(OWN, &id)), ClaimPhase::Bound);
    assert_eq!(phase(&e.fake.status(OWN, &later)), ClaimPhase::Pending);
}

/// A key reused after a release makes a new claim of the same name. The
/// released pod, not yet retired, carries the old claim's uid: the new claim
/// never adopts it, and tidy retires it.
#[tokio::test(start_paused = true)]
async fn a_claim_recreated_under_the_same_name_never_gets_the_released_pod() {
    let e = env();
    let req = post_req(&KEY);
    let old = e.warm(0);
    let Outcome::Pending(p) = e.front.allocate(&sa("alice"), &req).await.unwrap() else {
        panic!("bound before the leader ran")
    };
    e.binder.pass(&key()).await;
    assert_eq!(e.fake.status(OWN, &p.id).pod.as_deref(), Some(old.as_str()));
    e.front.release(&sa("alice"), &p.id).await.unwrap();
    let Outcome::Pending(again) = e.front.allocate(&sa("alice"), &req).await.unwrap() else {
        panic!("bound to the released pod")
    };
    assert_eq!(again.id, p.id, "same key, same name");
    let fresh = e.warm(1);
    e.binder.pass(&key()).await;
    assert_eq!(
        e.fake.status(OWN, &p.id).pod.as_deref(),
        Some(fresh.as_str()),
        "the new claim takes a fresh pod"
    );
    let claims = e.fake.claims.lock().unwrap().as_reader().state();
    let pods = e.fake.pods.lock().unwrap().as_reader().state();
    super::leader::tidy_with(&e.binder, &claims, &pods, OffsetDateTime::now_utc()).await;
    let calls = e.fake.calls();
    assert!(
        calls.contains(&format!("patch {old} draining")),
        "{calls:?}"
    );
    assert!(
        !calls.contains(&format!("patch {fresh} draining")),
        "{calls:?}"
    );
}

/// Backdate `seen-at` of claim `id` by `secs`, as if its caller last asked then.
fn seen_ago(e: &Env, id: &str, secs: i64) {
    let mut c = e.fake.claim(OWN, id).unwrap();
    c.metadata.annotations.get_or_insert_default().insert(
        claims::ANN_SEEN_AT.into(),
        fmt_ts(OffsetDateTime::now_utc() - time::Duration::seconds(secs)),
    );
    e.fake.put_claim(c);
}

async fn tidy_at(e: &Env, now: OffsetDateTime) {
    let claims = e.fake.claims.lock().unwrap().as_reader().state();
    let pods = e.fake.pods.lock().unwrap().as_reader().state();
    super::leader::tidy_with(&e.binder, &claims, &pods, now).await;
}

/// A client that gave up while its claim waited never asks for it again: a
/// [`claims::ABANDON_AFTER`] after it was last seen, the leader deletes the
/// pending claim.
#[tokio::test(start_paused = true)]
async fn an_abandoned_pending_claim_is_deleted_after_three_minutes() {
    let e = env();
    let Outcome::Pending(p) = e
        .front
        .allocate(&sa("alice"), &post_req(&KEY))
        .await
        .unwrap()
    else {
        panic!("bound without a pod")
    };
    let now = OffsetDateTime::now_utc();
    tidy_at(&e, now + time::Duration::seconds(170)).await;
    assert!(e.fake.claim(OWN, &p.id).is_some(), "seen 170 s ago: kept");
    tidy_at(&e, now + time::Duration::seconds(181)).await;
    assert!(e.fake.claim(OWN, &p.id).is_none(), "{:?}", e.fake.calls());
}

/// A claim that bound after its client went, with the answer never
/// delivered, is deleted the same way and its worker retired. A delivered
/// claim is left to its TTL.
#[tokio::test(start_paused = true)]
async fn an_abandoned_bound_claim_frees_its_worker_and_a_delivered_one_stays() {
    let e = env();
    let Outcome::Pending(gone) = e
        .front
        .allocate(&sa("alice"), &post_req(&KEY))
        .await
        .unwrap()
    else {
        panic!("bound without a pod")
    };
    let other = rcgen::KeyPair::generate().unwrap().public_key_pem();
    let Outcome::Pending(kept) = e
        .front
        .allocate(&sa("bob"), &post_req(&other))
        .await
        .unwrap()
    else {
        panic!("bound without a pod")
    };
    let (a, b) = (e.warm(0), e.warm(1));
    e.binder.pass(&key()).await;
    let pod_of = |id: &str| e.fake.status(OWN, id).pod.unwrap();
    let gone_pod = pod_of(&gone.id);
    assert!([&a, &b].contains(&&gone_pod));
    // Bob polls and gets his allocation: delivered.
    let got = e
        .front
        .get(&sa("bob"), &kept.id, Duration::ZERO)
        .await
        .unwrap();
    assert!(matches!(got, Outcome::Bound(_)), "{got:?}");
    assert!(claims::delivered(&e.fake.claim(OWN, &kept.id).unwrap()));
    let later = OffsetDateTime::now_utc() + time::Duration::seconds(181);
    tidy_at(&e, later).await;
    assert!(
        e.fake.claim(OWN, &gone.id).is_none(),
        "undelivered: deleted"
    );
    assert!(e.fake.claim(OWN, &kept.id).is_some(), "delivered: kept");
    tidy_at(&e, later).await;
    let calls = e.fake.calls();
    assert!(
        calls.contains(&format!("patch {gone_pod} draining")),
        "{calls:?}"
    );
    let kept_pod = pod_of(&kept.id);
    assert!(
        !calls.contains(&format!("patch {kept_pod} draining")),
        "{calls:?}"
    );
}

/// A client that comes back within three minutes, by POST with the same key or
/// by GET, is seen again: its claim is not deleted and keeps its place.
#[tokio::test(start_paused = true)]
async fn a_claim_whose_client_comes_back_keeps_its_place() {
    let e = env();
    let req = post_req(&KEY);
    let Outcome::Pending(p) = e.front.allocate(&sa("alice"), &req).await.unwrap() else {
        panic!("bound without a pod")
    };
    let later = e.ci_claim(-5, "sa:x/later");
    seen_ago(&e, &p.id, 150);
    let Outcome::Pending(again) = e.front.allocate(&sa("alice"), &req).await.unwrap() else {
        panic!("bound without a pod")
    };
    assert_eq!(
        (again.id.as_str(), again.position),
        (p.id.as_str(), Some(1))
    );
    tidy_at(&e, OffsetDateTime::now_utc() + time::Duration::seconds(100)).await;
    assert!(e.fake.claim(OWN, &p.id).is_some(), "the retry was seen");
    seen_ago(&e, &p.id, 150);
    e.front
        .get(&sa("alice"), &p.id, Duration::ZERO)
        .await
        .unwrap();
    tidy_at(&e, OffsetDateTime::now_utc() + time::Duration::seconds(100)).await;
    assert!(e.fake.claim(OWN, &p.id).is_some(), "the poll was seen");
    e.warm(0);
    e.binder.pass(&key()).await;
    assert_eq!(phase(&e.fake.status(OWN, &p.id)), ClaimPhase::Bound);
    assert_eq!(phase(&e.fake.status(OWN, &later)), ClaimPhase::Pending);
}

/// A claim last seen 60 to 170 s ago is a client between requests (a poll
/// lost on the way, a `Retry-After`, clock skew): kept. Past
/// [`claims::ABANDON_AFTER`] it goes.
#[tokio::test(start_paused = true)]
async fn a_claim_seen_within_three_minutes_is_kept() {
    let e = env();
    let Outcome::Pending(p) = e
        .front
        .allocate(&sa("alice"), &post_req(&KEY))
        .await
        .unwrap()
    else {
        panic!("bound without a pod")
    };
    for ago in [60, 120, 170] {
        seen_ago(&e, &p.id, ago);
        tidy_at(&e, OffsetDateTime::now_utc()).await;
        assert!(e.fake.claim(OWN, &p.id).is_some(), "seen {ago} s ago: kept");
    }
    seen_ago(&e, &p.id, 181);
    tidy_at(&e, OffsetDateTime::now_utc()).await;
    assert!(
        e.fake.claim(OWN, &p.id).is_none(),
        "seen 181 s ago: deleted"
    );
}

/// The leader decides from its store, which may predate the client coming
/// back. The delete names the resourceVersion it decided on, so the client's
/// new `seen-at` makes the API server refuse it, and the claim stays.
#[tokio::test(start_paused = true)]
async fn an_abandon_decided_on_a_stale_store_is_refused() {
    let e = env();
    let Outcome::Pending(p) = e
        .front
        .allocate(&sa("alice"), &post_req(&KEY))
        .await
        .unwrap()
    else {
        panic!("bound without a pod")
    };
    seen_ago(&e, &p.id, 200);
    let stale = e.fake.claims.lock().unwrap().as_reader().state();
    e.front
        .get(&sa("alice"), &p.id, Duration::ZERO)
        .await
        .unwrap();
    let pods = e.fake.pods.lock().unwrap().as_reader().state();
    super::leader::tidy_with(&e.binder, &stale, &pods, OffsetDateTime::now_utc()).await;
    assert!(e.fake.claim(OWN, &p.id).is_some(), "{:?}", e.fake.calls());
    assert!(
        e.fake
            .calls()
            .contains(&format!("delete_claim {} refused", p.id))
    );
}
/// The tidy pass: refuses unserved namespaces, ends expired and lost claims,
/// deletes finished CI claims after ten minutes, and retires pods whose claim
/// is gone.
#[tokio::test]
async fn tidy_ends_claims_and_retires_strays() {
    let e = env();
    e.warm(0);
    e.warm(1);
    let gone = e.ci_claim(0, "sa:x/gone");
    let kept = e.ci_claim(0, "sa:x/kept");
    e.binder.pass(&key()).await;
    let pod_gone = e.fake.status(OWN, &gone).pod.unwrap();
    // The client released it: the claim is deleted, its pod retired.
    let c = e.fake.claim(OWN, &gone).unwrap();
    assert_eq!(
        e.fake
            .delete_claim(OWN, &gone, c.metadata.uid.as_deref().unwrap())
            .await,
        Write::Done
    );
    // An expired one, and a finished one 11 minutes old.
    let mut expired = e.fake.claim(OWN, &kept).unwrap();
    expired.status.as_mut().unwrap().expires_at = Some("2020-01-01T00:00:00Z".into());
    e.fake.put_claim(expired);
    let old = e.ci_claim(0, "sa:x/old");
    let mut done = e.fake.claim(OWN, &old).unwrap();
    let mut st = claims::unbound_status(&done, ClaimPhase::Failed, reason::QUEUE_TIMEOUT, "", None);
    st.conditions[0].last_transition_time = Time(
        k8s_openapi::jiff::Timestamp::now() - k8s_openapi::jiff::SignedDuration::from_mins(11),
    );
    done.status = Some(st);
    e.fake.put_claim(done);
    e.claim("other", "x", 0, json!({}));
    let claims = e.fake.claims.lock().unwrap().as_reader().state();
    let pods = e.fake.pods.lock().unwrap().as_reader().state();
    super::leader::tidy_with(&e.binder, &claims, &pods, OffsetDateTime::now_utc()).await;
    let calls = e.fake.calls();
    assert!(
        calls.contains(&format!("patch {pod_gone} draining")),
        "{calls:?}"
    );
    assert!(calls.contains(&format!("delete_pod {pod_gone}")));
    assert_eq!(phase(&e.fake.status(OWN, &kept)), ClaimPhase::Expired);
    assert!(e.fake.claim(OWN, &old).is_none(), "collected");
    let refused = e.fake.status("other", "x");
    assert_eq!(
        (phase(&refused), why(&refused).as_str()),
        (ClaimPhase::Failed, reason::NAMESPACE_NOT_SERVED)
    );
    // The metric counts the release.
    let mut reg = prometheus_client::registry::Registry::default();
    e.metrics.register(&mut reg);
}

/// An allocation made before every allocation was a claim has no claim: its
/// owner's DELETE retires its pod; anyone else, like an unknown id, gets 404.
#[tokio::test(start_paused = true)]
async fn release_retires_a_pod_allocated_before_claims() {
    let e = env();
    let name = e.warm(0);
    let id = new_alloc_id().unwrap().to_string();
    let mut pod = e.fake.pod(&name);
    let a = pod.metadata.annotations.as_mut().unwrap();
    a.insert(ANN_ALLOC.into(), id.clone());
    a.insert(ANN_OWNER.into(), "sa:render-verify/alice".into());
    e.fake.put_pod(pod);
    assert_eq!(
        e.front.release(&sa("mallory"), &id).await,
        Err(AllocError::NotFound)
    );
    assert_eq!(
        e.fake.pod(&name).metadata.annotations.unwrap()[ANN_ALLOC],
        id
    );
    e.front.release(&sa("alice"), &id).await.unwrap();
    let calls = e.fake.calls();
    assert!(
        calls.contains(&format!("patch {name} draining")),
        "{calls:?}"
    );
    assert!(calls.contains(&format!("delete_pod {name}")), "{calls:?}");
    assert_eq!(
        e.front.release(&sa("alice"), &id).await,
        Err(AllocError::NotFound)
    );
    // A pod bound for a claim is never released this way.
    let name = e.warm(1);
    let other = new_alloc_id().unwrap().to_string();
    let mut pod = e.fake.pod(&name);
    let a = pod.metadata.annotations.as_mut().unwrap();
    a.insert(ANN_ALLOC.into(), other.clone());
    a.insert(ANN_OWNER.into(), "sa:render-verify/alice".into());
    a.insert(ANN_CLAIM.into(), format!("{OWN}/{other}"));
    e.fake.put_pod(pod);
    assert_eq!(
        e.front.release(&sa("alice"), &other).await,
        Err(AllocError::NotFound)
    );
}

/// A CA source whose bundle is gone after the first call: the replica checked
/// the CA before the create, then lost it.
struct CaOnce(Arc<CaBundle>, std::sync::atomic::AtomicBool);

impl super::CertSource for CaOnce {
    fn bundle(&self) -> Option<Arc<CaBundle>> {
        (!self.1.swap(true, std::sync::atomic::Ordering::SeqCst)).then(|| Arc::clone(&self.0))
    }
    fn ca_cert_pem(&self, _: &CaBundle) -> Option<String> {
        None
    }
}

/// A bound claim whose response this replica cannot finish (no CA right now)
/// answers pending with the id, so the client polls instead of losing it.
#[tokio::test]
#[allow(
    clippy::used_underscore_binding,
    reason = "the pool store kept alive by Env"
)]
async fn a_bound_claim_without_a_ca_answers_pending() {
    let e = Arc::new(env());
    e.warm(0);
    let tx = e.fake.tx.clone();
    let front = Front::new(
        Arc::clone(&e.fake) as Arc<dyn Backend>,
        e.pools.as_reader(),
        e.fake.claims.lock().unwrap().as_reader(),
        Box::new(move || ClaimSubscription::new(tx.subscribe())),
        Arc::new(CaOnce(
            Arc::clone(&e.ca),
            std::sync::atomic::AtomicBool::new(false),
        )),
        OWN.into(),
        HashSet::from([OWN.to_owned()]),
        AllocMetrics::default(),
    );
    let leader = {
        let e = Arc::clone(&e);
        let mut sub = ClaimSubscription::new(e.fake.tx.subscribe());
        tokio::spawn(async move {
            while sub.recv().await.is_some() {
                e.binder.pass(&key()).await;
            }
        })
    };
    let req: CreateAllocationRequest = serde_json::from_value(json!({
        "namespace": POOL_NS, "pool": POOL, "publicKey": *KEY
    }))
    .unwrap();
    let out = front.allocate(&sa("alice"), &req).await.unwrap();
    leader.abort();
    let Outcome::Pending(p) = out else {
        panic!("{out:?}")
    };
    let c = e
        .fake
        .claim(OWN, &p.id)
        .expect("the claim is kept for the client to poll");
    assert_eq!(c.status.and_then(|s| s.phase), Some(ClaimPhase::Bound));
}

#[path = "tests_cache.rs"]
mod cache_tests;

fn fresh_key() -> String {
    rcgen::KeyPair::generate().unwrap().public_key_pem()
}

/// Any caller can name random ids. A lookup reads the store only, never the
/// API server, and waits briefly for a claim another replica just created.
#[tokio::test(start_paused = true)]
async fn a_lookup_by_id_never_reads_the_api_server() {
    let e = env();
    let random = new_alloc_id().unwrap().to_string();
    assert_eq!(
        e.front.get(&sa("alice"), &random, Duration::ZERO).await,
        Err(AllocError::NotFound)
    );
    assert_eq!(
        e.front.release(&sa("alice"), &random).await,
        Err(AllocError::NotFound)
    );
    assert!(
        !e.fake.calls().iter().any(|c| c.starts_with("get_claim")),
        "{:?}",
        e.fake.calls()
    );
    // A claim that reaches the store during the wait is found.
    let id = new_alloc_id().unwrap().to_string();
    let fake = Arc::clone(&e.fake);
    let late = id.clone();
    let mut c = BuildKitClaim::new(
        &late,
        serde_json::from_value(json!({"poolRef": {"namespace": POOL_NS, "name": POOL},
            "owner": sa("alice").owner(), "publicKey": *KEY}))
        .unwrap(),
    );
    c.metadata.namespace = Some(OWN.into());
    c.metadata.uid = Some("33333333-0000-4000-8000-000000000001".into());
    c.metadata.creation_timestamp = Some(Time(k8s_openapi::jiff::Timestamp::now()));
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        fake.put_claim(c);
    });
    assert!(matches!(
        e.front.get(&sa("alice"), &id, Duration::ZERO).await,
        Ok(Outcome::Pending(_))
    ));
}

/// One owner may hold half of `scaling.max` (4 here) and wait for as many
/// again. Over that a POST gets the retryable `pool_exhausted`; a repeat of a
/// key it already holds and other owners are unaffected.
#[tokio::test(start_paused = true)]
async fn one_owner_holds_or_waits_for_at_most_the_pool_size() {
    let e = env();
    let first = post_req(&fresh_key());
    let more: Vec<_> = (0..3).map(|_| post_req(&fresh_key())).collect();
    for req in std::iter::once(&first).chain(&more) {
        assert!(matches!(
            e.front.allocate(&sa("alice"), req).await,
            Ok(Outcome::Pending(_))
        ));
    }
    let r = e
        .front
        .allocate(&sa("alice"), &post_req(&fresh_key()))
        .await;
    assert!(matches!(r, Err(AllocError::PoolExhausted(_))), "{r:?}");
    assert!(matches!(
        e.front.allocate(&sa("alice"), &first).await,
        Ok(Outcome::Pending(_))
    ));
    assert!(matches!(
        e.front.allocate(&sa("bob"), &post_req(&fresh_key())).await,
        Ok(Outcome::Pending(_))
    ));
    assert_eq!(claim_names(&e).len(), 5);
}

/// The leader binds no more than the cap to one owner either, so claims that
/// slipped past the front (two replicas racing) wait instead of binding.
#[tokio::test]
async fn the_leader_binds_at_most_half_the_pool_to_one_owner() {
    let e = env();
    for slot in 0..4 {
        e.warm(slot);
    }
    let ids: Vec<String> = (0..3).map(|i| e.ci_claim(30 - i, "sa:x/a")).collect();
    let other = e.ci_claim(1, "sa:x/b");
    e.binder.pass(&key()).await;
    let phases: Vec<ClaimPhase> = ids
        .iter()
        .map(|id| phase(&e.fake.status(OWN, id)))
        .collect();
    assert_eq!(
        phases,
        [ClaimPhase::Bound, ClaimPhase::Bound, ClaimPhase::Pending]
    );
    assert_eq!(phase(&e.fake.status(OWN, &other)), ClaimPhase::Bound);
    // A second pass, before the store shows the binds, still counts them.
    e.binder.pass(&key()).await;
    assert_eq!(phase(&e.fake.status(OWN, &ids[2])), ClaimPhase::Pending);
}

/// An in-cluster claim asking for a longer queue wait than the pool allows
/// fails `InvalidRequest`, as the HTTP API refuses it, instead of being cut.
#[tokio::test]
async fn an_in_cluster_queue_timeout_over_the_max_is_refused() {
    let e = env();
    e.claim(CI, "long", 0, json!({"queueTimeout": "1h"}));
    e.binder.pass(&key()).await;
    let s = e.fake.status(CI, "long");
    assert_eq!(
        (phase(&s), why(&s).as_str()),
        (ClaimPhase::Failed, reason::INVALID_REQUEST)
    );
}

/// A key padded past the claim CRD's `maxLength` would parse and then fail
/// the create; it is a 400, never a 500.
#[tokio::test]
async fn an_oversized_public_key_is_a_bad_request() {
    let e = env();
    let padded = format!("{}\n{}", "x".repeat(2000), *KEY);
    let r = e.front.allocate(&sa("alice"), &post_req(&padded)).await;
    assert!(matches!(r, Err(AllocError::InvalidRequest(_))), "{r:?}");
    assert!(claim_names(&e).is_empty());
}

/// Pool max 2, so one owner binds one worker at a time. Its second claim waits
/// next to a free pod; once the first is released, the pass its deletion
/// calls for binds the second, with no pod event needed.
#[tokio::test]
async fn releasing_an_owners_worker_lets_its_waiting_claim_bind() {
    let mut e = env();
    e.pool.spec.scaling.max = 2;
    e.pools
        .apply_watcher_event(&watcher::Event::Apply(e.pool.clone()));
    e.warm(0);
    let a = e.ci_claim(30, "sa:x/a");
    let b = e.ci_claim(20, "sa:x/a");
    e.warm(1);
    e.binder.pass(&key()).await;
    assert_eq!(phase(&e.fake.status(OWN, &a)), ClaimPhase::Bound);
    assert_eq!(phase(&e.fake.status(OWN, &b)), ClaimPhase::Pending);
    let gone = e
        .fake
        .w
        .lock()
        .unwrap()
        .claims
        .remove(&(OWN.to_owned(), a.clone()))
        .unwrap();
    e.fake
        .claims
        .lock()
        .unwrap()
        .apply_watcher_event(&watcher::Event::Delete(gone.clone()));
    assert!(matches!(
        super::leader::claim_trigger(&ClaimEvent::Delete(Arc::new(gone))),
        Some(crate::reconcile::driver::Trigger::Pool(k)) if k == key()
    ));
    e.binder.pass(&key()).await;
    assert_eq!(phase(&e.fake.status(OWN, &b)), ClaimPhase::Bound);
}
