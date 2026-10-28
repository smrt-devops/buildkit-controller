//! The components `main` wires into the runtime.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

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
