//! Executor tests against a fake Kubernetes API covering action order, the skip
//! rules, requeue after outcomes, and term cancellation.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use buildkit_controller_api::v1alpha1::{BuildKitPool, BuildKitPoolStatus, condition, reason};
use futures::future::BoxFuture;
use k8s_openapi::api::core::v1::{Pod, Secret};
use k8s_openapi::api::events::v1::Event;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use k8s_openapi::jiff::Timestamp;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream_shim::ReceiverStream;

use super::driver::{PoolKey, Trigger, schedule};
use super::exec::{CaGuard, Env, execute};
use super::ops::{Applied, KubeOps, OpError, OpResult};
use super::pass::{PoolMemory, remember};
use super::plan::{Action, CaEvent, CreateOutcome, CreateResult, Plan, RetireReason};
use super::spawn::WorkerSecrets;
use crate::pki::{CaBundle, LoadOutcome};
use crate::render::fixtures;

/// `(pod, resourceVersion precondition, grace)` of a delete.
type Delete = (String, Option<String>, Option<u32>);

/// Records calls; a call whose label is in `fail` returns that error.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<String>>,
    fail: HashMap<String, OpError>,
    statuses: Mutex<Vec<BuildKitPoolStatus>>,
    deletes: Mutex<Vec<Delete>>,
    /// Every Secret passed to `create_secret`.
    secrets: Mutex<Vec<Secret>>,
    /// When set, every apply waits for a permit first.
    gate: Option<Arc<tokio::sync::Notify>>,
}

impl Fake {
    fn failing(fail: &[(&str, OpError)]) -> Self {
        Self {
            fail: fail
                .iter()
                .map(|(k, e)| ((*k).to_owned(), e.clone()))
                .collect(),
            ..Self::default()
        }
    }

    fn call(&self, label: String) -> OpResult {
        let res = self.fail.get(&label).cloned().map_or(Ok(()), Err);
        self.calls.lock().unwrap().push(label);
        res
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// [`Self::calls`] with each random worker nonce replaced by `N`.
    fn calls_n(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .map(|c| match c.split_once(':') {
                Some((op @ ("secret_create" | "secret_reown" | "secret_delete"), rest)) => {
                    let (name, tail) = rest
                        .split_once(':')
                        .map_or((rest, None), |(n, t)| (n, Some(t)));
                    let pod = name.rsplit_once('-').map_or(name, |(p, _)| p);
                    tail.map_or_else(|| format!("{op}:{pod}-N"), |t| format!("{op}:{pod}-N:{t}"))
                }
                _ => c,
            })
            .collect()
    }
}

impl KubeOps for Fake {
    async fn apply<K: Applied>(&self, _obj: &K) -> OpResult {
        if let Some(g) = &self.gate {
            g.notified().await;
        }
        self.call(format!("apply:{}", K::kind(&())))
    }
    async fn create_pod(&self, _ns: &str, pod: &Pod) -> OpResult<Pod> {
        let name = pod.metadata.name.clone().unwrap_or_default();
        self.call(format!("create:{name}"))?;
        let mut p = pod.clone();
        p.metadata.uid = Some(format!("uid-{name}"));
        Ok(p)
    }
    async fn retire(
        &self,
        _ns: &str,
        pod: &str,
        _uid: &str,
        expected: &str,
        expires_at: Option<&str>,
    ) -> OpResult<Option<String>> {
        match expires_at {
            Some(e) => self.call(format!("retire:{pod}:{expected}:{e}"))?,
            None => self.call(format!("retire:{pod}:{expected}"))?,
        }
        Ok(Some("rv-7".into()))
    }
    async fn delete_pod(
        &self,
        _ns: &str,
        pod: &str,
        _uid: &str,
        rv: Option<String>,
        grace: Option<u32>,
    ) -> OpResult {
        self.deletes
            .lock()
            .unwrap()
            .push((pod.to_owned(), rv, grace));
        self.call(format!("delete:{pod}"))
    }
    async fn patch_status(&self, _ns: &str, _pool: &str, status: &BuildKitPoolStatus) -> OpResult {
        self.statuses.lock().unwrap().push(status.clone());
        self.call("status".into())
    }
    async fn create_secret(&self, s: &Secret) -> OpResult {
        self.secrets.lock().unwrap().push(s.clone());
        let name = s.metadata.name.clone().unwrap_or_default();
        let labelled = s.metadata.labels.as_ref().is_some_and(|l| {
            l.get("app.kubernetes.io/component").map(String::as_str) == Some("ca-replaced")
        });
        let kind = if name.contains("-replaced-") && labelled {
            "backup"
        } else {
            "ca"
        };
        self.call(format!("create_secret:{kind}"))
    }
    async fn replace_secret(&self, _s: &Secret) -> OpResult {
        self.call("replace_secret".into())
    }
    async fn create_event(&self, e: &Event) -> OpResult {
        self.call(format!("event:{}", e.reason.clone().unwrap_or_default()))
    }
}

fn now() -> Timestamp {
    Timestamp::from_second(1_790_000_000).unwrap()
}

fn pool() -> BuildKitPool {
    let mut p = fixtures::pool("p", json!({})).unwrap();
    p.metadata.generation = Some(3);
    p
}

fn valid_ca() -> LoadOutcome {
    LoadOutcome::Valid(Box::new(
        CaBundle::generate(super::exec::odt(now())).unwrap(),
    ))
}

fn env<'a>(
    pool: &'a BuildKitPool,
    ca: &'a LoadOutcome,
    secret: Option<&'a Secret>,
    epoch: u64,
) -> Env<'a> {
    static RENDER: std::sync::OnceLock<crate::render::RenderConfig> = std::sync::OnceLock::new();
    Env {
        pool,
        ca,
        ca_secret: secret,
        ca_epoch: epoch,
        ca_secret_name: "bk-ca",
        render: RENDER.get_or_init(fixtures::default_config),
        worker_cert_validity: crate::pki::LeafValidity::default().worker_serving,
        replica: "r0",
        now: now(),
    }
}

