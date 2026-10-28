//! Leader election with its own Lease loop.
//!
//! The loop never compares timestamps written by other nodes with the local
//! clock. It keeps `observed_at`, a monotonic instant reset whenever the
//! holder, `renewTime` or `resourceVersion` of the Lease changes, and judges
//! expiry by `observed_at.elapsed() >= leaseDuration`.
//!
//! * [`LeaseApi`] abstracts the four Lease calls so the loop runs against a
//!   fake in unit tests; [`kube_api::KubeLeaseApi`] is the real one.
//! * [`Clock`] abstracts the monotonic clock (tokio's, so paused test time works).
//! * [`Elector::new`] returns the loop and a `watch::Receiver<LeaderState>`.
//!   Every leadership term has a fresh `term`, so a fenced-then-reacquired
//!   leader is observably a new leader and consumers restart their work.
//! * On shutdown the state goes to `Follower` first; the caller stops its
//!   leader work, and only then cancels [`Elector::run`]'s token to release the Lease (see main).

use std::future::Future;
use std::sync::Arc;

use futures::StreamExt;
use futures::stream::BoxStream;
use k8s_openapi::api::coordination::v1::LeaseSpec;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use tokio::sync::watch;
use tokio::time::{Instant, sleep_until, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::ElectionTiming;

pub mod kube_api;
#[cfg(test)]
mod tests;

/// Leadership as published to consumers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaderState {
    /// Not leading.
    Follower,
    /// Leading; `term` increases every time leadership is gained.
    Leader {
        /// Leadership term of this process.
        term: u64,
    },
}

impl LeaderState {
    /// True while leading.
    #[must_use]
    pub fn is_leader(self) -> bool {
        matches!(self, Self::Leader { .. })
    }
}

/// A Lease as last seen, with its `resourceVersion` and spec.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LeaseSnapshot {
    /// `metadata.resourceVersion`.
    pub resource_version: String,
    /// `spec`.
    pub spec: LeaseSpec,
}

impl LeaseSnapshot {
    fn holder(&self) -> Option<&str> {
        self.spec
            .holder_identity
            .as_deref()
            .filter(|h| !h.is_empty())
    }
}

/// Lease API failure.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// HTTP 409, someone else wrote first (or the object already exists).
    #[error("conflict")]
    Conflict,
    /// HTTP 404: the Lease is gone (a replace), or its namespace is.
    #[error("not found")]
    NotFound,
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

/// The Lease calls the loop needs. Timeouts are applied by the loop.
pub trait LeaseApi: Send + Sync + 'static {
    /// Current Lease, `None` when absent.
    fn get(&self) -> impl Future<Output = Result<Option<LeaseSnapshot>, ApiError>> + Send;
    /// Create the Lease; `Conflict` when it exists.
    fn create(
        &self,
        spec: LeaseSpec,
    ) -> impl Future<Output = Result<LeaseSnapshot, ApiError>> + Send;
    /// Replace guarded by `prev.resource_version`; `Conflict` on mismatch.
    fn replace(
        &self,
        prev: &LeaseSnapshot,
        spec: LeaseSpec,
    ) -> impl Future<Output = Result<LeaseSnapshot, ApiError>> + Send;
    /// Changes of the Lease (`None` = deleted). Must survive API outages itself.
    fn watch(&self) -> BoxStream<'static, Option<LeaseSnapshot>>;
}

/// Monotonic clock.
pub trait Clock: Send + Sync + 'static {
    /// Now.
    fn now(&self) -> Instant;
}

/// tokio's clock; honours `tokio::time::pause`.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioClock;

impl Clock for TokioClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

struct Observed {
    snap: LeaseSnapshot,
    at: Instant,
}

enum Wake {
    Timer,
    Event,
    Shutdown,
}

/// The election loop.
pub struct Elector<A, C> {
    api: Arc<A>,
    clock: C,
    identity: String,
    t: ElectionTiming,
    tx: watch::Sender<LeaderState>,
    observed: Option<Observed>,
    known: bool,
    leading: bool,
    term: u64,
    last_renew_sent: Option<Instant>,
}

impl<A: LeaseApi, C: Clock> Elector<A, C> {
    /// Build the loop and its leadership receiver.
    pub fn new(
        api: Arc<A>,
        clock: C,
        identity: String,
        timing: ElectionTiming,
    ) -> (Self, watch::Receiver<LeaderState>) {
        let (tx, rx) = watch::channel(LeaderState::Follower);
        let me = Self {
            api,
            clock,
            identity,
            t: timing,
            tx,
            observed: None,
            known: false,
            leading: false,
            term: 0,
            last_renew_sent: None,
        };
        (me, rx)
    }

