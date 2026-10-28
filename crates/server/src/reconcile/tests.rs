//! Table tests for the planner.

use std::sync::LazyLock;

use buildkit_controller_api::v1alpha1::{
    BuildKitPool, BuildKitPoolSpec, BuildKitPoolStatus, Day, Waiter, Window,
};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, OwnerReference, Time};
use k8s_openapi::jiff::tz::TimeZone;
use k8s_openapi::jiff::{SignedDuration, Timestamp};

use super::plan::*;
use super::pods::{Alloc, Phase, WorkerPod, foreign_pod_name, parse_slot};
use super::schedule;
use crate::pki::{CaBundle, CaError, LoadOutcome};

/// Tuesday 2026-10-20 12:00:00 UTC.
pub(super) static NOW: LazyLock<Timestamp> =
    LazyLock::new(|| "2026-10-20T12:00:00Z".parse().unwrap());
pub(super) static CA: LazyLock<LoadOutcome> =
    LazyLock::new(|| LoadOutcome::Valid(Box::new(CaBundle::generate(odt(*NOW)).unwrap())));
pub(super) static MISSING: LoadOutcome = LoadOutcome::Missing;
pub(super) static INVALID: LazyLock<LoadOutcome> =
    LazyLock::new(|| LoadOutcome::Invalid(CaError::Missing("internal-ca.crt")));
pub(super) const ALLOC_ID: &str = "abcdefghijklmnopqrstuvwxyz";
/// CAs generated 7305 - 100 days ago, so they expire in 100 days.
static OLD: LazyLock<LoadOutcome> = LazyLock::new(|| {
    let t = odt(*NOW) - time::Duration::days(7305 - 100);
    LoadOutcome::Valid(Box::new(CaBundle::generate(t).unwrap()))
});

/// (name, min, max, pods, fresh waiters, expected creates)
type SizingCase = (&'static str, i32, i32, Vec<WorkerPod>, i32, Vec<u32>);

fn odt(t: Timestamp) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(t.as_second()).unwrap()
}

pub(super) fn fp() -> String {
    match &*CA {
        LoadOutcome::Valid(b) => b.internal_ca().fingerprint_hex(),
        _ => unreachable!(),
    }
}

pub(super) fn ago(s: i64) -> Timestamp {
    *NOW - SignedDuration::from_secs(s)
}

pub(super) fn later(s: i64) -> Timestamp {
    *NOW + SignedDuration::from_secs(s)
}

pub(super) struct Fx {
    pub pool: BuildKitPool,
    pub pods: Vec<WorkerPod>,
    pub ca: &'static LoadOutcome,
    pub tz: Option<TimeZone>,
    pub template: TemplateCheck,
    pub sandbox_gen: Option<i64>,
    pub prev_fail: bool,
    pub creates: Vec<CreateOutcome>,
    pub backoff: Option<Timestamp>,
    pub foreign: Vec<String>,
    pub slot_holds: std::collections::BTreeMap<u32, Timestamp>,
    pub became_ready: std::collections::BTreeSet<String>,
    pub waiters_seen: std::collections::BTreeMap<(String, Timestamp), Timestamp>,
}

/// Steady state with min 0, max 10, CA valid, sandbox applied, worker Secret current
/// until NOW+30d.
pub(super) fn fx() -> Fx {
    let mut pool = BuildKitPool::new("p", BuildKitPoolSpec::default());
    pool.metadata.namespace = Some("ns".into());
    pool.metadata.uid = Some("pool-uid".into());
    pool.metadata.generation = Some(1);
    pool.status = Some(BuildKitPoolStatus::default());
    Fx {
        pool,
        pods: Vec::new(),
        ca: &CA,
        tz: Some(TimeZone::UTC),
        template: TemplateCheck::Current { hash: "h".into() },
        sandbox_gen: Some(1),
        prev_fail: false,
        creates: Vec::new(),
        backoff: None,
        foreign: Vec::new(),
        slot_holds: std::collections::BTreeMap::new(),
        became_ready: std::collections::BTreeSet::new(),
        waiters_seen: std::collections::BTreeMap::new(),
    }
}

/// A Ready, Running free pod in `slot`, one hour old.
pub(super) fn pod(slot: u32) -> WorkerPod {
    WorkerPod {
        name: format!("p-{slot}"),
        uid: format!("uid-{slot}"),
        slot: Some(slot),
        alloc: Alloc::Free,
        phase: Phase::Running,
        ready: true,
        has_ip: true,
        created: Some(ago(3600)),
        deletion: None,
        grace_seconds: 5,
        template_hash: Some("h".into()),
        expires_at: None,
        expires_at_raw: None,
        worker_cert_not_after: Some(later(30 * 86_400)),
        internal_ca: Some(fp()[..16].to_owned()),
        evicted: false,
    }
}

fn allocated(slot: u32) -> WorkerPod {
    WorkerPod {
        alloc: Alloc::Allocated(ALLOC_ID.into()),
        expires_at: Some(later(3600)),
        ..pod(slot)
    }
}

