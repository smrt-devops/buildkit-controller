//! Shared stores.
//!
//! One stable `reflector::store` per kind, driven by one root task per kind on
//! every replica. The root task consumes the reflector output itself (which
//! carries `Delete`) and forwards pod events on a `broadcast` channel; it never
//! uses `store_shared`/`Writer::subscribe`. A receiver that lags is told to
//! rescan with [`PodEvent::Resync`].

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::{Stream, StreamExt};
use k8s_openapi::api::core::v1::{Pod, Secret};
use kube::api::Api;
use kube::runtime::reflector::{self, Store};
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Client, Resource, ResourceExt};
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use buildkit_controller_api::v1alpha1::BuildKitPool;

use super::metrics::Metrics;
use super::readiness::ReadyFlag;
use crate::render::{CONTROLLER, LABEL_MANAGED_BY, LABEL_POOL};

/// Capacity of the pod event broadcast.
pub const POD_EVENT_CAPACITY: usize = 1024;

/// A pod change, applied to the store before it is broadcast.
#[derive(Clone, Debug)]
pub enum PodEvent {
    /// Added or modified.
    Apply(Arc<Pod>),
    /// Deleted.
    Delete(Arc<Pod>),
    /// A relist completed, or this receiver lagged. Rescan the whole store.
    Resync,
}

/// A subscription to pod events that turns lag into [`PodEvent::Resync`].
#[derive(Debug)]
pub struct PodSubscription(broadcast::Receiver<PodEvent>);

impl PodSubscription {
    /// Wrap a receiver.
    #[must_use]
    pub fn new(rx: broadcast::Receiver<PodEvent>) -> Self {
        Self(rx)
    }

