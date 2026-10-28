//! Executor tests against a fake Kubernetes API covering action order, the skip
//! rules, requeue after outcomes, and term cancellation.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use buildkit_controller_api::v1alpha1::{BuildKitPool, BuildKitPoolStatus, condition, reason};
use futures::future::BoxFuture;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Secret};
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
use super::plan::{Action, CaEvent, CreateOutcome, CreateResult, Plan, RetireReason, WaiterRef};
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
    pvcs: Mutex<std::collections::BTreeMap<String, PersistentVolumeClaim>>,
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
    async fn prune_waiter(&self, _ns: &str, _pool: &str, e: &WaiterRef) -> OpResult {
        self.call(format!("prune:{}", e.index))
    }
    async fn create_secret(&self, s: &Secret) -> OpResult {
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
    async fn get_pvc(&self, _ns: &str, name: &str) -> OpResult<Option<PersistentVolumeClaim>> {
        self.call(format!("get_pvc:{name}"))?;
        Ok(self.pvcs.lock().unwrap().get(name).cloned())
    }
    async fn create_pvc(&self, pvc: &PersistentVolumeClaim) -> OpResult {
        let name = pvc.metadata.name.clone().unwrap_or_default();
        self.call(format!("create_pvc:{name}"))?;
        let mut p = pvc.clone();
        p.metadata.uid = Some(format!("uid-{name}"));
        self.pvcs.lock().unwrap().insert(name, p);
        Ok(())
    }
    async fn delete_pvc(&self, _ns: &str, name: &str, uid: &str) -> OpResult {
        self.call(format!("delete_pvc:{name}:{uid}"))?;
        self.pvcs.lock().unwrap().remove(name);
        Ok(())
    }
    async fn list_pvcs(&self, _ns: &str, _selector: &str) -> OpResult<Vec<PersistentVolumeClaim>> {
        self.call("list_pvcs".into())?;
        Ok(self.pvcs.lock().unwrap().values().cloned().collect())
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
        config_hash: "cfg",
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
        Action::PruneWaiters {
            entries: vec![
                WaiterRef {
                    index: 1,
                    replica: "b".into(),
                    updated_at: Time(now()),
                },
                WaiterRef {
                    index: 0,
                    replica: "a".into(),
                    updated_at: Time(now()),
                },
            ],
        },
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
            "prune:1",
            "prune:0",
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
        // Deletes, retires, status and prunes still run.
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
        ("create:p-1", OpError::Other("quota".into())),
    ]);
    let creates = vec![
        Action::CreatePod {
            slot: 0,
            name: "p-0".into(),
        },
        Action::CreatePod {
            slot: 1,
            name: "p-1".into(),
        },
        Action::CreatePod {
            slot: 2,
            name: "p-2".into(),
        },
    ];
    let r = execute(
        &fake,
        &CaGuard::default(),
        &env(&p, &ca, None, 0),
        &plan(creates),
    )
    .await;
    let results: Vec<_> = r.creates.iter().map(|c| c.result.clone()).collect();
    assert_eq!(results[0], CreateResult::SlotTaken);
    assert!(matches!(results[1], CreateResult::Failed { .. }));
    assert_eq!(results[2], CreateResult::Created);
    assert_eq!(r.created.len(), 1);
    // A refused create gets no Secret. An unknown outcome is read back first,
    // and no pod of this create means no Secret. The created pod owns its
    // Secret from the start.
    assert_eq!(
        fake.calls_n(),
        [
            "create:p-0",
            "create:p-1",
            "get_pod:p-1",
            "create:p-2",
            "secret_create:p-2-N:Pod/uid-p-2",
        ]
    );
}

