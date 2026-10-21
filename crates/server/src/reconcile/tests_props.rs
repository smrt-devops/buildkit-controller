//! Property tests check that random observed states keep the planner's safety rules.

use std::collections::BTreeSet;

use buildkit_controller_api::v1alpha1::Waiter;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use k8s_openapi::jiff::{SignedDuration, Timestamp};
use proptest::prelude::*;

use super::plan::{
    Action, CreateOutcome, CreateResult, PlanConfig, RetireReason, TemplateCheck, plan,
};
use super::pods::{Alloc, Phase, WorkerPod};
use super::tests::{ALLOC_ID, CA, INVALID, MISSING, NOW, ago, fp, fx, later, pod};

const START_TIMEOUT: i64 = 120;
const IDLE_TIMEOUT: i64 = 600;

fn arb_pod() -> impl Strategy<Value = WorkerPod> {
    (
        (0u32..14, any::<bool>(), 0u8..4, 0u8..5),
        any::<(bool, bool, bool, bool)>(),
        (
            -400i64..4000,
            prop::option::of(-300i64..300),
            prop::option::of(-200i64..200),
        ),
        (prop::option::of(0i64..200_000), any::<bool>()),
    )
        .prop_map(
            |(
                (slot, canonical, alloc, phase),
                (ready, ip, stale_hash, stale_ca),
                (age, exp, del),
                (cert, ca_ann),
            )| {
                let alloc = match alloc {
                    0 => Alloc::Free,
                    1 => Alloc::Draining,
                    2 => Alloc::Allocated(ALLOC_ID.into()),
                    _ => Alloc::Unknown,
                };
                let phase = [
                    Phase::Pending,
                    Phase::Running,
                    Phase::Succeeded,
                    Phase::Failed,
                    Phase::Unknown,
                ][usize::from(phase)];
                let base = pod(slot);
                WorkerPod {
                    name: if canonical {
                        base.name.clone()
                    } else {
                        format!("p-0{slot}")
                    },
                    uid: if canonical {
                        base.uid.clone()
                    } else {
                        format!("uid-0{slot}")
                    },
                    slot: canonical.then_some(slot),
                    alloc,
                    phase,
                    ready,
                    has_ip: ip,
                    created: Some(ago(age)),
                    deletion: del.map(later),
                    expires_at: exp.map(later),
                    template_hash: Some(if stale_hash { "old" } else { "h" }.into()),
                    worker_cert_not_after: cert.map(later),
                    evicted: stale_hash && phase == Phase::Failed,
                    internal_ca: ca_ann.then(|| {
                        if stale_ca {
                            "0".repeat(16)
                        } else {
                            fp()[..16].to_owned()
                        }
                    }),
                    ..base
                }
            },
        )
}

fn arb_create() -> impl Strategy<Value = CreateOutcome> {
    (0u32..14, 0u8..3).prop_map(|(slot, k)| CreateOutcome {
        slot,
        result: match k {
            0 => CreateResult::Created,
            1 => CreateResult::SlotTaken,
            _ => CreateResult::Failed {
                message: "quota".into(),
            },
        },
    })
}

fn after(t: Option<Timestamp>, secs: i64) -> bool {
    t.is_some_and(|c| c + SignedDuration::from_secs(secs) <= *NOW)
}

