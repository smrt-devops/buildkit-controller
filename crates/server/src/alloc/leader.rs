//! The [`Allocator`]: allocation's leader loop. While this replica leads it
//! runs the pool passes ([`Binder::pass`]) on claim and pod events, and every
//! 2 s (and at once after a claim is deleted) it tidies up: claims from
//! namespaces it does not serve fail, bound claims expire or lose their worker,
//! delivered claims nobody uses through the edge are released, finished claims
//! of CI callers are deleted after [`claims::FINISHED_TTL`], and pods whose
//! claim is gone or ended are retired.
//!
//! `main` runs it in the leader stage, which stops before the Lease is
//! released, so no claim or pod write of this replica overlaps the next
//! leader's.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use buildkit_controller_api::claim::{BuildKitClaim, ClaimPhase};
use futures::stream::{self, StreamExt};
use k8s_openapi::api::core::v1::Pod;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::backend::Write;
use super::claims::{self, reason};
use super::pods::{ann, fmt_ts, is_ready};
use super::{BindConfig, Binder, Shared};
use crate::metrics_doc::{Def, Handle, Label, Role, register_table};
use crate::reconcile::driver::{Trigger, schedule};
use crate::render::{ANN_ALLOC, ANN_CLAIM, ANN_OWNER, FREE, LABEL_POOL};
use crate::runtime::AppContext;
use crate::runtime::hooks::{BoxResult, Component, LeaderWork};
use crate::runtime::leader::{BoxFut, supervise};
use crate::runtime::stores::{ClaimEvent, PodEvent};

/// How often the allocator tidies claims.
const TICK: Duration = Duration::from_secs(2);
/// `buildkit_releases_total` reason of a worker whose delivered claim went
/// unused for the pool's `scaling.idleTimeout`. Not `idle`: that reason is
/// the reconciler's scale-down of free pods, and this one takes a worker
/// from a client.
pub const ALLOCATION_IDLE: &str = "allocation_idle";

type Labels = Vec<(&'static str, String)>;

const PHASES: &[&str] = &["Pending", "Bound", "Expired", "Lost", "Failed"];
const TIDY_RESULTS: &[&str] = &[
    "failed",
    "expired",
    "lost",
    "idle",
    "abandoned",
    "collected",
    "released",
];

/// The claim families, the leader's.
pub const CLAIM_TABLE: [Def; 2] = [
    Def::gauge(
        "claims",
        Role::Controller,
        &[Label::closed("phase", PHASES)],
        "BuildKitClaims in the leader's store by phase. Cleared when the leader's term ends",
    ),
    Def::counter(
        "claim_reconciles",
        Role::Controller,
        &[Label::closed("result", TIDY_RESULTS)],
        "What the leader's tidy pass did to claims. `failed`: a claim from a namespace it does not serve. `expired`: a bound claim's TTL ran out. `lost`: its worker went away. `idle`: a delivered claim went unused. `abandoned`: a CI claim whose client is gone was deleted. `collected`: a finished CI claim was deleted. `released`: the worker of a gone or ended claim was retired",
    ),
];

/// Claim metric families.
#[derive(Clone, Debug, Default)]
struct ClaimMetrics {
    claims: Family<Labels, Gauge>,
    reconciles: Family<Labels, Counter>,
}

impl ClaimMetrics {
    fn register(&self, metrics: &crate::runtime::metrics::Metrics) {
        metrics.with_registry(|r| {
            register_table(
                r,
                Role::Controller,
                &CLAIM_TABLE,
                vec![
                    ("claims", Handle::new(self.claims.clone())),
                    ("claim_reconciles", Handle::new(self.reconciles.clone())),
                ],
            );
        });
        CLAIM_TABLE[1].seed(None, &self.reconciles);
    }

    fn count(&self, result: &str) {
        self.reconciles
            .get_or_create(&vec![("result", result.to_owned())])
            .inc();
    }
}

/// Drops the `buildkit_claims` series when the term ends, so a replica that
/// lost the Lease stops exporting a count only the leader keeps.
struct TermClaims(Family<Labels, Gauge>);

impl Drop for TermClaims {
    fn drop(&mut self) {
        self.0.clear();
    }
}

/// The leader's half of allocation; made by
/// [`AllocService::allocator`](super::AllocService::allocator).
pub struct Allocator {
    shared: Shared,
}

impl Allocator {
    pub(super) fn new(shared: Shared) -> Self {
        Self { shared }
    }
}

impl Component for Allocator {
    fn name(&self) -> &'static str {
        "allocator"
    }

    /// Bind and tidy claims while this replica leads, until `shutdown`.
    /// Fails when a term's work stops while the replica still leads.
    fn run(&self, ctx: AppContext, shutdown: CancellationToken) -> BoxResult {
        let shared = self.shared.clone();
        Box::pin(async move {
            let (backend, certs) = shared.sources(&ctx)?;
            let c = ctx.config();
            let namespace = ctx.namespace().to_owned();
            let binder = Arc::new(Binder::new(
                backend,
                (ctx.pools(), ctx.pods(), ctx.claims()),
                certs,
                BindConfig {
                    endpoint: c.edge.endpoint.clone(),
                    service_endpoint: c.edge_service_endpoint(&namespace),
                    server_name: c.edge.server_name.clone(),
                    allocation_domain: Some(c.edge.allocation_domain.clone())
                        .filter(|d| !d.is_empty()),
                    served: c.served_claim_namespaces(&namespace),
                    namespace,
                    registry_host: (!c.registry.host.is_empty()).then(|| c.registry.host.clone()),
                },
                shared.metrics(&ctx),
            ));
            let metrics = ClaimMetrics::default();
            metrics.register(ctx.metrics());
            let work: Arc<dyn LeaderWork> = Arc::new(Term {
                ctx: ctx.clone(),
                binder,
                metrics,
            });
            let synced = ctx.clone();
            supervise(
                ctx.leader(),
                shutdown,
                move || {
                    let s = synced.clone();
                    Box::pin(async move { s.wait_synced().await })
                },
                work,
            )
            .await?;
            info!(component = "allocator", "allocator stopped");
            Ok(())
        })
    }
}

