//! One pool pass runs observe (stores and term memory), plan, execute, remember,
//! and compute the requeue.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use buildkit_controller_api::v1alpha1::BuildKitPool;
use k8s_openapi::api::core::v1::{Pod, Secret};
use k8s_openapi::jiff::tz::TimeZone;
use k8s_openapi::jiff::{SignedDuration, Timestamp};
use kube::ResourceExt;

use super::exec::{CaGuard, Env, Report, execute, odt};
use super::ops::KubeOps;
use super::plan::{
    BackoffSignal, CreateOutcome, Observed, Plan, PlanConfig, Requeue, TemplateCheck, plan,
};
use super::pods::{WorkerPod, foreign_pod_name};
use crate::pki::{CaBundle, LoadOutcome};
use crate::render::{self, RenderConfig};

/// First create backoff.
const CREATE_BACKOFF_MIN: SignedDuration = SignedDuration::from_secs(10);
/// Create backoff cap.
const CREATE_BACKOFF_MAX: SignedDuration = SignedDuration::from_mins(5);
/// First error requeue.
pub const ERROR_BACKOFF_MIN: Duration = Duration::from_secs(1);
/// Error requeue cap.
pub const ERROR_BACKOFF_MAX: Duration = Duration::from_mins(1);
/// How long a created pod stands in for itself until the store shows it.
const PENDING_CREATE_TTL: SignedDuration = SignedDuration::from_mins(1);

/// What the executor keeps per pool for the leadership term.
#[derive(Debug, Default)]
pub struct PoolMemory {
    /// Uid of the pool this memory belongs to; a recreated pool starts fresh.
    uid: Option<String>,
    slot_holds: BTreeMap<u32, Timestamp>,
    recent_creates: Vec<CreateOutcome>,
    /// Generation and time of the last successful sandbox apply.
    sandbox_applied: Option<(i64, Timestamp)>,
    create_backoff: Option<(Timestamp, SignedDuration)>,
    ready: BTreeSet<String>,
    /// Uids seen up (see [`WorkerPod::up`]) in this term.
    seen_up: BTreeSet<String>,
    /// [`Observed::down_since`].
    down_since: BTreeMap<String, Timestamp>,
    /// 201 responses not yet in the store, with their create time.
    pending: Vec<(Pod, Timestamp)>,
    error_backoff: Option<Duration>,
}

impl PoolMemory {
    /// Resets the memory when it belongs to another pool object of the same
    /// name (deleted and recreated): its sandbox generation, holds, creates
    /// and backoffs mean nothing for the new uid.
    pub(super) fn adopt(&mut self, uid: &str) {
        if self.uid.as_deref() != Some(uid) {
            *self = Self {
                uid: Some(uid.to_owned()),
                ..Self::default()
            };
        }
    }

    /// Tracks which pods came up and since when those went down again, for
    /// [`Observed::down_since`]. Forgets pods that left.
    pub(super) fn observe_up(&mut self, pods: &[WorkerPod], now: Timestamp) {
        let uids: BTreeSet<&str> = pods.iter().map(|p| p.uid.as_str()).collect();
        self.seen_up.retain(|u| uids.contains(u.as_str()));
        self.down_since.retain(|u, _| uids.contains(u.as_str()));
        for p in pods {
            if p.up() {
                self.seen_up.insert(p.uid.clone());
                self.down_since.remove(&p.uid);
            } else if self.seen_up.contains(&p.uid) {
                self.down_since.entry(p.uid.clone()).or_insert(now);
            }
        }
    }
}

/// The CA Secret as watched, with this term's version counter.
#[derive(Clone, Debug, Default)]
pub struct CaView {
    /// Bumped on every CA Secret watch change.
    pub epoch: u64,
    /// The Secret, `None` while absent.
    pub secret: Option<Arc<Secret>>,
}

/// Controller-wide pass inputs.
pub struct Settings {
    /// Planner timings and status strings.
    pub plan: PlanConfig,
    /// Render settings.
    pub render: RenderConfig,
    /// CA Secret name.
    pub ca_secret_name: String,
    /// `POD_NAME`.
    pub replica: String,
    /// Configured OIDC issuer URLs.
    pub issuers: Vec<String>,
    /// Worker serving cert validity.
    pub worker_cert_validity: time::Duration,
}