fn plan(actions: Vec<Action>) -> Plan {
    Plan {
        actions,
        requeue_at: None,
        backoff: super::plan::BackoffSignal::Keep,
        slot_holds: std::collections::BTreeMap::new(),
    }
}

fn ready_status() -> Box<BuildKitPoolStatus> {
    Box::new(BuildKitPoolStatus {
        conditions: vec![k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition {
            type_: condition::READY.into(),
            status: "True".into(),
            reason: reason::READY.into(),
            message: String::new(),
            last_transition_time: Time(now()),
            observed_generation: Some(3),
        }],
        ..BuildKitPoolStatus::default()
    })
}

fn full_pass() -> Vec<Action> {
    vec![
        Action::ApplySa,
        Action::ApplyNetworkPolicy,
        Action::DeleteDraining {
            pod: "p-4".into(),
            uid: "u4".into(),
        },
        Action::ForceDelete {
            pod: "p-5".into(),
            uid: "u5".into(),
        },
        Action::RetireFree {
            pod: "p-2".into(),
            uid: "u2".into(),
            reason: RetireReason::Idle,
        },
        Action::RetireAllocated {
            pod: "p-3".into(),
            uid: "u3".into(),
            alloc_id: "a3".into(),
            expires_at: Some("2026-01-01T00:00:00Z".into()),
            reason: RetireReason::Expired,
        },
        Action::CreatePod {
            slot: 0,
            name: "p-0".into(),
        },
        Action::PatchStatus(ready_status()),
    ]
}

#[tokio::test]
async fn actions_run_in_plan_order() {
    let (p, ca) = (pool(), valid_ca());
    let fake = Fake::default();
    let r = execute(
        &fake,
        &CaGuard::default(),
        &env(&p, &ca, None, 0),
        &plan(full_pass()),
    )
    .await;
    assert_eq!(
        fake.calls_n(),
        [
            "apply:ServiceAccount",
            "apply:NetworkPolicy",
            "delete:p-4",
            "delete:p-5",
            "retire:p-2:free",
            "delete:p-2",
            "retire:p-3:a3:2026-01-01T00:00:00Z",
            "delete:p-3",
            "create:p-0",
            "secret_create:p-0-N:Pod/uid-p-0",
            "status",
        ]
    );
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(r.sandbox_applied);
    assert_eq!(
        r.creates,
        [CreateOutcome {
            slot: 0,
            result: CreateResult::Created
        }]
    );
    assert_eq!(r.releases, ["expired"]);
    // Force delete uses grace 0; the retire delete carries the patched resourceVersion.
    let deletes = fake.deletes.lock().unwrap().clone();
    assert_eq!(deletes[1], ("p-5".into(), None, Some(0)));
    assert_eq!(deletes[2], ("p-2".into(), Some("rv-7".into()), None));
}

#[tokio::test]
/// Regression (control review P3): every failed apply was reported as
/// `WorkerTLSPending`, so a rejected `ServiceAccount` or `NetworkPolicy` sent the
/// operator to the wrong runbook row. A sandbox failure is `SandboxPending`,
/// also over a planned `Ready=False/WorkerTLSPending`.
async fn failed_apply_skips_creates_and_downgrades_ready() {
    for (failing, want, planned_pending) in [
        ("apply:ServiceAccount", reason::SANDBOX_PENDING, false),
        ("apply:NetworkPolicy", reason::SANDBOX_PENDING, false),
        ("apply:ServiceAccount", reason::SANDBOX_PENDING, true),
        ("apply:NetworkPolicy", reason::SANDBOX_PENDING, true),
    ] {
        let (p, ca) = (pool(), valid_ca());
        let fake = Fake::failing(&[(failing, OpError::Other("boom".into()))]);
        let mut actions = full_pass();
        if planned_pending {
            let mut st = ready_status();
            st.conditions[0].status = "False".into();
            st.conditions[0].reason = reason::SANDBOX_PENDING.into();
            for a in &mut actions {
                if let Action::PatchStatus(s) = a {
                    s.clone_from(&st);
                }
            }
        }
        let r = execute(
            &fake,
            &CaGuard::default(),
            &env(&p, &ca, None, 0),
            &plan(actions),
        )
        .await;
        let calls = fake.calls();
        assert!(
            !calls.iter().any(|c| c.starts_with("create:")),
            "{failing}: {calls:?}"
        );
        // Deletes, retires and status still run.
        assert!(calls.contains(&"retire:p-2:free".to_owned()));
        assert!(calls.contains(&"status".to_owned()));
        assert!(r.creates.is_empty());
        assert!(!r.errors.is_empty());
        let st = fake.statuses.lock().unwrap()[0].clone();
        let ready = st
            .conditions
            .iter()
            .find(|c| c.type_ == condition::READY)
            .unwrap();
        assert_eq!(
            (ready.status.as_str(), ready.reason.as_str()),
            ("False", want),
            "{failing} planned_pending={planned_pending}"
        );
    }
}

#[tokio::test]
async fn failed_retire_test_skips_its_delete() {
    let (p, ca) = (pool(), valid_ca());
    let fake = Fake::failing(&[("retire:p-2:free", OpError::Invalid("test failed".into()))]);
    let r = execute(
        &fake,
        &CaGuard::default(),
        &env(&p, &ca, None, 0),
        &plan(full_pass()),
    )
    .await;
    let calls = fake.calls();
    assert!(!calls.contains(&"delete:p-2".to_owned()));
    assert!(calls.contains(&"delete:p-3".to_owned()));
    assert!(
        r.errors.is_empty(),
        "a lost retire is not an error: {:?}",
        r.errors
    );
}

