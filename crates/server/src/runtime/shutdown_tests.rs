use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;

type Log = Arc<Mutex<Vec<String>>>;

fn push(log: &Log, s: impl Into<String>) {
    log.lock().unwrap().push(s.into());
}

/// A component that logs its stop and returns `linger` after it.
fn component(
    log: &Log,
    name: &'static str,
    linger: Duration,
) -> impl FnOnce(CancellationToken) -> std::pin::Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>
{
    let log = Arc::clone(log);
    move |stop| {
        Box::pin(async move {
            stop.cancelled().await;
            push(&log, format!("{name} stop"));
            tokio::time::sleep(linger).await;
            push(&log, format!("{name} done"));
            Ok(())
        })
    }
}

#[tokio::test(start_paused = true)]
async fn each_stage_stops_only_after_the_one_before_has_returned() {
    let log = Log::default();
    let mut s = Stages::new(&["api", "edge", "pki"]);
    s.spawn(2, "pki", component(&log, "pki", Duration::ZERO));
    s.spawn(1, "edge", component(&log, "edge", Duration::from_hours(1)));
    s.spawn(0, "http", component(&log, "http", Duration::from_secs(5)));
    s.spawn(0, "alloc", component(&log, "alloc", Duration::from_secs(1)));
    let start = Instant::now();
    assert!(s.stop(start + Duration::from_hours(2)).await);
    assert_eq!(
        start.elapsed(),
        Duration::from_hours(1) + Duration::from_secs(5)
    );
    let log = log.lock().unwrap().clone();
    let at = |e: &str| log.iter().position(|l| l == e).unwrap();
    assert!(at("edge stop") > at("http done"), "{log:?}");
    assert!(at("edge stop") > at("alloc done"), "{log:?}");
    assert!(at("pki stop") > at("edge done"), "{log:?}");
}

#[tokio::test(start_paused = true)]
async fn the_deadline_aborts_whatever_still_runs() {
    let log = Log::default();
    let mut s = Stages::new(&["api", "edge", "pki"]);
    s.spawn(0, "http", component(&log, "http", Duration::ZERO));
    s.spawn(1, "edge", component(&log, "edge", Duration::from_mins(10)));
    s.spawn(2, "pki", component(&log, "pki", Duration::from_mins(10)));
    let start = Instant::now();
    assert!(!s.stop(start + Duration::from_mins(5)).await);
    assert_eq!(start.elapsed(), Duration::from_mins(5));
    let log = log.lock().unwrap().clone();
    assert!(!log.contains(&"edge done".to_owned()), "{log:?}");
    assert!(!log.contains(&"pki done".to_owned()), "{log:?}");
}

#[tokio::test(start_paused = true)]
async fn an_early_exit_is_reported_with_its_name() {
    let mut s = Stages::new(&["api", "edge"]);
    s.spawn(1, "edge", |stop: CancellationToken| async move {
        stop.cancelled().await;
        Ok(())
    });
    s.spawn(0, "http", |_| async { anyhow::bail!("bind failed") });
    let e = s.exited().await;
    assert!(
        format!("{e:#}").contains("component http failed: bind failed"),
        "{e:#}"
    );
    s.spawn(0, "alloc", |_| async { Ok(()) });
    let e = s.exited().await;
    assert!(
        e.to_string().contains("component alloc exited early"),
        "{e}"
    );
}

/// The Lease is released only after the reconciler and every leader-gated
/// component (the allocator) have stopped.
#[tokio::test(start_paused = true)]
async fn step_down_releases_the_lease_after_all_leader_work_stopped() {
    let log = Log::default();
    let mut leader = Stages::new(&["leader"]);
    leader.spawn(
        0,
        "claims",
        component(&log, "claims", Duration::from_secs(3)),
    );
    let release = CancellationToken::new();
    let elector = tokio::spawn({
        let (log, release) = (Arc::clone(&log), release.clone());
        async move {
            release.cancelled().await;
            push(&log, "lease released");
        }
    });
    let reconciler = {
        let log = Arc::clone(&log);
        async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            push(&log, "reconciler done");
        }
    };
    let start = Instant::now();
    step_down(
        reconciler,
        leader,
        start + Duration::from_secs(30),
        release,
        elector,
    )
    .await;
    assert_eq!(start.elapsed(), Duration::from_secs(3));
    let log = log.lock().unwrap().clone();
    assert_eq!(
        log,
        [
            "claims stop",
            "reconciler done",
            "claims done",
            "lease released"
        ],
        "{log:?}"
    );
}

/// Records when the component holding it was dropped.
struct Dropped(Arc<Mutex<Option<Instant>>>);

impl Drop for Dropped {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = Some(Instant::now());
    }
}

/// A component that ignores its stop and runs for an hour; `at` gets the
/// instant it was dropped.
fn overrun(
    at: &Arc<Mutex<Option<Instant>>>,
) -> impl FnOnce(CancellationToken) -> std::pin::Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>
{
    let guard = Dropped(Arc::clone(at));
    move |_| {
        Box::pin(async move {
            let _guard = guard;
            tokio::time::sleep(Duration::from_hours(1)).await;
            Ok(())
        })
    }
}

/// An overrunning edge is aborted a scrape interval before the deadline, the
/// later stages still stop in order, and the call returns only at the deadline,
/// so the process is still up when Prometheus scrapes what the abort counted.
#[tokio::test(start_paused = true)]
async fn an_overrun_is_aborted_a_scrape_interval_early_and_held_to_the_deadline() {
    for (budget, cut) in [
        (
            Duration::from_mins(10),
            Duration::from_mins(10).saturating_sub(SCRAPE_GRACE),
        ),
        (Duration::from_secs(20), Duration::from_secs(10)),
    ] {
        let log = Log::default();
        let dropped = Arc::default();
        let mut s = Stages::new(&["api", "edge", "pki"]);
        s.spawn(1, "edge", overrun(&dropped));
        s.spawn(2, "pki", component(&log, "pki", Duration::from_secs(1)));
        let start = Instant::now();
        assert!(!s.stop(start + budget).await);
        assert_eq!(start.elapsed(), budget);
        assert_eq!(dropped.lock().unwrap().unwrap() - start, cut);
        assert_eq!(*log.lock().unwrap(), ["pki stop", "pki done"]);
    }
}

/// Leader work has no scrape grace: it runs until the deadline itself.
#[tokio::test(start_paused = true)]
async fn step_down_aborts_leader_work_at_the_deadline() {
    let dropped = Arc::default();
    let mut leader = Stages::new(&["leader"]);
    leader.spawn(0, "claims", overrun(&dropped));
    let release = CancellationToken::new();
    let elector = tokio::spawn({
        let release = release.clone();
        async move { release.cancelled().await }
    });
    let start = Instant::now();
    let deadline = start + Duration::from_secs(30);
    step_down(async {}, leader, deadline, release, elector).await;
    assert_eq!(dropped.lock().unwrap().unwrap(), deadline);
}
