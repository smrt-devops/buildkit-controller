//! Runs leader-only work for each leadership term.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::error;

use super::hooks::LeaderWork;
use super::stores::STALE_AFTER;
use crate::election::LeaderState;

/// How long a term waits for the stores to sync before it fails. A replica
/// campaigns only after its stores synced once, so a term waits only when a
/// store went stale since; past this bound the stores are not recovering, and
/// the replica gives up the Lease so that a replica that can sync takes over.
pub const SYNC_GATE: Duration = Duration::from_secs(2 * STALE_AFTER.as_secs());

/// Boxed unit future.
pub type BoxFut = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The leader work of a term stopped while the replica still led.
#[derive(Debug, thiserror::Error)]
pub enum TermFailed {
    /// The work panicked.
    #[error("leader work of term {term} panicked: {message}")]
    Panicked {
        /// The term.
        term: u64,
        /// The panic payload, when it is a string.
        message: String,
    },
    /// The work returned.
    #[error("leader work of term {term} returned")]
    Returned {
        /// The term.
        term: u64,
    },
    /// The stores did not sync within [`SYNC_GATE`].
    #[error(
        "leader work of term {term} did not start: the stores did not sync within {SYNC_GATE:?}"
    )]
    NotSynced {
        /// The term.
        term: u64,
    },
}

/// Follow `rx`. On each new term, wait for `synced()` (at most [`SYNC_GATE`])
/// and then run `work`.
/// Drop (abort) the work when leadership ends or the term changes.
/// Returns `Ok` after `shutdown` once the work has stopped.
///
/// # Errors
///
/// When the work of the current term panics or returns, or its stores do not
/// sync within [`SYNC_GATE`], while this replica still leads. A leader without
/// a reconciler must not keep the Lease, so the caller shuts the process down,
/// which releases it.
pub async fn supervise(
    mut rx: watch::Receiver<LeaderState>,
    shutdown: CancellationToken,
    synced: impl Fn() -> BoxFut,
    work: Arc<dyn LeaderWork>,
) -> Result<(), TermFailed> {
    let mut current: Option<(u64, JoinHandle<bool>)> = None;
    loop {
        let state = *rx.borrow_and_update();
        match state {
            LeaderState::Leader { term } if current.as_ref().is_none_or(|(t, _)| *t != term) => {
                stop(current.take()).await;
                let gate = synced();
                let run = work.run(term);
                current = Some((
                    term,
                    tokio::spawn(async move {
                        if tokio::time::timeout(SYNC_GATE, gate).await.is_err() {
                            return false;
                        }
                        run.await;
                        true
                    }),
                ));
            }
            LeaderState::Follower => stop(current.take()).await,
            LeaderState::Leader { .. } => {}
        }
        tokio::select! {
            () = shutdown.cancelled() => break,
            r = rx.changed() => if r.is_err() { break },
            (term, r) = ended(&mut current) => {
                let e = failure(term, r);
                error!(term, error = %e, "leader work stopped while leading");
                return Err(e);
            }
        }
    }
    stop(current.take()).await;
    Ok(())
}

/// Resolves when the current term's task ends (`false`: it never synced);
/// never without one.
async fn ended(current: &mut Option<(u64, JoinHandle<bool>)>) -> (u64, Result<bool, JoinError>) {
    match current {
        Some((term, h)) => (*term, h.await),
        None => std::future::pending().await,
    }
}

fn failure(term: u64, r: Result<bool, JoinError>) -> TermFailed {
    match r {
        Ok(true) => TermFailed::Returned { term },
        Ok(false) => TermFailed::NotSynced { term },
        Err(e) => {
            let message = match e.try_into_panic() {
                Ok(p) => p
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_default(),
                Err(e) => e.to_string(),
            };
            TermFailed::Panicked { term, message }
        }
    }
}

async fn stop(task: Option<(u64, JoinHandle<bool>)>) {
    if let Some((_, h)) = task {
        h.abort();
        let _ = h.await;
    }
}

#[cfg(test)]
#[path = "leader_tests.rs"]
mod tests;