#[derive(Clone)]
struct Term {
    ctx: AppContext,
    binder: Arc<Binder>,
    metrics: ClaimMetrics,
}

impl LeaderWork for Term {
    fn run(&self, term: u64) -> BoxFut {
        Box::pin(run_term(self.clone(), term))
    }
}

/// One leadership term: pool passes and tidying, until dropped.
async fn run_term(t: Term, term: u64) {
    let _claims = TermClaims(t.metrics.claims.clone());
    info!(term, component = "allocator", "allocator running");
    let claim_triggers = stream::unfold(t.ctx.claim_events(), |mut sub| async move {
        let ev = sub.recv().await?;
        Some((claim_trigger(&ev), sub))
    });
    let pod_triggers = stream::unfold(t.ctx.pod_events(), |mut sub| async move {
        let ev = sub.recv().await?;
        let trigger = match &ev {
            PodEvent::Apply(p) if offered(p) => p
                .metadata
                .labels
                .as_ref()
                .and_then(|l| l.get(LABEL_POOL))
                .map(|pool| {
                    Trigger::Pool((
                        p.metadata.namespace.clone().unwrap_or_default(),
                        pool.clone(),
                    ))
                }),
            PodEvent::Resync => Some(Trigger::All),
            _ => None,
        };
        Some((trigger, sub))
    });
    let triggers = stream::once(std::future::ready(Some(Trigger::All)))
        .chain(stream::select(claim_triggers, pod_triggers))
        .filter_map(std::future::ready);
    let binder = Arc::clone(&t.binder);
    let all = {
        let b = Arc::clone(&binder);
        move || b.pools_waiting()
    };
    let passes = schedule(triggers, all, move |key| {
        let b = Arc::clone(&binder);
        async move {
            let next = b.pass(&key).await;
            (key, next)
        }
    });
    tokio::join!(passes, tidy_loop(&t));
}

/// The pass a claim event calls for. A pending claim needs one when it is
/// new or its spec changed. Once the leader has written its status, further
/// events are the leader's own status writes and the API's `seen-at` and
/// `delivered` marks, and a pass for each would cost a pass per waiter per write.
/// A claim that ends or goes away changes what its owner holds, and so who
/// goes first under fair share, so it calls for a pass of its pool too.
pub(super) fn claim_trigger(ev: &ClaimEvent) -> Option<Trigger> {
    match ev {
        ClaimEvent::Apply(c) if claims::phase(c) == ClaimPhase::Pending => {
            let seen = c
                .status
                .as_ref()
                .is_some_and(|s| s.observed_generation == c.metadata.generation);
            (!seen).then(|| Trigger::Pool(claims::pool_key(c)))
        }
        ClaimEvent::Apply(c) if claims::phase(c).is_terminal() => {
            Some(Trigger::Pool(claims::pool_key(c)))
        }
        ClaimEvent::Delete(c) => Some(Trigger::Pool(claims::pool_key(c))),
        ClaimEvent::Resync => Some(Trigger::All),
        ClaimEvent::Apply(_) => None,
    }
}

