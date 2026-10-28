//! Runs one pool's [`Plan`] against [`KubeOps`], following the executor
//! contract in the [`super::plan`] module doc.

use std::collections::{BTreeMap, VecDeque};
use std::sync::PoisonError;
use std::time::Duration;

use buildkit_controller_api::v1alpha1::{BuildKitPool, BuildKitPoolStatus, condition, reason};
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::{ObjectReference, Pod, Secret};
use k8s_openapi::api::events::v1::Event;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta, Time};
use k8s_openapi::jiff::Timestamp;
use kube::ResourceExt;
use time::OffsetDateTime;
use tokio::time::Instant;

use super::ops::{KubeOps, OpError, OpResult};
use super::plan::{Action, CaEvent, CreateOutcome, CreateResult, Plan};
use super::spawn::{self, PodCreated, PodOutcome};
use crate::pki::{CaBundle, LoadOutcome};
use crate::render::{self, CONTROLLER, LABEL_MANAGED_BY, RenderConfig};

/// Term-wide CA bookkeeping. CA actions run once per CA Secret version
/// (`epoch`, bumped on every CA Secret watch change), not once per pool.
///
/// The lock is a plain mutex and is never held across an API call: a CA
/// action claims its epoch, releases the lock, calls, and then records the
/// result, so a slow CA write never stalls the other pools' passes.
#[derive(Debug, Default)]
pub struct CaGuard(std::sync::Mutex<CaDone>);

#[derive(Debug, Default)]
struct CaDone {
    /// Epoch whose create/replace already ran, lost a race, or is in flight.
    replaced: Option<u64>,
    /// Epoch whose bundle `ConfigMap` was applied.
    bundle: Option<u64>,
    /// Epoch whose bundle apply is in flight.
    bundle_inflight: Option<u64>,
    /// When each recent CA create or replace succeeded (self-heal rate limit).
    heals: VecDeque<Instant>,
}

/// At most this many CA creates or replaces per [`HEAL_WINDOW`]. A writer that
/// keeps restoring an unusable CA Secret would otherwise make the leader
/// replace it, and retire every pod, about once a second.
pub const HEAL_LIMIT: usize = 3;
/// Window of [`HEAL_LIMIT`].
pub const HEAL_WINDOW: Duration = Duration::from_mins(10);

/// The claim an epoch replaced, restored when the claimed action fails.
#[derive(Clone, Copy)]
struct Prev(Option<u64>);

/// A claimed CA create or replace.
enum Claim {
    /// Run the action; `prev` restores the claim when it fails.
    Go { prev: Prev },
    /// Already done (or in flight) for this epoch.
    Done,
    /// Rate-limited; retry after the window frees a slot.
    Paused { heals: usize },
}

impl CaGuard {
    fn done(&self) -> std::sync::MutexGuard<'_, CaDone> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The bundle `ConfigMap` matches the CA of `epoch`.
    pub fn bundle_current(&self, epoch: u64) -> bool {
        self.done().bundle == Some(epoch)
    }

    fn claim_heal(&self, epoch: u64) -> Claim {
        let mut d = self.done();
        if d.replaced == Some(epoch) {
            return Claim::Done;
        }
        let now = Instant::now();
        while d
            .heals
            .front()
            .is_some_and(|t| now.duration_since(*t) >= HEAL_WINDOW)
        {
            d.heals.pop_front();
        }
        if d.heals.len() >= HEAL_LIMIT {
            return Claim::Paused {
                heals: d.heals.len(),
            };
        }
        Claim::Go {
            prev: Prev(d.replaced.replace(epoch)),
        }
    }

    /// Records the end of a claimed create or replace: `healed` when the
    /// Secret was written, `keep` when the epoch needs no other attempt.
    fn finish_heal(&self, epoch: u64, prev: Prev, healed: bool, keep: bool) {
        let mut d = self.done();
        if healed {
            d.heals.push_back(Instant::now());
        }
        if !keep && d.replaced == Some(epoch) {
            d.replaced = prev.0;
        }
    }
}

