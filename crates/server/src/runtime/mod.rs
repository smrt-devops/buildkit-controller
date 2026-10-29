//! The server runtime: the kube client, the shared stores, readiness, metrics,
//! leadership and the ordered shutdown that `main` runs the components on.
//!
//! # The context
//!
//! Each of reconcile, alloc, http, edge, the certificate loader and the claim
//! controller receives an [`AppContext`] and nothing else global. Its surface:
//!
//! | Accessor | Gives |
//! |---|---|
//! | [`AppContext::client`] | the `kube::Client` |
//! | [`AppContext::settings`] / [`AppContext::config`] | flags+env, and the validated file |
//! | [`AppContext::replica`] / [`AppContext::namespace`] | `POD_NAME`, `POD_NAMESPACE` |
//! | [`AppContext::pools`] / [`AppContext::pods`] | cloned `Store<BuildKitPool>` / `Store<Pod>` readers |
//! | [`AppContext::pod_events`] | a [`PodSubscription`]; `Resync` means rescan [`AppContext::pods`] |
//! | [`AppContext::claims`] / [`AppContext::claim_events`] | the `BuildKitClaim` store and its events |
//! | [`AppContext::ca_secret`] / [`AppContext::serving_secret`] | `watch::Receiver<SecretState>` of the Secret watches |
//! | [`AppContext::leader`] | `watch::Receiver<LeaderState>` |
//! | [`AppContext::metrics`] | the registry ([`Metrics::with_registry`]) |
//! | [`AppContext::readiness`] | register `/readyz` checks: `ca_loaded`, `serving_cert`, `api_listener`, `edge_listener` |
//! | [`AppContext::wait_synced`] | resolves once the Pool, Pod and claim stores and the CA Secret watch saw `InitDone` |
//!
//! Long-running modules implement [`hooks::Component`] (alloc, http, edge and
//! the certificate loader run on every replica until their stage's shutdown
//! token fires; the allocator acts only while leading) or
//! [`hooks::LeaderWork`] (reconcile; it starts per leadership term, after
//! `wait_synced`, and is dropped when leadership ends). `main` fills
//! [`hooks::Hooks`] with them.

mod idle;

pub mod hooks;
pub mod leader;
pub mod listen;
pub mod metrics;
pub mod readiness;
pub mod shutdown;
pub mod stop;
pub mod stores;

use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::runtime::reflector::Store;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use buildkit_controller_api::claim::BuildKitClaim;
use buildkit_controller_api::v1alpha1::BuildKitPool;

use crate::config::{Config, Settings};
use crate::election::LeaderState;
use metrics::Metrics;
use readiness::Readiness;
use stores::{ClaimSubscription, PodSubscription, SecretState, StoreFreshness, StoreSpec, Stores};

/// Longest a request to the API server may go without receiving anything
/// before it fails: from sending the request to its response headers, and
/// then between frames of the response body. The timers belong to each
/// request, so one sent on a pooled connection that sat idle still gets the
/// full limit.
///
/// Without it a connection that stalls without an error (an API server that
/// hangs, a load balancer that silently drops the flow) leaves a watch, a list
/// or the start of a new watch waiting forever. kube's watcher notices a stall
/// in the middle of a watch only after its 295 s idle timeout, and then
/// re-watches without reporting it, so the stores never learn they were stale.
/// With the timeout a stall becomes a watch error, which starts a relist and
/// the [`stores::STALE_AFTER`] clock like any other watch error. A stalled
/// watch therefore goes unnoticed for at most this long, and the store's
/// readiness flag clears at most `READ_TIMEOUT + STALE_AFTER` (150 s) after
/// the last byte, unless a relist completes first.
///
/// A healthy watch is never silent this long: kube asks for bookmarks, and the
/// API server's watch cache sends one on every watch about once a minute and
/// just before the watch's own 290 s timeout. 90 s leaves 30 s for timer
/// slack and a loaded API server. Where the watch cache is disabled there are
/// no bookmarks, and a watch that sees no change fails after 90 s and relists;
/// the store stays ready through it.
///
/// The timeout counts silence, not the length of a request, so a long but
/// flowing list is unaffected. A request is cut only if the API server sends
/// nothing for 90 s. Calls bounded by `kube.requestTimeout` (5 s by default;
/// the allocation, reconcile, token review and Lease calls) keep that bound,
/// which config validation holds below this one so that it is the one that
/// applies. Calls without their own bound gain this one. It is also beyond
/// how long the API server takes to start answering a list, even a paged one served from etcd. The
/// controller makes no exec, attach or log requests, the long-lived calls that
/// a read timeout would break.
pub const READ_TIMEOUT: Duration = Duration::from_secs(90);

/// The controller's kube client: [`Client::try_default`] (the inferred
/// config, in-cluster or the kubeconfig, with kube's TLS, auth refresh,
/// retries and headers unchanged), wrapped so that each request fails after
/// [`READ_TIMEOUT`] of silence. Every watch, list and write shares it.
///
/// # Errors
///
/// No config could be inferred, or the client could not be built from it.
pub async fn kube_client() -> Result<Client, kube::Error> {
    let config = kube::Config::infer()
        .await
        .map_err(kube::Error::InferConfig)?;
    client_from(config, READ_TIMEOUT)
}

