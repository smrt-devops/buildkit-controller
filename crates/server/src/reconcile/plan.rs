//! The pure pool planner. No Kubernetes I/O and no rendering, so the
//! executor (pass 2) applies the ordered [`Action`]s, renders objects from
//! their typed inputs, and arms the requeue timer at [`Plan::requeue_at`].
//!
//! Executor contract:
//! - actions run in order; Kubernetes calls are not retried inside a pass;
//! - `Event` runs only if the CA action just before it succeeded;
//! - a failed `ApplySa`/`ApplyNetworkPolicy` skips every later `CreatePod` and
//!   downgrades `Ready` in the `PatchStatus`, or in a status patch of its own
//!   when the plan has none (the planned status equals the cached one);
//! - `CreatePod` issues the pod's own serving cert and Secret first;
//! - `Retire*` is the test-and-replace patch on `uid` and the expected `P alloc`
//!   value, followed by the uid-preconditioned delete; a 422 means skip;
//! - CA actions are cluster-wide, so run them once per CA Secret version, not per pool;
//! - every `CreatePod` outcome is passed to the next pass in
//!   [`Observed::recent_creates`], and the executor arms
//!   [`Plan::requeue_after_outcomes`]: a 409 requeues the pool at once, any
//!   other failure under the error backoff (neither produces a pod event);
//! - [`Plan::slot_holds`] is kept per pool for the leadership term and passed
//!   back in [`Observed::slot_holds`].
//!
//! Slot holds (a create got 409 and the name is absent from the store):
//! - a hold is `(slot, until)`, `until = 409 time + PlanConfig::slot_hold`;
//!   while `now < until` the slot is occupied, and `Degraded=SlotTaken` once
//!   the hold is `PlanConfig::slot_taken_grace` old (a refreshed hold at once);
//! - once it lapses, if this pass emits a `CreatePod` into the slot (demand
//!   needs it and `slot < max`), `SlotTaken` is reported for this pass only and
//!   the outcome refreshes the hold (409) or clears it (201); otherwise the
//!   hold is dropped at once;
//! - a hold on a slot `>= max` is dropped at once;
//! - the name appearing in the store (owned or foreign) clears the hold.

use std::collections::{BTreeMap, BTreeSet};

use buildkit_controller_api::v1alpha1::{
    BuildKitPool, BuildKitPoolSpec, BuildKitPoolStatus, Waiter, condition,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use k8s_openapi::jiff::tz::TimeZone;
use k8s_openapi::jiff::{SignedDuration, Timestamp};
use kube::ResourceExt;

use super::pods::{Alloc, Phase, WorkerPod, parse_slot, slot_name};
use super::schedule;
use crate::pki::{CA_EXPIRY_WARNING, CaBundle, LoadOutcome};
use crate::render::{WorkerNetwork, refused_allow_cidrs};

/// Max length of a condition message.
const MESSAGE_MAX: usize = 1024;

/// How long an applied `ServiceAccount`, `NetworkPolicy` or CA bundle
/// `ConfigMap` counts as current. Nothing watches them, so the leader applies
/// them again this often; an out-of-band delete or edit lasts at most this long.
pub const REAPPLY_AFTER: SignedDuration = SignedDuration::from_mins(5);

/// `Degraded=True` reasons by precedence.
const DEGRADED_ORDER: [&str; 6] = [
    "PodCreateFailed",
    "PodStartTimeout",
    "PodFailed",
    "UnknownIssuer",
    "SlotTaken",
    "CAReplaced",
];

/// Fixed timings of the spec, injectable for tests.
#[derive(Clone, Debug)]
pub struct PlanConfig {
    /// A `status.waiters` entry older than this is stale (90 s).
    pub waiter_stale_after: SignedDuration,
    /// How long a slot that answered 409 stays held before one retry (10 min).
    pub slot_hold: SignedDuration,
    /// A new hold reports `SlotTaken` only once it is this old (2 s), so the
    /// usual race with an API replica's cold create, whose pod reaches the
    /// store a moment later, does not flap `Degraded`.
    pub slot_taken_grace: SignedDuration,
    /// A terminating pod (free, allocated or draining) is force-deleted this
    /// long after `deletionTimestamp + grace` (2 min).
    pub force_delete_after: SignedDuration,
    /// A free pod is retired when its worker cert has less left (25 h).
    pub worker_cert_min_left: SignedDuration,
    /// `status.endpoint`.
    pub endpoint: String,
    /// `status.serverName`.
    pub server_name: String,
}

impl Default for PlanConfig {
    fn default() -> Self {
        Self {
            waiter_stale_after: SignedDuration::from_secs(90),
            slot_hold: SignedDuration::from_mins(10),
            slot_taken_grace: SignedDuration::from_secs(2),
            force_delete_after: SignedDuration::from_mins(2),
            worker_cert_min_left: SignedDuration::from_hours(25),
            endpoint: String::new(),
            server_name: String::new(),
        }
    }
}

/// Result of the controller-side template check (render's `validate_template` and `template_hash`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TemplateCheck {
    /// Template valid; the current `P template-hash`.
    Current {
        /// 10 hex chars.
        hash: String,
    },
    /// The template sets a controller-owned field.
    Conflict {
        /// Condition message.
        message: String,
    },
}

/// Outcome of a `CreatePod` from the previous pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreateResult {
    /// 201.
    Created,
    /// HTTP 409, something holds the name.
    SlotTaken,
    /// Any other failure (quota, admission, API error).
    Failed {
        /// Error text.
        message: String,
    },
    /// The slot's cache volume is terminating or was just replaced.
    /// No hold and no `Degraded`; the pass requeues under the error backoff to retry.
    VolumePending,
}