#[tokio::test]
async fn create_outcomes_are_classified() {
    let (p, ca) = (pool(), valid_ca());
    let fake = Fake::failing(&[
        ("create:p-0", OpError::Conflict("exists".into())),
        ("create:p-2", OpError::Other("quota".into())),
    ]);
    let creates = (0..4)
        .map(|slot| Action::CreatePod {
            slot,
            name: format!("p-{slot}"),
        })
        .collect();
    let r = execute(
        &fake,
        &CaGuard::default(),
        &env(&p, &ca, None, 0),
        &plan(creates),
    )
    .await;
    let results: Vec<_> = r.creates.iter().map(|c| c.result.clone()).collect();
    assert_eq!(results[0], CreateResult::SlotTaken);
    assert_eq!(results[1], CreateResult::Created);
    assert!(matches!(results[2], CreateResult::Failed { .. }));
    assert_eq!(r.created.len(), 1);
    // A refused create gets no Secret. An unknown outcome is read back first,
    // and no pod of this create means no Secret. The created pod owns its
    // Secret from the start. After a failed create the pass creates nothing
    // more, so a full quota costs one refused POST per create backoff.
    assert_eq!(
        fake.calls_n(),
        [
            "create:p-0",
            "create:p-1",
            "secret_create:p-1-N:Pod/uid-p-1",
            "create:p-2",
            "get_pod:p-2",
        ]
    );
    assert_eq!(results.len(), 3);
    assert!(r.trace.contains(&"skip_create:p-3".to_owned()));
}

#[tokio::test]
async fn a_refused_secret_create_deletes_the_pod_at_once() {
    let (p, ca) = (pool(), valid_ca());
    let fake = Fake::failing(&[("secret_create", OpError::Other("exists".into()))]);
    let create = vec![Action::CreatePod {
        slot: 0,
        name: "p-0".into(),
    }];
    let r = execute(
        &fake,
        &CaGuard::default(),
        &env(&p, &ca, None, 0),
        &plan(create),
    )
    .await;
    // A Secret of the same name planted by someone else would let the pod
    // start, so it is deleted (uid-preconditioned, grace 0) in the same pass,
    // and the failure counts toward the create backoff.
    assert_eq!(r.created.len(), 1);
    assert!(matches!(r.creates[0].result, CreateResult::Failed { .. }));
    assert_eq!(
        fake.calls_n(),
        [
            "create:p-0",
            "secret_create:p-0-N:Pod/uid-p-0",
            "delete:p-0"
        ]
    );
    assert_eq!(
        *fake.deletes.lock().unwrap(),
        [("p-0".to_owned(), None, Some(0))]
    );
}

fn ca_actions(replace: bool) -> Vec<Action> {
    let (first, reason, type_) = if replace {
        (
            Action::BackupAndReplaceCa {
                reason: "invalid",
                message: "bad".into(),
            },
            "CAReplaced",
            "Warning",
        )
    } else {
        (Action::CreateCa, "CACreated", "Normal")
    };
    vec![
        first,
        Action::Event(CaEvent {
            reason,
            type_,
            message: "m".into(),
        }),
    ]
}

#[tokio::test]
async fn event_only_after_its_ca_action_succeeds() {
    let p = pool();
    let missing = LoadOutcome::Missing;
    // On failure there is no Event and the error backs off.
    let fake = Fake::failing(&[("create_secret:ca", OpError::Other("down".into()))]);
    let r = execute(
        &fake,
        &CaGuard::default(),
        &env(&p, &missing, None, 1),
        &plan(ca_actions(false)),
    )
    .await;
    assert_eq!(fake.calls(), ["create_secret:ca"]);
    assert!(!r.errors.is_empty());
    // On success there is an Event, and a second pool in the same CA epoch does nothing.
    let (fake, guard) = (Fake::default(), CaGuard::default());
    let r = execute(
        &fake,
        &guard,
        &env(&p, &missing, None, 1),
        &plan(ca_actions(false)),
    )
    .await;
    assert_eq!(r.ca_replacements, ["missing"]);
    execute(
        &fake,
        &guard,
        &env(&p, &missing, None, 1),
        &plan(ca_actions(false)),
    )
    .await;
    assert_eq!(fake.calls(), ["create_secret:ca", "event:CACreated"]);
    // After a lost race (409) there is no Event and no error.
    let fake = Fake::failing(&[("create_secret:ca", OpError::Conflict("exists".into()))]);
    let r = execute(
        &fake,
        &CaGuard::default(),
        &env(&p, &missing, None, 2),
        &plan(ca_actions(false)),
    )
    .await;
    assert_eq!(fake.calls(), ["create_secret:ca"]);
    assert!(r.errors.is_empty());
}

#[tokio::test]
async fn invalid_ca_is_backed_up_then_replaced() {
    let p = pool();
    let invalid = LoadOutcome::Missing; // the executor only reads the Secret object here
    let mut cur = Secret::default();
    cur.metadata.resource_version = Some("41".into());
    let keys = CaBundle::generate(super::exec::odt(now()))
        .unwrap()
        .to_secret_data();
    cur.data = Some(
        keys.into_iter()
            .map(|(k, v)| (k, k8s_openapi::ByteString(v)))
            .collect(),
    );
    cur.string_data = Some([(crate::pki::CLIENT_CA_KEY.to_owned(), "k".to_owned())].into());
    let fake = Fake::default();
    let r = execute(
        &fake,
        &CaGuard::default(),
        &env(&p, &invalid, Some(&cur), 5),
        &plan(ca_actions(true)),
    )
    .await;
    assert_eq!(
        fake.calls(),
        ["create_secret:backup", "replace_secret", "event:CAReplaced"]
    );
    assert_eq!(r.ca_replacements, ["invalid"]);
    // The backup keeps the CA certificates and no private key.
    let backup = fake.secrets.lock().unwrap()[0].clone();
    let kept: Vec<&str> = backup
        .data
        .iter()
        .flatten()
        .map(|(k, _)| k.as_str())
        .collect();
    assert_eq!(kept, ["client-ca.crt", "generated-by", "internal-ca.crt"]);
    assert!(backup.string_data.is_none_or(|s| s.is_empty()));
    // A failed backup stops the replace.
    let fake = Fake::failing(&[("create_secret:backup", OpError::Other("forbidden".into()))]);
    execute(
        &fake,
        &CaGuard::default(),
        &env(&p, &invalid, Some(&cur), 5),
        &plan(ca_actions(true)),
    )
    .await;
    assert_eq!(fake.calls(), ["create_secret:backup"]);
}