/// A client for `config` whose requests fail after `after` of silence.
fn client_from(config: kube::Config, after: Duration) -> Result<Client, kube::Error> {
    let namespace = config.default_namespace.clone();
    let inner = Client::try_from(config)?;
    Ok(Client::new(idle::IdleTimeout { inner, after }, namespace))
}

/// Shared handle to the runtime; cheap to clone.
#[derive(Clone)]
pub struct AppContext {
    inner: Arc<Inner>,
}

struct Inner {
    client: Client,
    settings: Settings,
    stores: Stores,
    metrics: Arc<Metrics>,
    readiness: Arc<Readiness>,
    leader: watch::Receiver<LeaderState>,
}

impl AppContext {
    /// Start the stores and assemble the context. `shutdown` stops the root watches.
    #[must_use]
    pub fn build(
        client: Client,
        settings: Settings,
        metrics: Arc<Metrics>,
        readiness: Arc<Readiness>,
        leader: watch::Receiver<LeaderState>,
        shutdown: &CancellationToken,
    ) -> Self {
        readiness.register("config").set();
        tracing::info!(namespaces = ?settings.config.watched_namespaces(), "watch scope, [] = all namespaces");
        let stores = Stores::spawn(
            StoreSpec {
                client: client.clone(),
                watch_namespaces: settings.config.watched_namespaces(),
                claim_namespaces: &settings
                    .config
                    .claim_namespaces(&settings.cli.pod_namespace),
                namespace: &settings.cli.pod_namespace,
                ca_secret_name: &settings.config.pki.ca_secret_name,
                serving_secret_name: &settings.config.edge.tls.secret_name,
                metrics: Arc::clone(&metrics),
                readiness: Arc::clone(&readiness),
            },
            shutdown,
        );
        Self {
            inner: Arc::new(Inner {
                client,
                settings,
                stores,
                metrics,
                readiness,
                leader,
            }),
        }
    }

    /// Kubernetes client.
    #[must_use]
    pub fn client(&self) -> Client {
        self.inner.client.clone()
    }

    /// Flags, environment and file.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.inner.settings
    }

    /// The validated file.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.inner.settings.config
    }

    /// `POD_NAME`.
    #[must_use]
    pub fn replica(&self) -> &str {
        &self.inner.settings.cli.pod_name
    }

    /// `POD_NAMESPACE`.
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.inner.settings.cli.pod_namespace
    }

    /// Pool store reader.
    #[must_use]
    pub fn pools(&self) -> Store<BuildKitPool> {
        self.inner.stores.pools.clone()
    }

    /// Worker pod store reader.
    #[must_use]
    pub fn pods(&self) -> Store<Pod> {
        self.inner.stores.pods.clone()
    }

    /// Whether the Pool and Pod stores are fresh.
    #[must_use]
    pub fn store_freshness(&self) -> StoreFreshness {
        let r = &self.inner.readiness;
        StoreFreshness::new(
            r.register("pool_store").subscribe(),
            r.register("pod_store").subscribe(),
        )
    }

    /// A new pod event subscription.
    #[must_use]
    pub fn pod_events(&self) -> PodSubscription {
        PodSubscription::new(self.inner.stores.pod_events.subscribe())
    }

    /// Claim store reader.
    #[must_use]
    pub fn claims(&self) -> Store<BuildKitClaim> {
        self.inner.stores.claims.clone()
    }

    /// A new claim event subscription.
    #[must_use]
    pub fn claim_events(&self) -> ClaimSubscription {
        ClaimSubscription::new(self.inner.stores.claim_events.subscribe())
    }

    /// CA Secret watch.
    #[must_use]
    pub fn ca_secret(&self) -> watch::Receiver<SecretState> {
        self.inner.stores.ca_secret.clone()
    }

    /// User serving Secret watch.
    #[must_use]
    pub fn serving_secret(&self) -> watch::Receiver<SecretState> {
        self.inner.stores.serving_secret.clone()
    }

    /// Leadership.
    #[must_use]
    pub fn leader(&self) -> watch::Receiver<LeaderState> {
        self.inner.leader.clone()
    }

    /// Metrics registry.
    #[must_use]
    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.inner.metrics
    }

    /// Readiness checks.
    #[must_use]
    pub fn readiness(&self) -> &Arc<Readiness> {
        &self.inner.readiness
    }

    /// Resolve once the Pool, Pod and claim stores and the CA Secret watch have seen `InitDone`.
    pub async fn wait_synced(&self) {
        self.inner
            .readiness
            .wait_for(&["pool_store", "pod_store", "claim_store"])
            .await;
        let mut ca = self.ca_secret();
        let _ = ca.wait_for(|s| s.init_done).await;
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod client_tests;
