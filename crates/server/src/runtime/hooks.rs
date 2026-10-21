//! Named slots where the other phase-2 modules plug in.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::info;

use super::AppContext;
use super::leader::BoxFut;

/// Result of a [`Component::run`].
pub type BoxResult =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>>;

/// A module that runs on every replica (alloc, http, edge) until `shutdown` fires,
/// then drains and returns.
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
    /// Allocation module.
    pub alloc: Arc<dyn Component>,
    /// HTTPS API.
    pub http: Arc<dyn Component>,
    /// mTLS edge.
    pub edge: Arc<dyn Component>,
    /// Optional components (the claim controller when `claims.enabled` is set).
    pub extra: Vec<Arc<dyn Component>>,
}

impl Hooks {
    /// Placeholders that log and idle.
    #[must_use]
    pub fn stubs() -> Self {
        Self {
            reconcile: |_| Arc::new(StubLeaderWork),
            pki: Arc::new(StubComponent("pki", &["ca_loaded", "serving_cert"])),
            alloc: Arc::new(StubComponent("alloc", &[])),
            http: Arc::new(StubComponent("http", &["api_listener"])),
            edge: Arc::new(StubComponent("edge", &["edge_listener"])),
            extra: Vec::new(),
        }
    }
}

/// Idle stand-in for a [`Component`]; marks the readiness checks it owns (name, checks).
pub struct StubComponent(pub &'static str, pub &'static [&'static str]);

impl Component for StubComponent {
    fn name(&self) -> &'static str {
        self.0
    }

    fn run(&self, ctx: AppContext, shutdown: CancellationToken) -> BoxResult {
        let name = self.0;
        for check in self.1 {
            ctx.readiness().register(check).set();
        }
        Box::pin(async move {
            info!(component = name, "stub component running");
            shutdown.cancelled().await;
            Ok(())
        })
    }
}

/// Idle stand-in for the reconciler.
pub struct StubLeaderWork;

impl LeaderWork for StubLeaderWork {
    fn run(&self, term: u64) -> BoxFut {
        Box::pin(async move {
            info!(term, "stub leader work running");
            std::future::pending::<()>().await;
        })
    }
}