    /// The larger of our `leaseDuration` and the one the holder wrote.
    fn expiry(&self, o: &Observed) -> std::time::Duration {
        let theirs = u64::try_from(o.snap.spec.lease_duration_seconds.unwrap_or(0)).unwrap_or(0);
        self.t
            .lease_duration
            .max(std::time::Duration::from_secs(theirs))
    }

    fn secs(&self) -> i32 {
        i32::try_from(self.t.lease_duration.as_secs()).unwrap_or(i32::MAX)
    }

    fn observe(&mut self, snap: Option<LeaseSnapshot>) {
        self.known = true;
        let now = self.clock.now();
        self.observed = snap.map(|snap| {
            let at = match &self.observed {
                Some(o)
                    if o.snap.holder() == snap.holder()
                        && o.snap.spec.renew_time == snap.spec.renew_time
                        && o.snap.resource_version == snap.resource_version =>
                {
                    o.at
                }
                _ => now,
            };
            Observed { snap, at }
        });
    }

    fn holder_is_me(&self) -> bool {
        self.observed
            .as_ref()
            .is_some_and(|o| o.snap.holder() == Some(self.identity.as_str()))
    }

    fn set_leading(&mut self, leading: bool, why: &str) {
        if leading == self.leading {
            return;
        }
        self.leading = leading;
        if leading {
            self.term += 1;
            info!(identity = %self.identity, term = self.term, "leadership gained");
            self.tx
                .send_replace(LeaderState::Leader { term: self.term });
        } else {
            self.last_renew_sent = None;
            info!(identity = %self.identity, reason = why, "leadership lost");
            self.tx.send_replace(LeaderState::Follower);
        }
    }

