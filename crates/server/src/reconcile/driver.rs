//! The leader's reconcile loop. One task per leadership term that runs pool
//! passes on Pool events, Pod events, CA Secret changes and requeue
//! deadlines, and on claim events (a pool is sized for its pending claims).
//! Nothing is polled. Every pass runs
//! inside the term's future, so dropping it (leadership lost) stops them all.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use buildkit_controller_api::v1alpha1::BuildKitPool;
use futures::stream::{FuturesUnordered, StreamExt};
use futures::{Stream, stream};
use k8s_openapi::jiff::{SignedDuration, Timestamp};
use kube::runtime::reflector::{self, Store};
use kube::runtime::watcher;
use kube::{Client, ResourceExt};
use tokio::time::Instant;
use tracing::{Instrument, debug, info, info_span, warn};

use super::exec::CaGuard;
use super::metrics::{PodCounts, ReconcileMetrics, TermGauges};
use super::ops::{KubeClient, Timed};
use super::pass::{self, CaView, PoolMemory, Settings};
use super::plan::{PlanConfig, REAPPLY_AFTER};
use super::pods::WorkerPod;
use crate::claim::logic as claims;
use crate::config::ElectionTiming;
use crate::render::LABEL_POOL;
use crate::runtime::AppContext;
use crate::runtime::hooks::LeaderWork;
use crate::runtime::leader::BoxFut;
use crate::runtime::stores::{ClaimEvent, NamespaceReport, PodEvent, tolerant_watch};

/// `(namespace, name)` of a pool.
pub type PoolKey = (String, String);

/// Why passes are due.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// One pool changed (its own event or one of its pods').
    Pool(PoolKey),
    /// Every pool (CA change, pod relist or lag).
    All,
}

/// Runs passes for due pools until `triggers` ends; never two passes of one
/// pool at once (a trigger during a pass reruns it afterwards). `run`
/// returns the pool's next requeue (`None`: wait for events).
pub async fn schedule<T, A, F, Fut>(triggers: T, all: A, mut run: F)
where
    T: Stream<Item = Trigger>,
    A: Fn() -> Vec<PoolKey>,
    F: FnMut(PoolKey) -> Fut,
    Fut: Future<Output = (PoolKey, Option<Duration>)>,
{
    let mut triggers = std::pin::pin!(triggers);
    let mut inflight = FuturesUnordered::new();
    let mut running: BTreeSet<PoolKey> = BTreeSet::new();
    let mut rerun: BTreeSet<PoolKey> = BTreeSet::new();
    let mut dirty: BTreeSet<PoolKey> = BTreeSet::new();
    let mut deadlines: HashMap<PoolKey, Instant> = HashMap::new();
    loop {
        for k in std::mem::take(&mut dirty) {
            if running.contains(&k) {
                rerun.insert(k);
            } else {
                deadlines.remove(&k);
                running.insert(k.clone());
                inflight.push(run(k));
            }
        }
        let next = deadlines.values().min().copied();
        tokio::select! {
            t = triggers.next() => match t {
                None => return,
                Some(Trigger::Pool(k)) => { dirty.insert(k); }
                Some(Trigger::All) => dirty.extend(all()),
            },
            Some((k, requeue)) = inflight.next(), if !inflight.is_empty() => {
                running.remove(&k);
                if let Some(d) = requeue {
                    deadlines.insert(k.clone(), Instant::now() + d);
                }
                if rerun.remove(&k) {
                    dirty.insert(k);
                }
            }
            () = sleep_until(next), if next.is_some() => {
                let now = Instant::now();
                let due: Vec<PoolKey> =
                    deadlines.iter().filter(|(_, t)| **t <= now).map(|(k, _)| k.clone()).collect();
                for k in due {
                    deadlines.remove(&k);
                    dirty.insert(k);
                }
            }
        }
    }
}