/// Result of a pass.
#[derive(Debug)]
pub struct Outcome {
    /// What ran.
    pub report: Report,
    /// When to reconcile the pool again without an event; `None` = wait.
    pub requeue: Option<Duration>,
}

/// Loads the CA Secret's data.
#[must_use]
pub fn load_ca(ca: &CaView, now: Timestamp) -> LoadOutcome {
    let data: Option<BTreeMap<String, Vec<u8>>> = ca.secret.as_ref().map(|s| {
        s.data
            .iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.0.clone()))
            .collect()
    });
    CaBundle::load(data.as_ref(), odt(now))
}

/// The pool-independent CA pass: create, back up and replace,
/// and the bundle `ConfigMap`, run at leadership start and on every CA
/// Secret change. Without it a controller with no pools never creates its
/// CA (so no replica turns Ready) and never heals a deleted or corrupted one.
/// Pool passes plan the same actions; [`CaGuard`] makes each run once per
/// Secret version.
pub async fn ca_pass<O: KubeOps>(
    ops: &O,
    guard: &CaGuard,
    settings: &Settings,
    ca: &CaView,
    now: Timestamp,
) -> Report {
    // The CA actions never read the pool; `Env` needs one.
    static NO_POOL: std::sync::LazyLock<BuildKitPool> = std::sync::LazyLock::new(|| {
        BuildKitPool::new(
            "",
            buildkit_controller_api::v1alpha1::BuildKitPoolSpec::default(),
        )
    });
    let loaded = load_ca(ca, now);
    let actions = super::plan::ca_actions(&loaded, guard.bundle_current(ca.epoch, now));
    let env = Env {
        pool: &NO_POOL,
        ca: &loaded,
        ca_secret: ca.secret.as_deref(),
        ca_epoch: ca.epoch,
        ca_secret_name: &settings.ca_secret_name,
        render: &settings.render,
        worker_cert_validity: settings.worker_cert_validity,
        replica: &settings.replica,
        now,
    };
    let plan = Plan {
        actions,
        requeue_at: None,
        backoff: BackoffSignal::Keep,
        slot_holds: BTreeMap::new(),
    };
    execute(ops, guard, &env, &plan).await
}

/// Runs one pass of `pool`, which has `waiting` pending claims.
#[allow(clippy::too_many_arguments, reason = "the pass's whole input")]
pub async fn run<O: KubeOps>(
    ops: &O,
    guard: &CaGuard,
    settings: &Settings,
    pool: &BuildKitPool,
    store_pods: &[Arc<Pod>],
    ca: &CaView,
    mem: &mut PoolMemory,
    waiting: i32,
    now: Timestamp,
) -> Outcome {
    let loaded = load_ca(ca, now);
    let ns = pool.namespace().unwrap_or_default();
    let name = pool.name_any();
    let uid = pool.uid().unwrap_or_default();
    mem.adopt(&uid);

    // A created pod counts until the store has its name, so the pass reads its own writes.
    let in_store: BTreeSet<&str> = store_pods
        .iter()
        .filter(|p| p.namespace().as_deref() == Some(ns.as_str()))
        .filter_map(|p| p.metadata.name.as_deref())
        .collect();
    mem.pending.retain(|(p, at)| {
        p.metadata
            .name
            .as_deref()
            .is_some_and(|n| !in_store.contains(n))
            && *at + PENDING_CREATE_TTL > now
    });
    let mut pods: Vec<WorkerPod> = store_pods
        .iter()
        .filter_map(|p| WorkerPod::from_pod(p, &ns, &name, &uid))
        .collect();
    pods.extend(
        mem.pending
            .iter()
            .filter_map(|(p, _)| WorkerPod::from_pod(p, &ns, &name, &uid)),
    );
    let ready_now: BTreeSet<String> = pods
        .iter()
        .filter(|p| p.ready)
        .map(|p| p.uid.clone())
        .collect();
    mem.observe_up(&pods, now);
    let observed = Observed {
        pool,
        time_zone: TimeZone::get(&pool.spec.scaling.time_zone).ok(),
        template: template_check(pool),
        network: &settings.render.worker_network,
        controller_namespace: &settings.render.controller_namespace,
        platforms: platforms(pool),
        ca: &loaded,
        ca_bundle_current: guard.bundle_current(ca.epoch, now),
        pods,
        sandbox_applied: mem.sandbox_applied,
        recent_creates: std::mem::take(&mut mem.recent_creates),
        create_backoff_until: mem.create_backoff.map(|(t, _)| t),
        unknown_issuer: unknown_issuer(pool, &settings.issuers),
        foreign_pods: store_pods
            .iter()
            .filter_map(|p| foreign_pod_name(p, &ns, &name, &uid))
            .collect(),
        slot_holds: std::mem::take(&mut mem.slot_holds),
        became_ready: ready_now.difference(&mem.ready).cloned().collect(),
        waiting,
        down_since: mem.down_since.clone(),
    };
    let plan = plan(&observed, now, &settings.plan);
    let env = Env {
        pool,
        ca: &loaded,
        ca_secret: ca.secret.as_deref(),
        ca_epoch: ca.epoch,
        ca_secret_name: &settings.ca_secret_name,
        render: &settings.render,
        worker_cert_validity: settings.worker_cert_validity,
        replica: &settings.replica,
        now,
    };
    let report = execute(ops, guard, &env, &plan).await;
    let requeue = remember(mem, pool, &plan, &report, ready_now, now);
    Outcome { report, requeue }
}