    async fn wait(
        &mut self,
        until: Instant,
        events: &mut futures::stream::Fuse<BoxStream<'static, Option<LeaseSnapshot>>>,
        shutdown: &CancellationToken,
    ) -> Wake {
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return Wake::Shutdown,
                () = sleep_until(until) => return Wake::Timer,
                Some(snap) = events.next() => {
                    // A leader trusts only its own writes and GETs, because a watch event can be
                    // older than what a write already told it.
                    if !self.leading {
                        self.observe(snap);
                        return Wake::Event;
                    }
                }
            }
        }
    }

    fn spec_for(&self, prev: Option<&LeaseSnapshot>, acquire: bool) -> LeaseSpec {
        let now = MicroTime(k8s_openapi::jiff::Timestamp::now());
        let mut spec = prev.map(|p| p.spec.clone()).unwrap_or_default();
        spec.holder_identity = Some(self.identity.clone());
        spec.lease_duration_seconds = Some(self.secs());
        spec.renew_time = Some(now.clone());
        if acquire {
            spec.acquire_time = Some(now);
            spec.lease_transitions = Some(spec.lease_transitions.unwrap_or(-1) + 1);
        }
        spec
    }

    async fn refresh(&mut self, budget: std::time::Duration) -> bool {
        match timeout(budget, self.api.get()).await {
            Ok(Ok(snap)) => {
                self.observe(snap);
                true
            }
            Ok(Err(e)) => {
                warn!(error = %e, "lease get failed");
                false
            }
            Err(_) => {
                warn!("lease get timed out");
                false
            }
        }
    }

    /// Run until `shutdown` is cancelled, then release the Lease if held.
    pub async fn run(mut self, shutdown: CancellationToken) {
        let mut events = self.api.watch().fuse();
        while !shutdown.is_cancelled() {
            if self.leading {
                self.lead_step(&mut events, &shutdown).await;
            } else {
                self.follow_step(&mut events, &shutdown).await;
            }
        }
        self.release().await;
    }

    async fn follow_step(
        &mut self,
        events: &mut futures::stream::Fuse<BoxStream<'static, Option<LeaseSnapshot>>>,
        shutdown: &CancellationToken,
    ) {
        let interval = self.t.renew_interval;
        let retry = move |start: Instant| start + interval;
        if !self.known {
            let start = self.clock.now();
            if !self.refresh(self.t.request_timeout).await {
                self.wait(retry(start), events, shutdown).await;
            }
            return;
        }
        let acquire_now = match &self.observed {
            None => true,
            Some(o) => {
                o.snap.holder().is_none()
                    || self.holder_is_me()
                    || self.clock.now().saturating_duration_since(o.at) >= self.expiry(o)
            }
        };
        if !acquire_now {
            let now = self.clock.now();
            let (at, expiry) = self
                .observed
                .as_ref()
                .map_or((now, self.t.lease_duration), |o| (o.at, self.expiry(o)));
            self.wait(at + expiry, events, shutdown).await;
            return;
        }
        let start = self.clock.now();
        let result = match &self.observed {
            None => {
                timeout(
                    self.t.request_timeout,
                    self.api.create(self.spec_for(None, true)),
                )
                .await
            }
            Some(o) => {
                let spec = self.spec_for(Some(&o.snap), true);
                timeout(self.t.request_timeout, self.api.replace(&o.snap, spec)).await
            }
        };
        match result {
            Ok(Ok(snap)) => {
                self.observe(Some(snap));
                self.last_renew_sent = Some(start);
                self.set_leading(true, "acquired");
            }
            Ok(Err(ApiError::Conflict)) => self.known = false,
            Ok(Err(e)) => {
                // The Lease may have been deleted or replaced meanwhile: read it again.
                warn!(error = %e, "lease acquire failed");
                self.known = false;
                self.wait(retry(start), events, shutdown).await;
            }
            Err(_) => {
                warn!("lease acquire timed out");
                self.wait(retry(start), events, shutdown).await;
            }
        }
    }

    async fn lead_step(
        &mut self,
        events: &mut futures::stream::Fuse<BoxStream<'static, Option<LeaseSnapshot>>>,
        shutdown: &CancellationToken,
    ) {
        let Some(sent) = self.last_renew_sent else {
            self.set_leading(false, "no renew");
            return;
        };
        let deadline = sent + self.t.renew_deadline();
        let start = self.clock.now();
        if !self.holder_is_me() {
            self.set_leading(false, "lease taken");
            return;
        }
        if start >= deadline {
            warn!(identity = %self.identity, "renew deadline passed, self-fencing");
            self.set_leading(false, "fenced");
            return;
        }
        let budget = self.t.request_timeout.min(deadline - start);
        let Some(o) = &self.observed else { return };
        let spec = self.spec_for(Some(&o.snap), false);
        match timeout(budget, self.api.replace(&o.snap, spec)).await {
            Ok(Ok(snap)) => {
                self.observe(Some(snap));
                self.last_renew_sent = Some(start);
            }
            Ok(Err(ApiError::Conflict)) => {
                // Learn the newest version. A foreign holder ends leadership at once.
                let left = deadline.saturating_duration_since(self.clock.now());
                self.refresh(budget.min(left)).await;
                return;
            }
            Ok(Err(ApiError::NotFound)) => {
                // Someone deleted the Lease. Stop leading; as a follower this
                // replica creates it again (or sees who did).
                warn!(identity = %self.identity, "lease deleted while leading");
                self.observe(None);
                self.set_leading(false, "lease deleted");
                return;
            }
            Ok(Err(e)) => warn!(error = %e, "lease renew failed"),
            Err(_) => warn!("lease renew timed out"),
        }
        let fence_at = self
            .last_renew_sent
            .map_or(start, |s| s + self.t.renew_deadline());
        let next = (start + self.t.renew_interval).min(fence_at);
        self.wait(next, events, shutdown).await;
    }

    async fn release(&mut self) {
        self.set_leading(false, "shutdown");
        if !self.holder_is_me() {
            return;
        }
        // A renew that timed out here may still have committed, leaving our
        // resourceVersion stale. On a conflict, read the Lease and try once more.
        for retry in [true, false] {
            let Some(o) = &self.observed else { return };
            let mut spec = o.snap.spec.clone();
            spec.holder_identity = None;
            match timeout(self.t.request_timeout, self.api.replace(&o.snap, spec)).await {
                Ok(Ok(_)) => info!(identity = %self.identity, "lease released"),
                Ok(Err(ApiError::Conflict)) if retry => {
                    if self.refresh(self.t.request_timeout).await && self.holder_is_me() {
                        continue;
                    }
                }
                Ok(Err(ApiError::NotFound)) => info!("lease already gone"),
                Ok(Err(e)) => warn!(error = %e, "lease release failed"),
                Err(_) => warn!("lease release timed out"),
            }
            return;
        }
    }
}