fn outcome(result: CreateResult) -> super::exec::Report {
    super::exec::Report {
        creates: vec![CreateOutcome { slot: 0, result }],
        ..super::exec::Report::default()
    }
}

#[test]
fn requeue_after_create_outcomes() {
    let p = pool();
    let mut pl = plan(Vec::new());
    pl.requeue_at = Some(now() + k8s_openapi::jiff::SignedDuration::from_secs(30));
    let mut mem = PoolMemory::default();
    let rq = |mem: &mut PoolMemory, r: &super::exec::Report| {
        remember(mem, &p, &pl, r, BTreeSet::new(), now())
    };
    // After a 201, wait for events or the plan deadline.
    assert_eq!(
        rq(&mut mem, &outcome(CreateResult::Created)),
        Some(Duration::from_secs(30))
    );
    // After a 409, requeue at once.
    assert_eq!(
        rq(&mut mem, &outcome(CreateResult::SlotTaken)),
        Some(Duration::ZERO)
    );
    // For any other failure the error backoff starts at 1 s, doubles, caps at 60 s and resets on success.
    // Regression (RECON-8): it never delays the plan deadline (30 s here),
    // so an expiry or start timeout runs on time while some call keeps failing.
    let failed = outcome(CreateResult::Failed {
        message: "quota".into(),
    });
    let seq: Vec<_> = (0..8)
        .map(|_| rq(&mut mem, &failed).unwrap().as_secs())
        .collect();
    assert_eq!(seq, [1, 2, 4, 8, 16, 30, 30, 30]);
    assert_eq!(
        rq(&mut mem, &outcome(CreateResult::Created)),
        Some(Duration::from_secs(30))
    );
    assert_eq!(rq(&mut mem, &failed), Some(Duration::from_secs(1)));
    // Without a plan deadline the backoff alone counts, up to its 60 s cap.
    pl.requeue_at = None;
    let seq: Vec<_> = (0..8)
        .map(|_| {
            remember(&mut mem, &p, &pl, &failed, BTreeSet::new(), now())
                .unwrap()
                .as_secs()
        })
        .collect();
    assert_eq!(seq, [2, 4, 8, 16, 32, 60, 60, 60]);
}

fn key(n: &str) -> PoolKey {
    ("ns".into(), n.into())
}

fn triggers() -> (mpsc::UnboundedSender<Trigger>, ReceiverStream) {
    let (tx, rx) = mpsc::unbounded_channel();
    (tx, ReceiverStream(rx))
}