/// A pod a waiting claim could take.
fn offered(p: &Pod) -> bool {
    ann(p, ANN_ALLOC) == Some(FREE) && is_ready(p) && p.metadata.deletion_timestamp.is_none()
}

async fn tidy_loop(t: &Term) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut sub = Some(t.ctx.claim_events());
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            ev = async { sub.as_mut()?.recv().await }, if sub.is_some() => match ev {
                Some(ClaimEvent::Delete(_)) => {}
                Some(_) => continue,
                None => {
                    sub = None;
                    continue;
                }
            },
        }
        tidy(t).await;
    }
}

/// One tidy pass over the claim store, leaving out the namespaces the claim
/// watch cannot list.
async fn tidy(t: &Term) {
    let (claims, pods) = listed_only(
        t.ctx.claims().state(),
        t.ctx.pods().state(),
        &t.ctx.unlisted_claim_namespaces(),
    );
    let outcomes = tidy_with(&t.binder, &claims, &pods, OffsetDateTime::now_utc()).await;
    for o in outcomes {
        t.metrics.count(o);
    }
    let mut per_phase: HashMap<&'static str, i64> = HashMap::new();
    for c in &claims {
        *per_phase.entry(claims::phase(c).as_str()).or_default() += 1;
    }
    for p in [
        ClaimPhase::Pending,
        ClaimPhase::Bound,
        ClaimPhase::Expired,
        ClaimPhase::Lost,
        ClaimPhase::Failed,
    ] {
        let n = per_phase.get(p.as_str()).copied().unwrap_or(0);
        t.metrics
            .claims
            .get_or_create(&vec![("phase", p.as_str().to_owned())])
            .set(n);
    }
}

/// One tidy pass over `claims` and `pods` at `now`; returns what it did, as
/// `buildkit_claim_reconciles_total` results.
pub(crate) async fn tidy_with(
    binder: &Binder,
    claims: &[Arc<BuildKitClaim>],
    pods: &[Arc<Pod>],
    now: OffsetDateTime,
) -> Vec<&'static str> {
    let cfg = binder.config();
    let (own, served) = (cfg.namespace.as_str(), &cfg.served);
    let mut done = Vec::new();
    for c in claims {
        if c.metadata.deletion_timestamp.is_some() {
            continue;
        }
        let outcome = match claims::phase(c) {
            _ if claims::abandoned(c, own, now) => abandon(binder, c).await,
            ClaimPhase::Pending if !claims::served(c, served) => refuse(binder, c).await,
            ClaimPhase::Bound if claims::expired(c, now) => {
                end(binder, c, ClaimPhase::Expired).await
            }
            ClaimPhase::Bound if claims::lost(c, pods.iter().map(AsRef::as_ref), now) => {
                end(binder, c, ClaimPhase::Lost).await
            }
            ClaimPhase::Bound => match idle_timeout(binder, c, own, now) {
                Some(t) => release_idle(binder, c, &t).await,
                None => None,
            },
            p if p.is_terminal() && claims::collectable(c, own, now) => collect(binder, c).await,
            _ => None,
        };
        done.extend(outcome);
    }
    done.extend(sweep(binder, claims, pods, served, own).await);
    done
}

/// `claims` and `pods` without the claims of `unlisted` namespaces and the
/// workers bound to them. Those claims are as last listed or absent, so their
/// workers would look like strays of a gone claim.
fn listed_only(
    mut claims: Vec<Arc<BuildKitClaim>>,
    mut pods: Vec<Arc<Pod>>,
    unlisted: &HashSet<String>,
) -> (Vec<Arc<BuildKitClaim>>, Vec<Arc<Pod>>) {
    if unlisted.is_empty() {
        return (claims, pods);
    }
    let listed = |ns: Option<&str>| !ns.is_some_and(|ns| unlisted.contains(ns));
    claims.retain(|c| listed(c.metadata.namespace.as_deref()));
    pods.retain(|p| {
        listed(
            // Workers bound by an earlier version name the claim only in the owner.
            ann(p, ANN_CLAIM)
                .and_then(|r| r.split_once('/'))
                .map(|(ns, _)| ns)
                .or_else(|| {
                    ann(p, ANN_OWNER)
                        .and_then(claims::claim_owner)
                        .map(|(ns, _)| ns)
                }),
        )
    });
    (claims, pods)
}