impl Fx {
    pub fn plan(&self) -> Plan {
        plan(&self.observed(), *NOW, &PlanConfig::default())
    }

    pub fn observed(&self) -> Observed<'_> {
        Observed {
            pool: &self.pool,
            time_zone: self.tz.clone(),
            template: self.template.clone(),
            config_hash: "cfg".into(),
            platforms: Vec::new(),
            ca: self.ca,
            ca_bundle_current: true,
            pods: self.pods.clone(),
            sandbox_applied_generation: self.sandbox_gen,
            previous_pod_failure: self.prev_fail,
            recent_creates: self.creates.clone(),
            create_backoff_until: self.backoff,
            unknown_issuer: None,
            foreign_pods: self.foreign.clone(),
            slot_holds: self.slot_holds.clone(),
            became_ready: self.became_ready.clone(),
            waiters_seen: self.waiters_seen.clone(),
        }
    }

    fn scale(mut self, min: i32, max: i32) -> Self {
        self.pool.spec.scaling.min = min;
        self.pool.spec.scaling.max = max;
        self
    }

    fn waiter(&mut self, replica: &str, count: i32, updated: Timestamp) {
        let s = self.pool.status.get_or_insert_with(Default::default);
        s.waiters.push(Waiter {
            replica: replica.into(),
            count,
            updated_at: Time(updated),
        });
        // The leader saw it when it was written (clocks agree).
        self.waiters_seen
            .insert((replica.to_owned(), updated), updated);
    }
}

pub(super) fn creates(p: &Plan) -> Vec<u32> {
    p.actions
        .iter()
        .filter_map(|a| match a {
            Action::CreatePod { slot, .. } => Some(*slot),
            _ => None,
        })
        .collect()
}

fn retires(p: &Plan) -> Vec<(String, RetireReason)> {
    p.actions
        .iter()
        .filter_map(|a| match a {
            Action::RetireFree { pod, reason, .. }
            | Action::RetireAllocated { pod, reason, .. } => Some((pod.clone(), *reason)),
            _ => None,
        })
        .collect()
}

fn status(p: &Plan) -> &BuildKitPoolStatus {
    p.actions
        .iter()
        .find_map(|a| match a {
            Action::PatchStatus(s) => Some(&**s),
            _ => None,
        })
        .expect("PatchStatus")
}

fn cond<'a>(p: &'a Plan, ty: &str) -> &'a Condition {
    status(p)
        .conditions
        .iter()
        .find(|c| c.type_ == ty)
        .expect("condition")
}

#[test]
fn sizing_table() {
    let draining = |s, deleting: bool| WorkerPod {
        alloc: Alloc::Draining,
        deletion: deleting.then(|| ago(10)),
        ..pod(s)
    };
    // (name, min, max, pods, fresh waiters, expected creates)
    let cases: Vec<SizingCase> = vec![
        ("min warm from empty", 2, 10, vec![], 0, vec![0, 1]),
        ("min already warm", 2, 10, vec![pod(0), pod(1)], 0, vec![]),
        ("lowest free slots", 3, 10, vec![pod(1)], 0, vec![0, 2]),
        ("capped by max", 5, 3, vec![], 0, vec![0, 1, 2]),
        (
            "draining and terminating hold slots",
            2,
            3,
            vec![draining(0, false), draining(1, true)],
            0,
            vec![2],
        ),
        ("allocated holds slot", 1, 2, vec![allocated(0)], 0, vec![1]),
        (
            "full pool",
            3,
            2,
            vec![allocated(0), allocated(1)],
            0,
            vec![],
        ),
        ("waiters drive scale-up", 0, 10, vec![pod(0)], 3, vec![1, 2]),
        (
            "starting pods count as free",
            1,
            10,
            vec![WorkerPod {
                ready: false,
                created: Some(ago(5)),
                ..pod(0)
            }],
            0,
            vec![],
        ),
    ];
    for (name, min, max, pods, waiting, want) in cases {
        let mut f = fx().scale(min, max);
        f.pods = pods;
        if waiting > 0 {
            f.waiter("r1", waiting, ago(10));
        }
        assert_eq!(creates(&f.plan()), want, "{name}");
    }
}

#[test]
fn slot_taken_and_create_failures() {
    let mut f = fx().scale(2, 3);
    f.creates = vec![CreateOutcome {
        slot: 0,
        result: CreateResult::SlotTaken,
    }];
    let p = f.plan();
    assert_eq!(creates(&p), vec![1, 2], "slot 0 is held by a foreign pod");
    // Regression (control review P3): the pass right after the 409 reported
    // SlotTaken although the other replica's pod usually reaches the store a
    // moment later, so Degraded flapped. The hold is reported after 2 s.
    assert_eq!(cond(&p, "Degraded").reason, "AsExpected");
    assert_eq!(p.requeue_at, Some(later(2)), "recheck after the grace");
    f.creates.clear();
    f.slot_holds = [(0, later(598))].into();
    let p = f.plan();
    let d = cond(&p, "Degraded");
    assert_eq!(
        (d.status.as_str(), d.reason.as_str()),
        ("True", "SlotTaken")
    );
    assert!(d.message.contains("p-0"));

    // A 409 for our own pod not yet in the store at create time gives no SlotTaken now.
    f.pods = vec![pod(0)];
    f.creates = vec![CreateOutcome {
        slot: 0,
        result: CreateResult::SlotTaken,
    }];
    let p = f.plan();
    assert_eq!(cond(&p, "Degraded").reason, "AsExpected");

    f.creates = vec![CreateOutcome {
        slot: 1,
        result: CreateResult::Failed {
            message: "quota".into(),
        },
    }];
    assert_eq!(cond(&f.plan(), "Degraded").reason, "PodCreateFailed");
}

