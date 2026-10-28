//! `buildkit-controller`: API, edge and reconciler in one binary.
//!
//! Startup runs config (exit 2 when invalid), telemetry, kube client, shared
//! stores, election, ops server, then the module hooks. On SIGTERM or SIGINT,
//! `/readyz` fails, leader work stops and the Lease is released, then after
//! `shutdown.delay` the components stop in stages: the API (queued and new
//! allocations get 503 `shutting_down`), the edge (open connections drain until
//! each ends on its own), the certificate loader. The drain is bounded by
//! `shutdown.drainTimeout`, or when unset by the longest connection lifetime.
//! The watches and the ops server stop last.

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use buildkit_controller::alloc::AllocService;
use buildkit_controller::certs::{Certs, InvalidServingSecret};
use buildkit_controller::claim::ClaimController;
use buildkit_controller::config::{Cli, ConfigError, Settings};
use buildkit_controller::edge::EdgeComponent;
use buildkit_controller::election::kube_api::KubeLeaseApi;
use buildkit_controller::election::{Elector, TokioClock};
use buildkit_controller::http::HttpComponent;
use buildkit_controller::reconcile::Reconciler;
use buildkit_controller::runtime::AppContext;
use buildkit_controller::runtime::hooks::{Component, Hooks, LeaderWork};
use buildkit_controller::runtime::leader::supervise;
use buildkit_controller::runtime::metrics::Metrics;
use buildkit_controller::runtime::readiness::Readiness;
use buildkit_controller::runtime::shutdown::{Stages, step_down};
use buildkit_controller::telemetry::{logs, ops};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let settings = match Settings::load(cli, |k| std::env::var(k).ok()) {
        Ok(s) => s,
        Err(e @ (ConfigError::Invalid(_) | ConfigError::Parse(_) | ConfigError::Read { .. })) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = logs::init(
        &settings.cli.log_level,
        settings.cli.log_format,
        &settings.cli.pod_name,
    ) {
        eprintln!("{e:#}");
        return ExitCode::from(2);
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!(error = %e, "building the tokio runtime");
            return ExitCode::FAILURE;
        }
    };
    let certs = Arc::new(Certs::new());
    let alloc = AllocService::new(certs.subscribe());
    // The claim controller is opt-in: config key `claims.enabled`.
    let extra: Vec<Arc<dyn Component>> = if settings.config.claims.enabled {
        vec![Arc::new(ClaimController::new(
            alloc.clone(),
            settings.config.claims.namespaces.clone(),
        ))]
    } else {
        Vec::new()
    };
    let hooks = Hooks {
        reconcile: |ctx| -> Arc<dyn LeaderWork> { Arc::new(Reconciler::new(ctx)) },
        pki: Arc::clone(&certs) as Arc<dyn Component>,
        alloc: Arc::new(alloc.clone()),
        http: Arc::new(HttpComponent::new(alloc, certs.subscribe())),
        edge: Arc::new(EdgeComponent::proxy(certs.subscribe())),
        extra,
    };
    match rt.block_on(run(settings, hooks)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = format!("{e:#}"), "fatal");
            if e.downcast_ref::<InvalidServingSecret>().is_some() {
                return ExitCode::from(2);
            }
            ExitCode::FAILURE
        }
    }
}

async fn signal() -> anyhow::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
    Ok(())
}