    /// Next event; `None` when the runtime has shut down. After `Lagged`
    /// returns `Resync`; the caller rescans its pod store.
    pub async fn recv(&mut self) -> Option<PodEvent> {
        match self.0.recv().await {
            Ok(ev) => Some(ev),
            Err(broadcast::error::RecvError::Lagged(_)) => Some(PodEvent::Resync),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }
}

/// Latest state of a single-object Secret watch.
#[derive(Clone, Debug, Default)]
pub struct SecretState {
    /// The first list completed.
    pub init_done: bool,
    /// The Secret, `None` while absent. Never log its data.
    pub secret: Option<Arc<Secret>>,
}

/// First wait before a watch restarts after an error; it doubles up to [`RESTART_MAX`].
const RESTART_MIN: Duration = Duration::from_secs(1);
/// Longest wait between watch restarts.
const RESTART_MAX: Duration = Duration::from_secs(30);

/// Drive a store from `watch()`. It applies events, taps them, and sets `ready`
/// at each `InitDone`. A watch error clears `ready` and starts a new watch
/// (a full relist) after a backoff, because kube's watcher resumes a failed
/// watch without a relist and so never reports when the store caught up. A
/// cleared flag therefore means the store may be missing changes.
/// Returns when a stream ends (only test streams do).
pub async fn drive<K, S>(
    mut writer: reflector::store::Writer<K>,
    mut watch: impl FnMut() -> S,
    ready: ReadyFlag,
    on_error: impl Fn(&watcher::Error),
    mut tap: impl FnMut(&watcher::Event<K>),
) where
    K: Resource + Clone + 'static,
    K::DynamicType: Clone + Eq + std::hash::Hash + Default,
    S: Stream<Item = watcher::Result<watcher::Event<K>>>,
{
    let mut gap = RESTART_MIN;
    loop {
        let events = watch();
        futures::pin_mut!(events);
        loop {
            match events.next().await {
                None => return,
                Some(Ok(ev)) => {
                    writer.apply_watcher_event(&ev);
                    if matches!(ev, watcher::Event::InitDone) {
                        ready.set();
                        gap = RESTART_MIN;
                    }
                    tap(&ev);
                }
                Some(Err(e)) => {
                    on_error(&e);
                    ready.clear();
                    break;
                }
            }
        }
        tokio::time::sleep(gap).await;
        gap = (gap * 2).min(RESTART_MAX);
    }
}

/// Forward pod watcher events into the broadcast channel.
pub fn forward_pod(tx: &broadcast::Sender<PodEvent>, ev: &watcher::Event<Pod>) {
    let out = match ev {
        watcher::Event::Apply(p) => PodEvent::Apply(Arc::new(p.clone())),
        watcher::Event::Delete(p) => PodEvent::Delete(Arc::new(p.clone())),
        watcher::Event::InitDone => PodEvent::Resync,
        // InitApply is buffered by kube-runtime until InitDone; the Resync covers it.
        watcher::Event::Init | watcher::Event::InitApply(_) => return,
    };
    // Having no receivers is fine, because nobody is listening yet.
    let _ = tx.send(out);
}

/// Drive a single-object Secret watch into a `watch` channel.
pub async fn drive_secret<S>(
    events: S,
    tx: watch::Sender<SecretState>,
    on_error: impl Fn(&watcher::Error),
) where
    S: Stream<Item = watcher::Result<watcher::Event<Secret>>>,
{
    futures::pin_mut!(events);
    let mut seen = false;
    while let Some(item) = events.next().await {
        match item {
            Ok(watcher::Event::Init) => seen = false,
            Ok(watcher::Event::Apply(s) | watcher::Event::InitApply(s)) => {
                seen = true;
                tx.send_modify(|st| st.secret = Some(Arc::new(s)));
            }
            Ok(watcher::Event::Delete(_)) => tx.send_modify(|st| st.secret = None),
            Ok(watcher::Event::InitDone) => tx.send_modify(|st| {
                if !seen {
                    st.secret = None;
                }
                st.init_done = true;
            }),
            Err(e) => on_error(&e),
        }
    }
}

/// Whether the Pool and Pod stores are fresh: each finished a list and saw no
/// watch error since.
#[derive(Clone, Debug)]
pub struct StoreFreshness {
    pools: watch::Receiver<bool>,
    pods: watch::Receiver<bool>,
}

impl StoreFreshness {
    /// From the `pool_store` and `pod_store` readiness flags.
    #[must_use]
    pub fn new(pools: watch::Receiver<bool>, pods: watch::Receiver<bool>) -> Self {
        Self { pools, pods }
    }

    /// A fixed answer (tests).
    #[must_use]
    pub fn fixed(fresh: bool) -> Self {
        let rx = watch::channel(fresh).1;
        Self::new(rx.clone(), rx)
    }

    /// Both stores are fresh.
    #[must_use]
    pub fn is_fresh(&self) -> bool {
        *self.pools.borrow() && *self.pods.borrow()
    }
}

/// Everything the runtime shares.
#[derive(Clone)]
pub struct Stores {
    /// `BuildKitPool` store.
    pub pools: Store<BuildKitPool>,
    /// Worker pod store (label filtered).
    pub pods: Store<Pod>,
    /// Pod event broadcast.
    pub pod_events: broadcast::Sender<PodEvent>,
    /// CA Secret watch.
    pub ca_secret: watch::Receiver<SecretState>,
    /// User serving Secret watch (always `init_done` with no secret when none is configured).
    pub serving_secret: watch::Receiver<SecretState>,
}

/// Inputs of [`Stores::spawn`].
pub struct StoreSpec<'a> {
    /// API client.
    pub client: Client,
    /// `watchNamespace`, `""` = all.
    pub watch_namespace: &'a str,
    /// Controller namespace.
    pub namespace: &'a str,
    /// CA Secret name.
    pub ca_secret_name: &'a str,
    /// Serving Secret name, `""` = none.
    pub serving_secret_name: &'a str,
    /// Metrics.
    pub metrics: Arc<Metrics>,
    /// `pool_store` flag.
    pub pools_ready: ReadyFlag,
    /// `pod_store` flag.
    pub pods_ready: ReadyFlag,
}