/// What a pass reads besides the plan.
pub struct Env<'a> {
    /// The pool, as planned.
    pub pool: &'a BuildKitPool,
    /// The loaded CA.
    pub ca: &'a LoadOutcome,
    /// The CA Secret object as last watched (backup and replace read it).
    pub ca_secret: Option<&'a Secret>,
    /// CA Secret version counter of this term.
    pub ca_epoch: u64,
    /// CA Secret name.
    pub ca_secret_name: &'a str,
    /// Render settings (controller namespace, worker network).
    pub render: &'a RenderConfig,
    /// Worker serving cert validity.
    pub worker_cert_validity: time::Duration,
    /// `POD_NAME`, the Event `reportingInstance`.
    pub replica: &'a str,
    /// `config_hash(buildkitdConfig)`.
    pub config_hash: &'a str,
    /// Clock of the pass.
    pub now: Timestamp,
}

/// What a pass did, for the next pass, the requeue and the metrics.
#[derive(Debug, Default)]
pub struct Report {
    /// Every `CreatePod` that ran.
    pub creates: Vec<CreateOutcome>,
    /// Pods returned by a 201 (read-your-writes until the store has them).
    pub created: Vec<Pod>,
    /// SA and `NetworkPolicy` were applied and both succeeded.
    pub sandbox_applied: bool,
    /// The status the pass sent (`PatchStatus` ran), whether or not it succeeded.
    pub status_sent: Option<BuildKitPoolStatus>,
    /// Calls that failed and need the error backoff.
    pub errors: Vec<String>,
    /// `(op, result)` for `buildkit_pod_operations_total`.
    pub pod_ops: Vec<(&'static str, &'static str)>,
    /// `(op, result)` for `buildkit_cache_volume_operations_total`.
    pub volume_ops: Vec<(&'static str, &'static str)>,
    /// `buildkit_ca_replacements_total{reason}`.
    pub ca_replacements: Vec<&'static str>,
    /// `buildkit_releases_total{reason}` (allocated pods retired).
    pub releases: Vec<&'static str>,
    /// Labels of each executed action, in order (tests, debug logs).
    pub trace: Vec<String>,
}

