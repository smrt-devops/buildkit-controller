use futures::channel::mpsc;
use futures::stream;
use k8s_openapi::api::core::v1::{Pod, Secret};
use kube::api::ObjectMeta;
use kube::runtime::reflector;
use kube::runtime::watcher::Event;
use tokio::sync::broadcast;

use super::*;
use crate::runtime::metrics::Metrics;
use crate::runtime::readiness::Readiness;

fn pod(name: &str) -> Pod {
    Pod {
        metadata: ObjectMeta {
            name: Some(name.into()),
            namespace: Some("ns".into()),
            ..ObjectMeta::default()
        },
        ..Pod::default()
    }
}

/// A watch factory that yields `events` once; a second watch ends at once.
fn once<K>(
    events: Vec<watcher::Result<Event<K>>>,
) -> impl FnMut() -> stream::Iter<std::vec::IntoIter<watcher::Result<Event<K>>>> {
    let mut events = Some(events);
    move || stream::iter(events.take().unwrap_or_default())
}

fn name_of(p: &Pod) -> String {
    p.metadata.name.clone().unwrap_or_default()
}

async fn run_pods(
    events: Vec<Event<Pod>>,
    cap: usize,
) -> (Store<Pod>, broadcast::Receiver<PodEvent>, Arc<Readiness>) {
    let (store, writer) = reflector::store::<Pod>();
    let (tx, rx) = broadcast::channel(cap);
    let readiness = Arc::new(Readiness::default());
    let flag = readiness.register("pod_store");
    drive(
        writer,
        restarting(once(events.into_iter().map(Ok).collect())),
        flag,
        |_| {},
        move |ev| forward(&tx, ev),
    )
    .await;
    (store, rx, readiness)
}

#[tokio::test]
async fn delete_events_reach_subscribers_after_the_store_applied_them() {
    let events = vec![
        Event::Init,
        Event::InitApply(pod("a")),
        Event::InitDone,
        Event::Apply(pod("b")),
        Event::Delete(pod("a")),
    ];
    let (store, rx, readiness) = run_pods(events, 16).await;
    let mut sub = PodSubscription::new(rx);
    let mut seen = Vec::new();
    while let Ok(ev) = sub.0.try_recv() {
        seen.push(match ev {
            PodEvent::Apply(p) => format!("apply {}", name_of(&p)),
            PodEvent::Delete(p) => format!("delete {}", name_of(&p)),
            PodEvent::Resync => "resync".into(),
        });
    }
    assert_eq!(seen, ["resync", "apply b", "delete a"]);
    let names: Vec<_> = store.state().iter().map(|p| name_of(p)).collect();
    assert_eq!(names, ["b"], "store applied the delete");
    assert!(readiness.failing().is_empty(), "InitDone flips the flag");
}

#[tokio::test]
async fn not_ready_before_init_done() {
    let (_, _, readiness) = run_pods(vec![Event::Init, Event::InitApply(pod("a"))], 4).await;
    assert_eq!(readiness.failing(), ["pod_store"]);
}

#[tokio::test]
async fn a_lagged_receiver_is_told_to_rescan() {
    let mut events = vec![Event::Init, Event::InitDone];
    events.extend((0..10).map(|i| Event::Apply(pod(&format!("p{i}")))));
    let (store, rx, _) = run_pods(events, 2).await;
    let mut sub = PodSubscription::new(rx);
    assert!(
        matches!(sub.recv().await, Some(PodEvent::Resync)),
        "lag becomes a rescan"
    );
    assert_eq!(store.state().len(), 10, "the rescan target is complete");
    assert!(
        matches!(sub.recv().await, Some(PodEvent::Apply(_))),
        "newest events still arrive"
    );
}

