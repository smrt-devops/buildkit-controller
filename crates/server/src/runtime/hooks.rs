//! The components `main` wires into the runtime, and the stage each runs in.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::AppContext;
use super::leader::BoxFut;
use super::shutdown::Stages;

/// Result of a [`Component::run`].
pub type BoxResult =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>>;

/// A module that runs until `shutdown` fires, then drains and returns.
pub trait Component: Send + Sync {
    /// Name for logs.
    fn name(&self) -> &'static str;
    /// Run until `shutdown`.
    fn run(&self, ctx: AppContext, shutdown: CancellationToken) -> BoxResult;
}

/// Work that only the leader does (reconcile). A fresh future per leadership
/// term, started after the stores have synced, dropped when leadership ends.
pub trait LeaderWork: Send + Sync {
    /// The future for leadership term `term`.
    fn run(&self, term: u64) -> BoxFut;
}

/// The slots `main` fills.
pub struct Hooks {
    /// Builds the leader reconciler.
    pub reconcile: fn(&AppContext) -> Arc<dyn LeaderWork>,
    /// CA and serving cert loading (marks `ca_loaded`, `serving_cert`).
    pub pki: Arc<dyn Component>,
    /// Allocation's front door.
    pub alloc: Arc<dyn Component>,
    /// Allocation's leader loop: binds and tidies claims while leading.
    pub allocator: Arc<dyn Component>,
    /// HTTPS API.
    pub http: Arc<dyn Component>,
    /// mTLS edge.
    pub edge: Arc<dyn Component>,
}

impl Hooks {
    /// Start every component through `start`, which receives the component
    /// and its stage's stop token. Returns the leader stage (the allocator),
    /// which [`super::shutdown::step_down`] stops before the Lease is
    /// released, and the drain stages in stop order: the API (alloc, http),
    /// the edge, whose open connections drain until each ends on its own, and
    /// the certificate loader they depend on.
    pub fn start(
        &self,
        start: impl Fn(Arc<dyn Component>, CancellationToken) -> BoxResult,
    ) -> (Stages, Stages) {
        let mut leader = Stages::new(&["leader"]);
        let mut drain = Stages::new(&["api", "edge", "pki"]);
        let spawn = |stages: &mut Stages, stage: usize, c: &Arc<dyn Component>| {
            stages.spawn(stage, c.name(), |stop| start(Arc::clone(c), stop));
        };
        spawn(&mut leader, 0, &self.allocator);
        spawn(&mut drain, 0, &self.alloc);
        spawn(&mut drain, 0, &self.http);
        spawn(&mut drain, 1, &self.edge);
        spawn(&mut drain, 2, &self.pki);
        (leader, drain)
    }
}
