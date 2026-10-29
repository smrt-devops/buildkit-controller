//! Shared stores.
//!
//! One stable `reflector::store` per kind, driven by one root task per kind on
//! every replica. The root task consumes the reflector output itself (which
//! carries `Delete`) and forwards pod events on a `broadcast` channel; it never
//! uses `store_shared`/`Writer::subscribe`. A receiver that lags is told to
//! rescan with [`Event::Resync`].

use std::collections::{HashSet, VecDeque};
use std::fmt::Debug;
use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, BoxStream};
use futures::{Stream, StreamExt};
use k8s_openapi::api::core::v1::{Pod, Secret};
use kube::api::Api;
use kube::core::NamespaceResourceScope;
use kube::runtime::reflector::{self, Store};
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Client, Resource, ResourceExt};
use serde::de::DeserializeOwned;
use tokio::sync::{broadcast, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use buildkit_controller_api::claim::BuildKitClaim;
use buildkit_controller_api::v1alpha1::BuildKitPool;

use super::metrics::Metrics;
use super::readiness::{Readiness, ReadyFlag};
use crate::render::{CONTROLLER, LABEL_MANAGED_BY, LABEL_POOL};

/// Capacity of the pod and claim event broadcasts.
pub const POD_EVENT_CAPACITY: usize = 1024;

/// A change to a watched object, applied to the store before it is broadcast.
#[derive(Clone, Debug)]
pub enum Event<K> {
    /// Added or modified.
    Apply(Arc<K>),
    /// Deleted.
    Delete(Arc<K>),
    /// A relist completed, or this receiver lagged. Rescan the whole store.
    Resync,
}

/// A worker pod change.
pub type PodEvent = Event<Pod>;
/// A `BuildKitClaim` change.
pub type ClaimEvent = Event<BuildKitClaim>;

/// A subscription to store events that turns lag into [`Event::Resync`].
#[derive(Debug)]
pub struct Subscription<K>(broadcast::Receiver<Event<K>>);

/// Pod events.
pub type PodSubscription = Subscription<Pod>;
/// Claim events.
pub type ClaimSubscription = Subscription<BuildKitClaim>;

impl<K: Clone> Subscription<K> {
    /// Wrap a receiver.
    #[must_use]
    pub fn new(rx: broadcast::Receiver<Event<K>>) -> Self {
        Self(rx)
    }

    /// Next event; `None` when the runtime has shut down. After `Lagged`
    /// returns `Resync`; the caller rescans its store.
    pub async fn recv(&mut self) -> Option<Event<K>> {
        match self.0.recv().await {
            Ok(ev) => Some(ev),
            Err(broadcast::error::RecvError::Lagged(_)) => Some(Event::Resync),
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
/// How long a store that listed once may go without a working watch before
/// its readiness flag clears.
///
/// A watch error means the store may miss changes from then on, until a
/// restarted watch completes a full list. Within this bound the store keeps
/// serving its last list: `/readyz` stays 200 and the edge still admits. An
/// API server blip reaches every replica at once, so failing at the first
/// error would empty the Service and refuse every new edge connection on all
/// replicas together. The restart backoff fits five relist attempts (after 1,
/// 3, 7, 15 and 31 s) inside the bound, so an outage of up to about 30 s
/// recovers without the flag clearing.
///
/// What staleness can let through inside the bound is narrow. Workers are
/// single-use, so a stale entry can only show an allocation that was released
/// or a pool that was deleted, and the client it admits is the one that
/// already held that worker. Expiry is checked against the clock, not a
/// watch event. A stale IP that now belongs to another tenant's pod fails the
/// upstream handshake, because that pod's serving certificate names a
/// different pod and nonce.
pub const STALE_AFTER: Duration = Duration::from_mins(1);

/// Restart `watch()` after each error: yield the error, wait a backoff, then
/// start a new watch (a full relist), because kube's watcher resumes a failed
/// watch without a relist and so never reports when the stream caught up. The
/// backoff starts at [`RESTART_MIN`], doubles to [`RESTART_MAX`] and resets at
/// each `InitDone`.
///
/// Ends when a watch ends (only test streams do).
pub fn restarting<K, S>(
    watch: impl FnMut() -> S + Send + 'static,
) -> impl Stream<Item = watcher::Result<watcher::Event<K>>> + Send + 'static
where
    K: Send + 'static,
    S: Stream<Item = watcher::Result<watcher::Event<K>>> + Send + 'static,
{
    struct State<F, S> {
        watch: F,
        events: Option<std::pin::Pin<Box<S>>>,
        gap: Duration,
        /// Wait this long before the next watch.
        pause: Option<Duration>,
    }
    let state = State {
        watch,
        events: None,
        gap: RESTART_MIN,
        pause: None,
    };
    stream::unfold(state, |mut st| async move {
        if let Some(p) = st.pause.take() {
            tokio::time::sleep(p).await;
        }
        let events = st.events.get_or_insert_with(|| Box::pin((st.watch)()));
        let item = events.next().await?;
        match &item {
            Ok(watcher::Event::InitDone) => st.gap = RESTART_MIN,
            Err(_) => {
                st.events = None;
                st.pause = Some(st.gap);
                st.gap = (st.gap * 2).min(RESTART_MAX);
            }
            Ok(_) => {}
        }
        Some((item, st))
    })
}

/// Drive a store from `events`, a watch that restarts itself after an error
/// (see [`restarting`] and [`tolerant_watch`]). It applies events, taps them,
/// and sets `ready` at each `InitDone`.
///
/// The reflector writer buffers a relist and swaps it in at `InitDone`, so
/// the store keeps its last list meanwhile.
///
/// `ready` means the store is usable for decisions:
/// - It is false until the first `InitDone`, with no grace period.
/// - A watch error leaves it set. It clears once [`STALE_AFTER`] has passed
///   since the first error without a completed relist; errors during the
///   relist do not restart that clock.
/// - The next `InitDone` sets it again.
///
/// A watch that hangs without an error becomes one: the kube client fails a
/// request that receives nothing for [`READ_TIMEOUT`](super::READ_TIMEOUT), so
/// a stall clears `ready` at most `READ_TIMEOUT + STALE_AFTER` after the API
/// server last sent anything.
///
/// Returns when `events` ends (only test streams do).
pub async fn drive<K, S>(
    mut writer: reflector::store::Writer<K>,
    events: S,
    ready: ReadyFlag,
    on_error: impl Fn(&watcher::Error),
    mut tap: impl FnMut(&watcher::Event<K>),
) where
    K: Resource + Clone + 'static,
    K::DynamicType: Clone + Eq + std::hash::Hash + Default,
    S: Stream<Item = watcher::Result<watcher::Event<K>>>,
{
    // When the store last stopped being known current; `None` while the
    // latest watch has listed and not failed since.
    let mut failed_at: Option<Instant> = None;
    futures::pin_mut!(events);
    loop {
        let stale_at = failed_at.map(|t| t + STALE_AFTER);
        match clear_at(events.next(), stale_at, &ready).await {
            None => return,
            Some(Ok(ev)) => {
                writer.apply_watcher_event(&ev);
                if matches!(ev, watcher::Event::InitDone) {
                    ready.set();
                    failed_at = None;
                }
                tap(&ev);
            }
            Some(Err(e)) => {
                on_error(&e);
                failed_at.get_or_insert_with(Instant::now);
            }
        }
    }
}

/// Await `fut`. If `deadline` passes first, clear `ready` and keep waiting.
async fn clear_at<T>(
    fut: impl Future<Output = T>,
    deadline: Option<Instant>,
    ready: &ReadyFlag,
) -> T {
    futures::pin_mut!(fut);
    if let Some(at) = deadline {
        tokio::select! {
            out = &mut fut => return out,
            () = tokio::time::sleep_until(at) => ready.clear(),
        }
    }
    fut.await
}

/// Forward watcher events into the broadcast channel.
pub fn forward<K: Clone>(tx: &broadcast::Sender<Event<K>>, ev: &watcher::Event<K>) {
    let out = match ev {
        watcher::Event::Apply(p) => Event::Apply(Arc::new(p.clone())),
        watcher::Event::Delete(p) => Event::Delete(Arc::new(p.clone())),
        watcher::Event::InitDone => Event::Resync,
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

/// Whether the Pool and Pod stores can be trusted for edge decisions. It reads
/// their readiness flags, so it agrees with `/readyz`: each store listed once
/// and has not gone [`STALE_AFTER`] without a working watch (see [`drive`]).
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
    /// `BuildKitClaim` store.
    pub claims: Store<BuildKitClaim>,
    /// Claim event broadcast.
    pub claim_events: broadcast::Sender<ClaimEvent>,
    /// CA Secret watch.
    pub ca_secret: watch::Receiver<SecretState>,
    /// User serving Secret watch (always `init_done` with no secret when none is configured).
    pub serving_secret: watch::Receiver<SecretState>,
}

/// Inputs of [`Stores::spawn`].
pub struct StoreSpec<'a> {
    /// API client.
    pub client: Client,
    /// Watched namespaces, `[]` = all.
    pub watch_namespaces: &'a [String],
    /// Namespaces whose claims are watched, `[]` = all.
    pub claim_namespaces: &'a [String],
    /// Controller namespace.
    pub namespace: &'a str,
    /// CA Secret name.
    pub ca_secret_name: &'a str,
    /// Serving Secret name, `""` = none.
    pub serving_secret_name: &'a str,
    /// Metrics.
    pub metrics: Arc<Metrics>,
    /// Readiness: the `pool_store` and `pod_store` checks, and the namespaces
    /// the watches gave up on.
    pub readiness: Arc<Readiness>,
}

/// A watch of `K` in `namespaces`, `[]` = all namespaces. More than one
/// namespace runs one watcher per namespace, merged by [`merge_namespaces`];
/// `store` is the reader of the store the events are applied to.
///
/// The merged watch completes its first list only once every namespace has
/// listed, so one namespace that cannot be listed holds it forever. Use it
/// where acting on a partial list is unsafe (the claim sweep releases workers
/// of claims missing from its store); elsewhere use [`tolerant_watch`].
#[must_use]
pub fn scoped_watch<K>(
    client: Client,
    namespaces: &[String],
    cfg: &watcher::Config,
    store: Store<K>,
) -> BoxStream<'static, watcher::Result<watcher::Event<K>>>
where
    K: Resource<Scope = NamespaceResourceScope>
        + Clone
        + DeserializeOwned
        + Debug
        + Send
        + Sync
        + 'static,
    K::DynamicType: Default + Clone + Eq + Hash,
{
    match namespaces {
        [] => watcher(Api::all(client), cfg.clone()).boxed(),
        [ns] => watcher(Api::namespaced(client, ns), cfg.clone()).boxed(),
        many => {
            let tagged = stream::select_all(many.iter().enumerate().map(|(i, ns)| {
                watcher(Api::<K>::namespaced(client.clone(), ns), cfg.clone())
                    .map(move |ev| (i, ev))
                    .boxed()
            }));
            merge_namespaces(tagged.boxed(), many.to_vec(), store).boxed()
        }
    }
}

/// How long a namespace of a [`tolerant_watch`] may fail without a completed
/// list before the watch carries on without it. Below [`STALE_AFTER`], so the
/// store is whole again before an error in that namespace clears its
/// readiness; the restart backoff fits five relists in it.
pub const GIVE_UP_AFTER: Duration = Duration::from_secs(30);

/// Where a [`tolerant_watch`] reports a namespace it gave up on, and its return.
#[derive(Clone)]
pub struct NamespaceReport {
    kind: &'static str,
    metrics: Option<Arc<Metrics>>,
    readiness: Option<Arc<Readiness>>,
}

impl NamespaceReport {
    /// Log only; `kind` names the watch in the log.
    #[must_use]
    pub fn log(kind: &'static str) -> Self {
        Self {
            kind,
            metrics: None,
            readiness: None,
        }
    }

    /// Log, set `buildkit_watch_namespace_unlisted{kind,namespace}`, and list
    /// `<kind>_store/<namespace>` as degraded on `/readyz`.
    #[must_use]
    pub fn full(kind: &'static str, metrics: Arc<Metrics>, readiness: Arc<Readiness>) -> Self {
        Self {
            kind,
            metrics: Some(metrics),
            readiness: Some(readiness),
        }
    }

    fn set(&self, namespace: &str, unlisted: bool) {
        if let Some(m) = &self.metrics {
            m.set_namespace_unlisted(self.kind, namespace, unlisted);
        }
        if let Some(r) = &self.readiness {
            r.set_degraded(&format!("{}_store/{namespace}", self.kind), unlisted);
        }
    }

    fn unlisted(&self, namespace: &str, error: &str) {
        warn!(
            kind = self.kind,
            namespace, error, "namespace cannot be listed; serving the other namespaces without it"
        );
        self.set(namespace, true);
    }

    fn listed(&self, namespace: &str) {
        info!(kind = self.kind, namespace, "namespace listed again");
        self.set(namespace, false);
    }
}

/// A watch of `K` in `namespaces`, `[]` = all namespaces, that restarts itself
/// after an error (see [`restarting`]). With more than one namespace each one
/// restarts on its own, and a namespace that fails for [`GIVE_UP_AFTER`]
/// without completing a list no longer holds the others:
///
/// - The first list completes once every namespace has listed or been given
///   up, and at least one listed. Until then the store stays not ready.
/// - The errors of a namespace given up are dropped (logged at debug), so they
///   do not count against the store's readiness, and `report` is told.
/// - Objects of a namespace given up stay in the store as last listed. Dropping
///   them would show the reconciler a pool without its pods whenever the Pool
///   and Pod watches lose or regain the namespace at different times.
/// - When it lists again it is merged back and `report` is told.
///
/// Whenever every namespace is current or given up again, the stream sends
/// the whole store as a fresh list (`Init` .. `InitDone`), so the store's
/// readiness follows the namespaces that can be listed.
#[must_use]
pub fn tolerant_watch<K>(
    client: Client,
    namespaces: &[String],
    cfg: &watcher::Config,
    store: Store<K>,
    report: NamespaceReport,
) -> BoxStream<'static, watcher::Result<watcher::Event<K>>>
where
    K: Resource<Scope = NamespaceResourceScope>
        + Clone
        + DeserializeOwned
        + Debug
        + Send
        + Sync
        + 'static,
    K::DynamicType: Default + Clone + Eq + Hash,
{
    let one = |api: Api<K>, cfg: &watcher::Config| {
        let cfg = cfg.clone();
        restarting(move || watcher(api.clone(), cfg.clone()))
    };
    match namespaces {
        [] => one(Api::all(client), cfg).boxed(),
        [ns] => one(Api::namespaced(client, ns), cfg).boxed(),
        many => {
            let tagged = stream::select_all(many.iter().enumerate().map(|(i, ns)| {
                one(Api::namespaced(client.clone(), ns), cfg)
                    .map(move |ev| (i, ev))
                    .boxed()
            }));
            merge_tolerant(tagged.boxed(), many.to_vec(), store, report).boxed()
        }
    }
}

/// Merge watch streams of single namespaces into one stream that a single
/// reflector writer can apply. Each item is tagged with its index in
/// `namespaces`.
///
/// The writer replaces its whole store at `InitDone`, so the merged stream
/// sends one `Init`, `InitApply`.., `InitDone` once every namespace finished
/// its first list, and holds earlier events until then. A later relist of one
/// namespace becomes `Apply` for each listed object and `Delete` for each
/// object `store` holds in that namespace that the list lacked. Errors pass
/// through unchanged.
pub fn merge_namespaces<K>(
    tagged: BoxStream<'static, (usize, watcher::Result<watcher::Event<K>>)>,
    namespaces: Vec<String>,
    store: Store<K>,
) -> impl Stream<Item = watcher::Result<watcher::Event<K>>> + Send + 'static
where
    K: Resource + Clone + Send + Sync + 'static,
    K::DynamicType: Default + Clone + Eq + Hash,
{
    Merge::new(namespaces, store, None).run(tagged)
}

/// [`merge_namespaces`] that gives up on a namespace failing for
/// [`GIVE_UP_AFTER`]; see [`tolerant_watch`]. Each tagged stream must restart
/// itself after an error.
pub fn merge_tolerant<K>(
    tagged: BoxStream<'static, (usize, watcher::Result<watcher::Event<K>>)>,
    namespaces: Vec<String>,
    store: Store<K>,
    report: NamespaceReport,
) -> impl Stream<Item = watcher::Result<watcher::Event<K>>> + Send + 'static
where
    K: Resource + Clone + Send + Sync + 'static,
    K::DynamicType: Default + Clone + Eq + Hash,
{
    Merge::new(namespaces, store, Some(report)).run(tagged)
}

/// One namespace of a merged watch.
struct Namespace<K> {
    name: String,
    /// The objects of a list in progress.
    listing: Option<Vec<K>>,
    /// Its first list completed.
    listed: bool,
    /// Listed, and no error since.
    current: bool,
    /// The first error since it was last current.
    failing_since: Option<Instant>,
    last_error: String,
    /// Left out of the store's readiness (tolerant merges only).
    given_up: bool,
}

struct Merge<K>
where
    K: Resource + 'static,
    K::DynamicType: Hash + Eq,
{
    ns: Vec<Namespace<K>>,
    /// Objects held until the first list completes; `None` afterwards.
    initial: Option<Vec<K>>,
    store: Store<K>,
    out: VecDeque<watcher::Result<watcher::Event<K>>>,
    /// `Some` for a tolerant merge.
    report: Option<NamespaceReport>,
}

fn same<K: Resource>(a: &K, b: &K) -> bool {
    a.meta().name == b.meta().name && a.meta().namespace == b.meta().namespace
}

impl<K> Merge<K>
where
    K: Resource + Clone + Send + Sync + 'static,
    K::DynamicType: Default + Clone + Eq + Hash,
{
    fn new(namespaces: Vec<String>, store: Store<K>, report: Option<NamespaceReport>) -> Self {
        Self {
            ns: namespaces
                .into_iter()
                .map(|name| Namespace {
                    name,
                    listing: None,
                    listed: false,
                    current: false,
                    failing_since: None,
                    last_error: String::new(),
                    given_up: false,
                })
                .collect(),
            initial: Some(Vec::new()),
            store,
            out: VecDeque::new(),
            report,
        }
    }

    fn run(
        self,
        tagged: BoxStream<'static, (usize, watcher::Result<watcher::Event<K>>)>,
    ) -> impl Stream<Item = watcher::Result<watcher::Event<K>>> + Send + 'static {
        stream::unfold((tagged, self), |(mut tagged, mut m)| async move {
            loop {
                if let Some(ev) = m.out.pop_front() {
                    return Some((ev, (tagged, m)));
                }
                let give_up = m.next_give_up();
                tokio::select! {
                    item = tagged.next() => {
                        let (i, item) = item?;
                        m.handle(i, item);
                    }
                    () = sleep_until(give_up) => m.give_up(Instant::now()),
                }
            }
        })
    }

    fn handle(&mut self, i: usize, item: watcher::Result<watcher::Event<K>>) {
        let ev = match item {
            Ok(ev) => ev,
            Err(e) => {
                self.failed(i, e);
                return;
            }
        };
        match ev {
            watcher::Event::Init => {
                if let Some(n) = self.ns.get_mut(i) {
                    n.listing = Some(Vec::new());
                }
            }
            watcher::Event::InitApply(o) => match self.ns.get_mut(i) {
                Some(Namespace {
                    listing: Some(l), ..
                }) => l.push(o),
                _ => self.apply(o),
            },
            watcher::Event::InitDone => {
                let objs = self
                    .ns
                    .get_mut(i)
                    .and_then(|n| n.listing.take())
                    .unwrap_or_default();
                self.list_done(i, objs);
            }
            watcher::Event::Apply(o) => self.apply(o),
            watcher::Event::Delete(o) => self.delete(o),
        }
    }

    fn failed(&mut self, i: usize, e: watcher::Error) {
        let tolerant = self.report.is_some();
        let Some(n) = self.ns.get_mut(i) else {
            return;
        };
        n.current = false;
        if tolerant && n.given_up {
            debug!(namespace = %n.name, error = %e, "watch error in a namespace given up");
            return;
        }
        n.failing_since.get_or_insert_with(Instant::now);
        n.last_error = e.to_string();
        self.out.push_back(Err(e));
    }

    fn apply(&mut self, o: K) {
        match &mut self.initial {
            Some(held) => {
                held.retain(|x| !same(x, &o));
                held.push(o);
            }
            None => self.out.push_back(Ok(watcher::Event::Apply(o))),
        }
    }

    fn delete(&mut self, o: K) {
        match &mut self.initial {
            Some(held) => held.retain(|x| !same(x, &o)),
            None => self.out.push_back(Ok(watcher::Event::Delete(o))),
        }
    }

    /// Every namespace is current or given up, and one is current.
    fn whole(&self) -> bool {
        self.ns.iter().all(|n| n.current || n.given_up) && self.ns.iter().any(|n| n.current)
    }

    fn list_done(&mut self, i: usize, objs: Vec<K>) {
        let Some(n) = self.ns.get_mut(i) else {
            return;
        };
        n.listed = true;
        n.current = true;
        n.failing_since = None;
        if std::mem::take(&mut n.given_up)
            && let Some(r) = &self.report
        {
            r.listed(&n.name);
        }
        let ns = n.name.clone();
        if let Some(held) = &mut self.initial {
            held.retain(|x| x.meta().namespace.as_ref() != Some(&ns));
            held.extend(objs);
            self.release();
            return;
        }
        if self.report.is_some() && self.whole() {
            let others = self
                .store
                .state()
                .into_iter()
                .filter(|s| s.meta().namespace.as_ref() != Some(&ns))
                .map(|s| K::clone(&s));
            let all: Vec<K> = others.chain(objs).collect();
            self.relist(all);
            return;
        }
        let names: HashSet<&str> = objs
            .iter()
            .filter_map(|o| o.meta().name.as_deref())
            .collect();
        let stale: Vec<K> = self
            .store
            .state()
            .iter()
            .filter(|s| {
                s.meta().namespace.as_ref() == Some(&ns)
                    && !s.meta().name.as_deref().is_some_and(|n| names.contains(n))
            })
            .map(|s| K::clone(s))
            .collect();
        self.out
            .extend(objs.into_iter().map(|o| Ok(watcher::Event::Apply(o))));
        self.out
            .extend(stale.into_iter().map(|o| Ok(watcher::Event::Delete(o))));
    }

    /// Send the held objects as the first list once every namespace listed
    /// (or, when tolerant, was given up, with at least one listed).
    fn release(&mut self) {
        let ready =
            self.ns.iter().all(|n| n.listed || n.given_up) && self.ns.iter().any(|n| !n.given_up);
        if ready && let Some(all) = self.initial.take() {
            self.relist(all);
        }
    }

    /// `objs` as one full list.
    fn relist(&mut self, objs: Vec<K>) {
        self.out.push_back(Ok(watcher::Event::Init));
        self.out
            .extend(objs.into_iter().map(|o| Ok(watcher::Event::InitApply(o))));
        self.out.push_back(Ok(watcher::Event::InitDone));
    }

    /// The earliest time a failing namespace is given up (tolerant only).
    fn next_give_up(&self) -> Option<Instant> {
        self.report.as_ref()?;
        self.ns
            .iter()
            .filter(|n| !n.given_up)
            .filter_map(|n| n.failing_since)
            .min()
            .map(|t| t + GIVE_UP_AFTER)
    }

    fn give_up(&mut self, now: Instant) {
        let Some(report) = &self.report else {
            return;
        };
        let mut any = false;
        for n in &mut self.ns {
            if !n.given_up && n.failing_since.is_some_and(|t| t + GIVE_UP_AFTER <= now) {
                n.given_up = true;
                n.listing = None;
                report.unlisted(&n.name, &n.last_error);
                any = true;
            }
        }
        if !any {
            return;
        }
        if self.initial.is_some() {
            self.release();
        } else if self.whole() {
            let all = self.store.state().iter().map(|s| K::clone(s)).collect();
            self.relist(all);
        }
    }
}

/// Sleep until `at`; never when `None`.
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
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

/// The claim store and its event broadcast. Every allocation is a claim. The
/// watch is strict, because the leader releases workers of claims missing from
/// the store, so a partial list must never count as synced.
fn claim_watch(
    client: &Client,
    claim_namespaces: &[String],
    metrics: &Arc<Metrics>,
    readiness: &Arc<Readiness>,
    shutdown: &CancellationToken,
) -> (Store<BuildKitClaim>, broadcast::Sender<ClaimEvent>) {
    let (claim_store, claim_writer) = reflector::store::<BuildKitClaim>();
    let (claim_tx, _) = broadcast::channel(POD_EVENT_CAPACITY);
    let events = scoped_watch(
        client.clone(),
        claim_namespaces,
        &watcher::Config::default(),
        claim_store.clone(),
    )
    .default_backoff()
    .modify(strip);
    let m = Arc::clone(metrics);
    let ctx = claim_tx.clone();
    spawn_until(
        shutdown.clone(),
        drive(
            claim_writer,
            events,
            readiness.register("claim_store"),
            move |e| {
                warn!(error = %e, kind = "claim", "watch error");
                m.watch_error("claim");
            },
            move |ev| forward(&ctx, ev),
        ),
    );
    (claim_store, claim_tx)
}

/// The worker stores: every `BuildKitPool` and every controller-managed pod
/// in `namespaces` (`[]` = all), with the `pool_store` and `pod_store`
/// readiness checks, and the pod event broadcast.
#[must_use]
pub fn worker_watches(
    client: &Client,
    namespaces: &[String],
    metrics: &Arc<Metrics>,
    readiness: &Arc<Readiness>,
    shutdown: &CancellationToken,
) -> (Store<BuildKitPool>, Store<Pod>, broadcast::Sender<PodEvent>) {
    let report = |kind| NamespaceReport::full(kind, Arc::clone(metrics), Arc::clone(readiness));

    let (pool_store, pool_writer) = reflector::store::<BuildKitPool>();
    let events = tolerant_watch(
        client.clone(),
        namespaces,
        &watcher::Config::default(),
        pool_store.clone(),
        report("pool"),
    )
    .modify(strip);
    let m = Arc::clone(metrics);
    spawn_until(
        shutdown.clone(),
        drive(
            pool_writer,
            events,
            readiness.register("pool_store"),
            move |e| {
                warn!(error = %e, kind = "pool", "watch error");
                m.watch_error("pool");
            },
            |_| {},
        ),
    );

    let (pod_store, pod_writer) = reflector::store::<Pod>();
    let (tx, _) = broadcast::channel(POD_EVENT_CAPACITY);
    let cfg =
        watcher::Config::default().labels(&format!("{LABEL_MANAGED_BY}={CONTROLLER},{LABEL_POOL}"));
    let events = tolerant_watch(
        client.clone(),
        namespaces,
        &cfg,
        pod_store.clone(),
        report("pod"),
    )
    .modify(strip);
    let m = Arc::clone(metrics);
    let ptx = tx.clone();
    spawn_until(
        shutdown.clone(),
        drive(
            pod_writer,
            events,
            readiness.register("pod_store"),
            move |e| {
                warn!(error = %e, kind = "pod", "watch error");
                m.watch_error("pod");
            },
            move |ev| forward(&ptx, ev),
        ),
    );
    (pool_store, pod_store, tx)
}

impl Stores {
    /// Start the root watch tasks.
    #[must_use]
    pub fn spawn(spec: StoreSpec<'_>, shutdown: &CancellationToken) -> Self {
        let StoreSpec {
            client,
            watch_namespaces,
            claim_namespaces,
            namespace,
            ca_secret_name,
            serving_secret_name,
            metrics,
            readiness,
        } = spec;
        let (pool_store, pod_store, tx) =
            worker_watches(&client, watch_namespaces, &metrics, &readiness, shutdown);

        let (claim_store, claim_tx) =
            claim_watch(&client, claim_namespaces, &metrics, &readiness, shutdown);

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
            claims: claim_store,
            claim_events: claim_tx,
            ca_secret,
            serving_secret,
        }
    }
}

#[cfg(test)]
#[path = "stores_tests.rs"]
mod tests;