#[tokio::test]
async fn secret_watch_tracks_apply_delete_and_init() {
    let secret = |n: &str| Secret {
        metadata: ObjectMeta {
            name: Some(n.into()),
            ..ObjectMeta::default()
        },
        ..Secret::default()
    };
    let (tx, rx) = watch::channel(SecretState::default());
    let evs = vec![
        Ok(Event::Init),
        Ok(Event::InitDone),
        Ok(Event::Apply(secret("ca"))),
    ];
    drive_secret(stream::iter(evs), tx.clone(), |_| {}).await;
    let st = rx.borrow().clone();
    assert!(st.init_done && st.secret.is_some());
    drive_secret(
        stream::iter(vec![Ok(Event::Delete(secret("ca")))]),
        tx,
        |_| {},
    )
    .await;
    assert!(rx.borrow().secret.is_none());
}

fn pod_in(ns: &str, name: &str) -> Pod {
    let mut p = pod(name);
    p.metadata.namespace = Some(ns.into());
    p
}

fn key(p: &Pod) -> String {
    format!(
        "{}/{}",
        p.metadata.namespace.as_deref().unwrap_or(""),
        name_of(p)
    )
}

type Tagged = (usize, watcher::Result<Event<Pod>>);

/// Drive a store from the merge of namespaces `a` (0) and `b` (1). Returns the
/// store, each event the writer applied, the readiness, and the error count.
async fn run_merged(input: Vec<Tagged>) -> (Store<Pod>, Vec<String>, Arc<Readiness>, usize) {
    let (store, writer) = reflector::store::<Pod>();
    let readiness = Arc::new(Readiness::default());
    let flag = readiness.register("pod_store");
    let merged = merge_namespaces(
        stream::iter(input).boxed(),
        vec!["a".into(), "b".into()],
        store.clone(),
    );
    let mut seen = Vec::new();
    let errors = std::sync::atomic::AtomicUsize::new(0);
    drive(
        writer,
        merged,
        flag,
        |_| {
            errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        },
        |ev| {
            seen.push(match ev {
                Event::Init => "init".into(),
                Event::InitApply(p) => format!("init-apply {}", key(p)),
                Event::InitDone => "init-done".into(),
                Event::Apply(p) => format!("apply {}", key(p)),
                Event::Delete(p) => format!("delete {}", key(p)),
            });
        },
    )
    .await;
    let errors = errors.into_inner();
    (store, seen, readiness, errors)
}

fn keys(store: &Store<Pod>) -> Vec<String> {
    let mut k: Vec<_> = store.state().iter().map(|p| key(p)).collect();
    k.sort();
    k
}

#[tokio::test]
async fn merged_store_is_not_ready_until_every_namespace_listed() {
    let (store, seen, readiness, _) = run_merged(vec![
        (0, Ok(Event::Init)),
        (0, Ok(Event::InitApply(pod_in("a", "p1")))),
        (0, Ok(Event::InitDone)),
        (0, Ok(Event::Apply(pod_in("a", "p2")))),
        (1, Ok(Event::Init)),
    ])
    .await;
    assert!(seen.is_empty(), "{seen:?}");
    assert!(keys(&store).is_empty());
    assert_eq!(readiness.failing(), ["pod_store"]);
}

#[tokio::test]
async fn merged_initial_lists_become_one_list() {
    let (store, seen, readiness, _) = run_merged(vec![
        (0, Ok(Event::Init)),
        (0, Ok(Event::InitApply(pod_in("a", "p1")))),
        (1, Ok(Event::Init)),
        (0, Ok(Event::InitDone)),
        // `a` is watching while `b` still lists: these are held, not lost.
        (0, Ok(Event::Apply(pod_in("a", "p2")))),
        (0, Ok(Event::Delete(pod_in("a", "p1")))),
        (1, Ok(Event::InitApply(pod_in("b", "q1")))),
        (1, Ok(Event::InitDone)),
        (1, Ok(Event::Apply(pod_in("b", "q2")))),
    ])
    .await;
    assert_eq!(
        seen,
        [
            "init",
            "init-apply a/p2",
            "init-apply b/q1",
            "init-done",
            "apply b/q2"
        ]
    );
    assert_eq!(keys(&store), ["a/p2", "b/q1", "b/q2"]);
    assert!(readiness.failing().is_empty());
}

