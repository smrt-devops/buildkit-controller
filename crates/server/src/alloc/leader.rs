//! The [`Allocator`]: allocation's leader loop. While this replica leads it
//! runs the pool passes ([`Binder::pass`]) on claim and pod events, and every
//! 2 s (and at once after a claim is deleted) it tidies up: claims from
//! namespaces it does not serve fail, bound claims expire or lose their worker,
//! finished claims of CI callers are deleted after [`claims::FINISHED_TTL`],
//! and pods whose claim is gone or ended are retired.
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
use super::pods::{ann, is_ready};
use super::{BindConfig, Binder, Shared};
use crate::reconcile::driver::{Trigger, schedule};
use crate::render::{ANN_ALLOC, FREE, LABEL_POOL};
use crate::runtime::AppContext;
use crate::runtime::hooks::{BoxResult, Component, LeaderWork};
use crate::runtime::leader::{BoxFut, supervise};
use crate::runtime::stores::{ClaimEvent, PodEvent};

/// How often the allocator tidies claims.
const TICK: Duration = Duration::from_secs(2);

type Labels = Vec<(&'static str, String)>;

/// Claim metric families.
#[derive(Clone, Debug, Default)]
struct ClaimMetrics {
    claims: Family<Labels, Gauge>,
    reconciles: Family<Labels, Counter>,
}

impl ClaimMetrics {
    fn count(&self, result: &str) {
        self.reconciles
            .get_or_create(&vec![("result", result.to_owned())])
            .inc();
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
            ctx.metrics().with_registry(|r| {
                r.register(
                    "claims",
                    "BuildKitClaims in the store, by phase",
                    metrics.claims.clone(),
                );
                r.register(
                    "claim_reconciles",
                    "Claim reconcile outcomes",
                    metrics.reconciles.clone(),
                );
            });
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
fn claim_trigger(ev: &ClaimEvent) -> Option<Trigger> {
    match ev {
        ClaimEvent::Apply(c) if claims::phase(c) == ClaimPhase::Pending => {
            let seen = c
                .status
                .as_ref()
                .is_some_and(|s| s.observed_generation == c.metadata.generation);
            (!seen).then(|| Trigger::Pool(claims::pool_key(c)))
        }
        ClaimEvent::Resync => Some(Trigger::All),
        _ => None,
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

/// One tidy pass over the claim store.
async fn tidy(t: &Term) {
    let claims = t.ctx.claims().state();
    let pods = t.ctx.pods().state();
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
            p if p.is_terminal() && claims::collectable(c, own, now) => collect(binder, c).await,
            _ => None,
        };
        done.extend(outcome);
    }
    done.extend(sweep(binder, claims, pods, served, own).await);
    done
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
    let s = claims::unbound_status(
        c,
        ClaimPhase::Failed,
        reason::NAMESPACE_NOT_SERVED,
        &msg,
        None,
    );
    write_status(b, c, &s).await.then_some("failed")
}

/// End a bound claim: delete its Secret, then write `Expired` or `Lost`. A
/// lost claim is not rebound: the client may still hold the certificate for
/// the gone pod. The pod is retired by the reconciler at its expiry, or by
/// the sweep once the claim has ended.
async fn end(b: &Binder, c: &BuildKitClaim, phase: ClaimPhase) -> Option<&'static str> {
    let (ns, name, uid) = names(c)?;
    if let Some(secret) = c.status.as_ref().and_then(|s| s.secret_name.as_deref()) {
        match b.backend().get_secret(ns, secret).await {
            Ok(Some(s)) if claims::controlled_by(&s, uid) => {
                // The uid precondition spares a Secret recreated since the read.
                let suid = s.metadata.uid.unwrap_or_default();
                if !matches!(
                    b.backend().delete_secret(ns, secret, &suid).await,
                    Write::Done | Write::Gone
                ) {
                    return None;
                }
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    if !write_status(b, c, &claims::ended_status(c, phase)).await {
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
    let mut done = Vec::new();
    for p in claims::strays(pods.iter().map(AsRef::as_ref), claims, served, own) {
        let (Some(ns), Some(pool), Some(id)) = (
            p.metadata.namespace.as_deref(),
            p.metadata.labels.as_ref().and_then(|l| l.get(LABEL_POOL)),
            ann(p, ANN_ALLOC),
        ) else {
            continue;
        };
        if b.retire(ns, pool, p, id, "released").await {
            info!(namespace = ns, pool = %pool, pod = ?p.metadata.name, alloc_id = id, reason = "released", "released worker of a gone or ended claim");
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
        assert!(matches!(
            claim_trigger(&ClaimEvent::Resync),
            Some(Trigger::All)
        ));
    }
}
