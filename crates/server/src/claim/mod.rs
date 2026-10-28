//! `BuildKitClaim` controller. It runs only on the
//! leader and allocates through [`AllocService::allocate`] with the claim's
//! `Identity::Claim`, so the pool's access rules apply exactly as they do to
//! API callers.

pub mod logic;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use buildkit_controller_api::claim::{BuildKitClaim, BuildKitClaimStatus, ClaimPhase, secret_name};
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Pod, Secret};
use kube::api::{DeleteParams, Patch, PatchParams, PostParams, Preconditions};
use kube::runtime::reflector;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Api, Client};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use time::OffsetDateTime;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::alloc::AllocService;
use crate::config::Config;
use crate::runtime::AppContext;
use crate::runtime::hooks::{BoxResult, Component, LeaderWork};
use crate::runtime::leader::{BoxFut, supervise};
use crate::runtime::stores::scoped_watch;
use logic::{Binding, Disposition};

/// Claims allocating at once; the rest wait for the next tick.
pub const MAX_IN_FLIGHT: usize = 64;
/// How often the controller walks the claim store.
const TICK: Duration = Duration::from_secs(2);
/// Reason for a claim from a namespace outside `claims.namespaces`.
const REFUSED: &str = "NamespaceNotServed";

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

/// The claim controller as a runtime [`Component`]; it does work only while
/// this replica leads.
pub struct ClaimController {
    alloc: AllocService,
    served: Arc<HashSet<String>>,
}

impl ClaimController {
    /// A controller allocating through `alloc` for claims in `namespaces`
    /// (config `claims.namespaces`, where the chart grants Secret access).
    #[must_use]
    pub fn new(alloc: AllocService, namespaces: impl IntoIterator<Item = String>) -> Self {
        Self {
            alloc,
            served: Arc::new(namespaces.into_iter().collect()),
        }
    }
}

impl Component for ClaimController {
    fn name(&self) -> &'static str {
        "claims"
    }

    fn run(&self, ctx: AppContext, shutdown: CancellationToken) -> BoxResult {
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
            alloc: self.alloc.clone(),
            metrics,
            served: Arc::clone(&self.served),
        });
        Box::pin(async move {
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
        })
    }
}

#[derive(Clone)]
struct Term {
    ctx: AppContext,
    alloc: AllocService,
    metrics: ClaimMetrics,
    served: Arc<HashSet<String>>,
}

impl LeaderWork for Term {
    fn run(&self, term: u64) -> BoxFut {
        Box::pin(run_term(self.clone(), term))
    }
}

/// Namespaces whose claims are watched, `[]` = all. Cluster-wide the
/// controller reads every claim and refuses those outside `claims.namespaces`;
/// with `watchNamespaces` set it has no cluster-wide read and watches
/// `claims.namespaces` only.
fn claim_scope(cfg: &Config) -> &[String] {
    if cfg.watched_namespaces().is_empty() {
        &[]
    } else {
        &cfg.claims.namespaces
    }
}

/// One leadership term: watch claims and reconcile them until dropped.
async fn run_term(t: Term, term: u64) {
    let (ctx, metrics) = (&t.ctx, &t.metrics);
    let client = ctx.client();
    let (store, writer) = reflector::store::<BuildKitClaim>();
    let watch = reflector(
        writer,
        scoped_watch(
            client.clone(),
            claim_scope(ctx.config()),
            &watcher::Config::default(),
            store.clone(),
        )
        .default_backoff(),
    )
    .applied_objects()
    .for_each(|_| std::future::ready(()));
    tokio::pin!(watch);
    // The sweep releases workers of claims missing from the store, so it must
    // never run on an unsynced store.
    tokio::select! {
        () = &mut watch => return,
        r = store.wait_until_ready() => if r.is_err() { return },
    }
    info!(term, component = "claims", "claim controller running");
    let mut tasks: JoinSet<(String, Outcome)> = JoinSet::new();
    // At most one sweep in flight, off the select loop; dropped with the term.
    let mut sweeping: JoinSet<()> = JoinSet::new();
    let mut busy: HashSet<String> = HashSet::new();
    let mut retry: HashMap<String, (Instant, Duration)> = HashMap::new();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = &mut watch => return,
            Some(done) = tasks.join_next() => {
                let Ok((uid, outcome)) = done else { continue };
                busy.remove(&uid);
                metrics.count(outcome.as_str());
                if outcome == Outcome::Retry {
                    let prev = retry.get(&uid).map(|(_, d)| *d);
                    let d = logic::next_backoff(prev);
                    retry.insert(uid, (Instant::now() + d, d));
                } else {
                    retry.remove(&uid);
                }
            }
            _ = tick.tick() => {
                let claims = store.state();
                pass(&claims, &t, &mut tasks, &mut busy, &retry);
                let live: HashSet<String> = claims.iter().filter_map(|c| c.metadata.uid.clone()).collect();
                retry.retain(|uid, _| live.contains(uid));
                while sweeping.try_join_next().is_some() {}
                if sweeping.is_empty() {
                    sweep(&t, &live, &mut sweeping);
                }
            }
        }
    }
}