#[test]
fn idle_reaping_oldest_first_down_to_desired() {
    let aged = |s, secs| WorkerPod {
        created: Some(ago(secs)),
        ..pod(s)
    };
    let mut f = fx().scale(1, 10);
    f.pods = vec![aged(0, 700), aged(1, 3600), aged(2, 1200)];
    let p = f.plan();
    assert_eq!(
        retires(&p),
        vec![
            ("p-1".into(), RetireReason::Idle),
            ("p-2".into(), RetireReason::Idle)
        ]
    );
    assert!(creates(&p).is_empty());

    // Young pods are not idle yet. Requeue when the oldest reaches idleTimeout.
    f.pods = vec![aged(0, 60), aged(1, 120)];
    let p = f.plan();
    assert!(retires(&p).is_empty());
    assert_eq!(p.requeue_at, Some(later(600 - 120)));

    // Starting pods are never retired for idleness.
    f = fx();
    f.pods = vec![WorkerPod {
        ready: false,
        created: Some(ago(60)),
        ..pod(0)
    }];
    assert!(retires(&f.plan()).is_empty());
}

#[test]
fn expiry_retires_allocated_with_expected_id() {
    let mut f = fx();
    f.pods = vec![WorkerPod {
        expires_at: Some(*NOW),
        expires_at_raw: Some("2026-01-01T00:00:00Z".into()),
        ..allocated(0)
    }];
    let p = f.plan();
    // The retire tests the value it judged expired, so it loses to a commit
    // that moved `expires-at` from a claim's short value in the meantime.
    assert!(p.actions.contains(&Action::RetireAllocated {
        pod: "p-0".into(),
        uid: "uid-0".into(),
        alloc_id: ALLOC_ID.into(),
        expires_at: Some("2026-01-01T00:00:00Z".into()),
        reason: RetireReason::Expired,
    }));

    // Other reasons hold whatever `expires-at` says, so they do not test it.
    f.pods = vec![WorkerPod {
        phase: Phase::Succeeded,
        expires_at_raw: Some("2099-01-01T00:00:00Z".into()),
        ..allocated(0)
    }];
    assert!(f.plan().actions.iter().any(|a| matches!(
        a,
        Action::RetireAllocated {
            reason: RetireReason::PodFinished,
            expires_at: None,
            ..
        }
    )));

    f.pods = vec![allocated(0)];
    let p = f.plan();
    assert!(retires(&p).is_empty());
    assert_eq!(p.requeue_at, Some(later(3600)));
}

#[test]
fn draining_deleted_every_pass_and_stuck_ones_forced() {
    let d = |s, del: Option<Timestamp>| WorkerPod {
        alloc: Alloc::Draining,
        deletion: del,
        ..pod(s)
    };
    let mut f = fx();
    f.pods = vec![d(0, None), d(1, Some(ago(60))), d(2, Some(ago(125)))];
    let p = f.plan();
    assert!(p.actions.contains(&Action::DeleteDraining {
        pod: "p-0".into(),
        uid: "uid-0".into()
    }));
    assert!(p.actions.contains(&Action::ForceDelete {
        pod: "p-2".into(),
        uid: "uid-2".into()
    }));
    assert!(!p.actions.iter().any(|a| matches!(a,
        Action::DeleteDraining { pod, .. } | Action::ForceDelete { pod, .. } if pod == "p-1")));
    // p-1 is forced at deletionTimestamp (-60 s) + 5 s grace + 2 min, which is +65 s.
    assert_eq!(p.requeue_at, Some(later(65)));
    // The same input gives the same plan, so draining deletes repeat every pass.
    assert_eq!(f.plan(), p);
    // Draining pods appear in no count.
    assert_eq!(status(&p).warm, Some(0));
}

