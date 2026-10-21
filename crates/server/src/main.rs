//! `buildkit-controller`: API, edge and reconciler in one binary.
//!
//! Startup runs config (exit 2 when invalid), telemetry, kube client, shared
//! stores, election, ops server, then the module hooks. On SIGTERM or SIGINT,
//! `/readyz` fails, wait `shutdown.delay`, stop the components (drain, bounded
//! by `shutdown.drainTimeout`), stop leader work, release the Lease, stop ops.

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use kube::Client;
use tokio::task::JoinSet;
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
    let client = Client::try_default()
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
    let ops_listener = ops::bind(ctx.config().listen.ops).await?;
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

    let comp_stop = CancellationToken::new();
    let mut components = JoinSet::new();
    for c in [&hooks.pki, &hooks.alloc, &hooks.http, &hooks.edge]
        .into_iter()
        .chain(&hooks.extra)
    {
        let c: Arc<dyn Component> = Arc::clone(c);
        let (ctx, stop) = (ctx.clone(), comp_stop.clone());
        components.spawn(async move { (c.name(), c.run(ctx, stop).await) });
    }

    let mut failed = None;
    let mut ops_done = false;
    let mut leader_done = false;
    tokio::select! {
        r = signal() => r?,
        Some(done) = components.join_next() => {
            let e = match done {
                Ok((name, Ok(()))) => anyhow::anyhow!("component {name} exited early"),
                Ok((name, Err(e))) => e.context(format!("component {name} failed")),
                Err(e) => anyhow::anyhow!("component task panicked: {e}"),
            };
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
    tokio::time::sleep(ctx.config().shutdown_delay()).await;
    comp_stop.cancel();
    let drain = ctx.config().drain_timeout();
    if tokio::time::timeout(drain, async {
        while components.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        warn!(drain_timeout_s = drain.as_secs(), "drain timed out");
        components.abort_all();
    }
    leader_stop.cancel();
    if !leader_done {
        let _ = leader_task.await;
    }
    elect_stop.cancel();
    let _ = elect_task.await;
    watches.cancel();
    ops_stop.cancel();
    if !ops_done {
        let _ = ops_task.await;
    }
    info!("exited");
    failed.map_or(Ok(()), Err)
}
