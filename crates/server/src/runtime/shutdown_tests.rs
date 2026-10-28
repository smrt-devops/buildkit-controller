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
/// component (the claim controller) have stopped.
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