/// A recent create of the leader for this pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateOutcome {
    /// Slot created.
    pub slot: u32,
    /// What happened.
    pub result: CreateResult,
}

/// Everything the planner reads for one pool.
pub struct Observed<'a> {
    /// The cached pool (name, uid, generation, spec, status incl. `waiters`).
    pub pool: &'a BuildKitPool,
    /// `spec.scaling.timeZone` resolved by the caller; `None` yields `InvalidSpec`.
    pub time_zone: Option<TimeZone>,
    /// Template validation and hash.
    pub template: TemplateCheck,
    /// `workerNetwork`: the planner refuses (`InvalidSpec`) a pool whose
    /// `allowCIDRs` would reopen a range it keeps closed.
    pub network: &'a WorkerNetwork,
    /// Derived `status.platforms`.
    pub platforms: Vec<String>,
    /// Result of loading the CA Secret.
    pub ca: &'a LoadOutcome,
    /// The CA bundle `ConfigMap` matches the loaded CA.
    pub ca_bundle_current: bool,
    /// The pool's pods (owner-uid checked).
    pub pods: Vec<WorkerPod>,
    /// `metadata.generation` and time of the last successful sandbox apply in
    /// this term. The sandbox is applied again on a new generation and
    /// [`REAPPLY_AFTER`] after the last apply, which repairs a deleted or edited
    /// `ServiceAccount` or `NetworkPolicy`.
    pub sandbox_applied: Option<(i64, Timestamp)>,
    /// The leader's create outcomes since the previous pass.
    pub recent_creates: Vec<CreateOutcome>,
    /// End of the in-memory create backoff.
    pub create_backoff_until: Option<Timestamp>,
    /// An `oidc` rule names an unknown issuer.
    pub unknown_issuer: Option<String>,
    /// Names of pods in the pool's namespace whose `P pool` label is the pool's
    /// name and whose controller ownerReference uid is not the pool's uid; build
    /// it with [`super::pods::foreign_pod_name`]. Same-name pods of a pool in
    /// another namespace are never foreign.
    pub foreign_pods: Vec<String>,
    /// [`Plan::slot_holds`] of the previous pass (empty at the start of a term).
    pub slot_holds: BTreeMap<u32, Timestamp>,
    /// Uids of pods that turned Ready since the previous pass (resets the backoff).
    pub became_ready: BTreeSet<String>,
    /// Pods seen up earlier in this term (free: Ready; allocated: Running with
    /// an IP) that are not up now, with when the leader first saw them down.
    /// `startTimeout` judges only pods never seen up; see [`RetireReason::Unready`].
    pub down_since: BTreeMap<String, Timestamp>,
    /// When the leader first saw each `status.waiters` entry, keyed by
    /// `(replica, updatedAt)`, on its own clock. Staleness is judged from this,
    /// never from `updatedAt`, which another replica's clock wrote. An entry
    /// missing here counts as seen now.
    pub waiters_seen: BTreeMap<(String, Timestamp), Timestamp>,
}

/// Why a pod is retired.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RetireReason {
    /// `P expires-at <= now`.
    Expired,
    /// Phase `Failed` or `Succeeded`, except [`Self::DaemonCrashed`].
    PodFinished,
    /// Free, phase `Failed`, not evicted: buildkitd exited non-zero before any
    /// build ran, so the state it loaded is suspect.
    DaemonCrashed,
    /// Never up within `startTimeout`: not Ready (free) or not Running with an
    /// IP (allocated), and not seen up earlier in this term.
    StartTimeout,
    /// Free, seen Ready earlier in this term, and not Ready again within
    /// `startTimeout` of the leader first seeing it unready. Not a start
    /// failure: no `Degraded`, no create backoff, and the slot keeps its cache volume.
    Unready,
    /// `P internal-ca` differs from the loaded internal CA.
    CaRotated,
    /// Free with a stale `P template-hash`.
    StaleTemplate,
    /// Free with `worker-cert-not-after < now + 25h`.
    CertExpiring,
    /// Free in slot `>= max` (or a non-canonical name).
    OutOfRange,
    /// Free, Ready, above `desiredWarm`, idle for `idleTimeout`.
    Idle,
}

impl RetireReason {
    /// Reasons that may retire an allocated pod.
    #[must_use]
    pub fn allowed_for_allocated(self) -> bool {
        matches!(
            self,
            Self::Expired | Self::PodFinished | Self::StartTimeout | Self::CaRotated
        )
    }

    /// Stable label for logs and metrics.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::PodFinished | Self::DaemonCrashed => "failed",
            Self::StartTimeout => "start_timeout",
            Self::Unready => "unready",
            Self::CaRotated => "ca_rotated",
            Self::StaleTemplate => "stale_template",
            Self::CertExpiring => "cert_expiring",
            Self::OutOfRange => "out_of_range",
            Self::Idle => "idle",
        }
    }
}

/// `events.k8s.io/v1` Event on the CA Secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaEvent {
    /// `CACreated` or `CAReplaced`.
    pub reason: &'static str,
    /// `Normal` or `Warning`.
    pub type_: &'static str,
    /// Human text.
    pub message: String,
}

/// A `status.waiters` entry to prune with `test replica, test updatedAt, remove`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaiterRef {
    /// Index in `status.waiters`.
    pub index: usize,
    /// `replica` (tested).
    pub replica: String,
    /// `updatedAt` (tested).
    pub updated_at: Time,
}

