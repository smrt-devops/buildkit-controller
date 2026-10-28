//! Ordered stop of the components once the shutdown delay has passed.
//!
//! Components run in stages. [`Stages::stop`] stops one stage at a time and
//! waits for every component in it to return before it stops the next, so a
//! later stage keeps serving what an earlier one still needs. `main` uses three:
//! the API (alloc, http), then the edge, whose open connections drain until each
//! ends on its own, then the certificate loader they depend on. Leader-gated
//! components (the claim controller) are stopped earlier, by [`step_down`],
//! before the Lease is released.

use std::future::Future;
use std::time::Duration;

use futures::future::select_all;
use tokio::task::{JoinError, JoinHandle, JoinSet};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// How long before the drain deadline [`Stages::stop`] aborts what still runs, so
/// a Prometheus scrape (30 s is the common interval) can read the counters the
/// abort moved before the process exits.
pub const SCRAPE_GRACE: Duration = Duration::from_secs(30);

/// A finished component: its name and result.
pub type Exit = (&'static str, anyhow::Result<()>);

struct Stage {
    name: &'static str,
    stop: CancellationToken,
    tasks: JoinSet<Exit>,
}

/// Components grouped into stages that stop in order.
pub struct Stages {
    stages: Vec<Stage>,
}

impl Stages {
    /// Empty stages, stopped in the order of `names`.
    #[must_use]
    pub fn new(names: &[&'static str]) -> Self {
        Self {
            stages: names
                .iter()
                .map(|&name| Stage {
                    name,
                    stop: CancellationToken::new(),
                    tasks: JoinSet::new(),
                })
                .collect(),
        }
    }

    /// Spawn component `name` in stage `stage` (an index into the names given to
    /// [`Stages::new`]). `run` receives the stage's stop token.
    ///
    /// # Panics
    ///
    /// When `stage` is out of range (a programming error in `main`).
    pub fn spawn<F>(
        &mut self,
        stage: usize,
        name: &'static str,
        run: impl FnOnce(CancellationToken) -> F,
    ) where
        F: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        #[allow(clippy::indexing_slicing, reason = "documented panic on a bad index")]
        let s = &mut self.stages[stage];
        let fut = run(s.stop.clone());
        s.tasks.spawn(async move { (name, fut.await) });
    }

    /// Resolves when any component returns. Before shutdown that is always a
    /// failure, so the result is the error to shut down with. Never resolves
    /// when no component runs.
    pub async fn exited(&mut self) -> anyhow::Error {
        let running = self
            .stages
            .iter_mut()
            .filter(|s| !s.tasks.is_empty())
            .map(|s| Box::pin(s.tasks.join_next()))
            .collect::<Vec<_>>();
        if running.is_empty() {
            return std::future::pending().await;
        }
        match select_all(running).await.0 {
            Some(done) => exit_error(done),
            None => anyhow::anyhow!("component set emptied"),
        }
    }

    /// Stop the stages in order, each only after every component of the one
    /// before has returned. What still runs [`SCRAPE_GRACE`] before `deadline`
    /// (half the remaining time when that is shorter) is aborted then, the
    /// later stages are stopped in order, and the call returns at `deadline`:
    /// the process keeps serving metrics in between, so a scrape sees what the
    /// abort counted (the edge's `drain` revocations). At `deadline` whatever
    /// still runs is aborted. Returns false when the drain was cut short.
    pub async fn stop(self, deadline: Instant) -> bool {
        self.stop_with(deadline, SCRAPE_GRACE).await
    }

    async fn stop_with(mut self, deadline: Instant, grace: Duration) -> bool {
        let left = deadline.saturating_duration_since(Instant::now());
        let cut = deadline - grace.min(left / 2);
        if tokio::time::timeout_at(cut, stop_in_order(&mut self.stages))
            .await
            .is_ok()
        {
            return true;
        }
        // The stages stopped so far that are still running overran: abort them.
        abort_running(self.stages.iter_mut().filter(|s| s.stop.is_cancelled())).await;
        if tokio::time::timeout_at(deadline, stop_in_order(&mut self.stages))
            .await
            .is_err()
        {
            abort_running(self.stages.iter_mut()).await;
        }
        tokio::time::sleep_until(deadline).await;
        false
    }
}

/// Cancels each stage in turn and waits for all its components to return.
async fn stop_in_order(stages: &mut [Stage]) {
    for s in stages {
        if !s.stop.is_cancelled() {
            info!(stage = s.name, running = s.tasks.len(), "stopping stage");
            s.stop.cancel();
        }
        while let Some(done) = s.tasks.join_next().await {
            match done {
                Ok((_, Ok(()))) => {}
                Ok((name, Err(e))) => {
                    warn!(
                        component = name,
                        error = format!("{e:#}"),
                        "component failed while stopping"
                    );
                }
                Err(e) if e.is_cancelled() => {}
                Err(e) => warn!(error = %e, "component task panicked while stopping"),
            }
        }
    }
}

/// Aborts every component still running in `stages` and waits until each is dropped.
async fn abort_running<'a>(stages: impl Iterator<Item = &'a mut Stage>) {
    for s in stages {
        if !s.tasks.is_empty() {
            warn!(
                stage = s.name,
                running = s.tasks.len(),
                "drain budget spent; aborting"
            );
        }
        s.stop.cancel();
        s.tasks.abort_all();
        while s.tasks.join_next().await.is_some() {}
    }
}

/// Hand leadership over: wait for `reconciler` (the caller has already told the
/// reconciler supervisor to stop) and stop every leader-gated component in
/// `leader` (aborted at `deadline`), and only then cancel `release`, the
/// elector's token, and wait for the elector to give up the Lease. No leader
/// work of this replica can overlap the next leader's.
pub async fn step_down(
    reconciler: impl Future<Output = ()>,
    leader: Stages,
    deadline: Instant,
    release: CancellationToken,
    elector: JoinHandle<()>,
) {
    let ((), _) = tokio::join!(reconciler, leader.stop_with(deadline, Duration::ZERO));
    release.cancel();
    let _ = elector.await;
}

fn exit_error(done: Result<Exit, JoinError>) -> anyhow::Error {
    match done {
        Ok((name, Ok(()))) => anyhow::anyhow!("component {name} exited early"),
        Ok((name, Err(e))) => e.context(format!("component {name} failed")),
        Err(e) => anyhow::anyhow!("component task panicked: {e}"),
    }
}

#[cfg(test)]
#[path = "shutdown_tests.rs"]
mod tests;