/// Fail a claim from a namespace the controller does not serve. The chart
/// grants Secret access only in served namespaces, so binding would end in a
/// 403 after allocating.
async fn refuse(b: &Binder, c: &BuildKitClaim) -> Option<&'static str> {
    if claims::reason(c) == Some(reason::NAMESPACE_NOT_SERVED) {
        return None;
    }
    let msg = format!(
        "namespace {} is not in the controller's claims.namespaces",
        c.metadata.namespace.as_deref().unwrap_or_default()
    );
    let s = claims::unbound_status(c, ClaimPhase::Failed, reason::NAMESPACE_NOT_SERVED, &msg);
    write_status(b, c, &s).await.then_some("failed")
}

/// End a bound claim: delete its Secret, then write `Expired` or `Lost`. A
/// lost claim is not rebound: the client may still hold the certificate for
/// the gone pod. The pod is retired by the reconciler at its expiry, or by
/// the sweep once the claim has ended.
async fn end(b: &Binder, c: &BuildKitClaim, phase: ClaimPhase) -> Option<&'static str> {
    let (ns, name, _) = names(c)?;
    if !finish(b, c, &claims::ended_status(c, phase)).await {
        return None;
    }
    info!(
        namespace = ns,
        claim = name,
        outcome = phase.as_str(),
        "claim ended"
    );
    Some(if phase == ClaimPhase::Lost {
        "lost"
    } else {
        "expired"
    })
}

/// The pool's `scaling.idleTimeout`, as written, when delivered claim `c` has
/// been [`claims::idle`] that long at `now`.
fn idle_timeout(b: &Binder, c: &BuildKitClaim, own: &str, now: OffsetDateTime) -> Option<String> {
    let t = &b.pool(&claims::pool_key(c))?.spec.scaling.idle_timeout;
    claims::idle(c, own, t.seconds().ok()?, now).then(|| t.as_str().to_owned())
}

/// End a delivered claim nobody has used through the edge for its pool's
/// `scaling.idleTimeout`, as [`end`] does at its TTL: the client went without
/// a `DELETE`. The sweep then retires its worker as [`ALLOCATION_IDLE`]. The
/// status write names the resourceVersion read, so an edge stamp written since
/// makes the API server refuse it.
async fn release_idle(b: &Binder, c: &BuildKitClaim, idle_timeout: &str) -> Option<&'static str> {
    let (ns, name, _) = names(c)?;
    if !finish(b, c, &claims::idle_status(c, idle_timeout)).await {
        return None;
    }
    let s = c.status.as_ref();
    info!(
        namespace = ns,
        claim = name,
        pool_namespace = %c.spec.pool_ref.namespace,
        pool = %c.spec.pool_ref.name,
        alloc_id = s.and_then(|s| s.allocation_id.as_deref()).unwrap_or_default(),
        pod = s.and_then(|s| s.pod.as_deref()).unwrap_or_default(),
        idle_timeout,
        last_active = claims::last_active(c).map(fmt_ts).unwrap_or_default(),
        "released an idle allocation"
    );
    Some("idle")
}

/// Delete the Secret of ending claim `c`, then write its final `status`.
async fn finish(
    b: &Binder,
    c: &BuildKitClaim,
    status: &buildkit_controller_api::claim::BuildKitClaimStatus,
) -> bool {
    let Some((ns, _, uid)) = names(c) else {
        return false;
    };
    if let Some(secret) = c.status.as_ref().and_then(|s| s.secret_name.as_deref()) {
        match b.backend().get_secret(ns, secret).await {
            Ok(Some(s)) if claims::controlled_by(&s, uid) => {
                // The uid precondition spares a Secret recreated since the read.
                let suid = s.metadata.uid.unwrap_or_default();
                if !matches!(
                    b.backend().delete_secret(ns, secret, &suid).await,
                    Write::Done | Write::Gone
                ) {
                    return false;
                }
            }
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    write_status(b, c, status).await
}

/// Delete a CI claim whose client is gone: never answered with its bound
/// allocation and not asked for since [`claims::ABANDON_AFTER`]. The sweep
/// then retires its pod, if it had one.
async fn abandon(b: &Binder, c: &BuildKitClaim) -> Option<&'static str> {
    let (ns, name, uid) = names(c)?;
    let rv = c.metadata.resource_version.as_deref()?;
    // Only the claim as read: a client that came back since wrote seen-at or
    // delivered, which moved the resourceVersion, and the delete is refused.
    match b.backend().delete_claim_at(ns, name, uid, rv).await {
        Write::Done => {
            info!(namespace = ns, claim = name, phase = ?claims::phase(c), "deleted an abandoned claim");
            Some("abandoned")
        }
        Write::Gone => None,
        w => {
            warn!(namespace = ns, claim = name, outcome = ?w, "deleting an abandoned claim");
            None
        }
    }
}

/// Delete a finished claim of a CI caller.
async fn collect(b: &Binder, c: &BuildKitClaim) -> Option<&'static str> {
    let (ns, name, uid) = names(c)?;
    match b.backend().delete_claim(ns, name, uid).await {
        Write::Done | Write::Gone => {
            info!(namespace = ns, claim = name, "deleted finished claim");
            Some("collected")
        }
        w => {
            warn!(namespace = ns, claim = name, outcome = ?w, "deleting finished claim");
            None
        }
    }
}