#[tokio::test(start_paused = true)]
async fn scheduler_serialises_reruns_and_requeues() {
    let (tx, rx) = triggers();
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let active = Arc::new(AtomicUsize::new(0));
    let runs = Arc::new(AtomicUsize::new(0));
    let task = tokio::spawn({
        let (log, active, runs) = (log.clone(), active.clone(), runs.clone());
        schedule(
            rx,
            || vec![key("a"), key("b")],
            move |k| {
                let (log, active, runs) = (log.clone(), active.clone(), runs.clone());
                async move {
                    assert_eq!(
                        active.fetch_add(1, Ordering::SeqCst),
                        0,
                        "concurrent pass of one pool"
                    );
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    let n = runs.fetch_add(1, Ordering::SeqCst);
                    log.lock().unwrap().push(format!("{}@{n}", k.1));
                    // The first pass asks for a requeue in 10 s.
                    (k, (n == 0).then_some(Duration::from_secs(10)))
                }
            },
        )
    });
    tx.send(Trigger::Pool(key("a"))).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Two triggers during the pass collapse into one rerun.
    tx.send(Trigger::Pool(key("a"))).unwrap();
    tx.send(Trigger::Pool(key("a"))).unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(*log.lock().unwrap(), ["a@0", "a@1"]);
    // The rerun replaced the 10 s deadline by returning None, so nothing more is due.
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(log.lock().unwrap().len(), 2);
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn scheduler_fires_deadline_without_events() {
    let (tx, rx) = triggers();
    let log = Arc::new(Mutex::new(Vec::<tokio::time::Instant>::new()));
    let task = tokio::spawn({
        let log = log.clone();
        schedule(rx, Vec::new, move |k| {
            let log = log.clone();
            async move {
                log.lock().unwrap().push(tokio::time::Instant::now());
                let n = log.lock().unwrap().len();
                (k, (n < 3).then_some(Duration::from_secs(7)))
            }
        })
    });
    let t0 = tokio::time::Instant::now();
    tx.send(Trigger::Pool(key("a"))).unwrap();
    tokio::time::sleep(Duration::from_mins(1)).await;
    let at: Vec<u64> = log
        .lock()
        .unwrap()
        .iter()
        .map(|t| (*t - t0).as_secs())
        .collect();
    assert_eq!(at, [0, 7, 14]);
    task.abort();
}

#[tokio::test]
async fn term_end_drops_inflight_passes() {
    struct Flag(Arc<AtomicBool>);
    impl Drop for Flag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let (tx, rx) = triggers();
    let dropped = Arc::new(AtomicBool::new(false));
    let started = Arc::new(tokio::sync::Notify::new());
    let task = tokio::spawn({
        let (dropped, started) = (dropped.clone(), started.clone());
        schedule(rx, Vec::new, move |k| {
            let flag = Flag(dropped.clone());
            let started = started.clone();
            async move {
                started.notify_one();
                std::future::pending::<()>().await;
                drop(flag);
                (k, None)
            }
        })
    });
    tx.send(Trigger::Pool(key("a"))).unwrap();
    started.notified().await;
    assert!(!dropped.load(Ordering::SeqCst));
    // What `runtime::leader::supervise` does when the term ends.
    task.abort();
    let _ = task.await;
    assert!(
        dropped.load(Ordering::SeqCst),
        "the in-flight pass outlived the term"
    );
}

mod tokio_stream_shim {
    //! `mpsc::UnboundedReceiver` as a `Stream` (no tokio-stream dependency).
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use super::Trigger;

    pub struct ReceiverStream(pub tokio::sync::mpsc::UnboundedReceiver<Trigger>);

    impl futures::Stream for ReceiverStream {
        type Item = Trigger;
        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Trigger>> {
            self.0.poll_recv(cx)
        }
    }
}

fn ca_settings() -> super::pass::Settings {
    super::pass::Settings {
        plan: super::plan::PlanConfig::default(),
        render: fixtures::default_config(),
        ca_secret_name: "bk-ca".into(),
        replica: "r0".into(),
        issuers: Vec::new(),
        worker_cert_validity: crate::pki::LeafValidity::default().worker_serving,
    }
}

fn ca_view(
    epoch: u64,
    data: Option<std::collections::BTreeMap<String, Vec<u8>>>,
) -> super::pass::CaView {
    super::pass::CaView {
        epoch,
        secret: data.map(|d| {
            let mut s = Secret::default();
            s.metadata.resource_version = Some("7".into());
            s.data = Some(
                d.into_iter()
                    .map(|(k, v)| (k, k8s_openapi::ByteString(v)))
                    .collect(),
            );
            Arc::new(s)
        }),
    }
}

/// Regression (soak): the CA pass needs no pool. Before it existed a
/// controller with no `BuildKitPool` never created its CA, so no replica
/// became Ready, and a deleted or corrupted CA Secret healed only once some
/// pool was reconciled.
#[tokio::test]
async fn ca_pass_without_pools_creates_heals_and_publishes() {
    use super::pass::ca_pass;
    let (s, guard) = (ca_settings(), CaGuard::default());
    // A missing Secret is created, once per Secret version.
    let fake = Fake::default();
    let r = ca_pass(&fake, &guard, &s, &ca_view(1, None), now()).await;
    ca_pass(&fake, &guard, &s, &ca_view(1, None), now()).await;
    assert_eq!(fake.calls(), ["create_secret:ca", "event:CACreated"]);
    assert_eq!(r.ca_replacements, ["missing"]);
    // A valid Secret gets its bundle ConfigMap applied once per Secret version.
    let data = CaBundle::generate(crate::reconcile::exec::odt(now()))
        .unwrap()
        .to_secret_data();
    let fake = Fake::default();
    ca_pass(&fake, &guard, &s, &ca_view(2, Some(data.clone())), now()).await;
    ca_pass(&fake, &guard, &s, &ca_view(2, Some(data.clone())), now()).await;
    assert_eq!(fake.calls(), ["apply:ConfigMap"]);
    // A corrupted Secret is backed up, then replaced.
    let mut bad = data;
    bad.insert("client-ca.crt".into(), b"corrupted".to_vec());
    let fake = Fake::default();
    let r = ca_pass(&fake, &guard, &s, &ca_view(3, Some(bad)), now()).await;
    assert_eq!(
        fake.calls(),
        ["create_secret:backup", "replace_secret", "event:CAReplaced"]
    );
    assert_eq!(r.ca_replacements, ["invalid"]);
}

/// Regression (helm-verify flake, 1 run in 4): a pool present at term start
/// got no pass. The reflector publishes `InitApply` objects only at
/// `InitDone`, so the pass `InitApply` triggered found no pool and was
/// dropped, and the CA-created `All` that followed could still precede
/// `InitDone`. Now `InitApply` triggers nothing and `InitDone` requeues every
/// pool, so the pool runs once the store has it whatever the order of the
/// CA and Pool events.
#[tokio::test(start_paused = true)]
async fn pool_present_at_term_start_runs_after_init_done() {
    use kube::ResourceExt;
    use kube::runtime::watcher::Event;

    use super::driver::{ca_key, pool_trigger};
    let p = pool();
    let k = (p.namespace().unwrap_or_default(), p.name_any());
    assert_eq!(pool_trigger(&Event::Init), None);
    assert_eq!(pool_trigger(&Event::InitApply(p.clone())), None);
    assert_eq!(pool_trigger(&Event::InitDone), Some(Trigger::All));
    assert_eq!(
        pool_trigger(&Event::Apply(p.clone())),
        Some(Trigger::Pool(k.clone()))
    );

    // The pass sees the store as the reflector leaves it, empty until InitDone.
    let store = Arc::new(Mutex::new(Vec::<PoolKey>::new()));
    let ran = Arc::new(Mutex::new(Vec::<PoolKey>::new()));
    let (tx, rx) = triggers();
    let task = tokio::spawn({
        let (store, ran) = (store.clone(), ran.clone());
        let all = {
            let store = store.clone();
            move || {
                let mut v = store.lock().unwrap().clone();
                v.push(ca_key());
                v
            }
        };
        schedule(rx, all, move |key| {
            let (store, ran) = (store.clone(), ran.clone());
            async move {
                if store.lock().unwrap().contains(&key) {
                    ran.lock().unwrap().push(key.clone());
                }
                (key, None)
            }
        })
    });
    // At term start the executor runs the CA pass, the buffered InitApply, then the CA Secret
    // the CA pass created (`All` while the store is still empty).
    tx.send(Trigger::Pool(ca_key())).unwrap();
    if let Some(t) = pool_trigger(&Event::InitApply(p.clone())) {
        tx.send(t).unwrap();
    }
    tx.send(Trigger::All).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(ran.lock().unwrap().is_empty());
    // InitDone publishes the pool and must requeue it.
    store.lock().unwrap().push(k.clone());
    tx.send(pool_trigger(&Event::InitDone).unwrap()).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(*ran.lock().unwrap(), [k]);
    task.abort();
}

/// An API server that never answers. Records how long each call was pending
/// before the caller dropped it.
#[derive(Default)]
struct Hang {
    lifetimes: Arc<Mutex<Vec<(String, Duration)>>>,
}

struct Pending {
    call: String,
    since: tokio::time::Instant,
    out: Arc<Mutex<Vec<(String, Duration)>>>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        let held = self.since.elapsed();
        self.out
            .lock()
            .unwrap()
            .push((std::mem::take(&mut self.call), held));
    }
}