/// One step of a pass, in execution order.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// CA Secret absent, so create new CAs.
    CreateCa,
    /// CA Secret unusable, so back it up, then replace it.
    BackupAndReplaceCa {
        /// `invalid` | `foreign` | `expired` (metric label).
        reason: &'static str,
        /// The failed check.
        message: String,
    },
    /// Apply the CA bundle `ConfigMap` from the loaded CA.
    ApplyCaBundle,
    /// Event for the CA action just before.
    Event(CaEvent),
    /// SSA the worker `ServiceAccount`.
    ApplySa,
    /// SSA the worker `NetworkPolicy`.
    ApplyNetworkPolicy,
    /// Delete a `draining` pod (uid precondition; 404/409 done).
    DeleteDraining {
        /// Pod name.
        pod: String,
        /// Precondition.
        uid: String,
    },
    /// Delete a stuck terminating pod with grace 0.
    ForceDelete {
        /// Pod name.
        pod: String,
        /// Precondition.
        uid: String,
    },
    /// Retire patch (expected `free`) then delete.
    RetireFree {
        /// Pod name.
        pod: String,
        /// Tested uid.
        uid: String,
        /// Why.
        reason: RetireReason,
    },
    /// Retire patch (expected = the allocation id) then delete.
    RetireAllocated {
        /// Pod name.
        pod: String,
        /// Tested uid.
        uid: String,
        /// Tested `P alloc`.
        alloc_id: String,
        /// Tested `P expires-at`, for expiry and start-timeout retires only, so
        /// they lose to a commit that moved it from the claim value after
        /// the state they judged.
        expires_at: Option<String>,
        /// Why.
        reason: RetireReason,
    },
    /// Create a free pod in `slot`.
    CreatePod {
        /// Slot.
        slot: u32,
        /// `<pool>-<slot>`.
        name: String,
    },
    /// SSA the status (leader-owned fields; `waiters` untouched).
    PatchStatus(Box<BuildKitPoolStatus>),
    /// Prune `status.waiters`, highest index first so removals do not shift later ones.
    PruneWaiters {
        /// Entries to remove.
        entries: Vec<WaiterRef>,
    },
}

impl Action {
    /// Touches a pod (create, retire or delete).
    #[must_use]
    pub fn is_pod_action(&self) -> bool {
        matches!(
            self,
            Self::DeleteDraining { .. }
                | Self::ForceDelete { .. }
                | Self::RetireFree { .. }
                | Self::RetireAllocated { .. }
                | Self::CreatePod { .. }
        )
    }
}

/// How the executor moves the create backoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackoffSignal {
    /// A pod failed or timed out. Start the backoff or double it.
    Bump,
    /// A pod is Ready. Reset the backoff.
    Reset,
    /// Leave it.
    Keep,
}

/// The planner's output.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// Ordered actions.
    pub actions: Vec<Action>,
    /// Earliest real deadline; `None` = wait for events.
    pub requeue_at: Option<Timestamp>,
    /// Create backoff update.
    pub backoff: BackoffSignal,
    /// Slots held by a pod the pool does not own (a create got 409 and the
    /// name is absent from the store), each until the given time. The executor
    /// keeps this map for the leadership term and passes it back as
    /// [`Observed::slot_holds`]. The rule is in the module doc.
    pub slot_holds: BTreeMap<u32, Timestamp>,
}

/// What the executor does after running a plan's `CreatePod`s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requeue {
    /// Every create returned 201, or none ran. Wait for events and
    /// [`Plan::requeue_at`].
    Events,
    /// A create returned 409. Reconcile the pool again at once so the planner
    /// records the hold and `SlotTaken`. The held slot stops the retry loop.
    Immediately,
    /// Any other create failure. Requeue under the error backoff
    /// (1 s doubling to 60 s), as for a failed pass.
    ErrorBackoff,
}

impl Plan {
    /// The requeue the executor must arm after a pass's creates. A non-201
    /// create produces no pod event, so without this nothing guarantees the next
    /// pass that reads the outcomes (passed in as [`Observed::recent_creates`]).
    /// A failure outranks a 409 so a failing create is never retried at once.
    #[must_use]
    pub fn requeue_after_outcomes(outcomes: &[CreateOutcome]) -> Requeue {
        if outcomes.iter().any(|c| {
            matches!(
                c.result,
                CreateResult::Failed { .. } | CreateResult::VolumePending
            )
        }) {
            Requeue::ErrorBackoff
        } else if outcomes.iter().any(|c| c.result == CreateResult::SlotTaken) {
            Requeue::Immediately
        } else {
            Requeue::Events
        }
    }
}

/// The CA Secret actions for `ca`: create when missing, back up
/// and replace when unusable (each followed by its Event), apply the bundle
/// `ConfigMap` when valid and not yet applied for this Secret version. Planned
/// by every pool pass and by the pool-independent CA pass.
#[must_use]
pub fn ca_actions(ca: &LoadOutcome, bundle_current: bool) -> Vec<Action> {
    match ca {
        LoadOutcome::Valid(_) if bundle_current => Vec::new(),
        LoadOutcome::Valid(_) => vec![Action::ApplyCaBundle],
        LoadOutcome::Missing => vec![
            Action::CreateCa,
            Action::Event(CaEvent {
                reason: "CACreated",
                type_: "Normal",
                message: "created new CAs".to_owned(),
            }),
        ],
        LoadOutcome::Invalid(e) => {
            let message = e.to_string();
            vec![
                Action::BackupAndReplaceCa {
                    reason: e.reason(),
                    message: message.clone(),
                },
                Action::Event(CaEvent {
                    reason: "CAReplaced",
                    type_: "Warning",
                    message,
                }),
            ]
        }
    }
}

