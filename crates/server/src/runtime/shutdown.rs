//! Ordered stop of the components once the shutdown delay has passed.
//!
//! Components run in stages. [`Stages::stop`] stops one stage at a time and
//! waits for every component in it to return before it stops the next, so a
//! later stage keeps serving what an earlier one still needs. `main` uses three:
//! the API (alloc, http, claims), then the edge, whose open connections drain
//! until each ends on its own, then the certificate loader they depend on.

use std::future::Future;

use futures::future::select_all;
use tokio::task::{JoinError, JoinSet};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

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
    /// before has returned. At `deadline` abort whatever still runs. Returns
    /// false when the deadline cut the drain short.
    pub async fn stop(mut self, deadline: Instant) -> bool {
        let ordered = async {
            for s in &mut self.stages {
                info!(stage = s.name, running = s.tasks.len(), "stopping stage");
                s.stop.cancel();
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
                        Err(e) => warn!(error = %e, "component task panicked while stopping"),
                    }
                }
            }
        };
        if tokio::time::timeout_at(deadline, ordered).await.is_ok() {
            return true;
        }
        for s in &mut self.stages {
            if !s.tasks.is_empty() {
                warn!(
                    stage = s.name,
                    running = s.tasks.len(),
                    "drain deadline reached; aborting"
                );
            }
            s.stop.cancel();
            s.tasks.abort_all();
            while s.tasks.join_next().await.is_some() {}
        }
        false
    }
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