impl Hang {
    async fn hang<T>(&self, call: &str) -> OpResult<T> {
        let _p = Pending {
            call: call.to_owned(),
            since: tokio::time::Instant::now(),
            out: Arc::clone(&self.lifetimes),
        };
        std::future::pending().await
    }
}

impl KubeOps for Hang {
    async fn apply<K: Applied>(&self, _obj: &K) -> OpResult {
        self.hang(&format!("apply:{}", K::kind(&()))).await
    }
    async fn create_pod(&self, _ns: &str, _pod: &Pod) -> OpResult<Pod> {
        self.hang("create").await
    }
    async fn retire(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: Option<&str>,
    ) -> OpResult<Option<String>> {
        self.hang("retire").await
    }
    async fn delete_pod(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: Option<String>,
        _: Option<u32>,
    ) -> OpResult {
        self.hang("delete").await
    }
    async fn patch_status(&self, _: &str, _: &str, _: &BuildKitPoolStatus) -> OpResult {
        self.hang("status").await
    }
    async fn create_secret(&self, _: &Secret) -> OpResult {
        self.hang("create_secret").await
    }
    async fn replace_secret(&self, _: &Secret) -> OpResult {
        self.hang("replace_secret").await
    }
    async fn create_event(&self, _: &Event) -> OpResult {
        self.hang("event").await
    }
}

/// Regression (control review P2): the reconciler's API calls had no time
/// bound, so a server that never answered froze a pool's pass, and a write
/// sent before the self-fence could land after another replica took the
/// Lease. Every call is now dropped at the call timeout, which never exceeds
/// the fence margin, and the pass finishes with each failure reported.
#[tokio::test(start_paused = true)]
async fn hanging_api_calls_end_within_the_fence_margin() {
    use super::driver::call_timeout;
    use super::ops::Timed;
    use crate::config::ElectionTiming;

    let timing = ElectionTiming {
        lease_duration: Duration::from_secs(15),
        renew_interval: Duration::from_secs(2),
        fence_margin: Duration::from_secs(4),
        request_timeout: Duration::from_secs(5),
    };
    let bound = call_timeout(&timing);
    assert_eq!(bound, timing.fence_margin, "never past the fence margin");
    let hang = Hang::default();
    let ops = Timed::new(hang, bound);
    let (p, ca) = (pool(), valid_ca());
    let started = tokio::time::Instant::now();
    // A regression leaves the calls pending forever; fail instead of hanging.
    let r = tokio::time::timeout(
        bound * 4,
        execute(
            &ops,
            &CaGuard::default(),
            &env(&p, &ca, None, 0),
            &plan(vec![
                Action::ApplySa,
                Action::DeleteDraining {
                    pod: "p-4".into(),
                    uid: "u4".into(),
                },
                Action::PatchStatus(ready_status()),
            ]),
        ),
    )
    .await
    .expect("the pass must end within the call bounds");
    assert_eq!(started.elapsed(), bound * 3, "the pass finishes");
    assert_eq!(r.errors.len(), 3, "{:?}", r.errors);
    let lifetimes = ops_lifetimes(&ops);
    assert_eq!(lifetimes.len(), 3);
    for (call, held) in lifetimes {
        assert!(held <= timing.fence_margin, "{call} pending for {held:?}");
    }
}

fn ops_lifetimes(ops: &super::ops::Timed<Hang>) -> Vec<(String, Duration)> {
    ops.inner().lifetimes.lock().unwrap().clone()
}

/// Regression (control review P2b): CA create/replace held the term-wide
/// `CaGuard` lock across the API call, and every pool pass waited on that
/// lock, so one slow CA write stalled every pool. The lock is now released
/// before the call.
#[tokio::test(start_paused = true)]
async fn a_slow_ca_write_does_not_stall_pool_passes() {
    use super::pass::ca_pass;
    let (s, guard) = (ca_settings(), CaGuard::default());
    let hang = Hang::default();
    let view = ca_view(1, None);
    let ca_write = ca_pass(&hang, &guard, &s, &view, now());
    let (p, ca) = (pool(), valid_ca());
    let fake = Fake::default();
    let pool_pass = async {
        // What a pool pass reads before it plans.
        let _ = guard.bundle_current(1, now());
        // A second pass of the same epoch skips the claimed create.
        let r = execute(
            &fake,
            &guard,
            &env(&p, &ca, None, 1),
            &plan(vec![Action::CreateCa]),
        )
        .await;
        execute(
            &fake,
            &guard,
            &env(&p, &ca, None, 1),
            &plan(vec![Action::ApplySa]),
        )
        .await;
        r
    };
    let started = tokio::time::Instant::now();
    tokio::select! {
        biased;
        () = async { ca_write.await; } => panic!("the hanging CA write returned"),
        r = pool_pass => assert!(r.errors.is_empty(), "{:?}", r.errors),
    }
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(fake.calls(), ["apply:ServiceAccount"]);
}