/// Retire the pods whose claim is gone or ended. The store has synced: the
/// term starts only after `wait_synced`, and the claim watch never reports a
/// partial list as synced.
async fn sweep(
    b: &Binder,
    claims: &[Arc<BuildKitClaim>],
    pods: &[Arc<Pod>],
    served: &HashSet<String>,
    own: &str,
) -> Vec<&'static str> {
    // Workers of claims released for idleness, by `P claim`.
    let idle: HashSet<String> = claims
        .iter()
        .filter(|c| claims::reason(c) == Some(reason::IDLE))
        .filter_map(|c| claims::claim_ref(c))
        .collect();
    let mut done = Vec::new();
    for p in claims::strays(pods.iter().map(AsRef::as_ref), claims, served, own) {
        let (Some(ns), Some(pool), Some(id)) = (
            p.metadata.namespace.as_deref(),
            p.metadata.labels.as_ref().and_then(|l| l.get(LABEL_POOL)),
            ann(p, ANN_ALLOC),
        ) else {
            continue;
        };
        let why = if ann(p, ANN_CLAIM).is_some_and(|r| idle.contains(r)) {
            ALLOCATION_IDLE
        } else {
            "released"
        };
        if b.retire(ns, pool, p, id, why).await {
            info!(namespace = ns, pool = %pool, pod = ?p.metadata.name, alloc_id = id, reason = why, "released worker of a gone or ended claim");
            done.push("released");
        }
    }
    done
}

async fn write_status(
    b: &Binder,
    c: &BuildKitClaim,
    s: &buildkit_controller_api::claim::BuildKitClaimStatus,
) -> bool {
    let Some((ns, name, _)) = names(c) else {
        return false;
    };
    let rv = c.metadata.resource_version.as_deref().unwrap_or_default();
    match b.backend().patch_claim_status(ns, name, rv, s).await {
        Write::Done => true,
        Write::Gone => false,
        w => {
            warn!(namespace = ns, claim = name, outcome = ?w, "writing claim status");
            false
        }
    }
}

