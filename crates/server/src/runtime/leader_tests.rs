use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::Notify;

use super::*;

struct Counting(Arc<AtomicUsize>, Arc<AtomicUsize>);

struct OnDrop(Arc<AtomicUsize>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl LeaderWork for Counting {
    fn run(&self, _term: u64) -> BoxFut {
        let (started, dropped) = (Arc::clone(&self.0), Arc::clone(&self.1));
        Box::pin(async move {
            let _g = OnDrop(dropped);
            started.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<()>().await;
        })
    }
}

#[tokio::test(start_paused = true)]
async fn work_waits_for_sync_restarts_per_term_and_stops_on_follower() {
    let (tx, rx) = watch::channel(LeaderState::Follower);
    let started = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Notify::new());
    let g = Arc::clone(&gate);
    let stop = CancellationToken::new();
    let sup = tokio::spawn(supervise(
        rx,
        stop.clone(),
        move || {
            let g = Arc::clone(&g);
            Box::pin(async move { g.notified().await })
        },
        Arc::new(Counting(Arc::clone(&started), Arc::clone(&dropped))),
    ));
    let settle = || tokio::time::sleep(Duration::from_millis(10));
    tx.send_replace(LeaderState::Leader { term: 1 });
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(started.load(Ordering::SeqCst), 0, "not before InitDone");
    gate.notify_one();
    settle().await;
    assert_eq!(started.load(Ordering::SeqCst), 1);
    tx.send_replace(LeaderState::Leader { term: 2 });
    settle().await;
    assert_eq!(dropped.load(Ordering::SeqCst), 1, "old term dropped");
    gate.notify_one();
    settle().await;
    assert_eq!(started.load(Ordering::SeqCst), 2);
    tx.send_replace(LeaderState::Follower);
    settle().await;
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
    stop.cancel();
    sup.await.unwrap().unwrap();
}

struct Panics;

impl LeaderWork for Panics {
    fn run(&self, _term: u64) -> BoxFut {
        Box::pin(async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            panic!("boom");
        })
    }
}

struct Returns;

impl LeaderWork for Returns {
    fn run(&self, _term: u64) -> BoxFut {
        Box::pin(async {})
    }
}

/// Regression (control review P3): a term whose work panicked or returned
/// was never noticed, so the replica kept renewing the Lease with no
/// reconciler and failover never happened. `supervise` now returns the
/// failure, and the caller shuts down, which releases the Lease.
#[tokio::test(start_paused = true)]
async fn a_panicked_or_finished_term_ends_supervision() {
    for (work, panicked) in [
        (Arc::new(Panics) as Arc<dyn LeaderWork>, true),
        (Arc::new(Returns), false),
    ] {
        let (tx, rx) = watch::channel(LeaderState::Leader { term: 7 });
        let r = tokio::time::timeout(
            Duration::from_secs(5),
            supervise(rx, CancellationToken::new(), || Box::pin(async {}), work),
        )
        .await
        .expect("supervise noticed the term end");
        match r {
            Err(TermFailed::Panicked { term: 7, message }) if panicked => {
                assert_eq!(message, "boom");
            }
            Err(TermFailed::Returned { term: 7 }) if !panicked => {}
            other => panic!("unexpected {other:?}"),
        }
        drop(tx);
    }
}

/// A leader whose stores never sync (a namespace it cannot list) must not hold
/// the Lease with no work running: the term fails after [`SYNC_GATE`].
#[tokio::test(start_paused = true)]
async fn a_term_whose_stores_never_sync_ends_supervision() {
    let started = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let (_tx, rx) = watch::channel(LeaderState::Leader { term: 3 });
    let sup = tokio::spawn(supervise(
        rx,
        CancellationToken::new(),
        || Box::pin(std::future::pending()),
        Arc::new(Counting(Arc::clone(&started), Arc::clone(&dropped))),
    ));
    tokio::time::sleep(SYNC_GATE.checked_sub(Duration::from_secs(1)).unwrap()).await;
    assert!(!sup.is_finished(), "still inside the bound");
    let r = tokio::time::timeout(Duration::from_secs(2), sup)
        .await
        .expect("supervise gave up after the bound")
        .unwrap();
    assert!(matches!(r, Err(TermFailed::NotSynced { term: 3 })), "{r:?}");
    assert_eq!(started.load(Ordering::SeqCst), 0);
}