fn scoped<K>(client: Client, ns: &str) -> Api<K>
where
    K: Resource<Scope = kube::core::NamespaceResourceScope>,
    K::DynamicType: Default,
{
    if ns.is_empty() {
        Api::all(client)
    } else {
        Api::namespaced(client, ns)
    }
}

fn secret_watch(
    client: Client,
    ns: &str,
    name: &str,
    metrics: Arc<Metrics>,
    kind: &'static str,
    shutdown: CancellationToken,
) -> watch::Receiver<SecretState> {
    let (tx, rx) = watch::channel(SecretState::default());
    if name.is_empty() {
        tx.send_replace(SecretState {
            init_done: true,
            secret: None,
        });
        return rx;
    }
    let api: Api<Secret> = Api::namespaced(client, ns);
    let cfg = watcher::Config::default().fields(&format!("metadata.name={name}"));
    let events = watcher(api, cfg).default_backoff();
    spawn_until(
        shutdown,
        drive_secret(events, tx, move |e| {
            warn!(error = %e, kind, "watch error");
            metrics.watch_error(kind);
        }),
    );
    rx
}

fn spawn_until(shutdown: CancellationToken, fut: impl Future<Output = ()> + Send + 'static) {
    tokio::spawn(async move {
        shutdown.run_until_cancelled(fut).await;
    });
}

fn strip<K: ResourceExt>(o: &mut K) {
    o.managed_fields_mut().clear();
}

impl Stores {
    /// Start the root watch tasks.
    #[must_use]
    pub fn spawn(spec: StoreSpec<'_>, shutdown: &CancellationToken) -> Self {
        let StoreSpec {
            client,
            watch_namespace,
            namespace,
            ca_secret_name,
            serving_secret_name,
            metrics,
            pools_ready,
            pods_ready,
        } = spec;

        let (pool_store, pool_writer) = reflector::store::<BuildKitPool>();
        let pools: Api<BuildKitPool> = scoped(client.clone(), watch_namespace);
        let m = Arc::clone(&metrics);
        let events = move || watcher(pools.clone(), watcher::Config::default()).modify(strip);
        spawn_until(
            shutdown.clone(),
            drive(
                pool_writer,
                events,
                pools_ready,
                move |e| {
                    warn!(error = %e, kind = "pool", "watch error");
                    m.watch_error("pool");
                },
                |_| {},
            ),
        );

        let (pod_store, pod_writer) = reflector::store::<Pod>();
        let (tx, _) = broadcast::channel(POD_EVENT_CAPACITY);
        let pod_api: Api<Pod> = scoped(client.clone(), watch_namespace);
        let cfg = watcher::Config::default()
            .labels(&format!("{LABEL_MANAGED_BY}={CONTROLLER},{LABEL_POOL}"));
        let events = move || watcher(pod_api.clone(), cfg.clone()).modify(strip);
        let m = Arc::clone(&metrics);
        let ptx = tx.clone();
        spawn_until(
            shutdown.clone(),
            drive(
                pod_writer,
                events,
                pods_ready,
                move |e| {
                    warn!(error = %e, kind = "pod", "watch error");
                    m.watch_error("pod");
                },
                move |ev| forward_pod(&ptx, ev),
            ),
        );

        let ca_secret = secret_watch(
            client.clone(),
            namespace,
            ca_secret_name,
            Arc::clone(&metrics),
            "ca_secret",
            shutdown.clone(),
        );
        let serving_secret = secret_watch(
            client,
            namespace,
            serving_secret_name,
            metrics,
            "serving_secret",
            shutdown.clone(),
        );
        Self {
            pools: pool_store,
            pods: pod_store,
            pod_events: tx,
            ca_secret,
            serving_secret,
        }
    }
}

#[cfg(test)]
#[path = "stores_tests.rs"]
mod tests;