/// Regression (control review P3): CA self-heal had no rate limit, so a
/// writer that kept restoring an unusable CA Secret made the leader replace
/// it (and retire every pod) about once a second. After `HEAL_LIMIT` heals
/// in `HEAL_WINDOW` the leader stops and reports why, and resumes once the
/// window frees a slot.
#[tokio::test(start_paused = true)]
async fn ca_self_heal_is_rate_limited() {
    use super::exec::{HEAL_LIMIT, HEAL_WINDOW};
    use super::pass::ca_pass;
    let (s, guard) = (ca_settings(), CaGuard::default());
    let mut bad = CaBundle::generate(super::exec::odt(now()))
        .unwrap()
        .to_secret_data();
    bad.insert("client-ca.crt".into(), b"corrupted".to_vec());
    let fake = Fake::default();
    let mut epoch = 0;
    let mut replaced = 0;
    for _ in 0..HEAL_LIMIT + 2 {
        epoch += 1;
        let r = ca_pass(&fake, &guard, &s, &ca_view(epoch, Some(bad.clone())), now()).await;
        replaced += r.ca_replacements.len();
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    assert_eq!(replaced, HEAL_LIMIT);
    let r = ca_pass(&fake, &guard, &s, &ca_view(epoch, Some(bad.clone())), now()).await;
    assert!(
        r.errors.iter().any(|e| e.contains("self-heal paused")),
        "{:?}",
        r.errors
    );
    tokio::time::advance(HEAL_WINDOW).await;
    let r = ca_pass(&fake, &guard, &s, &ca_view(epoch, Some(bad)), now()).await;
    assert_eq!(r.ca_replacements, ["invalid"]);
}

/// Regression (control review P3): a pool deleted while the leader's Pool
/// watch was relisting produced no `Delete` event, and `InitDone` triggered
/// only the pools still in the store, so its memory and gauges stayed for the
/// rest of the term. `All` now also runs every remembered pool.
#[test]
fn all_trigger_reaches_remembered_pools_that_left_the_store() {
    use super::driver::{all_keys, ca_key};
    let live = Arc::new(pool());
    let live_key = ("default".to_owned(), "p".to_owned());
    let gone = ("default".to_owned(), "deleted".to_owned());
    let mut live_ns = (*live).clone();
    live_ns.metadata.namespace = Some("default".into());
    let keys = all_keys(&[Arc::new(live_ns)], [&gone, &live_key].into_iter());
    assert_eq!(keys, vec![ca_key(), gone, live_key]);
}

impl WorkerSecrets for Fake {
    /// Records the owner; the key `secret_create` in `fail` fails every create.
    fn create_worker_secret(&self, s: &Secret) -> BoxFuture<'static, Result<(), String>> {
        let name = s.metadata.name.clone().unwrap_or_default();
        let owners: Vec<String> = (s.metadata.owner_references.iter().flatten())
            .map(|o| format!("{}/{}", o.kind, o.uid))
            .collect();
        let res = self
            .call(format!("secret_create:{name}:{}", owners.join(",")))
            .and(self.fail.get("secret_create").cloned().map_or(Ok(()), Err));
        Box::pin(async move { res.map_err(|e| e.to_string()) })
    }
    fn get_worker_pod(
        &self,
        _: &str,
        name: &str,
    ) -> BoxFuture<'static, Result<Option<Pod>, String>> {
        let res = self.call(format!("get_pod:{name}"));
        Box::pin(async move { res.map(|()| None).map_err(|e| e.to_string()) })
    }
}

impl WorkerSecrets for Hang {
    fn create_worker_secret(&self, _: &Secret) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn get_worker_pod(&self, _: &str, _: &str) -> BoxFuture<'static, Result<Option<Pod>, String>> {
        Box::pin(async { Ok(None) })
    }
}

fn create(slot: u32) -> Action {
    Action::CreatePod {
        slot,
        name: format!("p-{slot}"),
    }
}

/// Regression (RECON-2): `SandboxPending` was written only inside a planned
/// `PatchStatus`, and the planner plans one only when the status changed. A
/// failed apply under an unchanged status (a new term's first pass, say)
/// left `Ready=True`. The executor now patches the status itself, once.
#[tokio::test]
async fn failed_apply_downgrades_ready_without_a_planned_status_patch() {
    let (mut p, ca) = (pool(), valid_ca());
    p.status = Some(*ready_status());
    let actions = vec![Action::ApplySa, Action::ApplyNetworkPolicy, create(0)];
    let fake = Fake::failing(&[("apply:NetworkPolicy", OpError::Other("denied".into()))]);
    let r = execute(
        &fake,
        &CaGuard::default(),
        &env(&p, &ca, None, 0),
        &plan(actions.clone()),
    )
    .await;
    assert_eq!(
        fake.calls(),
        ["apply:ServiceAccount", "apply:NetworkPolicy", "status"]
    );
    let sent = r.status_sent.clone().unwrap();
    let ready = sent
        .conditions
        .iter()
        .find(|c| c.type_ == condition::READY)
        .unwrap();
    assert_eq!(
        (ready.status.as_str(), ready.reason.as_str()),
        ("False", reason::SANDBOX_PENDING)
    );
    // With that status cached, the same failure sends nothing more.
    p.status = Some(sent);
    let fake = Fake::failing(&[("apply:NetworkPolicy", OpError::Other("denied".into()))]);
    let r = execute(
        &fake,
        &CaGuard::default(),
        &env(&p, &ca, None, 0),
        &plan(actions),
    )
    .await;
    assert_eq!(
        fake.calls(),
        ["apply:ServiceAccount", "apply:NetworkPolicy"]
    );
    assert!(r.status_sent.is_none());
}

/// Regression (RECON-7): a pass that read an older CA epoch could apply its
/// bundle after a newer epoch's, leaving the `ConfigMap` on a replaced CA
/// while the guard recorded it current. An older epoch never starts after a
/// newer one, and one that lands late makes the newest apply again.
#[tokio::test]
async fn an_older_ca_bundle_never_wins() {
    let guard = CaGuard::default();
    let (p, old, new) = (pool(), valid_ca(), valid_ca());
    let gate = Arc::new(tokio::sync::Notify::new());
    let slow = Fake {
        gate: Some(gate.clone()),
        ..Fake::default()
    };
    let bundle = || plan(vec![Action::ApplyCaBundle]);
    let (old_env, new_env) = (env(&p, &old, None, 1), env(&p, &new, None, 3));
    let slow_plan = bundle();
    let slow_pass = execute(&slow, &guard, &old_env, &slow_plan);
    tokio::pin!(slow_pass);
    assert!(futures::poll!(&mut slow_pass).is_pending());
    // Epoch 3 applies while epoch 1 is in flight.
    let fast = Fake::default();
    let r = execute(&fast, &guard, &new_env, &bundle()).await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(guard.bundle_current(3, now()));
    // A pass that read epoch 1 only now applies nothing.
    let late = Fake::default();
    execute(&late, &guard, &old_env, &bundle()).await;
    assert!(late.calls().is_empty(), "{:?}", late.calls());
    // The in-flight epoch 1 apply lands last: epoch 3 is due again and the
    // pass fails, so it requeues.
    gate.notify_one();
    let r = slow_pass.await;
    assert!(!r.errors.is_empty());
    assert!(!guard.bundle_current(3, now()));
    let again = Fake::default();
    execute(&again, &guard, &new_env, &bundle()).await;
    assert_eq!(again.calls(), ["apply:ConfigMap"]);
    assert!(guard.bundle_current(3, now()));
}