async fn sleep_until(t: Option<Instant>) {
    match t {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// The [`LeaderWork`] the runtime starts per leadership term.
pub struct Reconciler {
    ctx: AppContext,
    metrics: ReconcileMetrics,
}

impl Reconciler {
    /// Registers the metrics; build once.
    #[must_use]
    pub fn new(ctx: &AppContext) -> Self {
        Self {
            metrics: ReconcileMetrics::register(ctx.metrics()),
            ctx: ctx.clone(),
        }
    }
}

impl LeaderWork for Reconciler {
    fn run(&self, term: u64) -> BoxFut {
        let (ctx, metrics) = (self.ctx.clone(), self.metrics.clone());
        Box::pin(run_term(ctx, metrics).instrument(info_span!("reconcile", term)))
    }
}

fn settings(ctx: &AppContext) -> Settings {
    let c = ctx.config();
    let mut plan = PlanConfig {
        endpoint: c.edge.endpoint.clone(),
        server_name: c.edge.server_name.clone(),
        ..PlanConfig::default()
    };
    if let Some((_, min_left)) = c.test_worker_thresholds() {
        plan.worker_cert_min_left = SignedDuration::from_secs(min_left.whole_seconds());
    }
    Settings {
        plan,
        worker_cert_validity: c.leaf_validity().worker_serving,
        render: c.render_config(ctx.namespace()),
        ca_secret_name: c.pki.ca_secret_name.clone(),
        replica: ctx.replica().to_owned(),
        issuers: c.auth.oidc.iter().map(|o| o.url.clone()).collect(),
    }
}

/// Everything a term's passes share.
struct Term {
    ctx: AppContext,
    ops: Timed<KubeClient>,
    guard: CaGuard,
    settings: Settings,
    pools: Store<BuildKitPool>,
    ca: Mutex<CaView>,
    memory: Mutex<HashMap<PoolKey, PoolMemory>>,
    metrics: ReconcileMetrics,
    /// Namespaces whose claims are served.
    served: HashSet<String>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

async fn run_term(ctx: AppContext, metrics: ReconcileMetrics) {
    info!("reconciler started");
    let _gauges = TermGauges(metrics.clone());
    let client: Client = ctx.client();
    // The leader's own Pool watch. A pass reads the pool from it. A namespace
    // that cannot be listed is left out after a while rather than holding
    // every pool: a pool missing from the store is only forgotten, never acted on.
    let (pools, writer) = reflector::store();
    let pool_triggers = {
        let ctx = ctx.clone();
        reflector::reflector(
            writer,
            tolerant_watch(
                client.clone(),
                ctx.config().watched_namespaces(),
                &watcher::Config::default(),
                pools.clone(),
                NamespaceReport::log("controller"),
            ),
        )
        .filter_map(move |ev| {
            let t = match ev {
                Ok(ev) => pool_trigger(&ev),
                Err(e) => {
                    warn!(error = %e, "pool watch error");
                    ctx.metrics().watch_error("controller");
                    None
                }
            };
            std::future::ready(t)
        })
    };
    let pod_triggers = stream::unfold(ctx.pod_events(), |mut sub| async move {
        let ev = sub.recv().await?;
        let t = match &ev {
            PodEvent::Apply(p) | PodEvent::Delete(p) => p
                .labels()
                .get(LABEL_POOL)
                .map(|pool| Trigger::Pool((p.namespace().unwrap_or_default(), pool.clone()))),
            PodEvent::Resync => Some(Trigger::All),
        };
        Some((t, sub))
    })
    .filter_map(std::future::ready);
    // A claim that waits, binds or goes changes its pool's demand.
    let claim_triggers = stream::unfold(ctx.claim_events(), |mut sub| async move {
        let ev = sub.recv().await?;
        let t = match &ev {
            ClaimEvent::Apply(c) | ClaimEvent::Delete(c) => Trigger::Pool(claims::pool_key(c)),
            ClaimEvent::Resync => Trigger::All,
        };
        Some((t, sub))
    });

    // One receiver. The view starts at the version it marks seen, so no CA
    // change between the two can be missed.
    let mut ca_rx = ctx.ca_secret();
    let term = Arc::new(Term {
        ops: Timed::new(
            KubeClient(client),
            call_timeout(&ctx.config().election_timing()),
        ),
        guard: CaGuard::default(),
        settings: settings(&ctx),
        pools: pools.clone(),
        ca: Mutex::new(CaView {
            epoch: 0,
            secret: ca_rx.borrow_and_update().secret.clone(),
        }),
        memory: Mutex::new(HashMap::new()),
        metrics,
        served: ctx.config().served_claim_namespaces(ctx.namespace()),
        ctx: ctx.clone(),
    });
    let ca_triggers = stream::unfold((ca_rx, Arc::clone(&term)), |(mut rx, term)| async move {
        rx.changed().await.ok()?;
        let secret = rx.borrow_and_update().secret.clone();
        {
            let mut ca = lock(&term.ca);
            ca.epoch += 1;
            ca.secret = secret;
        }
        Some((Trigger::All, (rx, term)))
    });

    // The CA pass runs once at leadership start, then with every `All`.
    let triggers = stream::once(std::future::ready(Trigger::Pool(ca_key()))).chain(stream::select(
        stream::select(pool_triggers, pod_triggers),
        stream::select(ca_triggers, claim_triggers),
    ));
    let all = {
        let term = Arc::clone(&term);
        move || all_keys(&term.pools.state(), lock(&term.memory).keys())
    };
    schedule(triggers, all, |key| {
        let term = Arc::clone(&term);
        async move {
            let requeue = pass_once(&term, &key).await;
            (key, requeue)
        }
    })
    .await;
    info!("reconciler stopped: watches ended");
}

async fn pass_once(term: &Term, key: &PoolKey) -> Option<Duration> {
    if *key == ca_key() {
        return ca_pass_once(term).await;
    }
    let Some(pool) = term
        .pools
        .state()
        .into_iter()
        .find(|p| p.namespace().as_deref() == Some(key.0.as_str()) && p.name_any() == key.1)
    else {
        lock(&term.memory).remove(key);
        term.metrics.forget(&key.0, &key.1);
        debug!(namespace = %key.0, pool = %key.1, "pool gone");
        return None;
    };
    let span = info_span!("reconcile", namespace = %key.0, pool = %key.1);
    async {
        let started = std::time::Instant::now();
        let mut mem = lock(&term.memory).remove(key).unwrap_or_default();
        let ca = lock(&term.ca).clone();
        let pods = term.ctx.pods().state();
        let waiting = claims::queues(&term.ctx.claims().state(), &term.served)
            .get(key)
            .map_or(0, |q| i32::try_from(q.len()).unwrap_or(i32::MAX));
        // Boxed: the pass future (pod spawns and volume writes) is too large for the stack.
        let out = Box::pin(pass::run(
            &term.ops,
            &term.guard,
            &term.settings,
            &pool,
            &pods,
            &ca,
            &mut mem,
            waiting,
            Timestamp::now(),
        ))
        .await;
        lock(&term.memory).insert(key.clone(), mem);
        let workers: Vec<WorkerPod> = pods
            .iter()
            .filter_map(|p| WorkerPod::from_pod(p, &key.0, &key.1, &pool.uid().unwrap_or_default()))
            .collect();
        let counts = PodCounts::of(&pool, &workers);
        let elapsed = started.elapsed();
        term.metrics.pass(
            &key.0,
            &key.1,
            out.report.status_sent.as_ref().or(pool.status.as_ref()),
            counts,
            &out.report,
            elapsed.as_secs_f64(),
        );
        let duration_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        if out.report.errors.is_empty() && out.report.trace.is_empty() {
            debug!(duration_ms, requeue_s = ?out.requeue.map(|d| d.as_secs_f64()), "pass");
        } else if out.report.errors.is_empty() {
            info!(duration_ms, actions = ?out.report.trace, requeue_s = ?out.requeue.map(|d| d.as_secs_f64()), "pass");
        } else {
            warn!(duration_ms, errors = ?out.report.errors, actions = ?out.report.trace, "pass failed");
        }
        out.requeue
    }
    .instrument(span)
    .await
}

/// The scheduler key of the pool-independent CA pass; no pool has an empty
/// namespace.
pub(super) fn ca_key() -> PoolKey {
    (String::new(), String::new())
}

/// [`pass::ca_pass`]; retried every 10 s while its writes fail, and run again
/// every [`REAPPLY_AFTER`] so a deleted or edited CA bundle `ConfigMap` is
/// restored without a pool or a CA change.
async fn ca_pass_once(term: &Term) -> Option<Duration> {
    let ca = lock(&term.ca).clone();
    let report = pass::ca_pass(
        &term.ops,
        &term.guard,
        &term.settings,
        &ca,
        Timestamp::now(),
    )
    .instrument(info_span!("reconcile_ca"))
    .await;
    term.metrics.record_ca(&report);
    if report.errors.is_empty() {
        if !report.trace.is_empty() {
            info!(actions = ?report.trace, "CA pass");
        }
        REAPPLY_AFTER.try_into().ok()
    } else {
        warn!(errors = ?report.errors, actions = ?report.trace, "CA pass failed");
        Some(Duration::from_secs(10))
    }
}

/// The trigger for one Pool watch event. The reflector buffers `InitApply`
/// objects and publishes them to the store only at `InitDone`, so a pass
/// triggered by `InitApply` finds no pool and is dropped; `InitDone` (start
/// and every relist) therefore requeues every pool instead.
pub(super) fn pool_trigger(ev: &watcher::Event<BuildKitPool>) -> Option<Trigger> {
    match ev {
        watcher::Event::Apply(p) | watcher::Event::Delete(p) => Some(Trigger::Pool((
            p.namespace().unwrap_or_default(),
            p.name_any(),
        ))),
        watcher::Event::InitDone => Some(Trigger::All),
        watcher::Event::Init | watcher::Event::InitApply(_) => None,
    }
}

/// Bound of every reconciler API call: `kube.requestTimeout`, never more than
/// the fence margin, so a write sent before the self-fence is abandoned before
/// another replica can take the Lease.
pub(super) fn call_timeout(t: &ElectionTiming) -> Duration {
    t.request_timeout.min(t.fence_margin)
}

/// Every key an `All` trigger runs: the pools in the leader's store, the CA
/// pass, and every pool the term still remembers. A pool deleted while the
/// watch was relisting produces no `Delete` event, so only its remembered key
/// brings it to the pass that forgets its memory and gauges.
pub(super) fn all_keys<'a>(
    pools: &[Arc<BuildKitPool>],
    remembered: impl Iterator<Item = &'a PoolKey>,
) -> Vec<PoolKey> {
    let mut keys: BTreeSet<PoolKey> = pools
        .iter()
        .map(|p| (p.namespace().unwrap_or_default(), p.name_any()))
        .collect();
    keys.extend(remembered.cloned());
    keys.insert(ca_key());
    keys.into_iter().collect()
}