#[tokio::test]
async fn a_relist_of_one_namespace_replaces_only_its_objects() {
    let (store, seen, _, _) = run_merged(vec![
        (0, Ok(Event::Init)),
        (0, Ok(Event::InitApply(pod_in("a", "p1")))),
        (0, Ok(Event::InitDone)),
        (1, Ok(Event::Init)),
        (1, Ok(Event::InitApply(pod_in("b", "q1")))),
        (1, Ok(Event::InitApply(pod_in("b", "q2")))),
        (1, Ok(Event::InitDone)),
        // `b` relists (a desync): q1 is gone, q3 is new.
        (1, Ok(Event::Init)),
        (1, Ok(Event::InitApply(pod_in("b", "q2")))),
        (1, Ok(Event::InitApply(pod_in("b", "q3")))),
        (1, Ok(Event::InitDone)),
    ])
    .await;
    assert_eq!(
        seen[5..],
        ["apply b/q2", "apply b/q3", "delete b/q1"],
        "{seen:?}"
    );
    assert_eq!(keys(&store), ["a/p1", "b/q2", "b/q3"]);
}

#[tokio::test(start_paused = true)]
async fn merged_errors_pass_through() {
    let (_, _, readiness, errors) = run_merged(vec![
        (0, Ok(Event::Init)),
        (0, Ok(Event::InitDone)),
        (1, Ok(Event::Init)),
        (1, Ok(Event::InitDone)),
        (1, Err(watcher::Error::NoResourceVersion)),
    ])
    .await;
    assert_eq!(errors, 1);
    assert!(
        readiness.failing().is_empty(),
        "an error alone keeps the store ready"
    );
}
type Item = watcher::Result<Event<Pod>>;

/// `n` watches fed by hand: the factory hands out one per (re)start, in
/// order, and a watch that never yields after the last.
fn fed(
    n: usize,
) -> (
    Vec<mpsc::UnboundedSender<Item>>,
    impl FnMut() -> BoxStream<'static, Item>,
) {
    let (txs, rxs): (Vec<_>, Vec<_>) = (0..n).map(|_| mpsc::unbounded()).unzip();
    let mut rxs = rxs.into_iter();
    let watch = move || {
        rxs.next()
            .map_or_else(|| stream::pending().boxed(), StreamExt::boxed)
    };
    (txs, watch)
}

fn send<T>(tx: &mpsc::UnboundedSender<T>, items: impl IntoIterator<Item = T>) {
    for item in items {
        tx.unbounded_send(item).unwrap();
    }
}

fn listed(pods: &[Pod]) -> Vec<Item> {
    let mut evs = vec![Ok(Event::Init)];
    evs.extend(pods.iter().cloned().map(|p| Ok(Event::InitApply(p))));
    evs.push(Ok(Event::InitDone));
    evs
}

fn failed() -> Item {
    Err(watcher::Error::NoResourceVersion)
}

/// Run `drive` in the background with a `pod_store` flag.
fn spawn_pods<S>(
    writer: reflector::store::Writer<Pod>,
    watch: impl FnMut() -> S + Send + 'static,
) -> Arc<Readiness>
where
    S: Stream<Item = Item> + Send + 'static,
{
    let readiness = Arc::new(Readiness::default());
    let flag = readiness.register("pod_store");
    tokio::spawn(drive(writer, restarting(watch), flag, |_| {}, |_| {}));
    readiness
}

/// Let the driver handle what was sent.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(1)).await;
}

/// Move the paused clock to `t0 + d`.
async fn at(t0: tokio::time::Instant, d: Duration) {
    tokio::time::sleep_until(t0 + d).await;
}

fn names(store: &Store<Pod>) -> Vec<String> {
    let mut n: Vec<_> = store.state().iter().map(|p| name_of(p)).collect();
    n.sort();
    n
}

const SEC: Duration = Duration::from_secs(1);

