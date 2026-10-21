//! The server runtime. Every phase-2 module plugs into it.
//!
//! # Plugging in
//!
//! Each of reconcile, alloc, http and edge receives an [`AppContext`] and
//! nothing else global. Its stable surface:
//!
//! | Accessor | Gives |
//! |---|---|
//! | [`AppContext::client`] | the `kube::Client` |
//! | [`AppContext::settings`] / [`AppContext::config`] | flags+env, and the validated file |
//! | [`AppContext::replica`] / [`AppContext::namespace`] | `POD_NAME`, `POD_NAMESPACE` |
//! | [`AppContext::pools`] / [`AppContext::pods`] | cloned `Store<BuildKitPool>` / `Store<Pod>` readers |
//! | [`AppContext::pod_events`] | a [`PodSubscription`]; `Resync` means rescan [`AppContext::pods`] |
//! | [`AppContext::ca_secret`] / [`AppContext::serving_secret`] | `watch::Receiver<SecretState>` of the Secret watches |
//! | [`AppContext::leader`] | `watch::Receiver<LeaderState>` |
//! | [`AppContext::metrics`] | the registry ([`Metrics::with_registry`]) |
//! | [`AppContext::readiness`] | register `/readyz` checks: `ca_loaded`, `serving_cert`, `api_listener`, `edge_listener` |
//! | [`AppContext::wait_synced`] | resolves once Pool and Pod stores and the CA Secret watch saw `InitDone` |
//!
//! Long-running modules implement [`hooks::Component`] (alloc, http and edge; they run on
//! every replica until the shutdown token fires) or [`hooks::LeaderWork`]
//! (reconcile; it starts per leadership term, after `wait_synced`, dropped when
//! leadership ends). Replace the stubs in [`hooks::Hooks`].

pub mod hooks;
pub mod leader;
pub mod metrics;
pub mod readiness;
pub mod stores;

use std::sync::Arc;

use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::runtime::reflector::Store;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use buildkit_controller_api::v1alpha1::BuildKitPool;

use crate::config::{Config, Settings};
use crate::election::LeaderState;
use metrics::Metrics;
use readiness::Readiness;
use stores::{PodSubscription, SecretState, StoreFreshness, StoreSpec, Stores};

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
        let stores = Stores::spawn(
            StoreSpec {
                client: client.clone(),
                watch_namespace: &settings.config.watch_namespace,
                namespace: &settings.cli.pod_namespace,
                ca_secret_name: &settings.config.pki.ca_secret_name,
                serving_secret_name: &settings.config.edge.tls.secret_name,
                metrics: Arc::clone(&metrics),
                pools_ready: readiness.register("pool_store"),
                pods_ready: readiness.register("pod_store"),
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

    /// Resolve once the Pool and Pod stores and the CA Secret watch have seen `InitDone`.
    pub async fn wait_synced(&self) {
        self.inner
            .readiness
            .wait_for(&["pool_store", "pod_store"])
            .await;
        let mut ca = self.ca_secret();
        let _ = ca.wait_for(|s| s.init_done).await;
    }
}
