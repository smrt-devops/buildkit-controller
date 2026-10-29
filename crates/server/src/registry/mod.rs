//! The `registry` role: the built-in cache registry behind `BuildKit`'s registry cache.
//!
//! It serves the part of the OCI distribution API that `--cache-to
//! type=registry` and `--cache-from type=registry` use ([`api`]), over mTLS
//! with the internal CA ([`listener`]), from one directory ([`store`]).
//! Every request is checked against the caller's [`Grant`], which an
//! [`Authorizer`] derives from the verified worker identity. A periodic
//! collection keeps each pool and the whole volume within budget ([`gc`]).
//! `/metrics`, `/healthz` and `/readyz` are on the ops listener, as in the
//! controller.

mod api;
mod auth;
mod config;
mod gc;
mod listener;
mod metrics;
mod pods;
mod store;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub use api::{MAX_MANIFEST, Registry, handle};
pub use auth::{Action, Authorizer, Grant, PeerError, StaticGrants, peer_identity};
pub use config::{RegistryArgs, parse_duration, parse_size};
pub use gc::{Budgets, FixedBudgets, Reason};
pub use listener::{Limits, server_config};
pub use metrics::RegistryMetrics;
pub use pods::{PodGrants, PoolBudgets, grant_of};
pub use store::{Digest, Reference, Stats, Store, StoreError, StoreLimits};

use crate::runtime::metrics::Metrics;
use crate::runtime::readiness::Readiness;
use crate::runtime::stores::StoreFreshness;
use crate::telemetry::ops;

/// Upload sessions idle this long are deleted; also the grace period of a
/// fresh blob link in garbage collection.
pub const UPLOAD_TTL: Duration = Duration::from_hours(1);

/// Runs the role until `shutdown`, then drains.
pub async fn run(args: RegistryArgs, shutdown: CancellationToken) -> anyhow::Result<()> {
    info!(
        version = env!("CARGO_PKG_VERSION"),
        "cache registry starting"
    );
    let metrics = Arc::new(Metrics::new());
    let readiness = Arc::new(Readiness::default());
    let ready = readiness.register("registry_listener");
    let ops_stop = CancellationToken::new();
    let ops_task = tokio::spawn(ops::serve(
        ops::bind(args.ops_listen)?,
        ops::OpsState {
            metrics: Arc::clone(&metrics),
            readiness: Arc::clone(&readiness),
        },
        ops_stop.clone(),
    ));

    let stop = CancellationToken::new();
    let (auth, budgets) = authorizer(&args, &metrics, &readiness, &stop).await?;
    let (files, tls) = listener::load(&args.tls_dir)?;
    let store = {
        let dir = args.data_dir.clone();
        let limits = StoreLimits {
            max_blob: args.max_blob_size,
            max_upload_bytes: args.max_upload_size,
        };
        tokio::task::spawn_blocking(move || Store::open(&dir, limits))
            .await?
            .with_context(|| format!("opening the store at {}", args.data_dir.display()))?
    };
    let reg = Arc::new(Registry {
        store,
        auth,
        metrics: RegistryMetrics::register(&metrics),
        manifest_puts: tokio::sync::Semaphore::new(api::MANIFEST_PUTS),
    });

    let gc_task = tokio::spawn(collect(
        Arc::clone(&reg),
        budgets,
        args.gc_interval,
        stop.clone(),
    ));
    let (tx, rx) = watch::channel(tls);
    let follow = tokio::spawn(listener::follow(
        args.tls_dir.clone(),
        tx,
        files,
        stop.clone(),
    ));
    let tcp = crate::runtime::listen::bind(args.listen)
        .with_context(|| format!("binding {}", args.listen))?;
    ready.set();
    info!(addr = %args.listen, data_dir = %args.data_dir.display(), "cache registry listening");
    tokio::spawn({
        let (r, s) = (Arc::clone(&readiness), shutdown.clone());
        async move {
            s.cancelled().await;
            r.set_shutting_down();
        }
    });
    listener::serve(tcp, rx, reg, Limits::default(), shutdown).await;
    stop.cancel();
    let _ = tokio::join!(gc_task, follow);
    ops_stop.cancel();
    let _ = ops_task.await;
    info!("cache registry stopped");
    Ok(())
}

/// Garbage collection and upload expiry, every `interval` until `stop`.
async fn collect(
    reg: Arc<Registry>,
    budgets: Arc<dyn Budgets>,
    interval: Duration,
    stop: CancellationToken,
) {
    loop {
        let expired = reg.store.expire_uploads(UPLOAD_TTL).await;
        let r = reg.store.gc(&*budgets, UPLOAD_TTL).await;
        reg.metrics.gc(&r);
        let stats = reg.store.stats().await;
        reg.metrics.stats(stats);
        if expired > 0 || r.deleted_blobs > 0 || r.evicted_pool + r.evicted_volume > 0 {
            info!(
                expired_uploads = expired,
                evicted_pool = r.evicted_pool,
                evicted_volume = r.evicted_volume,
                deleted_blobs = r.deleted_blobs,
                freed_bytes = r.freed_bytes,
                stored_bytes = stats.bytes,
                "cache garbage collected"
            );
        }
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(interval) => {}
        }
    }
}

/// Who may do what, and each pool's budget: from the worker pods and pools
/// in the watched namespaces, or, with `--grants-file`, from the file.
async fn authorizer(
    args: &RegistryArgs,
    metrics: &Arc<Metrics>,
    readiness: &Arc<Readiness>,
    stop: &CancellationToken,
) -> anyhow::Result<(Arc<dyn Authorizer>, Arc<dyn Budgets>)> {
    if let Some(p) = &args.grants_file {
        warn!(file = %p.display(), "grants come from a file, not from worker pods");
        let fixed = FixedBudgets {
            pool: args.pool_max_size,
            total: args.max_size,
        };
        return Ok((Arc::new(StaticGrants::load(p)?), Arc::new(fixed)));
    }
    let client = crate::runtime::kube_client()
        .await
        .context("building the kube client")?;
    let namespaces: Vec<String> = args
        .watch_namespaces
        .iter()
        .filter(|n| !n.is_empty())
        .cloned()
        .collect();
    info!(?namespaces, "watching worker pods, [] = all namespaces");
    let (pools, pods, _) =
        crate::runtime::stores::worker_watches(&client, &namespaces, metrics, readiness, stop);
    let fresh = || {
        StoreFreshness::new(
            readiness.register("pool_store").subscribe(),
            readiness.register("pod_store").subscribe(),
        )
    };
    let budgets = PoolBudgets {
        pools: pools.clone(),
        fresh: fresh(),
        default: args.pool_max_size,
        total: args.max_size,
    };
    let grants = PodGrants {
        pods,
        pools,
        fresh: fresh(),
    };
    Ok((Arc::new(grants), Arc::new(budgets)))
}
