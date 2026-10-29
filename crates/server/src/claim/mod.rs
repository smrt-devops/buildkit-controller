//! The claim controller, on the leader. It runs the allocator's pool passes
//! ([`Binder::pass`]) on claim and pod events, and every 2 s (and at once
//! after a claim is deleted) it tidies up: claims from namespaces it does not
//! serve fail, bound claims expire or lose their worker, finished claims of
//! CI callers are deleted after [`logic::FINISHED_TTL`], and pods whose claim
//! is gone or ended are retired.

pub mod logic;

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

use crate::alloc::backend::Write;
use crate::alloc::pods::{FREE, ann, is_ready};
use crate::alloc::{AllocService, Binder};
use crate::reconcile::driver::{Trigger, schedule};
use crate::render::{ANN_ALLOC, LABEL_POOL};
use crate::runtime::AppContext;
use crate::runtime::hooks::{BoxResult, Component, LeaderWork};
use crate::runtime::leader::{BoxFut, supervise};
use crate::runtime::stores::{ClaimEvent, PodEvent};
use logic::reason;

/// How often the controller tidies claims.
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

/// Kept for the binary's wiring, which adds it when `claims.enabled` is set.
/// The claim controller now always runs, inside the `alloc` component, because
/// every allocation is a claim; this component only waits for shutdown.
pub struct ClaimController;

impl ClaimController {
    /// The arguments are unused; see the type docs.
    #[must_use]
    pub fn new(_alloc: AllocService, _namespaces: impl IntoIterator<Item = String>) -> Self {
        Self
    }
}

impl Component for ClaimController {
    fn name(&self) -> &'static str {
        "claims"
    }

    fn run(&self, _ctx: AppContext, shutdown: CancellationToken) -> BoxResult {
        Box::pin(async move {
            shutdown.cancelled().await;
            Ok(())
        })
    }
}

/// Run the claim controller while this replica leads, until `shutdown`.
///
/// # Errors
///
/// When a term's work stops while the replica still leads.
pub async fn run(
    ctx: AppContext,
    binder: Arc<Binder>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
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
    let c = ctx.clone();
    supervise(
        ctx.leader(),
        shutdown,
        move || {
            let c = c.clone();
            Box::pin(async move { c.wait_synced().await })
        },
        work,
    )
    .await?;
    Ok(())
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
    info!(term, component = "claims", "claim controller running");
    let claim_triggers = stream::unfold(t.ctx.claim_events(), |mut sub| async move {
        let ev = sub.recv().await?;
        let trigger = match &ev {
            ClaimEvent::Apply(c) if logic::phase(c) == ClaimPhase::Pending => {
                Some(Trigger::Pool(logic::pool_key(c)))
            }
            ClaimEvent::Resync => Some(Trigger::All),
            _ => None,
        };
        Some((trigger, sub))
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
        *per_phase.entry(logic::phase(c).as_str()).or_default() += 1;
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
        let outcome = match logic::phase(c) {
            ClaimPhase::Pending if !logic::served(c, served) => refuse(binder, c).await,
            ClaimPhase::Bound if logic::expired(c, now) => {
                end(binder, c, ClaimPhase::Expired).await
            }
            ClaimPhase::Bound if logic::lost(c, pods.iter().map(AsRef::as_ref), now) => {
                end(binder, c, ClaimPhase::Lost).await
            }
            p if p.is_terminal() && logic::collectable(c, own, now) => collect(binder, c).await,
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
    if logic::reason(c) == Some(reason::NAMESPACE_NOT_SERVED) {
        return None;
    }
    let msg = format!(
        "namespace {} is not in the controller's claims.namespaces",
        c.metadata.namespace.as_deref().unwrap_or_default()
    );
    let s = logic::unbound_status(
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
            Ok(Some(s)) if logic::controlled_by(&s, uid) => {
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
    if !write_status(b, c, &logic::ended_status(c, phase)).await {
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
    for p in logic::strays(pods.iter().map(AsRef::as_ref), claims, served, own) {
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