/// The spec reason for retiring `p` holds.
fn reason_holds(p: &WorkerPod, r: RetireReason, max: u32, ca_valid: bool, conflict: bool) -> bool {
    let ca_stale = ca_valid && p.internal_ca.as_deref().is_some_and(|c| c != &fp()[..16]);
    match (&p.alloc, r) {
        (Alloc::Allocated(_), RetireReason::Expired) => p.expires_at.is_some_and(|e| e <= *NOW),
        (Alloc::Allocated(_), RetireReason::PodFinished) => p.phase.is_finished(),
        (Alloc::Free, RetireReason::PodFinished) => {
            p.phase == Phase::Succeeded || (p.phase == Phase::Failed && p.evicted)
        }
        (Alloc::Free, RetireReason::DaemonCrashed) => p.phase == Phase::Failed && !p.evicted,
        (Alloc::Allocated(_), RetireReason::StartTimeout) => {
            after(p.created, START_TIMEOUT) && (p.phase != Phase::Running || !p.has_ip)
        }
        (Alloc::Free, RetireReason::StartTimeout) => after(p.created, START_TIMEOUT) && !p.ready,
        (Alloc::Allocated(_) | Alloc::Free, RetireReason::CaRotated) => ca_stale,
        (Alloc::Free, RetireReason::StaleTemplate) => {
            !conflict && p.template_hash.as_deref() != Some("h")
        }
        (Alloc::Free, RetireReason::CertExpiring) => p
            .worker_cert_not_after
            .is_some_and(|t| t < *NOW + SignedDuration::from_hours(25)),
        (Alloc::Free, RetireReason::OutOfRange) => p.slot.is_none_or(|s| s >= max),
        (Alloc::Free, RetireReason::Idle) => p.ready && after(p.created, IDLE_TIMEOUT),
        _ => false,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn planner_safety(
        mut pods in prop::collection::vec(arb_pod(), 0..12),
        (max, min, ca) in (1i32..10, 0i32..10, 0u8..3),
        foreign_slots in prop::collection::btree_set(0u32..14, 0..3),
        holds in prop::collection::btree_map(0u32..14, -100i64..700, 0..3),
        creates in prop::collection::vec(arb_create(), 0..3),
        waiters in prop::collection::vec((0i32..6, 0i64..200), 0..4),
        backoff in prop::option::of(-100i64..100),
        (tz_none, conflict) in any::<(bool, bool)>(),
    ) {
        let mut names = BTreeSet::new();
        pods.retain(|p| names.insert(p.name.clone()));
        let mut f = fx();
        f.pool.spec.scaling.min = min.min(max);
        f.pool.spec.scaling.max = max;
        f.pods = pods.clone();
        f.ca = match ca { 0 => &*CA, 1 => &MISSING, _ => &*INVALID };
        f.foreign = foreign_slots.iter().map(|s| format!("p-{s}")).filter(|n| !names.contains(n)).collect();
        f.slot_holds = holds.iter().map(|(s, o)| (*s, later(*o))).collect();
        f.creates = creates;
        f.backoff = backoff.map(later);
        if tz_none { f.tz = None; }
        if conflict { f.template = TemplateCheck::Conflict { message: "x".into() }; }
        let s = f.pool.status.get_or_insert_with(Default::default);
        for (i, (count, age)) in waiters.iter().enumerate() {
            s.waiters.push(Waiter { replica: format!("r{i}"), count: *count, updated_at: Time(ago(*age)) });
        }
        let ca_valid = ca == 0;
        let p = plan(&f.observed(), *NOW, &PlanConfig::default());
        let max_u = u32::try_from(max).unwrap();

        // With no valid CA (or a blocked spec, or an active backoff) there are no creates, no CA-rule retire and no applies.
        let creates: Vec<u32> = super::tests::creates(&p);
        if !ca_valid || tz_none || conflict || backoff.is_some_and(|b| b > 0) {
            prop_assert!(creates.is_empty());
        }
        if !ca_valid {
            let bad = p.actions.iter().any(|a| matches!(a,
                Action::ApplySa | Action::ApplyNetworkPolicy
                | Action::RetireFree { reason: RetireReason::CaRotated, .. }
                | Action::RetireAllocated { reason: RetireReason::CaRotated, .. }));
            prop_assert!(!bad);
        }

        // Never more than max pods. Creates go to distinct slots below max that no
        // owned pod, foreign pool pod or held slot occupies.
        let mut occupied: BTreeSet<u32> = pods.iter().filter_map(|p| p.slot).collect();
        occupied.extend(foreign_slots.iter().filter(|s| !names.contains(&format!("p-{s}"))));
        occupied.extend(p.slot_holds.iter().filter(|(_, t)| **t > *NOW).map(|(s, _)| *s));
        let created: BTreeSet<u32> = creates.iter().copied().collect();
        prop_assert_eq!(created.len(), creates.len());
        prop_assert!(created.iter().all(|s| *s < max_u && !occupied.contains(s)));
        let in_range = occupied.iter().filter(|s| **s < max_u).count();
        prop_assert!(in_range + created.len() <= usize::try_from(max).unwrap());

        // Every retire has a spec reason; each pod at most once; never a terminating one.
        let mut retired = BTreeSet::new();
        for a in &p.actions {
            match a {
                Action::RetireAllocated { pod, uid, reason, .. } | Action::RetireFree { pod, uid, reason } => {
                    let w = pods.iter().find(|w| &w.name == pod).unwrap();
                    prop_assert_eq!(&w.uid, uid);
                    let is_alloc = matches!(a, Action::RetireAllocated { .. });
                    prop_assert_eq!(is_alloc, matches!(w.alloc, Alloc::Allocated(_)));
                    if is_alloc { prop_assert!(reason.allowed_for_allocated()); }
                    prop_assert!(reason_holds(w, *reason, max_u, ca_valid, conflict), "{:?} {:?}", w, reason);
                    prop_assert!(w.deletion.is_none() && retired.insert(uid.clone()));
                }
                Action::DeleteDraining { pod, .. } | Action::ForceDelete { pod, .. } => {
                    let w = pods.iter().find(|w| &w.name == pod).unwrap();
                    prop_assert_eq!(&w.alloc, &Alloc::Draining);
                }
                _ => {}
            }
        }

        // Idle retires never take free below desiredWarm.
        if p.actions.iter().any(|a| matches!(a, Action::RetireFree { reason: RetireReason::Idle, .. })) {
            let waiting: i32 = waiters.iter().filter(|(_, age)| *age <= 90).map(|(c, _)| *c).sum();
            let desired = min.min(max).max(waiting);
            let free = pods.iter().filter(|w| {
                w.alloc == Alloc::Free && w.deletion.is_none() && !retired.contains(&w.uid)
                    && (conflict || w.template_hash.as_deref() == Some("h"))
            }).count();
            prop_assert!(i32::try_from(free).unwrap() >= desired);
        }
        prop_assert!(p.requeue_at.is_none_or(|t| t > *NOW));
    }
}