/// Stores the term memory and returns the requeue.
pub(super) fn remember(
    mem: &mut PoolMemory,
    pool: &BuildKitPool,
    plan: &Plan,
    report: &Report,
    ready_now: BTreeSet<String>,
    now: Timestamp,
) -> Option<Duration> {
    mem.slot_holds.clone_from(&plan.slot_holds);
    mem.recent_creates.clone_from(&report.creates);
    if report.sandbox_applied {
        mem.sandbox_applied = Some((pool.metadata.generation.unwrap_or_default(), now));
    }
    mem.create_backoff = match plan.backoff {
        BackoffSignal::Bump => {
            let step = mem
                .create_backoff
                .map_or(CREATE_BACKOFF_MIN, |(_, s)| (s * 2).min(CREATE_BACKOFF_MAX));
            Some((now + step, step))
        }
        BackoffSignal::Reset => None,
        BackoffSignal::Keep => mem.create_backoff,
    };
    mem.ready = ready_now;
    mem.pending
        .extend(report.created.iter().map(|p| (p.clone(), now)));

    // The plan deadline and the end of a running create backoff. A lapsed
    // backoff is kept until a pod turns Ready, but it is no longer a deadline.
    let backoff_until = mem.create_backoff.map(|(t, _)| t).filter(|t| *t > now);
    let deadline = [plan.requeue_at, backoff_until]
        .into_iter()
        .flatten()
        .min()
        .map(|t| t.duration_since(now).try_into().unwrap_or(Duration::ZERO));

    let failed = !report.errors.is_empty()
        || Plan::requeue_after_outcomes(&report.creates) == Requeue::ErrorBackoff;
    if failed {
        let next = mem
            .error_backoff
            .map_or(ERROR_BACKOFF_MIN, |d| (d * 2).min(ERROR_BACKOFF_MAX));
        mem.error_backoff = Some(next);
        // A failing call must not delay an expiry or timeout due sooner.
        return Some(deadline.map_or(next, |d| d.min(next)));
    }
    mem.error_backoff = None;
    if Plan::requeue_after_outcomes(&report.creates) == Requeue::Immediately {
        return Some(Duration::ZERO);
    }
    deadline
}

fn template_check(pool: &BuildKitPool) -> TemplateCheck {
    let conflicts = render::validate_template(&pool.spec);
    if !conflicts.is_empty() {
        let message = conflicts
            .iter()
            .map(|c| format!("{c:?}"))
            .collect::<Vec<_>>()
            .join("; ");
        return TemplateCheck::Conflict { message };
    }
    match render::template_hash(pool) {
        Ok(hash) => TemplateCheck::Current { hash },
        Err(e) => TemplateCheck::Conflict {
            message: e.to_string(),
        },
    }
}

/// The `status.platforms` field.
fn platforms(pool: &BuildKitPool) -> Vec<String> {
    if !pool.spec.platforms.is_empty() {
        return pool.spec.platforms.clone();
    }
    pool.spec
        .template
        .spec
        .node_selector
        .get("kubernetes.io/arch")
        .map(|a| vec![format!("linux/{a}")])
        .unwrap_or_default()
}