fn pass(
    claims: &[Arc<BuildKitClaim>],
    t: &Term,
    tasks: &mut JoinSet<(String, Outcome)>,
    busy: &mut HashSet<String>,
    retry: &HashMap<String, (Instant, Duration)>,
) {
    let (ctx, alloc, metrics) = (&t.ctx, &t.alloc, &t.metrics);
    let client = ctx.client();
    let pods = ctx.pods().state();
    let now = OffsetDateTime::now_utc();
    let mut per_phase: HashMap<&'static str, i64> = HashMap::new();
    for c in claims {
        let phase = logic::phase(c);
        *per_phase.entry(phase.as_str()).or_default() += 1;
        let Some(uid) = c.metadata.uid.clone() else {
            continue;
        };
        if c.metadata.deletion_timestamp.is_some() || busy.contains(&uid) {
            continue;
        }
        let job: Option<Job> = match phase {
            ClaimPhase::Pending if !logic::served(c, &t.served) => Some(Job::Refuse),
            ClaimPhase::Pending if busy.len() < MAX_IN_FLIGHT => {
                let due = retry.get(&uid).is_none_or(|(at, _)| *at <= Instant::now());
                due.then_some(Job::Bind)
            }
            ClaimPhase::Bound if logic::expired(c, now) => Some(Job::Expire),
            ClaimPhase::Bound if logic::lost(c, pods.iter().map(AsRef::as_ref), now) => {
                Some(Job::Lose)
            }
            _ => None,
        };
        if let Some(job) = job {
            busy.insert(uid.clone());
            let (c, client, alloc, pods) =
                (Arc::clone(c), client.clone(), alloc.clone(), ctx.pods());
            tasks.spawn(async move { (uid, job.run(&c, &client, &alloc, &pods).await) });
        }
    }
    for p in [
        ClaimPhase::Pending,
        ClaimPhase::Bound,
        ClaimPhase::Expired,
        ClaimPhase::Lost,
        ClaimPhase::Failed,
    ] {
        let n = per_phase.get(p.as_str()).copied().unwrap_or(0);
        metrics
            .claims
            .get_or_create(&vec![("phase", p.as_str().to_owned())])
            .set(n);
    }
}

/// Release workers whose claim is gone, in a task on `sweeping`.
fn sweep(t: &Term, live: &HashSet<String>, sweeping: &mut JoinSet<()>) {
    let pods = t.ctx.pods().state();
    let pools = t.ctx.pools();
    let found = logic::orphans(
        pods.iter().map(AsRef::as_ref),
        live,
        &t.served,
        |ns, name| {
            pools
                .get(&ObjectRef::new(name).within(ns))
                .and_then(|p| p.metadata.uid.clone())
        },
    );
    if found.is_empty() {
        return;
    }
    let (alloc, metrics) = (t.alloc.clone(), t.metrics.clone());
    sweeping.spawn(async move {
        for (who, id) in found {
            match alloc.release(&who, &id).await {
                Ok(()) => {
                    info!(owner = %who.owner(), alloc_id = %id, "released worker of deleted claim");
                    metrics.count("released");
                }
                Err(e) => {
                    warn!(owner = %who.owner(), alloc_id = %id, error = %e, "claim release failed");
                }
            }
        }
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Bound,
    Retry,
    Failed,
    Expired,
    Lost,
}

impl Outcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Bound => "bound",
            Self::Retry => "queued",
            Self::Failed => "failed",
            Self::Expired => "expired",
            Self::Lost => "lost",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Job {
    Bind,
    Refuse,
    Expire,
    Lose,
}

impl Job {
    async fn run(
        self,
        c: &BuildKitClaim,
        client: &Client,
        alloc: &AllocService,
        pods: &Store<Pod>,
    ) -> Outcome {
        let r = match self {
            Self::Bind => bind(c, client, alloc, pods).await,
            Self::Refuse => refuse(c, client).await,
            Self::Expire => finish(c, client, Outcome::Expired).await,
            Self::Lose => finish(c, client, Outcome::Lost).await,
        };
        r.unwrap_or_else(|e| {
            warn!(namespace = ?c.metadata.namespace, claim = ?c.metadata.name, error = %e, "claim reconcile failed");
            Outcome::Retry
        })
    }
}

async fn write_status(
    c: &BuildKitClaim,
    client: &Client,
    status: &BuildKitClaimStatus,
) -> kube::Result<()> {
    let (Some(ns), Some(name)) = (c.metadata.namespace.as_deref(), c.metadata.name.as_deref())
    else {
        return Ok(());
    };
    Api::<BuildKitClaim>::namespaced(client.clone(), ns)
        .patch_status(
            name,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "status": status })),
        )
        .await
        .map(|_| ())
}

