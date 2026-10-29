//! `buildkit-controller`: API, edge and reconciler in one binary.
//!
//! `buildkit-controller cache` runs the cache registry role instead (see
//! `registry`): no kube client, no election, only its own flags.
//!
//! Startup runs config (exit 2 when invalid), telemetry, kube client, shared
//! stores, election, ops server, then the module hooks. On SIGTERM or SIGINT,
//! `/readyz` fails, leader work (the reconciler and the allocator) stops and
//! the Lease is released, then after `shutdown.delay` the components stop in
//! stages: the API (queued allocations are claims and outlive the replica),
//! the edge (open connections drain until each ends on its own), the
//! certificate loader. The delay and the drain
//! budget count from the signal. The drain is bounded by
//! `shutdown.drainTimeout`, or when unset by the longest connection lifetime.
//! The watches and the ops server stop last. A second SIGTERM or SIGINT during
//! shutdown exits at once.

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use futures::FutureExt;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use buildkit_controller::alloc::AllocService;
use buildkit_controller::certs::{Certs, InvalidServingSecret};
use buildkit_controller::config::{Cli, ConfigError, Settings};
use buildkit_controller::edge::EdgeComponent;
use buildkit_controller::election::kube_api::KubeLeaseApi;
use buildkit_controller::election::{Elector, TokioClock};
use buildkit_controller::http::HttpComponent;
use buildkit_controller::reconcile::Reconciler;
use buildkit_controller::registry::CacheArgs;
use buildkit_controller::runtime::AppContext;
use buildkit_controller::runtime::hooks::{Component, Hooks, LeaderWork};
use buildkit_controller::runtime::leader::supervise;
use buildkit_controller::runtime::metrics::Metrics;
use buildkit_controller::runtime::readiness::Readiness;
use buildkit_controller::runtime::stop::Shutdown;
use buildkit_controller::telemetry::{logs, ops};

fn main() -> ExitCode {
    if std::env::args_os().nth(1).is_some_and(|a| a == "cache") {
        return cache_main();
    }
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
    let hooks = Hooks {
        reconcile: |ctx| -> Arc<dyn LeaderWork> { Arc::new(Reconciler::new(ctx)) },
        pki: Arc::clone(&certs) as Arc<dyn Component>,
        alloc: Arc::new(alloc.clone()),
        allocator: Arc::new(alloc.allocator()),
        http: Arc::new(HttpComponent::new(alloc, certs.subscribe())),
        edge: Arc::new(EdgeComponent::proxy(certs.subscribe())),
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

/// The `cache` role. The first SIGTERM or SIGINT drains, a second exits at once.
fn cache_main() -> ExitCode {
    let args = CacheArgs::parse_from(std::env::args_os().skip(1));
    let replica = std::env::var("POD_NAME").unwrap_or_else(|_| "cache".to_owned());
    if let Err(e) = logs::init(&args.log_level, args.log_format, &replica) {
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
    let result = rt.block_on(async {
        let mut signals = Signals::new().context("installing signal handlers")?;
        let stop = CancellationToken::new();
        let mut task = tokio::spawn(buildkit_controller::registry::run(args, stop.clone()));
        tokio::select! {
            r = &mut task => return r?,
            code = signals.recv() => info!(signal_exit_code = code, "signal received"),
        }
        signals.exit_after(1);
        stop.cancel();
        task.await?
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = format!("{e:#}"), "fatal");
            ExitCode::FAILURE
        }
    }
}

/// SIGTERM and SIGINT.
struct Signals {
    term: Signal,
    int: Signal,
}

impl Signals {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
        })
    }

    /// The next signal, as the exit code a process killed by it reports.
    async fn recv(&mut self) -> i32 {
        tokio::select! {
            _ = self.term.recv() => 128 + 15,
            _ = self.int.recv() => 128 + 2,
        }
    }

    /// Exit the process at once on the `n`th signal from now, so an operator
    /// can cut a long drain short.
    fn exit_after(mut self, n: usize) {
        tokio::spawn(async move {
            let mut code = 1;
            for _ in 0..n {
                code = self.recv().await;
            }
            warn!("signal during shutdown, exiting now");
            std::process::exit(code);
        });
    }
}

#[allow(clippy::too_many_lines, reason = "linear startup and ordered shutdown")]
async fn run(settings: Settings, hooks: Hooks) -> anyhow::Result<()> {
    info!(
        version = env!("CARGO_PKG_VERSION"),
        "buildkit-controller starting"
    );
    let mut signals = Signals::new().context("installing signal handlers")?;
    settings.config.warn_test_knobs();
    settings.config.warn_unchecked_cluster_ranges();
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

    // The allocator stops with the reconciler, before the Lease is released.
    // After the shutdown delay the API stops (long polls answer 202; queued
    // claims stay in the API server), then the edge (open connections drain
    // until each ends), then the certificate loader. The watches stop last.
    let (mut leader, mut stages) = hooks.start(|c, stop| c.run(ctx.clone(), stop));

    let mut failed = None;
    let mut ops_done = false;
    let mut leader_done = false;
    // Signals left before the process exits at once: the first starts the
    // drain, the next one ends it.
    let mut signals_left = 2;
    tokio::select! {
        code = signals.recv() => {
            info!(signal_exit_code = code, "signal received");
            signals_left = 1;
        }
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
    let started = Instant::now();
    signals.exit_after(signals_left);
    let config = ctx.config();
    Shutdown {
        started,
        readiness,
        leader_stop,
        reconciler: async move {
            if !leader_done {
                let _ = leader_task.await;
            }
        }
        .boxed(),
        leader,
        request_timeout: config.request_timeout(),
        elect_stop,
        elector: elect_task,
        delay: config.shutdown_delay(),
        drain_budget: config.drain_budget(),
        stages,
        watches,
        ops_stop,
        ops: (!ops_done).then(|| {
            async move {
                let _ = ops_task.await;
            }
            .boxed()
        }),
    }
    .run()
    .await;
    failed.map_or(Ok(()), Err)
}