fn names(c: &BuildKitClaim) -> Option<(&str, &str, &str)> {
    Some((
        c.metadata.namespace.as_deref()?,
        c.metadata.name.as_deref()?,
        c.metadata.uid.as_deref()?,
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use buildkit_controller_api::claim::{BuildKitClaim, BuildKitClaimStatus, ClaimPhase};
    use serde_json::json;

    use super::{ClaimEvent, Trigger, claim_trigger};

    fn claim(status: Option<(ClaimPhase, Option<i64>)>, generation: i64) -> ClaimEvent {
        let mut c = BuildKitClaim::new(
            "c",
            serde_json::from_value(json!({"poolRef": {"namespace": "p", "name": "x"}})).unwrap(),
        );
        c.metadata.generation = Some(generation);
        c.status = status.map(|(phase, observed)| BuildKitClaimStatus {
            phase: Some(phase),
            observed_generation: observed,
            ..BuildKitClaimStatus::default()
        });
        ClaimEvent::Apply(Arc::new(c))
    }

    /// A new or changed pending claim calls for a pass. The leader's own status
    /// writes and the API's annotations on a claim it has seen do not, or each
    /// waiter would cost a pass per write.
    #[test]
    fn only_new_or_changed_pending_claims_trigger_a_pass() {
        let pass = |ev: &ClaimEvent| matches!(claim_trigger(ev), Some(Trigger::Pool(_)));
        assert!(pass(&claim(None, 1)));
        assert!(pass(&claim(Some((ClaimPhase::Pending, Some(1))), 2)));
        assert!(!pass(&claim(Some((ClaimPhase::Pending, Some(1))), 1)));
        assert!(!pass(&claim(Some((ClaimPhase::Bound, Some(1))), 1)));
        // An ended or deleted claim may free its owner to bind another.
        assert!(pass(&claim(Some((ClaimPhase::Expired, Some(1))), 1)));
        let ClaimEvent::Apply(c) = claim(Some((ClaimPhase::Bound, Some(1))), 1) else {
            unreachable!()
        };
        assert!(pass(&ClaimEvent::Delete(c)));
        assert!(matches!(
            claim_trigger(&ClaimEvent::Resync),
            Some(Trigger::All)
        ));
    }
}

#[cfg(test)]
mod listed_tests {
    use super::*;
    use kube::api::ObjectMeta;

    fn claim(ns: &str) -> Arc<BuildKitClaim> {
        let mut c: BuildKitClaim = serde_json::from_value(serde_json::json!({
            "apiVersion": "buildkit.smrt-devops.net/v1alpha1",
            "kind": "BuildKitClaim",
            "metadata": {"name": "c", "namespace": ns},
            "spec": {"poolRef": {"namespace": "ci", "name": "p"}}
        }))
        .unwrap();
        c.metadata.namespace = Some(ns.into());
        Arc::new(c)
    }

    fn worker(claim_ns: Option<&str>) -> Arc<Pod> {
        Arc::new(Pod {
            metadata: ObjectMeta {
                name: Some(format!("w-{}", claim_ns.unwrap_or("none"))),
                annotations: claim_ns.map(|ns| [(ANN_CLAIM.to_owned(), format!("{ns}/c"))].into()),
                ..ObjectMeta::default()
            },
            ..Pod::default()
        })
    }

    /// A worker bound by an earlier version: the claim is named only in the owner.
    fn owned_worker(claim_ns: &str) -> Arc<Pod> {
        Arc::new(Pod {
            metadata: ObjectMeta {
                name: Some(format!("old-{claim_ns}")),
                annotations: Some(
                    [(ANN_OWNER.to_owned(), format!("claim:{claim_ns}/c#u1"))].into(),
                ),
                ..ObjectMeta::default()
            },
            ..Pod::default()
        })
    }

    /// A claims namespace given up (its `RoleBinding` was deleted) must not
    /// make its bound workers look like strays to release.
    #[test]
    fn an_unlisted_claim_namespace_is_left_alone() {
        let unlisted = HashSet::from(["b".to_owned()]);
        let (claims, pods) = listed_only(
            vec![claim("a"), claim("b")],
            vec![
                worker(Some("a")),
                worker(Some("b")),
                worker(None),
                owned_worker("a"),
                owned_worker("b"),
            ],
            &unlisted,
        );
        let ns: Vec<_> = claims
            .iter()
            .map(|c| c.metadata.namespace.clone().unwrap())
            .collect();
        assert_eq!(ns, ["a"]);
        let names: Vec<_> = pods
            .iter()
            .map(|p| p.metadata.name.clone().unwrap())
            .collect();
        assert_eq!(names, ["w-a", "w-none", "old-a"]);
    }
}

/// Registers the claim families in `metrics`, as a leader term does.
#[cfg(test)]
pub(crate) fn register_claims_for_test(metrics: &crate::runtime::metrics::Metrics) {
    ClaimMetrics::default().register(metrics);
}

#[cfg(test)]
mod metrics_tests {
    use super::{ClaimMetrics, TermClaims};
    use crate::runtime::metrics::Metrics;

    #[test]
    fn claims_clear_when_the_term_ends() {
        let m = Metrics::new();
        let claims = ClaimMetrics::default();
        claims.register(&m);
        {
            let _term = TermClaims(claims.claims.clone());
            claims
                .claims
                .get_or_create(&vec![("phase", "Bound".to_owned())])
                .set(3);
            claims.count("expired");
            assert!(m.render().contains("buildkit_claims{phase=\"Bound\"} 3"));
        }
        let text = m.render();
        assert!(!text.contains("buildkit_claims{"), "{text}");
        assert!(
            text.contains("buildkit_claim_reconciles_total{result=\"expired\"} 1"),
            "counters stay: {text}"
        );
    }
}