fn unknown_issuer(pool: &BuildKitPool, issuers: &[String]) -> Option<String> {
    pool.spec
        .access
        .rules
        .iter()
        .filter_map(|r| r.oidc.as_ref())
        .map(|o| &o.issuer)
        .find(|i| !issuers.contains(i))
        .cloned()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use k8s_openapi::jiff::{SignedDuration, Timestamp};

    use super::{PoolMemory, remember};
    use crate::reconcile::exec::Report;
    use crate::reconcile::plan::{BackoffSignal, CreateOutcome, CreateResult, Plan};
    use crate::render::fixtures;

    #[test]
    fn recreated_pool_does_not_inherit_memory() {
        let now = Timestamp::from_second(1_790_000_000).unwrap();
        let mut pool = fixtures::pool("p", serde_json::json!({})).unwrap();
        pool.metadata.generation = Some(1);
        let plan = Plan {
            actions: Vec::new(),
            requeue_at: None,
            backoff: BackoffSignal::Bump,
            slot_holds: [(0, now + SignedDuration::from_mins(10))].into(),
        };
        let report = Report {
            sandbox_applied: true,
            creates: vec![CreateOutcome {
                slot: 1,
                result: CreateResult::SlotTaken,
            }],
            errors: vec!["boom".into()],
            ..Report::default()
        };
        let mut mem = PoolMemory::default();
        mem.adopt("uid-old");
        remember(
            &mut mem,
            &pool,
            &plan,
            &report,
            BTreeSet::from(["u".into()]),
            now,
        );
        assert_eq!(mem.sandbox_applied, Some((1, now)));
        assert!(mem.create_backoff.is_some() && mem.error_backoff.is_some());
        // With the same uid the entry is kept.
        mem.adopt("uid-old");
        assert_eq!(mem.sandbox_applied, Some((1, now)));
        assert_eq!(mem.slot_holds.len(), 1);
        // A pool recreated under the same name carries nothing over, so the new
        // pool gets its SA and NetworkPolicy applied again.
        mem.adopt("uid-new");
        assert_eq!(mem.uid.as_deref(), Some("uid-new"));
        assert_eq!(mem.sandbox_applied, None);
        assert!(mem.slot_holds.is_empty() && mem.recent_creates.is_empty());
        assert!(mem.create_backoff.is_none() && mem.error_backoff.is_none());
        assert!(mem.ready.is_empty() && mem.pending.is_empty());
    }

    /// RECON-1: a pod counts as down only once it was seen up in this term;
    /// it keeps the time it was first seen down until it comes back or leaves.
    #[test]
    fn down_since_tracks_pods_that_were_up() {
        use crate::reconcile::tests::pod;
        let t0 = Timestamp::from_second(1_790_000_000).unwrap();
        let t = |s| t0 + SignedDuration::from_secs(s);
        let unready = |slot| crate::reconcile::pods::WorkerPod {
            ready: false,
            ..pod(slot)
        };
        let mut mem = PoolMemory::default();
        // p-0 is up, p-1 never was.
        mem.observe_up(&[pod(0), unready(1)], t(0));
        assert!(mem.down_since.is_empty());
        mem.observe_up(&[unready(0), unready(1)], t(10));
        mem.observe_up(&[unready(0), unready(1)], t(20));
        assert_eq!(mem.down_since, [("uid-0".to_owned(), t(10))].into());
        // Back up clears it; gone forgets it.
        mem.observe_up(&[pod(0)], t(30));
        assert!(mem.down_since.is_empty());
        mem.observe_up(&[], t(40));
        assert!(mem.seen_up.is_empty());
    }

    /// A create backoff that has run out is kept until a pod turns Ready, but
    /// it is no deadline: it used to requeue the pool at once, every pass.
    #[test]
    fn a_lapsed_create_backoff_does_not_requeue_at_once() {
        let now = Timestamp::from_second(1_790_000_000).unwrap();
        let pool = fixtures::pool("p", serde_json::json!({})).unwrap();
        let plan = Plan {
            actions: Vec::new(),
            requeue_at: None,
            backoff: BackoffSignal::Keep,
            slot_holds: std::collections::BTreeMap::new(),
        };
        let mut mem = PoolMemory {
            create_backoff: Some((
                now - SignedDuration::from_secs(1),
                SignedDuration::from_secs(10),
            )),
            ..PoolMemory::default()
        };
        let report = Report::default();
        assert_eq!(
            remember(&mut mem, &pool, &plan, &report, BTreeSet::new(), now),
            None
        );
        assert!(mem.create_backoff.is_some());
    }
}
