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

/// A watch factory that yields each batch in turn, one batch per watch.
fn watches<K>(
    batches: Vec<Vec<watcher::Result<Event<K>>>>,
) -> impl FnMut() -> stream::Iter<std::vec::IntoIter<watcher::Result<Event<K>>>> {
    let mut batches = batches.into_iter();
    move || stream::iter(batches.next().unwrap_or_default())
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

#[tokio::test(start_paused = true)]
async fn a_watch_error_marks_the_store_stale_until_a_relist() {
    let readiness = Arc::new(Readiness::default());
    let flag = readiness.register("pod_store");
    // The restarted watch has not finished its list yet.
    let (_store, writer) = reflector::store::<Pod>();
    let first = || -> Vec<watcher::Result<Event<Pod>>> {
        vec![
            Ok(Event::Init),
            Ok(Event::InitDone),
            Err(watcher::Error::NoResourceVersion),
        ]
    };
    let relisting = vec![Ok(Event::Init), Ok(Event::InitApply(pod("a")))];
    drive(
        writer,
        watches(vec![first(), relisting]),
        flag.clone(),
        |_| {},
        |_| {},
    )
    .await;
    assert_eq!(readiness.failing(), ["pod_store"]);
    // The restarted watch lists again, and the store is fresh once that list is done.
    let (store, writer) = reflector::store::<Pod>();
    let relisted = vec![
        Ok(Event::Init),
        Ok(Event::InitApply(pod("a"))),
        Ok(Event::InitDone),
    ];
    drive(
        writer,
        watches(vec![first(), relisted]),
        flag,
        |_| {},
        |_| {},
    )
    .await;
    assert!(readiness.failing().is_empty());
    assert_eq!(store.state().len(), 1);
}