/// Write an unbound status unless the claim already shows this phase and reason.
async fn unbound(
    c: &BuildKitClaim,
    client: &Client,
    phase: ClaimPhase,
    reason: &'static str,
    message: &str,
) -> kube::Result<()> {
    if logic::phase(c) == phase && logic::reason(c) == Some(reason) {
        return Ok(());
    }
    write_status(c, client, &logic::unbound_status(c, phase, reason, message)).await
}

#[allow(clippy::too_many_lines, reason = "one linear bind flow")]
async fn bind(
    c: &BuildKitClaim,
    client: &Client,
    alloc: &AllocService,
    pods: &Store<Pod>,
) -> kube::Result<Outcome> {
    let (Some(who), Some(ns), Some(name), Some(uid)) = (
        logic::identity(c),
        c.metadata.namespace.as_deref(),
        c.metadata.name.as_deref(),
        c.metadata.uid.as_deref(),
    ) else {
        return Ok(Outcome::Failed);
    };
    let secrets = Api::<Secret>::namespaced(client.clone(), ns);
    if let Some(s) = secrets.get_opt(&secret_name(name)).await? {
        let Some(b) = logic::adopt(&s, uid) else {
            let msg = format!(
                "secret {} exists and this claim does not own it",
                secret_name(name)
            );
            unbound(c, client, ClaimPhase::Failed, "SecretConflict", &msg).await?;
            return Ok(Outcome::Failed);
        };
        let owner = who.owner();
        if logic::worker_serves(pods.state().iter().map(AsRef::as_ref), &b, &owner) {
            write_status(c, client, &logic::bound_status(c, &b)).await?;
            return Ok(Outcome::Bound);
        }
        // The Secret outlived its allocation. The uid precondition spares a
        // Secret recreated since the read; then the create below conflicts and
        // the claim retries.
        let dp = DeleteParams {
            preconditions: Some(Preconditions {
                uid: s.metadata.uid.clone(),
                resource_version: None,
            }),
            ..DeleteParams::default()
        };
        match secrets.delete(&secret_name(name), &dp).await {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => {}
            Err(e) => return Err(e),
        }
        // Retire the worker too, in case the pod store lags behind it.
        if alloc.release(&who, &b.alloc_id).await.is_ok() {
            info!(namespace = ns, claim = name, alloc_id = %b.alloc_id, "released worker of stale claim secret");
        }
        info!(namespace = ns, claim = name, alloc_id = %b.alloc_id, "claim secret has no worker, allocating again");
    }
    let ttl = match c
        .spec
        .ttl
        .as_ref()
        .map(buildkit_controller_api::v1alpha1::Duration::seconds)
        .transpose()
    {
        Ok(t) => t,
        Err(e) => {
            unbound(
                c,
                client,
                ClaimPhase::Failed,
                "InvalidRequest",
                &e.to_string(),
            )
            .await?;
            return Ok(Outcome::Failed);
        }
    };
    let req = match logic::request(c, ttl) {
        Ok(r) => r,
        Err(e) => {
            unbound(
                c,
                client,
                ClaimPhase::Failed,
                "InvalidRequest",
                &e.to_string(),
            )
            .await?;
            return Ok(Outcome::Failed);
        }
    };
    if logic::phase(c) != ClaimPhase::Pending || c.status.is_none() {
        unbound(
            c,
            client,
            ClaimPhase::Pending,
            "Queued",
            "waiting for a worker",
        )
        .await?;
    }
    let a = match alloc.allocate(&who, &req).await {
        Ok(a) => a,
        Err(e) => {
            let msg = e.to_string();
            return Ok(match logic::dispose(c, &e, OffsetDateTime::now_utc()) {
                Disposition::Fail(reason) => {
                    unbound(c, client, ClaimPhase::Failed, reason, &msg).await?;
                    Outcome::Failed
                }
                Disposition::Retry(reason) => {
                    unbound(c, client, ClaimPhase::Pending, reason, &msg).await?;
                    Outcome::Retry
                }
            });
        }
    };
    let Some(secret) = logic::secret(c, a.response()) else {
        // Dropping `a` retires the pod (orphan guard).
        return Ok(Outcome::Failed);
    };
    if let Err(e) = secrets.create(&PostParams::default(), &secret).await {
        // The orphan guard retires the pod.
        drop(a);
        if let Some(msg) = logic::secret_rejected(&e) {
            unbound(c, client, ClaimPhase::Failed, "SecretRejected", &msg).await?;
            return Ok(Outcome::Failed);
        }
        return Err(e);
    }
    let binding = Binding::from(a.response());
    let _ = a.commit();
    write_status(c, client, &logic::bound_status(c, &binding)).await?;
    info!(namespace = ns, claim = name, alloc_id = %binding.alloc_id, "claim bound");
    Ok(Outcome::Bound)
}

