//! The shutdown sequence `main` runs after SIGTERM, SIGINT or a failed component.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::readiness::Readiness;
use super::shutdown::{Stages, step_down};

/// How long `run` waits for the ops server after stopping it. The server
/// bounds its own stop; this only keeps a bug there from holding the exit.
const OPS_WAIT: Duration = Duration::from_secs(5);

/// Everything the shutdown sequence stops, in the order it stops them.
pub struct Shutdown {
    /// When shutdown began. The delay and the drain budget count from here, so
    /// a slow step-down eats into them instead of extending the stop.
    pub started: Instant,
    /// `/readyz` fails first.
    pub readiness: Arc<Readiness>,
    /// Stops the reconciler supervisor.
    pub leader_stop: CancellationToken,
    /// Resolves when the reconciler supervisor has returned.
    pub reconciler: BoxFuture<'static, ()>,
    /// Leader-gated components (the claim controller).
    pub leader: Stages,
    /// Bound on stopping the leader-gated components (`kube.requestTimeout`).
    pub request_timeout: Duration,
    /// Cancelling it makes the elector release the Lease and return.
    pub elect_stop: CancellationToken,
    /// The elector.
    pub elector: JoinHandle<()>,
    /// `shutdown.delay`: the wait after `/readyz` fails before listeners close.
    pub delay: Duration,
    /// The longest the drain may take.
    pub drain_budget: Duration,
    /// API, edge and certificate loader stages.
    pub stages: Stages,
    /// Stops the Pool, Pod and Secret watches.
    pub watches: CancellationToken,
    /// Stops the ops server.
    pub ops_stop: CancellationToken,
    /// Resolves when the ops server has returned; `None` when it already has.
    pub ops: Option<BoxFuture<'static, ()>>,
}

impl Shutdown {
    /// Fail `/readyz`, stop leader work and release the Lease, wait out the
    /// delay, drain the stages under the budget, then stop the watches and the
    /// ops server.
    pub async fn run(self) {
        info!("shutting down");
        self.readiness.set_shutting_down();
        // Leadership goes first, so another replica leads within seconds however
        // long this one drains. All leader work stops before the Lease is released.
        self.leader_stop.cancel();
        let bound = Instant::now() + self.request_timeout;
        step_down(
            self.reconciler,
            self.leader,
            bound,
            self.elect_stop,
            self.elector,
        )
        .await;
        info!("leadership released");
        let drain_from = self.started + self.delay;
        tokio::time::sleep_until(drain_from).await;
        let budget = self.drain_budget;
        info!(drain_budget_s = budget.as_secs(), "draining");
        if !self.stages.stop(drain_from + budget).await {
            warn!(drain_budget_s = budget.as_secs(), "drain timed out");
        }
        self.watches.cancel();
        self.ops_stop.cancel();
        if let Some(ops) = self.ops
            && tokio::time::timeout(OPS_WAIT, ops).await.is_err()
        {
            warn!("ops server did not stop in time");
        }
        info!("exited");
    }
}

#[cfg(test)]
#[path = "stop_tests.rs"]
mod tests;