#[test]
#[allow(clippy::too_many_lines)]
fn start_timeout_table() {
    let late = Some(ago(121));
    let cases: Vec<(&str, WorkerPod, Option<RetireReason>)> = vec![
        (
            "free not ready late",
            WorkerPod {
                ready: false,
                created: late,
                ..pod(0)
            },
            Some(RetireReason::StartTimeout),
        ),
        (
            "free not ready in time",
            WorkerPod {
                ready: false,
                created: Some(ago(60)),
                ..pod(0)
            },
            None,
        ),
        ("free ready old", pod(0), None),
        (
            "allocated pending late",
            WorkerPod {
                phase: Phase::Pending,
                created: late,
                ..allocated(0)
            },
            Some(RetireReason::StartTimeout),
        ),
        (
            "allocated no ip late",
            WorkerPod {
                has_ip: false,
                created: late,
                ..allocated(0)
            },
            Some(RetireReason::StartTimeout),
        ),
        (
            "allocated running",
            WorkerPod {
                created: late,
                ..allocated(0)
            },
            None,
        ),
        (
            "free failed",
            WorkerPod {
                phase: Phase::Failed,
                ..pod(0)
            },
            Some(RetireReason::DaemonCrashed),
        ),
        (
            "free evicted",
            WorkerPod {
                phase: Phase::Failed,
                evicted: true,
                ..pod(0)
            },
            Some(RetireReason::PodFinished),
        ),
        (
            "free succeeded",
            WorkerPod {
                phase: Phase::Succeeded,
                ..pod(0)
            },
            Some(RetireReason::PodFinished),
        ),
        (
            "allocated succeeded",
            WorkerPod {
                phase: Phase::Succeeded,
                ..allocated(0)
            },
            Some(RetireReason::PodFinished),
        ),
        (
            "allocated failed",
            WorkerPod {
                phase: Phase::Failed,
                ..allocated(0)
            },
            Some(RetireReason::PodFinished),
        ),
    ];
    for (name, pod, want) in cases {
        let allocated = matches!(pod.alloc, Alloc::Allocated(_));
        let mut f = fx().scale(1, 10); // min 1, so no idle reaping
        f.pods = vec![pod];
        let p = f.plan();
        assert_eq!(retires(&p).first().map(|r| r.1), want, "{name}");
        let degraded = &cond(&p, "Degraded").reason;
        match want {
            Some(RetireReason::StartTimeout) => {
                assert_eq!(degraded, "PodStartTimeout", "{name}");
                assert_eq!(p.backoff, BackoffSignal::Bump, "{name}");
            }
            // Regression (sandbox-deploy review #3): a build that killed its
            // daemon set PodFailed and bumped the pool-wide create backoff.
            Some(RetireReason::PodFinished) if allocated => {
                assert_eq!(degraded, "AsExpected", "{name}");
                assert_ne!(p.backoff, BackoffSignal::Bump, "{name}");
            }
            Some(_) => {
                assert_eq!(degraded, "PodFailed", "{name}");
                assert_eq!(p.backoff, BackoffSignal::Bump, "{name}");
            }
            None => assert_eq!(degraded, "AsExpected", "{name}"),
        }
    }
}

#[test]
fn free_retire_rules_and_allocated_left_alone() {
    let cases: Vec<(&str, i32, WorkerPod, Option<RetireReason>)> = vec![
        (
            "stale hash free",
            10,
            WorkerPod {
                template_hash: Some("old".into()),
                ..pod(0)
            },
            Some(RetireReason::StaleTemplate),
        ),
        (
            "stale hash allocated",
            10,
            WorkerPod {
                template_hash: Some("old".into()),
                ..allocated(0)
            },
            None,
        ),
        (
            "cert under 25h",
            10,
            WorkerPod {
                worker_cert_not_after: Some(later(24 * 3600)),
                ..pod(0)
            },
            Some(RetireReason::CertExpiring),
        ),
        (
            "slot out of range free",
            2,
            pod(5),
            Some(RetireReason::OutOfRange),
        ),
        ("slot out of range allocated", 2, allocated(5), None),
    ];
    for (name, max, pod, want) in cases {
        let mut f = fx().scale(0, max);
        f.pods = vec![pod];
        assert_eq!(retires(&f.plan()).first().map(|r| r.1), want, "{name}");
    }
}

#[test]
fn ca_missing_or_invalid_skips_only_applies_creates_and_ca_rule() {
    for (ca, first) in [
        (&MISSING, Action::CreateCa),
        (
            &*INVALID,
            Action::BackupAndReplaceCa {
                reason: CaError::Missing("x").reason(),
                message: CaError::Missing("internal-ca.crt").to_string(),
            },
        ),
    ] {
        let mut f = fx().scale(3, 10);
        f.ca = ca;
        f.pods = vec![
            WorkerPod {
                expires_at: Some(ago(1)),
                ..allocated(0)
            },
            WorkerPod {
                alloc: Alloc::Draining,
                ..pod(1)
            },
            WorkerPod {
                internal_ca: Some("0".repeat(16)),
                ..pod(2)
            },
            WorkerPod {
                template_hash: Some("old".into()),
                ..pod(3)
            },
            WorkerPod {
                ready: false,
                created: Some(ago(60)),
                ..pod(4)
            },
        ];
        let p = f.plan();
        assert_eq!(p.actions[0], first);
        assert!(matches!(p.actions[1], Action::Event(_)));
        // Steps 5 to 7 still run. The internal-ca rule, applies and creates do not.
        assert!(p.actions.contains(&Action::DeleteDraining {
            pod: "p-1".into(),
            uid: "uid-1".into()
        }));
        assert_eq!(
            retires(&p),
            vec![
                ("p-0".into(), RetireReason::Expired),
                ("p-3".into(), RetireReason::StaleTemplate)
            ]
        );
        assert!(creates(&p).is_empty());
        assert!(
            !p.actions
                .iter()
                .any(|a| matches!(a, Action::ApplySa | Action::ApplyNetworkPolicy))
        );
        let r = cond(&p, "Ready");
        assert_eq!(
            (r.status.as_str(), r.reason.as_str()),
            ("False", "CAUnavailable")
        );
        // Step 8 counts are live (p-2 warm, p-4 starting).
        assert_eq!((status(&p).warm, status(&p).starting), (Some(1), Some(1)));
        // Pod deadlines stay armed, such as the p-4 start timeout at +60 s.
        assert_eq!(p.requeue_at, Some(later(60)));
    }
}