/// Executes `plan` in order. Never retries a call.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per action, in contract order"
)]
pub async fn execute<O: KubeOps>(ops: &O, guard: &CaGuard, env: &Env<'_>, plan: &Plan) -> Report {
    let mut r = Report::default();
    let ns = env.pool.namespace().unwrap_or_default();
    let pool = env.pool.name_any();
    // The CA action just before an `Event` succeeded.
    let mut ca_ok = false;
    // When an SA or NetworkPolicy apply fails, no creates run.
    let mut apply_failed: Option<String> = None;
    let (mut sa_ok, mut np_ok) = (None, None);
    for action in &plan.actions {
        match action {
            Action::CreateCa => {
                r.trace.push("create_ca".into());
                ca_ok = create_ca(ops, guard, env, &mut r).await;
            }
            Action::BackupAndReplaceCa { reason, .. } => {
                r.trace.push("replace_ca".into());
                ca_ok = replace_ca(ops, guard, env, reason, &mut r).await;
            }
            Action::ApplyCaBundle => {
                r.trace.push("apply_ca_bundle".into());
                ca_ok = apply_bundle(ops, guard, env, &mut r).await;
            }
            Action::Event(ev) => {
                if ca_ok {
                    r.trace.push(format!("event:{}", ev.reason));
                    if let Err(e) = ops.create_event(&ca_event(env, ev)).await {
                        r.errors.push(format!("event {}: {e}", ev.reason));
                    }
                } else {
                    r.trace.push(format!("skip_event:{}", ev.reason));
                }
            }
            Action::ApplySa => {
                r.trace.push("apply_sa".into());
                let res = match render::service_account(env.pool) {
                    Ok(sa) => ops.apply(&sa).await.map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
                sa_ok = Some(note_apply(res, "ServiceAccount", &mut apply_failed, &mut r));
            }
            Action::ApplyNetworkPolicy => {
                r.trace.push("apply_network_policy".into());
                let res = match render::network_policy(env.pool, env.render) {
                    Ok(np) => ops.apply(&np).await.map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
                np_ok = Some(note_apply(res, "NetworkPolicy", &mut apply_failed, &mut r));
            }
            Action::DeleteDraining { pod, uid } => {
                r.trace.push(format!("delete:{pod}"));
                let res = ops.delete_pod(&ns, pod, uid, None, None).await;
                pod_op(&mut r, "delete", pod, res);
            }
            Action::ForceDelete { pod, uid } => {
                r.trace.push(format!("force_delete:{pod}"));
                let res = ops.delete_pod(&ns, pod, uid, None, Some(0)).await;
                pod_op(&mut r, "force_delete", pod, res);
            }
            Action::RetireFree { pod, uid, reason } => {
                r.trace.push(format!("retire:{pod}"));
                if retire(ops, &ns, pod, uid, "free", None, &mut r).await {
                    super::cache::reap_after_failure(ops, env.pool, pod, *reason, &mut r).await;
                }
            }
            Action::RetireAllocated {
                pod,
                uid,
                alloc_id,
                expires_at,
                reason,
            } => {
                r.trace.push(format!("retire:{pod}"));
                let exp = expires_at.as_deref();
                if retire(ops, &ns, pod, uid, alloc_id, exp, &mut r).await {
                    super::cache::reap_after_failure(ops, env.pool, pod, *reason, &mut r).await;
                    r.releases.push(match reason.label() {
                        "ca_rotated" => "ca_replaced",
                        other => other,
                    });
                }
            }
            Action::CreatePod { slot, name } => {
                if let Some(why) = &apply_failed {
                    r.trace.push(format!("skip_create:{name}"));
                    tracing::debug!(pod = %name, reason = %why, "create skipped");
                    continue;
                }
                r.trace.push(format!("create:{name}"));
                let result = create_pod(ops, env, *slot, &mut r).await;
                r.creates.push(CreateOutcome {
                    slot: *slot,
                    result,
                });
            }
            Action::PatchStatus(status) => {
                r.trace.push("patch_status".into());
                let mut status: BuildKitPoolStatus = (**status).clone();
                if let Some(why) = &apply_failed {
                    downgrade_ready(&mut status, env, reason::SANDBOX_PENDING, why);
                }
                if let Err(e) = ops.patch_status(&ns, &pool, &status).await {
                    r.errors.push(format!("status: {e}"));
                }
                r.status_sent = Some(status);
            }
            Action::PruneWaiters { entries } => {
                for w in entries {
                    r.trace.push(format!("prune_waiter:{}", w.index));
                    match ops.prune_waiter(&ns, &pool, w).await {
                        Ok(()) | Err(OpError::Invalid(_) | OpError::NotFound) => {}
                        Err(e) => r.errors.push(format!("prune waiter {}: {e}", w.index)),
                    }
                }
            }
        }
    }
    r.sandbox_applied = sa_ok == Some(true) && np_ok == Some(true);
    r
}

fn note_apply(
    res: Result<(), String>,
    what: &str,
    failed: &mut Option<String>,
    r: &mut Report,
) -> bool {
    match res {
        Ok(()) => true,
        Err(e) => {
            let msg = format!("apply {what}: {e}");
            r.errors.push(msg.clone());
            failed.get_or_insert(msg);
            false
        }
    }
}

fn pod_op(r: &mut Report, op: &'static str, pod: &str, res: Result<(), OpError>) -> bool {
    match res {
        Ok(()) => {
            r.pod_ops.push((op, "ok"));
            true
        }
        // The pod is already gone or replaced by a same-named successor, so there is nothing to do.
        Err(OpError::NotFound | OpError::Conflict(_)) => {
            r.pod_ops.push((op, "conflict"));
            false
        }
        Err(e) => {
            r.pod_ops.push((op, "error"));
            r.errors.push(format!("{op} {pod}: {e}"));
            false
        }
    }
}

/// Retire patch then the uid+resourceVersion-preconditioned delete; a failed
/// `test` (422) or a vanished pod skips the delete. True when retired.
async fn retire<O: KubeOps>(
    ops: &O,
    ns: &str,
    pod: &str,
    uid: &str,
    expected: &str,
    expires_at: Option<&str>,
    r: &mut Report,
) -> bool {
    match ops.retire(ns, pod, uid, expected, expires_at).await {
        Ok(rv) => {
            r.pod_ops.push(("retire", "ok"));
            let res = ops.delete_pod(ns, pod, uid, rv, None).await;
            pod_op(r, "delete", pod, res);
            true
        }
        Err(OpError::Invalid(_) | OpError::NotFound | OpError::Conflict(_)) => {
            r.pod_ops.push(("retire", "conflict"));
            false
        }
        Err(e) => {
            r.pod_ops.push(("retire", "error"));
            r.errors.push(format!("retire {pod}: {e}"));
            false
        }
    }
}

impl PodOutcome for OpResult<Pod> {
    fn pod_created(&self) -> PodCreated {
        match self {
            Ok(p) => match (&p.metadata.name, &p.metadata.uid) {
                (Some(name), Some(uid)) => PodCreated::Yes {
                    name: name.clone(),
                    uid: uid.clone(),
                },
                _ => PodCreated::Unknown,
            },
            // 409 (slot taken), 422, 403 (quota, admission) and every other 4xx
            // created nothing.
            Err(
                OpError::Conflict(_)
                | OpError::Invalid(_)
                | OpError::NotFound
                | OpError::Refused { .. },
            ) => PodCreated::No,
            // A timeout or 5xx: the pod may exist.
            Err(OpError::Other(_)) => PodCreated::Unknown,
        }
    }
}

/// Issues the pod's serving cert, creates the pod, then its Secret
/// ([`spawn::spawn`]).
async fn create_pod<O: KubeOps>(ops: &O, env: &Env<'_>, slot: u32, r: &mut Report) -> CreateResult {
    let failed = |message: String| CreateResult::Failed { message };
    let LoadOutcome::Valid(ca) = env.ca else {
        return failed("no valid CA".into());
    };
    let mut worker = match spawn::prepare(
        env.pool,
        ca,
        slot,
        None,
        odt(env.now),
        env.worker_cert_validity,
    ) {
        Ok(w) => w,
        Err(e) => return failed(e),
    };
    if let Some(skip) = super::cache::ensure_volume(ops, env.pool, slot, &mut worker.pod, r).await {
        return skip;
    }
    let ns = env.pool.namespace().unwrap_or_default();
    // Boxed: the create and its read back would bloat every pass future.
    let res = Box::pin(spawn::spawn(ops, worker, |pod| async move {
        ops.create_pod(&ns, &pod).await
    }))
    .await;
    match res.out {
        Ok(p) => {
            r.pod_ops.push(("create", "ok"));
            r.created.push(p);
            // The pod cannot start without its Secret and is retired at
            // startTimeout; the failure counts toward the create backoff.
            if let Err(e) = res.secret {
                r.errors.push(format!("create slot {slot}: {e}"));
                return failed(e);
            }
            CreateResult::Created
        }
        Err(OpError::Conflict(_)) => {
            r.pod_ops.push(("create", "conflict"));
            CreateResult::SlotTaken
        }
        Err(e) => {
            r.pod_ops.push(("create", "error"));
            r.errors.push(format!("create slot {slot}: {e}"));
            failed(e.to_string())
        }
    }
}

fn ca_secret(env: &Env<'_>, bundle: &CaBundle, resource_version: Option<String>) -> Secret {
    Secret {
        metadata: ObjectMeta {
            name: Some(env.ca_secret_name.to_owned()),
            namespace: Some(env.render.controller_namespace.clone()),
            labels: Some(BTreeMap::from([(
                LABEL_MANAGED_BY.into(),
                CONTROLLER.into(),
            )])),
            resource_version,
            ..ObjectMeta::default()
        },
        type_: Some("Opaque".into()),
        data: Some(
            bundle
                .to_secret_data()
                .into_iter()
                .map(|(k, v)| (k, ByteString(v)))
                .collect(),
        ),
        ..Secret::default()
    }
}

/// Claims the epoch's create or replace; `None` when it must not run now.
fn claim(guard: &CaGuard, env: &Env<'_>, r: &mut Report) -> Option<Prev> {
    match guard.claim_heal(env.ca_epoch) {
        Claim::Go { prev } => Some(prev),
        Claim::Done => None,
        Claim::Paused { heals } => {
            r.trace.push("ca_heal_paused".into());
            r.errors.push(format!(
                "CA self-heal paused: the CA Secret was rewritten {heals} times within {}s; \
                 something keeps restoring an unusable CA Secret",
                HEAL_WINDOW.as_secs()
            ));
            None
        }
    }
}

async fn create_ca<O: KubeOps>(ops: &O, guard: &CaGuard, env: &Env<'_>, r: &mut Report) -> bool {
    let Some(prev) = claim(guard, env, r) else {
        return false;
    };
    let bundle = match CaBundle::generate(odt(env.now)) {
        Ok(b) => b,
        Err(e) => {
            guard.finish_heal(env.ca_epoch, prev, false, false);
            r.errors.push(format!("generate CA: {e}"));
            return false;
        }
    };
    match ops.create_secret(&ca_secret(env, &bundle, None)).await {
        Ok(()) => {
            guard.finish_heal(env.ca_epoch, prev, true, true);
            r.ca_replacements.push("missing");
            tracing::info!(secret = env.ca_secret_name, "created the CA Secret");
            true
        }
        // Someone created it meanwhile, and the watch delivers it.
        Err(OpError::Conflict(_)) => {
            guard.finish_heal(env.ca_epoch, prev, false, true);
            false
        }
        Err(e) => {
            guard.finish_heal(env.ca_epoch, prev, false, false);
            r.errors.push(format!("create CA Secret: {e}"));
            false
        }
    }
}

async fn replace_ca<O: KubeOps>(
    ops: &O,
    guard: &CaGuard,
    env: &Env<'_>,
    reason: &'static str,
    r: &mut Report,
) -> bool {
    let Some(cur) = env.ca_secret else {
        return false;
    };
    let Some(prev) = claim(guard, env, r) else {
        return false;
    };
    let fail = |r: &mut Report, msg: String| {
        guard.finish_heal(env.ca_epoch, prev, false, false);
        r.errors.push(msg);
        false
    };
    let backup = Secret {
        metadata: ObjectMeta {
            name: Some(CaBundle::backup_name(env.ca_secret_name, odt(env.now))),
            namespace: Some(env.render.controller_namespace.clone()),
            // The CA Secret keeps backups for forensics. The controller never deletes them.
            labels: Some(BTreeMap::from([
                (LABEL_MANAGED_BY.into(), CONTROLLER.into()),
                ("app.kubernetes.io/component".into(), "ca-replaced".into()),
            ])),
            ..ObjectMeta::default()
        },
        type_: cur.type_.clone(),
        data: cur.data.clone(),
        string_data: cur.string_data.clone(),
        ..Secret::default()
    };
    match ops.create_secret(&backup).await {
        Ok(()) | Err(OpError::Conflict(_)) => {}
        Err(e) => return fail(r, format!("back up CA Secret: {e}")),
    }
    let bundle = match CaBundle::generate(odt(env.now)) {
        Ok(b) => b,
        Err(e) => return fail(r, format!("generate CA: {e}")),
    };
    let rv = cur.metadata.resource_version.clone();
    match ops.replace_secret(&ca_secret(env, &bundle, rv)).await {
        Ok(()) => {
            guard.finish_heal(env.ca_epoch, prev, true, true);
            r.ca_replacements.push(reason);
            tracing::warn!(
                secret = env.ca_secret_name,
                reason,
                "replaced the CA Secret"
            );
            true
        }
        Err(OpError::Conflict(_) | OpError::NotFound) => {
            guard.finish_heal(env.ca_epoch, prev, false, true);
            false
        }
        Err(e) => fail(r, format!("replace CA Secret: {e}")),
    }
}

async fn apply_bundle<O: KubeOps>(ops: &O, guard: &CaGuard, env: &Env<'_>, r: &mut Report) -> bool {
    let LoadOutcome::Valid(ca) = env.ca else {
        return false;
    };
    {
        let mut d = guard.done();
        if d.bundle == Some(env.ca_epoch) {
            return true;
        }
        // Another pass is applying it for this epoch.
        if d.bundle_inflight == Some(env.ca_epoch) {
            return false;
        }
        d.bundle_inflight = Some(env.ca_epoch);
    }
    let cm =
        render::ca_bundle_config_map(env.ca_secret_name, env.render, ca.client_ca().cert_pem());
    let res = ops.apply(&cm).await;
    let mut d = guard.done();
    if d.bundle_inflight == Some(env.ca_epoch) {
        d.bundle_inflight = None;
    }
    match res {
        Ok(()) => {
            d.bundle = Some(env.ca_epoch);
            true
        }
        Err(e) => {
            r.errors.push(format!("apply CA bundle: {e}"));
            false
        }
    }
}

fn ca_event(env: &Env<'_>, ev: &CaEvent) -> Event {
    let ns = env.render.controller_namespace.clone();
    let nanos = env.now.as_nanosecond();
    Event {
        metadata: ObjectMeta {
            name: Some(format!("{}.{nanos:x}", env.ca_secret_name)),
            namespace: Some(ns.clone()),
            ..ObjectMeta::default()
        },
        event_time: Some(MicroTime(env.now)),
        action: Some(ev.reason.to_owned()),
        reason: Some(ev.reason.to_owned()),
        type_: Some(ev.type_.to_owned()),
        note: Some(ev.message.chars().take(1024).collect()),
        reporting_controller: Some(format!("{}{CONTROLLER}", render::KEY_PREFIX)),
        reporting_instance: Some(env.replica.chars().take(128).collect()),
        regarding: Some(ObjectReference {
            api_version: Some("v1".into()),
            kind: Some("Secret".into()),
            name: Some(env.ca_secret_name.to_owned()),
            namespace: Some(ns),
            ..ObjectReference::default()
        }),
        ..Event::default()
    }
}

/// `Ready=False` after a failed apply: `SandboxPending` when the
/// `ServiceAccount` or `NetworkPolicy` failed. It replaces a planned `Ready=True` or a planned pending
/// reason, never another `Ready=False` reason (those come first).
/// `lastTransitionTime` moves only when the status flips.
fn downgrade_ready(status: &mut BuildKitPoolStatus, env: &Env<'_>, pending: &str, why: &str) {
    let cached = env
        .pool
        .status
        .as_ref()
        .and_then(|s| s.conditions.iter().find(|c| c.type_ == condition::READY));
    let since = cached
        .filter(|c| c.status == "False")
        .map_or(Time(env.now), |c| c.last_transition_time.clone());
    let message: String = why.chars().take(1024).collect();
    let cond = k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition {
        type_: condition::READY.to_owned(),
        status: "False".to_owned(),
        reason: pending.to_owned(),
        message,
        last_transition_time: since,
        observed_generation: env.pool.metadata.generation,
    };
    match status
        .conditions
        .iter_mut()
        .find(|c| c.type_ == condition::READY)
    {
        Some(c) if c.status == "False" => {
            if c.reason == reason::SANDBOX_PENDING {
                c.reason = cond.reason;
                c.message = cond.message;
            }
        }
        Some(c) => *c = cond,
        None => status.conditions.push(cond),
    }
}

/// `jiff` instant as `time` (the PKI's clock type).
#[must_use]
pub fn odt(t: Timestamp) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(t.as_nanosecond())
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}
