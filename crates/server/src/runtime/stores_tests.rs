use futures::channel::mpsc;
use futures::stream;
use k8s_openapi::api::core::v1::{Pod, Secret};
use kube::api::ObjectMeta;
use kube::runtime::reflector;
use kube::runtime::watcher::Event;
use tokio::sync::broadcast;

use super::*;
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
        once(events.into_iter().map(Ok).collect()),
        flag,
        |_| {},
        move |ev| forward_pod(&tx, ev),
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
    let mut first = Some(merged.boxed());
    let mut seen = Vec::new();
    let errors = std::sync::atomic::AtomicUsize::new(0);
    drive(
        writer,
        move || first.take().unwrap_or_else(|| stream::empty().boxed()),
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
    tokio::spawn(drive(writer, watch, flag, |_| {}, |_| {}));
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

#[tokio::test(start_paused = true)]
async fn one_namespace_failing_keeps_the_merged_store_ready_until_the_bound() {
    let (store, writer) = reflector::store::<Pod>();
    let (txs, rxs): (Vec<_>, Vec<_>) = (0..3).map(|_| mpsc::unbounded::<Tagged>()).unzip();
    let mut rxs = rxs.into_iter();
    let s = store.clone();
    let watch = move || match rxs.next() {
        Some(rx) => merge_namespaces(rx.boxed(), vec!["a".into(), "b".into()], s.clone()).boxed(),
        None => stream::pending().boxed(),
    };
    let readiness = spawn_pods(writer, watch);
    let list = |i: usize, pods: Vec<Pod>| listed(&pods).into_iter().map(move |e| (i, e));
    send(&txs[0], list(0, vec![pod_in("a", "p1")]));
    send(&txs[0], list(1, vec![pod_in("b", "q1")]));
    settle().await;
    assert!(readiness.failing().is_empty());

    let t0 = tokio::time::Instant::now();
    send(&txs[0], [(1, failed())]);
    // Both namespaces relist; `a` is done, `b` is not, and then fails again.
    at(t0, 2 * SEC).await;
    send(&txs[1], list(0, vec![pod_in("a", "p1")]));
    send(&txs[1], [(1, Ok(Event::Init)), (1, failed())]);
    at(t0, STALE_AFTER.checked_sub(SEC).unwrap()).await;
    assert!(readiness.failing().is_empty(), "inside the bound");
    assert_eq!(keys(&store), ["a/p1", "b/q1"], "last good state kept");
    at(t0, STALE_AFTER + SEC).await;
    assert_eq!(readiness.failing(), ["pod_store"]);

    send(&txs[2], list(0, vec![pod_in("a", "p1")]));
    send(&txs[2], list(1, vec![pod_in("b", "q2")]));
    settle().await;
    assert!(readiness.failing().is_empty(), "every namespace relisted");
    assert_eq!(keys(&store), ["a/p1", "b/q2"]);
}