#[tokio::test]
async fn a_refused_secret_create_leaves_a_pod_that_cannot_start() {
    let (p, ca) = (pool(), valid_ca());
    let fake = Fake::failing(&[("secret_create", OpError::Other("forbidden".into()))]);
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
    // The pod exists (the planner retires it at startTimeout), and the
    // failure counts toward the create backoff.
    assert_eq!(r.created.len(), 1);
    assert!(matches!(r.creates[0].result, CreateResult::Failed { .. }));
    assert_eq!(
        fake.calls_n(),
        ["create:p-0", "secret_create:p-0-N:Pod/uid-p-0"]
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
    let failed = outcome(CreateResult::Failed {
        message: "quota".into(),
    });
    let seq: Vec<_> = (0..8)
        .map(|_| rq(&mut mem, &failed).unwrap().as_secs())
        .collect();
    assert_eq!(seq, [1, 2, 4, 8, 16, 32, 60, 60]);
    assert_eq!(
        rq(&mut mem, &outcome(CreateResult::Created)),
        Some(Duration::from_secs(30))
    );
    assert_eq!(rq(&mut mem, &failed), Some(Duration::from_secs(1)));
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
    async fn prune_waiter(&self, _: &str, _: &str, _: &WaiterRef) -> OpResult {
        self.hang("prune").await
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
    async fn get_pvc(&self, _: &str, _: &str) -> OpResult<Option<PersistentVolumeClaim>> {
        self.hang("get_pvc").await
    }
    async fn create_pvc(&self, _: &PersistentVolumeClaim) -> OpResult {
        self.hang("create_pvc").await
    }
    async fn delete_pvc(&self, _: &str, _: &str, _: &str) -> OpResult {
        self.hang("delete_pvc").await
    }
    async fn list_pvcs(&self, _: &str, _: &str) -> OpResult<Vec<PersistentVolumeClaim>> {
        self.hang("list_pvcs").await
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
        let _ = guard.bundle_current(1);
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

/// A pool with `cache.local` (limit 2 of max 4), ready to create pods.
fn cached_pool() -> BuildKitPool {
    let mut p = fixtures::pool(
        "p",
        json!({"scaling": {"max": 4}, "workers": {"cache": {"local": {"maxVolumes": 2}}}}),
    )
    .unwrap();
    p.metadata.generation = Some(3);
    p.status = Some(BuildKitPoolStatus::default());
    p
}

/// Seeds the rendered volume of `slot` (as if created earlier), then lets `edit` change it.
fn seed_pvc(
    fake: &Fake,
    p: &BuildKitPool,
    slot: u32,
    edit: impl FnOnce(&mut PersistentVolumeClaim),
) {
    let mut pvc = crate::render::cache_volume(p, slot)
        .unwrap()
        .unwrap_or_else(|| {
            crate::render::cache_volume(&cached_pool(), 0)
                .unwrap()
                .unwrap()
        });
    let name = format!("p-cache-{slot}");
    pvc.metadata.name = Some(name.clone());
    pvc.metadata.uid = Some(format!("uid-{name}"));
    edit(&mut pvc);
    fake.pvcs.lock().unwrap().insert(name, pvc);
}

fn create(slot: u32) -> Action {
    Action::CreatePod {
        slot,
        name: format!("p-{slot}"),
    }
}

fn state_claim(p: &Pod) -> Option<String> {
    let vs = p.spec.as_ref()?.volumes.as_ref()?;
    let v = vs.iter().find(|v| v.name == "state")?;
    v.persistent_volume_claim
        .as_ref()
        .map(|c| c.claim_name.clone())
}

#[tokio::test]
async fn cached_slot_creates_its_volume_before_the_pod() {
    let (p, ca) = (cached_pool(), valid_ca());
    let fake = Fake::default();
    let r = execute(
        &fake,
        &CaGuard::default(),
        &env(&p, &ca, None, 0),
        &plan(vec![create(1), create(2)]),
    )
    .await;
    // The volume exists before the Secret and the pod; slot 2 has none.
    assert_eq!(
        fake.calls_n(),
        [
            "get_pvc:p-cache-1",
            "create_pvc:p-cache-1",
            "create:p-1",
            "secret_create:p-1-N:Pod/uid-p-1",
            "create:p-2",
            "secret_create:p-2-N:Pod/uid-p-2",
        ]
    );
    assert_eq!(state_claim(&r.created[0]).as_deref(), Some("p-cache-1"));
    assert_eq!(state_claim(&r.created[1]), None);
    assert_eq!(r.volume_ops, [("create", "ok")]);
}

#[tokio::test]
async fn volume_create_conflict_goes_on_and_other_errors_fail_the_create() {
    let (p, ca) = (cached_pool(), valid_ca());
    let fake = Fake::failing(&[
        ("create_pvc:p-cache-0", OpError::Conflict("exists".into())),
        (
            "create_pvc:p-cache-1",
            OpError::Other("exceeded quota".into()),
        ),
    ]);
    let r = execute(
        &fake,
        &CaGuard::default(),
        &env(&p, &ca, None, 0),
        &plan(vec![create(0), create(1)]),
    )
    .await;
    assert_eq!(r.creates[0].result, CreateResult::Created);
    assert!(matches!(r.creates[1].result, CreateResult::Failed { .. }));
    assert!(!fake.calls().contains(&"create:p-1".to_owned()));
    assert_eq!(r.volume_ops, [("create", "ok"), ("create", "error")]);
}

#[tokio::test]
async fn current_volume_passes_its_cache_key_to_the_pod() {
    use crate::render::{KEY_CACHE_GENERATION, KEY_CACHE_KEYS, KEY_CACHE_SCOPE, pod_cache_keys};
    let ann = |p: &Pod, k: &str| {
        p.metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(k).cloned())
    };
    let (p, ca) = (cached_pool(), valid_ca());
    // (PVC cache-scope, expected P cache-scope, P cache-generation, keys copied)
    let cases = [
        // Bound: the warm pod serves the owner and keeps the generation and keys.
        (Some("s1@g1"), "s1", Some("g1"), true),
        // Never bound (or a volume from before scopes): serves any scope, emptied.
        (None, "", None, false),
        // Unbound by a recycle.
        (Some(""), "", None, false),
        // Malformed: treated as unbound.
        (Some("s1"), "", None, false),
    ];
    for (binding, scope, generation, keys) in cases {
        let fake = Fake::default();
        seed_pvc(&fake, &p, 0, |v| {
            let a = v.metadata.annotations.get_or_insert_default();
            a.insert(KEY_CACHE_KEYS.into(), "repo-main,repo-dev".into());
            if let Some(b) = binding {
                a.insert(KEY_CACHE_SCOPE.into(), b.into());
            }
        });
        let r = execute(
            &fake,
            &CaGuard::default(),
            &env(&p, &ca, None, 0),
            &plan(vec![create(0)]),
        )
        .await;
        assert_eq!(
            fake.calls_n(),
            [
                "get_pvc:p-cache-0",
                "create:p-0",
                "secret_create:p-0-N:Pod/uid-p-0",
            ]
        );
        let pod = &r.created[0];
        assert_eq!(
            ann(pod, KEY_CACHE_SCOPE).as_deref(),
            Some(scope),
            "{binding:?}"
        );
        assert_eq!(
            ann(pod, KEY_CACHE_GENERATION).as_deref(),
            generation,
            "{binding:?}"
        );
        assert_eq!(
            pod_cache_keys(pod),
            keys.then_some("repo-main,repo-dev"),
            "{binding:?}"
        );
    }
}

#[tokio::test]
async fn terminating_replaced_or_foreign_volumes_block_the_create() {
    let (p, ca) = (cached_pool(), valid_ca());
    let fake = Fake::default();
    seed_pvc(&fake, &p, 0, |v| {
        v.metadata.deletion_timestamp = Some(Time(now()));
    });
    seed_pvc(&fake, &p, 1, |v| {
        v.metadata
            .labels
            .get_or_insert_default()
            .insert(crate::render::LABEL_VOLUME_HASH.into(), "old".into());
    });
    let r = execute(
        &fake,
        &CaGuard::default(),
        &env(&p, &ca, None, 0),
        &plan(vec![create(0), create(1)]),
    )
    .await;
    assert_eq!(r.creates[0].result, CreateResult::VolumePending);
    assert_eq!(r.creates[1].result, CreateResult::VolumePending);
    assert_eq!(
        fake.calls(),
        [
            "get_pvc:p-cache-0",
            "get_pvc:p-cache-1",
            "delete_pvc:p-cache-1:uid-p-cache-1"
        ]
    );
    assert_eq!(
        super::plan::Plan::requeue_after_outcomes(&r.creates),
        super::plan::Requeue::ErrorBackoff
    );

    let fake = Fake::default();
    seed_pvc(&fake, &p, 0, |v| v.metadata.owner_references = None);
    let r = execute(
        &fake,
        &CaGuard::default(),
        &env(&p, &ca, None, 0),
        &plan(vec![create(0)]),
    )
    .await;
    assert!(matches!(r.creates[0].result, CreateResult::Failed { .. }));
    assert_eq!(
        fake.calls(),
        ["get_pvc:p-cache-0"],
        "a foreign volume is never deleted"
    );
}

#[tokio::test]
async fn only_a_crash_or_start_timeout_in_a_cached_slot_loses_its_volume() {
    let (p, ca) = (cached_pool(), valid_ca());
    // Expired is the normal end of an unreleased build; PodFinished covers a
    // build that ended its own daemon and an evicted pod. Those keep the cache.
    let cases = [
        (RetireReason::StartTimeout, true),
        (RetireReason::DaemonCrashed, true),
        (RetireReason::Expired, false),
        (RetireReason::PodFinished, false),
        (RetireReason::Idle, false),
    ];
    for (reason, reaped) in cases {
        let fake = Fake::default();
        seed_pvc(&fake, &p, 0, |_| {});
        let action = Action::RetireFree {
            pod: "p-0".into(),
            uid: "u0".into(),
            reason,
        };
        let r = execute(
            &fake,
            &CaGuard::default(),
            &env(&p, &ca, None, 0),
            &plan(vec![action]),
        )
        .await;
        let pvc_calls: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|c| c.contains("pvc"))
            .collect();
        let want: &[&str] = if reaped {
            &["get_pvc:p-cache-0", "delete_pvc:p-cache-0:uid-p-cache-0"]
        } else {
            &[]
        };
        assert_eq!(pvc_calls, want, "{reason:?}");
        assert_eq!(r.volume_ops.len(), usize::from(reaped), "{reason:?}");
    }
}
#[tokio::test]
async fn volumes_above_the_limit_are_reaped_unless_a_pod_holds_the_slot() {
    let p = cached_pool();
    let fake = Fake::default();
    for s in 0..4 {
        seed_pvc(&fake, &p, s, |_| {});
    }
    let mut r = super::exec::Report::default();
    let held = BTreeSet::from([3]);
    assert!(!super::cache::reap_above_limit(&fake, &p, &held, &mut r).await);
    assert_eq!(
        fake.calls(),
        ["list_pvcs", "delete_pvc:p-cache-2:uid-p-cache-2"]
    );
    assert!(super::cache::reap_above_limit(&fake, &p, &BTreeSet::new(), &mut r).await);
    let left: Vec<String> = fake.pvcs.lock().unwrap().keys().cloned().collect();
    assert_eq!(left, ["p-cache-0", "p-cache-1"]);

    // Turning cache.local off reaps every volume of the pool.
    let mut off = p.clone();
    off.spec.workers.cache.local = None;
    assert!(super::cache::reap_above_limit(&fake, &off, &BTreeSet::new(), &mut r).await);
    assert!(fake.pvcs.lock().unwrap().is_empty());
}