/// When `CAExpiringSoon` turns True. Each pool's plan puts it in
/// [`Plan::requeue_at`] while it is in the future, so no separate timer is needed.
#[must_use]
pub fn ca_warning_at(ca: &CaBundle) -> Option<Timestamp> {
    ca.expiries()
        .into_iter()
        .map(|(_, t)| t - CA_EXPIRY_WARNING)
        .min()
        .and_then(|t| Timestamp::from_second(t.unix_timestamp()).ok())
}

/// The `InvalidSpec` message for refused `allowCIDRs` entries, naming each one.
fn egress_refused(spec: &BuildKitPoolSpec, net: &WorkerNetwork) -> Option<String> {
    let refused = refused_allow_cidrs(spec, net);
    (!refused.is_empty()).then(|| {
        refused
            .iter()
            .map(|c| format!("{}: {}", c.field, c.reason))
            .collect::<Vec<_>>()
            .join("; ")
    })
}

/// Plans one pass for one pool.
#[must_use]
pub fn plan(o: &Observed<'_>, now: Timestamp, cfg: &PlanConfig) -> Plan {
    let p = Planner {
        o,
        now,
        cfg,
        actions: Vec::new(),
        deadline: None,
        retired: BTreeSet::new(),
        degraded: Vec::new(),
        slot_holds: BTreeMap::new(),
    };
    p.run()
}

struct Planner<'p, 'a> {
    o: &'p Observed<'a>,
    now: Timestamp,
    cfg: &'p PlanConfig,
    actions: Vec<Action>,
    deadline: Option<Timestamp>,
    /// Uids retired this pass.
    retired: BTreeSet<String>,
    /// `(reason, message)` candidates for `Degraded=True`.
    degraded: Vec<(&'static str, String)>,
    /// Holds to persist ([`Plan::slot_holds`]).
    slot_holds: BTreeMap<u32, Timestamp>,
}

/// Counts after this pass's retires.
struct Counts {
    warm: i32,
    starting: i32,
    allocated: i32,
    desired_capped: i32,
    waiting: i32,
}

impl<'a> Planner<'_, 'a> {
    fn run(mut self) -> Plan {
        let o = self.o;
        let spec = &o.pool.spec;
        // Step 1.
        let spec_block = match (&o.time_zone, &o.template) {
            (None, _) => Some((
                "InvalidSpec",
                format!("unknown time zone `{}`", spec.scaling.time_zone),
            )),
            (_, TemplateCheck::Conflict { message }) => Some(("TemplateConflict", message.clone())),
            _ => None,
        };
        let egress = egress_refused(spec, o.network);
        // Refused entries still apply the sandbox: render leaves them out, which
        // narrows a policy an older spec or config opened wider. Pods that run
        // under it lose the range at once.
        let apply_sandbox = spec_block.is_none() || egress.is_some();
        let spec_block = spec_block.or_else(|| egress.map(|m| ("InvalidSpec", m)));
        // Step 2.
        let ca = self.plan_ca();
        let waiting = self.plan_waiters_deadlines();
        // Steps 4 and 9 and the `P internal-ca` rule need a valid CA. Steps 5 to 8 always run.
        let fp = ca.map(|c| c.internal_ca().fingerprint_hex());
        if fp.is_some() && apply_sandbox {
            self.plan_applies();
        }
        let counts = Some(self.plan_pods(fp.as_deref(), spec_block.is_none(), waiting));
        if let Some(fp) = &fp {
            self.ca_replaced_check(fp);
        }
        if let Some(t) = ca.and_then(ca_warning_at) {
            self.at(t);
        }
        let ready = match (&spec_block, ca) {
            (Some((r, m)), _) => (false, *r, m.clone()),
            (None, None) => (false, "CAUnavailable", "no valid CA loaded".to_owned()),
            (None, Some(_)) => (true, "Ready", String::new()),
        };
        let status = self.status(counts, &ready, ca);
        let cached = o.pool.status.clone().unwrap_or_default();
        let mut cached_cmp = cached.clone();
        cached_cmp.waiters.clear();
        if *status != cached_cmp {
            self.actions.push(Action::PatchStatus(status));
        }
        self.prune_waiters(&cached);
        let backoff = if self
            .degraded
            .iter()
            .any(|(r, _)| matches!(*r, "PodFailed" | "PodStartTimeout"))
        {
            BackoffSignal::Bump
        } else if o
            .pods
            .iter()
            .any(|p| p.ready && !p.terminating() && o.became_ready.contains(&p.uid))
        {
            BackoffSignal::Reset
        } else {
            BackoffSignal::Keep
        };
        Plan {
            actions: self.actions,
            requeue_at: self.deadline,
            backoff,
            slot_holds: self.slot_holds,
        }
    }

    /// Records a deadline if it is in the future.
    fn at(&mut self, t: Timestamp) {
        if t > self.now {
            self.deadline = Some(self.deadline.map_or(t, |d| d.min(t)));
        }
    }