#[test]
fn slot_holds_persist_until_store_or_lapse() {
    let mut f = fx().scale(2, 3);
    f.creates = vec![CreateOutcome {
        slot: 0,
        result: CreateResult::SlotTaken,
    }];
    let p = f.plan();
    assert_eq!(creates(&p), vec![1, 2]);
    assert_eq!(p.slot_holds, [(0, later(600))].into());

    // Later passes without a new 409 keep the slot held and, past the grace, Degraded.
    f.creates.clear();
    f.slot_holds = [(0, later(598))].into();
    f.pods = vec![pod(1)];
    let p = f.plan();
    assert_eq!(creates(&p), vec![2]);
    assert_eq!(cond(&p, "Degraded").reason, "SlotTaken");
    assert_eq!(p.slot_holds, f.slot_holds);

    // The hold blocks demand. Requeue when it lapses, then retry the slot.
    f.pool.spec.scaling.min = 3;
    f.pods = vec![pod(1), pod(2)];
    let p = f.plan();
    assert!(creates(&p).is_empty());
    assert_eq!(p.requeue_at, Some(later(598)));
    f.slot_holds = [(0, *NOW)].into();
    let p = f.plan();
    assert_eq!(creates(&p), vec![0]);
    // The lapsed hold stays, still Degraded, until the retry's outcome arrives.
    assert_eq!(p.slot_holds, f.slot_holds);
    assert_eq!(cond(&p, "Degraded").reason, "SlotTaken");
    let mut again = fx().scale(3, 3);
    again.pods = f.pods.clone();
    again.slot_holds = p.slot_holds.clone();
    again.creates = vec![CreateOutcome {
        slot: 0,
        result: CreateResult::SlotTaken,
    }];
    let p = again.plan();
    assert!(creates(&p).is_empty());
    assert_eq!(
        p.slot_holds,
        [(0, later(598))].into(),
        "409 again: refreshed, reported at once"
    );
    assert_eq!(cond(&p, "Degraded").reason, "SlotTaken");

    // Released when the name appears in the store, or a create into it succeeds.
    f.slot_holds = [(0, later(600))].into();
    f.pods = vec![pod(0)];
    assert!(f.plan().slot_holds.is_empty());
    f.pods.clear();
    f.creates = vec![CreateOutcome {
        slot: 0,
        result: CreateResult::Created,
    }];
    assert!(f.plan().slot_holds.is_empty());
}

#[test]
fn foreign_pool_labelled_pods_occupy_slots() {
    let mut f = fx().scale(2, 3);
    f.foreign = vec!["p-0".into(), "p-x".into()];
    let p = f.plan();
    assert_eq!(creates(&p), vec![1, 2]);
    let d = cond(&p, "Degraded");
    assert_eq!(
        (d.reason.as_str(), d.message.contains("p-0")),
        ("SlotTaken", true)
    );
}

#[test]
fn backoff_resets_only_on_ready_transition() {
    let mut f = fx().scale(1, 10);
    f.pods = vec![pod(0)];
    assert_eq!(f.plan().backoff, BackoffSignal::Keep, "retained Ready pod");
    f.became_ready = ["uid-0".to_owned()].into();
    assert_eq!(f.plan().backoff, BackoffSignal::Reset);
    f.pods.push(WorkerPod {
        phase: Phase::Failed,
        ..pod(1)
    });
    assert_eq!(f.plan().backoff, BackoffSignal::Bump, "Bump wins");
}

#[test]
fn stale_internal_ca_pods_retired_once_valid_ca_loaded() {
    let old = Some("0".repeat(16));
    let mut f = fx();
    f.pods = vec![
        WorkerPod {
            internal_ca: old.clone(),
            ..pod(0)
        },
        WorkerPod {
            internal_ca: old,
            ..allocated(1)
        },
    ];
    let p = f.plan();
    assert_eq!(
        retires(&p),
        vec![
            ("p-0".into(), RetireReason::CaRotated),
            ("p-1".into(), RetireReason::CaRotated)
        ]
    );
    assert_eq!(cond(&p, "Degraded").reason, "CAReplaced");
}