#[tokio::test(start_paused = true)]
async fn an_error_before_the_first_list_keeps_the_store_not_ready() {
    let (_store, writer) = reflector::store::<Pod>();
    let (txs, watch) = fed(2);
    let readiness = spawn_pods(writer, watch);
    send(&txs[0], [Ok(Event::Init), failed()]);
    settle().await;
    assert_eq!(readiness.failing(), ["pod_store"]);
    // Startup readiness has no grace: only the first InitDone sets it.
    tokio::time::sleep(STALE_AFTER * 2).await;
    assert_eq!(readiness.failing(), ["pod_store"]);
    send(&txs[1], listed(&[pod("a")]));
    settle().await;
    assert!(readiness.failing().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_watch_error_in_steady_state_keeps_the_store_ready_through_the_relist() {
    let (store, writer) = reflector::store::<Pod>();
    let (txs, watch) = fed(2);
    let readiness = spawn_pods(writer, watch);
    let t0 = tokio::time::Instant::now();
    send(&txs[0], listed(&[pod("a")]));
    settle().await;
    assert!(readiness.failing().is_empty());

    send(&txs[0], [failed()]);
    settle().await;
    assert!(
        readiness.failing().is_empty(),
        "an error alone keeps /readyz"
    );
    // The watch restarts after the 1 s backoff and relists.
    at(t0, 2 * SEC).await;
    send(&txs[1], [Ok(Event::Init), Ok(Event::InitApply(pod("b")))]);
    settle().await;
    assert!(readiness.failing().is_empty());
    assert_eq!(
        names(&store),
        ["a"],
        "the store keeps its last list meanwhile"
    );
    send(&txs[1], [Ok(Event::InitDone)]);
    settle().await;
    assert!(readiness.failing().is_empty());
    assert_eq!(names(&store), ["b"]);
}

#[tokio::test(start_paused = true)]
async fn a_long_outage_marks_the_store_stale_after_the_bound_until_a_relist() {
    let (store, writer) = reflector::store::<Pod>();
    let (txs, watch) = fed(4);
    let readiness = spawn_pods(writer, watch);
    send(&txs[0], listed(&[pod("a")]));
    settle().await;
    let t0 = tokio::time::Instant::now();
    send(&txs[0], [failed()]);
    // The restarted watch fails too; the bound still runs from the first error.
    at(t0, 10 * SEC).await;
    send(&txs[1], [failed()]);
    at(t0, STALE_AFTER.checked_sub(SEC).unwrap()).await;
    assert!(readiness.failing().is_empty(), "still inside the bound");
    at(t0, STALE_AFTER + SEC).await;
    assert_eq!(readiness.failing(), ["pod_store"], "stale past the bound");
    assert_eq!(names(&store), ["a"]);

    send(&txs[2], listed(&[pod("b")]));
    settle().await;
    assert!(
        readiness.failing().is_empty(),
        "a completed relist recovers"
    );
    assert_eq!(names(&store), ["b"]);

    // A later error starts a new bound.
    let t1 = tokio::time::Instant::now();
    send(&txs[2], [failed()]);
    at(t1, STALE_AFTER.checked_sub(SEC).unwrap()).await;
    assert!(readiness.failing().is_empty());
    at(t1, STALE_AFTER + SEC).await;
    assert_eq!(readiness.failing(), ["pod_store"]);
}

/// A tolerant merge of namespaces `a` and `b`, each fed by hand with `n`
/// watches (one per restart), driving a `pod_store` in the background.
struct Tolerant {
    store: Store<Pod>,
    a: Vec<mpsc::UnboundedSender<Item>>,
    b: Vec<mpsc::UnboundedSender<Item>>,
    readiness: Arc<Readiness>,
    metrics: Arc<Metrics>,
}

fn tolerant(n: usize) -> Tolerant {
    let (store, writer) = reflector::store::<Pod>();
    let (a, wa) = fed(n);
    let (b, wb) = fed(n);
    let tagged = stream::select_all([
        restarting(wa).map(|ev| (0, ev)).boxed(),
        restarting(wb).map(|ev| (1, ev)).boxed(),
    ]);
    let readiness = Arc::new(Readiness::default());
    let metrics = Arc::new(Metrics::new());
    let merged = merge_tolerant(
        tagged.boxed(),
        vec!["a".into(), "b".into()],
        store.clone(),
        NamespaceReport::full("pod", Arc::clone(&metrics), Arc::clone(&readiness)),
    );
    let flag = readiness.register("pod_store");
    tokio::spawn(drive(writer, merged, flag, |_| {}, |_| {}));
    Tolerant {
        store,
        a,
        b,
        readiness,
        metrics,
    }
}

/// Every restart of `txs` fails at once.
fn fail_every(txs: &[mpsc::UnboundedSender<Item>]) {
    for tx in txs {
        send(tx, [failed()]);
    }
}

fn unlisted_gauge(m: &Metrics, ns: &str) -> bool {
    m.render().contains(&format!(
        "buildkit_watch_namespace_unlisted{{kind=\"pod\",namespace=\"{ns}\"}} 1"
    ))
}

#[tokio::test(start_paused = true)]
async fn a_namespace_that_never_lists_is_given_up_and_the_others_are_served() {
    let t = tolerant(10);
    let t0 = tokio::time::Instant::now();
    send(&t.a[0], listed(&[pod_in("a", "p1")]));
    fail_every(&t.b);
    at(t0, GIVE_UP_AFTER.checked_sub(SEC).unwrap()).await;
    assert_eq!(t.readiness.failing(), ["pod_store"], "b still has time");
    assert!(keys(&t.store).is_empty());

    at(t0, GIVE_UP_AFTER + SEC).await;
    assert!(t.readiness.failing().is_empty(), "served without b");
    assert_eq!(keys(&t.store), ["a/p1"]);
    assert_eq!(t.readiness.degraded(), ["pod_store/b"]);
    assert!(unlisted_gauge(&t.metrics, "b"));
    // The errors of b no longer count against readiness.
    at(t0, GIVE_UP_AFTER + STALE_AFTER * 3).await;
    assert!(t.readiness.failing().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_namespace_given_up_is_merged_back_when_it_lists() {
    let t = tolerant(10);
    let t0 = tokio::time::Instant::now();
    send(&t.a[0], listed(&[pod_in("a", "p1")]));
    // b fails its first five watches, then lists.
    fail_every(&t.b[..5]);
    at(t0, GIVE_UP_AFTER + SEC).await;
    assert_eq!(t.readiness.degraded(), ["pod_store/b"]);
    send(&t.b[5], listed(&[pod_in("b", "q1")]));
    at(t0, GIVE_UP_AFTER + 40 * SEC).await;
    assert_eq!(keys(&t.store), ["a/p1", "b/q1"]);
    assert!(t.readiness.degraded().is_empty());
    assert!(!unlisted_gauge(&t.metrics, "b"));
    assert!(t.readiness.failing().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_namespace_failing_after_sync_keeps_the_store_ready_and_its_objects() {
    let t = tolerant(10);
    send(&t.a[0], listed(&[pod_in("a", "p1")]));
    send(&t.b[0], listed(&[pod_in("b", "q1")]));
    settle().await;
    assert!(t.readiness.failing().is_empty());
    let t0 = tokio::time::Instant::now();
    fail_every(&t.b);
    at(t0, STALE_AFTER * 3).await;
    assert!(
        t.readiness.failing().is_empty(),
        "one broken namespace does not take the store down"
    );
    assert_eq!(t.readiness.degraded(), ["pod_store/b"]);
    assert_eq!(keys(&t.store), ["a/p1", "b/q1"], "b's last list is kept");
    // a still flows.
    send(&t.a[0], [Ok(Event::Apply(pod_in("a", "p2")))]);
    settle().await;
    assert_eq!(keys(&t.store), ["a/p1", "a/p2", "b/q1"]);
}

#[tokio::test(start_paused = true)]
async fn a_relist_of_one_namespace_restarts_only_that_namespace() {
    let t = tolerant(3);
    send(&t.a[0], listed(&[pod_in("a", "p1")]));
    send(&t.b[0], listed(&[pod_in("b", "q1")]));
    settle().await;
    let t0 = tokio::time::Instant::now();
    send(&t.b[0], [failed()]);
    at(t0, 2 * SEC).await;
    send(&t.b[1], listed(&[pod_in("b", "q2")]));
    settle().await;
    // a's first watch is still the one in use: an event on it lands.
    send(&t.a[0], [Ok(Event::Apply(pod_in("a", "p2")))]);
    settle().await;
    assert_eq!(keys(&t.store), ["a/p1", "a/p2", "b/q2"]);
    at(t0, STALE_AFTER * 2).await;
    assert!(t.readiness.failing().is_empty(), "the relist made it whole");
}

#[tokio::test(start_paused = true)]
async fn every_namespace_failing_still_goes_stale() {
    let t = tolerant(10);
    send(&t.a[0], listed(&[pod_in("a", "p1")]));
    send(&t.b[0], listed(&[pod_in("b", "q1")]));
    settle().await;
    let t0 = tokio::time::Instant::now();
    fail_every(&t.a);
    fail_every(&t.b);
    at(t0, STALE_AFTER.checked_sub(SEC).unwrap()).await;
    assert!(t.readiness.failing().is_empty(), "inside the bound");
    at(t0, STALE_AFTER + SEC).await;
    assert_eq!(t.readiness.failing(), ["pod_store"]);
    assert_eq!(t.readiness.degraded(), ["pod_store/a", "pod_store/b"]);
}

#[tokio::test(start_paused = true)]
async fn every_namespace_failing_from_the_start_never_becomes_ready() {
    let t = tolerant(10);
    fail_every(&t.a);
    fail_every(&t.b);
    tokio::time::sleep(STALE_AFTER * 3).await;
    assert_eq!(t.readiness.failing(), ["pod_store"]);
}

/// What the client's read timeout does to a watch: when `events` yields
/// nothing for [`READ_TIMEOUT`](crate::runtime::READ_TIMEOUT), it yields an
/// error and ends.
fn read_timeout(events: BoxStream<'static, Item>) -> BoxStream<'static, Item> {
    stream::unfold(Some(events), |events| async move {
        let mut events = events?;
        match tokio::time::timeout(crate::runtime::READ_TIMEOUT, events.next()).await {
            Ok(item) => item.map(|item| (item, Some(events))),
            Err(_) => Some((failed(), None)),
        }
    })
    .boxed()
}

#[tokio::test(start_paused = true)]
async fn a_stalled_watch_turns_stale_within_the_read_timeout_plus_the_bound_and_recovers() {
    let read = crate::runtime::READ_TIMEOUT;
    let (store, writer) = reflector::store::<Pod>();
    let (txs, mut watch) = fed(3);
    let readiness = spawn_pods(writer, move || read_timeout(watch()));
    send(&txs[0], listed(&[pod("a")]));
    settle().await;
    let t0 = tokio::time::Instant::now();
    // The watch stalls: no event, no error. Within the read timeout that is
    // indistinguishable from a quiet watch.
    at(t0, read.checked_sub(SEC).unwrap()).await;
    assert!(readiness.failing().is_empty());
    // The read timeout fails it, and the relist that follows stalls too. The
    // bound runs from that first error.
    at(t0, (read + STALE_AFTER).checked_sub(SEC).unwrap()).await;
    assert!(readiness.failing().is_empty(), "inside the bound");
    at(t0, read + STALE_AFTER + SEC).await;
    assert_eq!(readiness.failing(), ["pod_store"], "stale past the bound");
    assert_eq!(names(&store), ["a"]);

    // The stalled relist fails at 2 x read, and the next watch lists.
    at(t0, 2 * read + 3 * SEC).await;
    send(&txs[2], listed(&[pod("b")]));
    settle().await;
    assert!(
        readiness.failing().is_empty(),
        "a completed relist recovers"
    );
    assert_eq!(names(&store), ["b"]);
}

/// The hand-fed watches of each namespace.
type Senders = std::collections::HashMap<String, Vec<mpsc::UnboundedSender<Item>>>;

/// Watch factories for [`merged_watch`] fed by hand: `n` watches per
/// namespace (`""` = all namespaces), handed out one per (re)start, in order.
fn fed_namespaces(
    names: &[&str],
    n: usize,
) -> (
    Senders,
    impl Fn(Option<&str>) -> BoxStream<'static, Item> + Clone + Send + 'static,
) {
    let mut txs = std::collections::HashMap::new();
    let mut rxs = std::collections::HashMap::new();
    for name in names {
        let (t, r): (Vec<_>, Vec<_>) = (0..n).map(|_| mpsc::unbounded()).unzip();
        txs.insert((*name).to_owned(), t);
        rxs.insert((*name).to_owned(), r.into_iter());
    }
    let rxs = Arc::new(std::sync::Mutex::new(rxs));
    let watch = move |ns: Option<&str>| {
        rxs.lock()
            .unwrap()
            .get_mut(ns.unwrap_or(""))
            .and_then(Iterator::next)
            .map_or_else(|| stream::pending().boxed(), StreamExt::boxed)
    };
    (txs, watch)
}

/// The claim watch path: after a watch error kube resumes the watch from its
/// last resourceVersion with `Apply` events and no `InitDone`. The store's
/// readiness must still come back, so an error is always followed by a relist.
#[tokio::test(start_paused = true)]
async fn a_strict_watch_relists_after_an_error_and_stays_ready() {
    for names in [&[""][..], &["a"], &["a", "b"]] {
        let (store, writer) = reflector::store::<Pod>();
        let (txs, watch) = fed_namespaces(names, 3);
        let namespaces: Vec<String> = names
            .iter()
            .filter(|n| !n.is_empty())
            .map(|n| (*n).to_owned())
            .collect();
        let events = merged_watch(&namespaces, store.clone(), None, watch);
        let readiness = Arc::new(Readiness::default());
        let flag = readiness.register("claim_store");
        tokio::spawn(drive(writer, events, flag, |_| {}, |_| {}));
        for (name, tx) in &txs {
            send(&tx[0], listed(&[pod_in(name, "p1")]));
        }
        settle().await;
        assert!(readiness.failing().is_empty(), "{names:?}");

        let t0 = tokio::time::Instant::now();
        let first = &txs[names[0]];
        // The error, then what a resumed kube watch sends: no InitDone.
        send(
            &first[0],
            [failed(), Ok(Event::Apply(pod_in(names[0], "p2")))],
        );
        at(t0, 2 * SEC).await;
        send(&first[1], listed(&[pod_in(names[0], "p3")]));
        at(t0, STALE_AFTER * 3).await;
        assert!(
            readiness.failing().is_empty(),
            "{names:?}: the relist after the error restored readiness"
        );
        let mut want: Vec<String> = names
            .iter()
            .map(|n| format!("{n}/{}", if *n == names[0] { "p3" } else { "p1" }))
            .collect();
        want.sort();
        assert_eq!(keys(&store), want, "{names:?}");
    }
}

/// With every namespace failing and recovering at different times, the strict
/// merge becomes ready again only once the last one relisted.
#[tokio::test(start_paused = true)]
async fn a_strict_merge_is_ready_again_once_every_namespace_relisted() {
    let (store, writer) = reflector::store::<Pod>();
    let (txs, watch) = fed_namespaces(&["a", "b"], 10);
    let events = merged_watch(&["a".into(), "b".into()], store.clone(), None, watch);
    let readiness = Arc::new(Readiness::default());
    let flag = readiness.register("claim_store");
    tokio::spawn(drive(writer, events, flag, |_| {}, |_| {}));
    send(&txs["a"][0], listed(&[pod_in("a", "p1")]));
    send(&txs["b"][0], listed(&[pod_in("b", "q1")]));
    settle().await;
    let t0 = tokio::time::Instant::now();
    send(&txs["a"][0], [failed()]);
    fail_every(&txs["b"][..5]);
    at(t0, 2 * SEC).await;
    send(&txs["a"][1], listed(&[pod_in("a", "p2")]));
    at(t0, STALE_AFTER + SEC).await;
    assert_eq!(
        readiness.failing(),
        ["claim_store"],
        "b has not relisted: no partial list counts as synced"
    );
    send(&txs["b"][5], listed(&[pod_in("b", "q2")]));
    at(t0, STALE_AFTER + 40 * SEC).await;
    assert!(readiness.failing().is_empty());
    assert_eq!(keys(&store), ["a/p2", "b/q2"]);
}