    fn plan_ca(&mut self) -> Option<&'a CaBundle> {
        let ca: &'a LoadOutcome = self.o.ca;
        self.actions
            .extend(ca_actions(ca, self.o.ca_bundle_current));
        match ca {
            LoadOutcome::Valid(b) => Some(b),
            LoadOutcome::Missing | LoadOutcome::Invalid(_) => None,
        }
    }

    /// Sum of fresh waiter counts, and their stale deadlines.
    fn plan_waiters_deadlines(&mut self) -> i32 {
        let waiters = self.o.pool.status.iter().flat_map(|s| &s.waiters);
        let mut sum: i64 = 0;
        let mut stale_at = Vec::new();
        for w in waiters {
            let t = add(self.waiter_seen(w), self.cfg.waiter_stale_after);
            if t >= self.now {
                sum += i64::from(w.count.max(0));
                stale_at.push(t);
            }
        }
        for t in stale_at {
            self.at(add(t, SignedDuration::from_secs(1)));
        }
        i32::try_from(sum).unwrap_or(i32::MAX)
    }

    /// Sandbox objects (step 4).
    fn plan_applies(&mut self) {
        let o = self.o;
        let generation = o.pool.metadata.generation.unwrap_or_default();
        match o.sandbox_applied {
            Some((g, at)) if g == generation && add(at, REAPPLY_AFTER) > self.now => {
                self.at(add(at, REAPPLY_AFTER));
            }
            _ => {
                self.actions.push(Action::ApplySa);
                self.actions.push(Action::ApplyNetworkPolicy);
            }
        }
    }

    /// Steps 5–9 (creates only when `may_create`).
    /// `fp` is the loaded internal CA fingerprint; `None` skips creates and the
    /// `P internal-ca` rule.
    fn plan_pods(&mut self, fp: Option<&str>, spec_ok: bool, waiting: i32) -> Counts {
        let o = self.o;
        let fp16 = fp.map(|f| f.get(..16).unwrap_or(f));
        let may_create = spec_ok && fp.is_some();
        let spec = &o.pool.spec;
        let max = u32::try_from(spec.scaling.max.max(0)).unwrap_or(0);
        let start_timeout = secs(spec.workers.start_timeout.seconds().ok());
        let current_hash = match &o.template {
            TemplateCheck::Current { hash } => Some(hash.as_str()),
            TemplateCheck::Conflict { .. } => None,
        };
        self.plan_draining();
        self.plan_lifecycle(fp16, max, current_hash, start_timeout);
        self.plan_sizing(may_create, waiting, max, current_hash)
    }

    /// Step 5 deletes draining pods and force-deletes stuck terminating ones.
    /// Any owned terminating pod is forced, not only a draining one: an
    /// evicted pod on a lost node, or one deleted by hand, otherwise holds its
    /// slot for as long as the kubelet never confirms. A pod with an unknown
    /// `P alloc` is never touched.
    fn plan_draining(&mut self) {
        let (o, now) = (self.o, self.now);
        for p in o.pods.iter().filter(|p| p.alloc != Alloc::Unknown) {
            match p.deletion {
                None if p.alloc == Alloc::Draining => self.actions.push(Action::DeleteDraining {
                    pod: p.name.clone(),
                    uid: p.uid.clone(),
                }),
                None => {}
                Some(d) => {
                    let force_at = add(
                        add(d, SignedDuration::from_secs(p.grace_seconds.max(0))),
                        self.cfg.force_delete_after,
                    );
                    if force_at <= now {
                        self.actions.push(Action::ForceDelete {
                            pod: p.name.clone(),
                            uid: p.uid.clone(),
                        });
                    } else {
                        self.at(force_at);
                    }
                }
            }
        }
    }

    /// Step 6 covers lifecycle retires.
    fn plan_lifecycle(
        &mut self,
        fp16: Option<&str>,
        max: u32,
        current_hash: Option<&str>,
        start_timeout: SignedDuration,
    ) {
        let (o, now) = (self.o, self.now);
        for p in o.pods.iter().filter(|p| !p.terminating()) {
            let down_since = o.down_since.get(&p.uid).copied();
            // A pod seen up in this term started; startTimeout no longer applies.
            let started_by = p
                .created
                .filter(|_| down_since.is_none())
                .map(|c| add(c, start_timeout));
            let ca_stale = fp16.is_some_and(|f| p.internal_ca.as_deref().is_some_and(|c| c != f));
            match &p.alloc {
                Alloc::Allocated(id) => {
                    // One seen up and down since is left to its expiry: a
                    // node problem, not a start failure, and its build may resume.
                    let not_up = !p.up();
                    let reason = if p.expires_at.is_some_and(|e| e <= now) {
                        Some(RetireReason::Expired)
                    } else if p.phase.is_finished() {
                        Some(RetireReason::PodFinished)
                    } else if not_up && started_by.is_some_and(|t| t <= now) {
                        Some(RetireReason::StartTimeout)
                    } else if ca_stale {
                        Some(RetireReason::CaRotated)
                    } else {
                        None
                    };
                    if let Some(reason) = reason {
                        self.retire_allocated(p, id, reason);
                    } else {
                        if let Some(e) = p.expires_at {
                            self.at(e);
                        }
                        if let (true, Some(t)) = (not_up, started_by) {
                            self.at(t);
                        }
                    }
                }
                Alloc::Free => {
                    let cert_min = add(now, self.cfg.worker_cert_min_left);
                    let unready_by = down_since.map(|d| add(d, start_timeout)).or(started_by);
                    let reason = if p.phase == Phase::Failed && !p.evicted {
                        Some(RetireReason::DaemonCrashed)
                    } else if p.phase.is_finished() {
                        Some(RetireReason::PodFinished)
                    } else if !p.ready && unready_by.is_some_and(|t| t <= now) {
                        Some(if down_since.is_some() {
                            RetireReason::Unready
                        } else {
                            RetireReason::StartTimeout
                        })
                    } else if ca_stale {
                        Some(RetireReason::CaRotated)
                    } else if current_hash.is_some_and(|h| p.template_hash.as_deref() != Some(h)) {
                        Some(RetireReason::StaleTemplate)
                    } else if p.worker_cert_not_after.is_some_and(|t| t < cert_min) {
                        Some(RetireReason::CertExpiring)
                    } else if p.slot.is_none_or(|s| s >= max) {
                        Some(RetireReason::OutOfRange)
                    } else {
                        None
                    };
                    if let Some(reason) = reason {
                        self.retire_free(p, reason);
                    } else {
                        if let (false, Some(t)) = (p.ready, unready_by) {
                            self.at(t);
                        }
                        if let Some(t) = p.worker_cert_not_after {
                            self.at(add(t, -self.cfg.worker_cert_min_left));
                        }
                    }
                }
                Alloc::Draining | Alloc::Unknown => {}
            }
        }
    }

    /// Steps 7 to 9 cover idle retire, counts and creates.
    fn plan_sizing(
        &mut self,
        may_create: bool,
        waiting: i32,
        max: u32,
        current_hash: Option<&str>,
    ) -> Counts {
        let (o, now) = (self.o, self.now);
        let spec = &o.pool.spec;
        let idle_timeout = secs(spec.scaling.idle_timeout.seconds().ok());
        let eff_min = match &o.time_zone {
            Some(tz) => {
                if let Some(t) = schedule::next_boundary(&spec.scaling.schedule, tz, now) {
                    self.at(t);
                }
                schedule::effective_min(&spec.scaling, tz, now)
            }
            // With an InvalidSpec no window can be evaluated, so never scale below any of them.
            None => spec
                .scaling
                .schedule
                .iter()
                .map(|w| w.min)
                .fold(spec.scaling.min, i32::max),
        };
        let desired = eff_min.max(waiting).max(0);
        let live_free = |pl: &Self, p: &&WorkerPod| {
            p.alloc == Alloc::Free
                && !p.terminating()
                && !pl.retired.contains(&p.uid)
                && current_hash.is_none_or(|h| p.template_hash.as_deref() == Some(h))
        };
        let free_n = o.pods.iter().filter(|p| live_free(self, p)).count();
        let free = i32::try_from(free_n).unwrap_or(i32::MAX);
        if free > desired {
            let mut idle: Vec<&WorkerPod> = o
                .pods
                .iter()
                .filter(|p| live_free(self, p) && p.ready && p.created.is_some())
                .collect();
            idle.sort_by_key(|p| (p.created, p.slot));
            let mut excess = usize::try_from(free - desired).unwrap_or(0);
            for p in idle {
                let idle_at = p.created.map_or(Timestamp::MAX, |c| add(c, idle_timeout));
                if excess > 0 && idle_at <= now {
                    self.retire_free(p, RetireReason::Idle);
                    excess -= 1;
                } else if excess > 0 {
                    self.at(idle_at);
                }
            }
        }
        // Step 8, counts.
        let live = |p: &&WorkerPod| !p.terminating() && !self.retired.contains(&p.uid);
        let count = |f: &dyn Fn(&WorkerPod) -> bool| {
            i32::try_from(o.pods.iter().filter(live).filter(|p| f(p)).count()).unwrap_or(i32::MAX)
        };
        let warm = count(&|p| p.alloc == Alloc::Free && p.ready);
        let starting = count(&|p| p.alloc == Alloc::Free && !p.ready);
        let allocated = count(&|p| matches!(p.alloc, Alloc::Allocated(_)));
        let free = o.pods.iter().filter(|p| live_free(self, p)).count();
        let free = i32::try_from(free).unwrap_or(i32::MAX);
        // Step 9, creates.
        let occupied = self.occupied_slots();
        let free_slots: Vec<u32> = (0..max).filter(|s| !occupied.contains(s)).collect();
        let available = i32::try_from(free_slots.len()).unwrap_or(i32::MAX);
        let backoff = o.create_backoff_until.filter(|t| *t > now);
        if let Some(t) = backoff {
            self.at(t);
        }
        let n = (desired - free).min(available);
        if desired - free > available {
            // When held slots block demand, retry the first one when its hold lapses.
            if let Some(t) = self
                .slot_holds
                .values()
                .filter(|t| **t > now)
                .min()
                .copied()
            {
                self.at(t);
            }
        }
        if may_create && backoff.is_none() && n > 0 {
            let pool = o.pool.name_any();
            for slot in free_slots.into_iter().take(usize::try_from(n).unwrap_or(0)) {
                self.actions.push(Action::CreatePod {
                    slot,
                    name: slot_name(&pool, slot),
                });
            }
        }
        self.settle_lapsed_holds();
        Counts {
            warm,
            starting,
            allocated,
            desired_capped: desired.min(free.saturating_add(available)),
            waiting,
        }
    }

    /// Occupied slots: every owned pod, every pool-labelled pod the pool
    /// does not own, and every held slot. Updates the holds from the recent
    /// creates and records `SlotTaken` / `PodCreateFailed`.
    fn occupied_slots(&mut self) -> BTreeSet<u32> {
        let (o, now) = (self.o, self.now);
        let pool = o.pool.name_any();
        let foreign: BTreeSet<u32> = o
            .foreign_pods
            .iter()
            .filter_map(|n| parse_slot(n, &pool))
            .collect();
        let mut holds = o.slot_holds.clone();
        for c in &o.recent_creates {
            match &c.result {
                CreateResult::SlotTaken => {
                    // A refreshed hold was reported already; it starts past its grace.
                    let since = if o.slot_holds.contains_key(&c.slot) {
                        add(now, -self.cfg.slot_taken_grace)
                    } else {
                        now
                    };
                    holds.insert(c.slot, add(since, self.cfg.slot_hold));
                }
                CreateResult::Created => {
                    holds.remove(&c.slot);
                }
                CreateResult::VolumePending => {}
                CreateResult::Failed { message } => self.degraded.push((
                    "PodCreateFailed",
                    format!("create `{}`: {message}", slot_name(&pool, c.slot)),
                )),
            }
        }
        // Dropped at once when the name is in the store (tracked there from
        // then on) or the slot is out of range. A lapsed hold survives only if
        // this pass retries the slot (see `settle_lapsed_holds`).
        let max = u32::try_from(o.pool.spec.scaling.max.max(0)).unwrap_or(0);
        holds.retain(|s, _| {
            *s < max && !foreign.contains(s) && !o.pods.iter().any(|p| p.slot == Some(*s))
        });
        let active: BTreeSet<u32> = holds
            .iter()
            .filter(|(_, until)| **until > now)
            .map(|(s, _)| *s)
            .collect();
        for s in &foreign {
            self.slot_taken(&pool, *s);
        }
        for (s, until) in holds.iter().filter(|(s, _)| active.contains(s)) {
            let reported_at = add(add(*until, -self.cfg.slot_hold), self.cfg.slot_taken_grace);
            if reported_at <= now {
                self.slot_taken(&pool, *s);
            } else {
                self.at(reported_at);
            }
        }
        let mut occupied: BTreeSet<u32> = o.pods.iter().filter_map(|p| p.slot).collect();
        occupied.extend(&foreign);
        occupied.extend(&active);
        self.slot_holds = holds;
        occupied
    }

    /// A lapsed hold is kept, with `SlotTaken`, only for the pass that retries
    /// its slot; the retry's outcome then refreshes (409) or clears (201) it.
    /// Every other lapsed hold is dropped, so nothing lingers.
    fn settle_lapsed_holds(&mut self) {
        let now = self.now;
        let retried: BTreeSet<u32> = self
            .actions
            .iter()
            .filter_map(|a| match a {
                Action::CreatePod { slot, .. } => Some(*slot),
                _ => None,
            })
            .collect();
        self.slot_holds
            .retain(|s, until| *until > now || retried.contains(s));
        let pool = self.o.pool.name_any();
        let lapsed: Vec<u32> = self
            .slot_holds
            .iter()
            .filter(|(_, until)| **until <= now)
            .map(|(s, _)| *s)
            .collect();
        for s in lapsed {
            self.slot_taken(&pool, s);
        }
    }

    fn slot_taken(&mut self, pool: &str, slot: u32) {
        self.degraded.push((
            "SlotTaken",
            format!("pod `{}` is not owned by the pool", slot_name(pool, slot)),
        ));
    }

    fn retire_free(&mut self, p: &WorkerPod, reason: RetireReason) {
        self.note_failure(p, reason);
        self.retired.insert(p.uid.clone());
        self.actions.push(Action::RetireFree {
            pod: p.name.clone(),
            uid: p.uid.clone(),
            reason,
        });
    }

    fn retire_allocated(&mut self, p: &WorkerPod, id: &str, reason: RetireReason) {
        // A build can end its own daemon (a shared PID namespace, an OOM kill).
        // That is not a start failure: it neither degrades the pool nor bumps the
        // create backoff, or any tenant could stall warm replenishment for all.
        if reason != RetireReason::PodFinished {
            self.note_failure(p, reason);
        }
        self.retired.insert(p.uid.clone());
        self.actions.push(Action::RetireAllocated {
            pod: p.name.clone(),
            uid: p.uid.clone(),
            alloc_id: id.to_owned(),
            expires_at: matches!(reason, RetireReason::Expired | RetireReason::StartTimeout)
                .then(|| p.expires_at_raw.clone())
                .flatten(),
            reason,
        });
    }

    fn note_failure(&mut self, p: &WorkerPod, reason: RetireReason) {
        let d = match reason {
            RetireReason::PodFinished | RetireReason::DaemonCrashed => {
                ("PodFailed", format!("pod `{}` finished", p.name))
            }
            RetireReason::StartTimeout => (
                "PodStartTimeout",
                format!("pod `{}` not ready within startTimeout", p.name),
            ),
            _ => return,
        };
        self.degraded.push(d);
    }

    fn ca_replaced_check(&mut self, fp: &str) {
        let fp16 = fp.get(..16).unwrap_or(fp);
        if let Some(p) = self
            .o
            .pods
            .iter()
            .find(|p| p.internal_ca.as_deref().is_some_and(|c| c != fp16))
        {
            self.degraded.push((
                "CAReplaced",
                format!("pod `{}` was issued under a replaced internal CA", p.name),
            ));
        }
    }

    fn status(
        &mut self,
        counts: Option<Counts>,
        ready: &(bool, &'static str, String),
        ca: Option<&CaBundle>,
    ) -> Box<BuildKitPoolStatus> {
        let o = self.o;
        let old = o.pool.status.clone().unwrap_or_default();
        let generation = o.pool.metadata.generation;
        let mut conditions = Vec::new();
        conditions.push(self.condition(&old, condition::READY, ready.0, ready.1, &ready.2));
        if let Some(u) = &o.unknown_issuer {
            self.degraded
                .push(("UnknownIssuer", format!("unknown oidc issuer `{u}`")));
        }
        let first = DEGRADED_ORDER
            .iter()
            .find_map(|r| self.degraded.iter().find(|(d, _)| d == r))
            .cloned();
        let old_degraded = old
            .conditions
            .iter()
            .find(|c| c.type_ == condition::DEGRADED);
        let backoff_on = o.create_backoff_until.is_some_and(|t| t > self.now);
        match (first, old_degraded) {
            (Some((r, m)), _) => {
                conditions.push(self.condition(&old, condition::DEGRADED, true, r, &m));
            }
            (None, Some(c)) if backoff_on => conditions.push(c.clone()),
            (None, _) => {
                conditions.push(self.condition(&old, condition::DEGRADED, false, "AsExpected", ""));
            }
        }
        match ca.map(|b| b.ca_expiry_warning(to_offset(self.now))) {
            Some(w) => {
                let (soon, r) = if w.is_some() {
                    (true, "CAExpiringSoon")
                } else {
                    (false, "AsExpected")
                };
                let msg = w.map_or(String::new(), |d| {
                    format!("a CA expires in {} days", d.whole_days())
                });
                conditions.push(self.condition(&old, condition::CA_EXPIRING_SOON, soon, r, &msg));
            }
            None => conditions.extend(
                old.conditions
                    .iter()
                    .find(|c| c.type_ == condition::CA_EXPIRING_SOON)
                    .cloned(),
            ),
        }
        let (warm, starting, allocated, desired, waiting) = match counts {
            Some(c) => (
                Some(c.warm),
                Some(c.starting),
                Some(c.allocated),
                Some(c.desired_capped),
                Some(c.waiting),
            ),
            None => (
                old.warm,
                old.starting,
                old.allocated,
                old.desired_warm,
                old.waiting,
            ),
        };
        Box::new(BuildKitPoolStatus {
            observed_generation: generation,
            warm,
            starting,
            allocated,
            desired_warm: desired,
            waiting,
            waiters: Vec::new(),
            platforms: o.platforms.clone(),
            endpoint: Some(self.cfg.endpoint.clone()).filter(|s| !s.is_empty()),
            server_name: Some(self.cfg.server_name.clone()).filter(|s| !s.is_empty()),
            cache: cache_status(o.pool),
            conditions,
        })
    }

    /// A condition; keeps `lastTransitionTime` while the status is unchanged.
    fn condition(
        &self,
        old: &BuildKitPoolStatus,
        type_: &str,
        status: bool,
        reason: &str,
        message: &str,
    ) -> Condition {
        let status = if status { "True" } else { "False" };
        let since = old
            .conditions
            .iter()
            .find(|c| c.type_ == type_ && c.status == status)
            .map_or(Time(self.now), |c| c.last_transition_time.clone());
        Condition {
            last_transition_time: since,
            message: truncate(message),
            observed_generation: self.o.pool.metadata.generation,
            reason: reason.to_owned(),
            status: status.to_owned(),
            type_: type_.to_owned(),
        }
    }

    /// When the leader first saw `w` (see [`Observed::waiters_seen`]).
    fn waiter_seen(&self, w: &Waiter) -> Timestamp {
        self.o
            .waiters_seen
            .get(&(w.replica.clone(), w.updated_at.0))
            .copied()
            .unwrap_or(self.now)
    }

    /// Step 10 prunes entries older than 90 s (stale, including `count: 0`).
    fn prune_waiters(&mut self, status: &BuildKitPoolStatus) {
        let mut entries: Vec<WaiterRef> = status
            .waiters
            .iter()
            .enumerate()
            .filter(|(_, w)| add(self.waiter_seen(w), self.cfg.waiter_stale_after) < self.now)
            .map(|(index, w)| WaiterRef {
                index,
                replica: w.replica.clone(),
                updated_at: w.updated_at.clone(),
            })
            .collect();
        if !entries.is_empty() {
            entries.reverse();
            self.actions.push(Action::PruneWaiters { entries });
        }
    }
}

fn add(t: Timestamp, d: SignedDuration) -> Timestamp {
    t.checked_add(d).unwrap_or(if d.is_negative() {
        Timestamp::MIN
    } else {
        Timestamp::MAX
    })
}

fn secs(s: Option<u64>) -> SignedDuration {
    SignedDuration::from_secs(
        s.and_then(|s| i64::try_from(s).ok())
            .unwrap_or(i64::MAX / 2),
    )
}

fn to_offset(t: Timestamp) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(t.as_second())
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}

fn truncate(m: &str) -> String {
    let mut end = m.len().min(MESSAGE_MAX);
    while !m.is_char_boundary(end) {
        end -= 1;
    }
    m[..end].to_owned()
}

/// `status.cache`: the registry flags for the pool's default key.
fn cache_status(pool: &BuildKitPool) -> Option<buildkit_controller_api::v1alpha1::CacheStatus> {
    let c = &pool.spec.workers.cache;
    let name = pool.metadata.name.as_deref()?;
    let r = c.registry_enabled()?;
    Some(buildkit_controller_api::v1alpha1::CacheStatus {
        registry: Some(r.resolve(name, c.default_key(name))),
    })
}