/// RECON-4: the CA bundle `ConfigMap` counts as current for `REAPPLY_AFTER`,
/// then the CA pass applies it again, so a deleted one comes back without a
/// pool or a CA change.
#[tokio::test]
async fn ca_bundle_is_applied_again_after_the_reapply_period() {
    use super::pass::ca_pass;
    use super::plan::REAPPLY_AFTER;
    let (s, guard) = (ca_settings(), CaGuard::default());
    let data = CaBundle::generate(super::exec::odt(now()))
        .unwrap()
        .to_secret_data();
    let view = ca_view(1, Some(data));
    let fake = Fake::default();
    ca_pass(&fake, &guard, &s, &view, now()).await;
    ca_pass(
        &fake,
        &guard,
        &s,
        &view,
        now() + REAPPLY_AFTER - k8s_openapi::jiff::SignedDuration::from_secs(1),
    )
    .await;
    assert_eq!(fake.calls(), ["apply:ConfigMap"]);
    ca_pass(&fake, &guard, &s, &view, now() + REAPPLY_AFTER).await;
    assert_eq!(fake.calls(), ["apply:ConfigMap", "apply:ConfigMap"]);
}

/// RECON-11: the driver over a pool watch of two namespaces. After the first
/// lists, a relist of one namespace that lacks a pool becomes a `Delete`, so
/// the pass for that pool runs, finds it gone and forgets it, and the other
/// namespace is untouched.
#[tokio::test]
async fn relist_of_one_namespace_triggers_its_deleted_pools() {
    use futures::StreamExt;
    use kube::runtime::reflector;
    use kube::runtime::watcher::Event;

    use super::driver::pool_trigger;
    use crate::runtime::stores::merge_namespaces;
    let in_ns = |ns: &str, name: &str| {
        let mut p = fixtures::pool(name, json!({})).unwrap();
        p.metadata.namespace = Some(ns.into());
        p
    };
    let (a1, bx, by) = (in_ns("a", "p1"), in_ns("b", "x"), in_ns("b", "y"));
    let input: Vec<(usize, kube::runtime::watcher::Result<Event<BuildKitPool>>)> = vec![
        (0, Ok(Event::Init)),
        (0, Ok(Event::InitApply(a1.clone()))),
        (0, Ok(Event::InitDone)),
        (1, Ok(Event::Init)),
        (1, Ok(Event::InitApply(bx.clone()))),
        (1, Ok(Event::InitApply(by.clone()))),
        (1, Ok(Event::InitDone)),
        // `x` is deleted while `b` relists.
        (1, Ok(Event::Init)),
        (1, Ok(Event::InitApply(by))),
        (1, Ok(Event::InitDone)),
    ];
    let (store, writer) = reflector::store::<BuildKitPool>();
    let merged = merge_namespaces(
        futures::stream::iter(input).boxed(),
        vec!["a".into(), "b".into()],
        store.clone(),
    );
    let triggers: Vec<Trigger> = reflector::reflector(writer, merged)
        .filter_map(|ev| std::future::ready(ev.ok().as_ref().and_then(pool_trigger)))
        .collect()
        .await;
    assert_eq!(
        triggers,
        [
            Trigger::All,
            Trigger::Pool(("b".into(), "y".into())),
            Trigger::Pool(("b".into(), "x".into())),
        ]
    );
    let mut left: Vec<String> = store
        .state()
        .iter()
        .map(|p| {
            format!(
                "{}/{}",
                p.metadata.namespace.as_deref().unwrap_or(""),
                p.metadata.name.as_deref().unwrap_or("")
            )
        })
        .collect();
    left.sort();
    assert_eq!(left, ["a/p1", "b/y"]);
}

/// The leader's Pool watch is tolerant: with namespace `b` failing every list,
/// the pools of `a` still get an `All` trigger once `b` is given up.
#[tokio::test(start_paused = true)]
async fn an_unlistable_namespace_does_not_hold_the_other_pools() {
    use futures::StreamExt;
    use kube::runtime::reflector;
    use kube::runtime::watcher::{Error, Event};

    use super::driver::pool_trigger;
    use crate::runtime::stores::{GIVE_UP_AFTER, NamespaceReport, merge_tolerant};
    let mut a1 = fixtures::pool("p1", json!({})).unwrap();
    a1.metadata.namespace = Some("a".into());
    let (tx, rx) = futures::channel::mpsc::unbounded();
    for item in [
        (0, Ok(Event::Init)),
        (0, Ok(Event::InitApply(a1))),
        (0, Ok(Event::InitDone)),
        (1, Err(Error::NoResourceVersion)),
    ] {
        tx.unbounded_send(item).unwrap();
    }
    let (store, writer) = reflector::store::<BuildKitPool>();
    let merged = merge_tolerant(
        rx.boxed(),
        vec!["a".into(), "b".into()],
        store.clone(),
        NamespaceReport::log("controller"),
    );
    let mut triggers = Box::pin(
        reflector::reflector(writer, merged)
            .filter_map(|ev| std::future::ready(ev.ok().as_ref().and_then(pool_trigger))),
    );
    let first = tokio::time::timeout(GIVE_UP_AFTER * 2, triggers.next())
        .await
        .expect("a trigger once b is given up");
    assert_eq!(first, Some(Trigger::All));
    assert_eq!(store.state().len(), 1);
    drop(tx);
}