#[allow(clippy::too_many_lines, reason = "linear startup and ordered shutdown")]
async fn run(settings: Settings, hooks: Hooks) -> anyhow::Result<()> {
    info!(
        version = env!("CARGO_PKG_VERSION"),
        "buildkit-controller starting"
    );
    settings.config.warn_test_knobs();
    let timing = settings.config.election_timing();
    let metrics = Arc::new(Metrics::new());
    let readiness = Arc::new(Readiness::with_spec_checks());
    let client = buildkit_controller::runtime::kube_client()
        .await
        .context("building the kube client")?;

    let lease_api = Arc::new(KubeLeaseApi::new(
        client.clone(),
        &settings.cli.pod_namespace,
        &settings.config.leader_election.lease_name,
        {
            let m = Arc::clone(&metrics);
            Arc::new(move || m.watch_error("lease"))
        },
    ));
    let (elector, leader_rx) =
        Elector::new(lease_api, TokioClock, settings.cli.pod_name.clone(), timing);

    let watches = CancellationToken::new();
    let ctx = AppContext::build(
        client,
        settings,
        Arc::clone(&metrics),
        Arc::clone(&readiness),
        leader_rx.clone(),
        &watches,
    );

    let ops_stop = CancellationToken::new();
    let ops_listener = ops::bind(ctx.config().listen.ops)?;
    let mut ops_task = tokio::spawn(ops::serve(
        ops_listener,
        ops::OpsState {
            metrics: Arc::clone(&metrics),
            readiness: Arc::clone(&readiness),
        },
        ops_stop.clone(),
    ));
    tokio::spawn({
        let m = Arc::clone(&metrics);
        async move { m.follow_leader(leader_rx).await }
    });

    let elect_stop = CancellationToken::new();
    let elect_task = tokio::spawn(elector.run(elect_stop.clone()));

    let leader_stop = CancellationToken::new();
    let work = (hooks.reconcile)(&ctx);
    let mut leader_task = tokio::spawn({
        let ctx = ctx.clone();
        let rx = ctx.leader();
        let stop = leader_stop.clone();
        async move {
            let c = ctx.clone();
            supervise(
                rx,
                stop,
                move || {
                    let c = c.clone();
                    Box::pin(async move { c.wait_synced().await })
                },
                work,
            )
            .await
        }
    });

    // Stop order after the shutdown delay: the API first (no new allocations,
    // queued ones get 503), then the edge (open connections drain until each
    // ends), then the certificate loader. The Pod and Secret watches stop last.
    let mut stages = Stages::new(&["api", "edge", "pki"]);
    for c in [&hooks.alloc, &hooks.http] {
        spawn(&mut stages, API_STAGE, c, &ctx);
    }
    // Leader-gated components (the claim controller) stop with the reconciler,
    // before the Lease is released.
    let mut leader = Stages::new(&["leader"]);
    for c in &hooks.extra {
        spawn(&mut leader, 0, c, &ctx);
    }
    spawn(&mut stages, EDGE_STAGE, &hooks.edge, &ctx);
    spawn(&mut stages, PKI_STAGE, &hooks.pki, &ctx);

    let mut failed = None;
    let mut ops_done = false;
    let mut leader_done = false;
    tokio::select! {
        r = signal() => r?,
        e = stages.exited() => {
            error!(error = format!("{e:#}"), "component stopped, shutting down");
            failed = Some(e);
        }
        e = leader.exited() => {
            error!(error = format!("{e:#}"), "component stopped, shutting down");
            failed = Some(e);
        }
        r = &mut ops_task => {
            ops_done = true;
            let e = match r {
                Ok(Ok(())) => anyhow::anyhow!("ops server exited early"),
                Ok(Err(e)) => e.context("ops server failed"),
                Err(e) => anyhow::anyhow!("ops server panicked: {e}"),
            };
            error!(error = format!("{e:#}"), "ops server stopped, shutting down");
            failed = Some(e);
        }
        // A leader without a reconciler must not keep the Lease: shutting down releases it.
        r = &mut leader_task => {
            leader_done = true;
            let e = match r {
                Ok(Ok(())) => anyhow::anyhow!("leader supervisor exited early"),
                Ok(Err(e)) => anyhow::Error::new(e),
                Err(e) => anyhow::anyhow!("leader supervisor panicked: {e}"),
            };
            error!(error = format!("{e:#}"), "reconciler stopped, shutting down");
            failed = Some(e);
        }
    }
    info!("shutting down");
    readiness.set_shutting_down();
    // Leadership goes first, so another replica leads within seconds however
    // long this one drains. All leader work stops before the Lease is released.
    leader_stop.cancel();
    let reconciler = async {
        if !leader_done {
            let _ = leader_task.await;
        }
    };
    let bound = Instant::now() + ctx.config().request_timeout();
    step_down(reconciler, leader, bound, elect_stop, elect_task).await;
    info!("leadership released");
    tokio::time::sleep(ctx.config().shutdown_delay()).await;
    let budget = ctx.config().drain_budget();
    info!(drain_budget_s = budget.as_secs(), "draining");
    if !stages.stop(Instant::now() + budget).await {
        warn!(drain_budget_s = budget.as_secs(), "drain timed out");
    }
    watches.cancel();
    ops_stop.cancel();
    if !ops_done {
        let _ = ops_task.await;
    }
    info!("exited");
    failed.map_or(Ok(()), Err)
}

const API_STAGE: usize = 0;
const EDGE_STAGE: usize = 1;
const PKI_STAGE: usize = 2;

fn spawn(stages: &mut Stages, stage: usize, c: &Arc<dyn Component>, ctx: &AppContext) {
    let (c, ctx) = (Arc::clone(c), ctx.clone());
    stages.spawn(stage, c.name(), move |stop| async move {
        c.run(ctx, stop).await
    });
}