#[test]
fn template_conflict_and_invalid_spec_stop_creates_but_retire() {
    for invalid_tz in [false, true] {
        let mut f = fx().scale(3, 10);
        if invalid_tz {
            f.tz = None;
        } else {
            f.template = TemplateCheck::Conflict {
                message: "sets metadata.name".into(),
            };
        }
        f.sandbox_gen = None;
        f.pods = vec![
            WorkerPod {
                expires_at: Some(ago(1)),
                ..allocated(0)
            },
            WorkerPod {
                alloc: Alloc::Draining,
                ..pod(1)
            },
        ];
        let p = f.plan();
        assert!(creates(&p).is_empty());
        assert!(
            !p.actions
                .iter()
                .any(|a| matches!(a, Action::ApplySa | Action::ApplyNetworkPolicy))
        );
        assert_eq!(retires(&p), vec![("p-0".into(), RetireReason::Expired)]);
        assert!(
            p.actions
                .iter()
                .any(|a| matches!(a, Action::DeleteDraining { .. }))
        );
        let want = if invalid_tz {
            "InvalidSpec"
        } else {
            "TemplateConflict"
        };
        assert_eq!(cond(&p, "Ready").reason, want);
    }
}

#[test]
fn schedule_windows() {
    let w = |days: &[Day], start: &str, end: &str, min| Window {
        days: days.to_vec(),
        start: start.into(),
        end: end.into(),
        min,
    };
    let mut f = fx().scale(1, 10);
    f.pool.spec.scaling.schedule = vec![
        w(&[Day::Tue], "09:00", "17:00", 4),
        w(&[Day::Mon], "08:00", "18:00", 9),
    ];
    let p = f.plan();
    assert_eq!(creates(&p), vec![0, 1, 2, 3]);
    assert_eq!(p.requeue_at, Some(later(5 * 3600)), "window close at 17:00");

    let utc = TimeZone::UTC;
    let s = &mut f.pool.spec.scaling;
    s.schedule = vec![w(&[Day::Mon], "22:00", "06:00", 5)];
    let tue_0200: Timestamp = "2026-10-20T02:00:00Z".parse().unwrap();
    assert_eq!(
        schedule::effective_min(s, &utc, tue_0200),
        5,
        "overnight window open"
    );
    assert_eq!(schedule::effective_min(s, &utc, *NOW), 1, "closed at noon");
    assert_eq!(
        schedule::next_boundary(&s.schedule, &utc, *NOW),
        Some("2026-10-26T22:00:00Z".parse().unwrap())
    );

    // DST gap. 02:30 does not exist on 2026-03-29 in CET and resolves forward to 03:30 CEST.
    let cet = TimeZone::posix("CET-1CEST,M3.5.0,M10.5.0/3").unwrap();
    s.schedule = vec![w(&[Day::Sun], "02:30", "05:00", 7)];
    let before: Timestamp = "2026-03-29T00:00:00Z".parse().unwrap();
    assert_eq!(
        schedule::next_boundary(&s.schedule, &cet, before),
        Some("2026-03-29T01:30:00Z".parse().unwrap())
    );
    let inside: Timestamp = "2026-03-29T01:45:00Z".parse().unwrap();
    assert_eq!(schedule::effective_min(s, &cet, inside), 7);
}

#[test]
fn waiters_pruned_after_90s_and_fresh_ones_scale_up() {
    let mut f = fx();
    f.waiter("a", 2, ago(91));
    f.waiter("b", 3, ago(30));
    f.waiter("c", 0, ago(10));
    f.waiter("d", 0, ago(200));
    let p = f.plan();
    assert_eq!(creates(&p), vec![0, 1, 2], "only fresh waiters count");
    let prune = p
        .actions
        .iter()
        .find_map(|a| match a {
            Action::PruneWaiters { entries } => Some(entries),
            _ => None,
        })
        .unwrap();
    let got: Vec<(usize, &str)> = prune
        .iter()
        .map(|e| (e.index, e.replica.as_str()))
        .collect();
    assert_eq!(got, vec![(3, "d"), (0, "a")], "highest index first");
    assert_eq!(status(&p).waiting, Some(3));
    // The next stale transition is b at -30 s + 90 s (+1 s past the boundary).
    assert_eq!(p.requeue_at, Some(later(61)));
}

#[test]
fn ca_expiring_soon_condition() {
    let p = fx().plan();
    let c = cond(&p, "CAExpiringSoon");
    assert_eq!(
        (c.status.as_str(), c.reason.as_str()),
        ("False", "AsExpected")
    );

    let mut f = fx();
    f.ca = &OLD;
    if let LoadOutcome::Valid(b) = &*OLD {
        assert!(ca_warning_at(b).unwrap() < *NOW);
    }
    let c = cond(&f.plan(), "CAExpiringSoon").clone();
    assert_eq!(
        (c.status.as_str(), c.reason.as_str()),
        ("True", "CAExpiringSoon")
    );
    assert!(
        c.message.contains("100 days") || c.message.contains("99 days"),
        "{}",
        c.message
    );
}