/// Fail a claim from a namespace outside `claims.namespaces`. The chart grants
/// Secret access only there, so binding would end in a 403 after allocating.
async fn refuse(c: &BuildKitClaim, client: &Client) -> kube::Result<Outcome> {
    let msg = format!(
        "namespace {} is not in the controller's claims.namespaces",
        c.metadata.namespace.as_deref().unwrap_or_default()
    );
    unbound(c, client, ClaimPhase::Failed, REFUSED, &msg).await?;
    Ok(Outcome::Failed)
}

/// End a bound claim: delete its Secret and write the terminal status. A lost
/// claim is not rebound: the client may still hold the certificate for the
/// gone pod, and a silent swap under the same Secret would hide that.
async fn finish(c: &BuildKitClaim, client: &Client, outcome: Outcome) -> kube::Result<Outcome> {
    let (Some(ns), Some(name), Some(uid)) = (
        c.metadata.namespace.as_deref(),
        c.metadata.name.as_deref(),
        c.metadata.uid.as_deref(),
    ) else {
        return Ok(Outcome::Failed);
    };
    let secrets = Api::<Secret>::namespaced(client.clone(), ns);
    if let Some(s) = secrets.get_opt(&secret_name(name)).await?
        && logic::controlled_by(&s, uid)
    {
        // The uid precondition stops a delete of a Secret recreated since the read.
        let dp = DeleteParams {
            preconditions: Some(Preconditions {
                uid: s.metadata.uid.clone(),
                resource_version: None,
            }),
            ..DeleteParams::default()
        };
        match secrets.delete(&secret_name(name), &dp).await {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => {}
            Err(e) => return Err(e),
        }
    }
    let status = if outcome == Outcome::Lost {
        logic::lost_status(c)
    } else {
        logic::expired_status(c)
    };
    write_status(c, client, &status).await?;
    info!(
        namespace = ns,
        claim = name,
        outcome = outcome.as_str(),
        "claim ended"
    );
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_labels_are_fixed() {
        let all = [
            Outcome::Bound,
            Outcome::Retry,
            Outcome::Failed,
            Outcome::Expired,
            Outcome::Lost,
        ];
        assert_eq!(
            all.map(Outcome::as_str),
            ["bound", "queued", "failed", "expired", "lost"]
        );
    }

    #[test]
    fn claims_are_watched_cluster_wide_only_when_pools_are() {
        let min = "edge:\n  endpoint: tcp://bk.example.com:8444\n  serverName: bk.example.com\nclaims:\n  enabled: true\n  namespaces: [apps]\n";
        let cfg = Config::from_yaml(min).unwrap();
        assert!(claim_scope(&cfg).is_empty());
        let cfg = Config::from_yaml(&format!("{min}watchNamespaces: [ci-a, ci-b]\n")).unwrap();
        assert_eq!(claim_scope(&cfg), ["apps"]);
    }
}