#[test]
fn requeue_earliest_deadline_and_no_polling() {
    // In steady state the only deadline is the CA expiry warning: each pod
    // carries its own cert, so no pool-wide cert renewal is scheduled.
    let p = fx().plan();
    let LoadOutcome::Valid(b) = &*CA else {
        panic!("fixture CA")
    };
    assert_eq!(p.requeue_at, ca_warning_at(b));
    // Applying the status twice leaves nothing to patch.
    let mut f = fx();
    let Action::PatchStatus(s) = p.actions.last().unwrap().clone() else {
        panic!("{p:?}")
    };
    f.pool.status = Some(*s);
    let again = f.plan();
    assert!(again.actions.is_empty(), "{again:?}");

    // With several deadlines, the earliest wins.
    f.pods = vec![
        WorkerPod {
            ready: false,
            created: Some(ago(90)),
            ..pod(0)
        },
        WorkerPod {
            expires_at: Some(later(10)),
            ..allocated(1)
        },
    ];
    assert_eq!(f.plan().requeue_at, Some(later(10)));

    // Create backoff blocks creates and requeues at its end.
    let mut f = fx().scale(2, 10);
    f.backoff = Some(later(40));
    let p = f.plan();
    assert!(creates(&p).is_empty());
    assert_eq!(p.requeue_at, Some(later(40)));
}

#[test]
fn sandbox_due() {
    let mut f = fx().scale(1, 10);
    f.sandbox_gen = Some(0);
    let p = f.plan();
    let kinds: Vec<&Action> = p
        .actions
        .iter()
        .filter(|a| !matches!(a, Action::PatchStatus(_)))
        .collect();
    assert_eq!(
        kinds,
        vec![
            &Action::ApplySa,
            &Action::ApplyNetworkPolicy,
            &Action::CreatePod {
                slot: 0,
                name: "p-0".into()
            }
        ]
    );
}

#[test]
fn from_pod_checks_owner_and_slot() {
    let mut pod = Pod::default();
    pod.metadata.name = Some("p-3".into());
    pod.metadata.uid = Some("u".into());
    pod.metadata.owner_references = Some(vec![OwnerReference {
        uid: "pool-uid".into(),
        controller: Some(true),
        ..Default::default()
    }]);
    pod.metadata.annotations =
        Some([(crate::render::ANN_ALLOC.to_owned(), ALLOC_ID.to_owned())].into());
    let w = WorkerPod::from_pod(&pod, "p", "pool-uid").unwrap();
    assert_eq!(
        (w.slot, w.alloc, w.grace_seconds),
        (Some(3), Alloc::Allocated(ALLOC_ID.into()), 5)
    );
    assert!(!w.evicted);
    pod.status = Some(k8s_openapi::api::core::v1::PodStatus {
        phase: Some("Failed".into()),
        reason: Some("Evicted".into()),
        ..Default::default()
    });
    let w = WorkerPod::from_pod(&pod, "p", "pool-uid").unwrap();
    assert!(w.evicted && w.phase == Phase::Failed);
    assert!(WorkerPod::from_pod(&pod, "p", "other").is_none());
    assert_eq!(parse_slot("p-03", "p"), None);
    assert_eq!(parse_slot("p-0", "p"), Some(0));
    assert_eq!(parse_slot("pp-1", "p"), None);
}

#[test]
fn requeue_after_create_outcomes() {
    let o = |result| CreateOutcome { slot: 0, result };
    let failed = || CreateResult::Failed {
        message: "quota".into(),
    };
    let cases = [
        (vec![], Requeue::Events),
        (vec![o(CreateResult::Created)], Requeue::Events),
        (
            vec![o(CreateResult::Created), o(CreateResult::SlotTaken)],
            Requeue::Immediately,
        ),
        (vec![o(failed())], Requeue::ErrorBackoff),
        (
            vec![o(CreateResult::SlotTaken), o(failed())],
            Requeue::ErrorBackoff,
        ),
    ];
    for (outcomes, want) in cases {
        assert_eq!(
            Plan::requeue_after_outcomes(&outcomes),
            want,
            "{outcomes:?}"
        );
    }
    // The immediate pass records the 409, holds the slot, retries nothing and
    // reports SlotTaken once the grace has passed.
    let mut f = fx().scale(1, 1);
    f.creates = vec![o(CreateResult::SlotTaken)];
    let p = f.plan();
    assert!(creates(&p).is_empty());
    assert_eq!(cond(&p, "Degraded").reason, "AsExpected");
    assert_eq!(p.requeue_at, Some(later(2)));
}

#[test]
fn foreign_pods_are_namespace_scoped() {
    let mk = |ns: &str, owner: &str, pool_label: &str| {
        let mut pod = Pod::default();
        pod.metadata.name = Some("p-0".into());
        pod.metadata.namespace = Some(ns.into());
        pod.metadata.labels =
            Some([(crate::render::LABEL_POOL.to_owned(), pool_label.to_owned())].into());
        pod.metadata.owner_references = Some(vec![OwnerReference {
            uid: owner.into(),
            controller: Some(true),
            ..Default::default()
        }]);
        pod
    };
    let f = |pod: &Pod| foreign_pod_name(pod, "ns", "p", "pool-uid");
    assert_eq!(f(&mk("ns", "other-uid", "p")), Some("p-0".into()));
    assert_eq!(f(&mk("ns", "pool-uid", "p")), None, "owned");
    assert_eq!(
        f(&mk("other-ns", "other-uid", "p")),
        None,
        "same-name pool elsewhere"
    );
    assert_eq!(f(&mk("ns", "other-uid", "q")), None, "other pool's label");
}

#[test]
fn slot_holds_never_linger() {
    // When max shrinks below a held slot, the hold drops at once and Degraded clears.
    let mut f = fx().scale(0, 2);
    f.slot_holds = [(2, later(300))].into();
    let p = f.plan();
    assert!(p.slot_holds.is_empty());
    assert_eq!(cond(&p, "Degraded").reason, "AsExpected");

    // Demand falls back after a 409 during a burst. While held, the status is SlotTaken;
    // the first pass after the hold lapses drops it and Degraded clears.
    let mut f = fx().scale(1, 3);
    f.pods = vec![pod(1)];
    f.slot_holds = [(0, later(1))].into();
    assert_eq!(cond(&f.plan(), "Degraded").reason, "SlotTaken");
    f.slot_holds = [(0, *NOW)].into();
    let p = f.plan();
    assert!(creates(&p).is_empty());
    assert!(p.slot_holds.is_empty());
    assert_eq!(cond(&p, "Degraded").reason, "AsExpected");
}

#[test]
fn held_slot_heals_after_foreign_pod_is_deleted() {
    let mut f = fx().scale(1, 3);
    f.slot_holds = [(0, *NOW)].into();
    let p = f.plan();
    assert_eq!(creates(&p), vec![0], "lapsed hold: retry the slot");
    assert_eq!(cond(&p, "Degraded").reason, "SlotTaken", "this pass only");
    // The retry returns 201 and the pod shows up, so the hold clears and Degraded clears.
    f.slot_holds = p.slot_holds;
    f.creates = vec![CreateOutcome {
        slot: 0,
        result: CreateResult::Created,
    }];
    f.pods = vec![pod(0)];
    let p = f.plan();
    assert!(p.slot_holds.is_empty());
    assert_eq!(cond(&p, "Degraded").reason, "AsExpected");
}

/// Regression (control review P3): staleness compared `updatedAt`, written by
/// another replica's clock, with the leader's clock. A replica whose clock
/// lagged by more than about 60 s had every entry judged stale at once, so its
/// queued demand was ignored and pruned on every pass. Staleness now runs from
/// when the leader first saw the `(replica, updatedAt)` pair.
#[test]
fn waiter_staleness_ignores_the_writers_clock() {
    let mut f = fx().scale(0, 10);
    f.waiter("skewed", 3, ago(300));
    // The leader has just seen it: fresh, counted, not pruned.
    f.waiters_seen.clear();
    let p = f.plan();
    assert_eq!(status(&p).waiting, Some(3));
    assert!(
        !p.actions
            .iter()
            .any(|a| matches!(a, Action::PruneWaiters { .. }))
    );
    assert_eq!(p.requeue_at, Some(later(91)), "stale check armed");
    // Unchanged for 91 s on the leader's clock: stale, pruned.
    f.waiters_seen.insert(("skewed".into(), ago(300)), ago(91));
    let p = f.plan();
    assert_eq!(status(&p).waiting, Some(0));
    assert!(
        p.actions
            .iter()
            .any(|a| matches!(a, Action::PruneWaiters { .. }))
    );
}

#[test]
fn volumes_in_use_counts_pods_below_the_cache_limit() {
    use super::metrics::PodCounts;
    use buildkit_controller_api::v1alpha1::LocalCache;
    let mut pool = fx().pool;
    pool.spec.scaling.max = 4;
    let draining = WorkerPod {
        alloc: Alloc::Draining,
        ..pod(3)
    };
    let foreign = WorkerPod {
        slot: None,
        ..pod(0)
    };
    let workers = [pod(0), allocated(1), pod(2), draining, foreign];
    let c = PodCounts::of(&pool, &workers);
    assert_eq!((c.draining, c.volumes_in_use), (1, None));
    // limit = min(maxVolumes 2, max 4): slots 0 and 1.
    pool.spec.workers.cache.local = Some(LocalCache {
        max_volumes: 2,
        ..LocalCache::default()
    });
    assert_eq!(PodCounts::of(&pool, &workers).volumes_in_use, Some(2));
    // limit = min(maxVolumes 10, max 3): slots 0 to 2.
    pool.spec.scaling.max = 3;
    pool.spec.workers.cache.local = Some(LocalCache::default());
    assert_eq!(PodCounts::of(&pool, &workers).volumes_in_use, Some(3));
}
