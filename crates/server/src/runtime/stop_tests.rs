use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::Shutdown;
use crate::runtime::AppContext;
use crate::runtime::hooks::{BoxResult, Component, Hooks};
use crate::runtime::readiness::Readiness;
use crate::runtime::shutdown::Stages;

/// What happened, with the time since the start.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<(String, Duration)>>>);

impl Log {
    fn push(&self, t0: Instant, what: &str) {
        self.0.lock().unwrap().push((what.into(), t0.elapsed()));
    }

    fn events(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(e, _)| e.clone())
            .collect()
    }

    fn at(&self, what: &str) -> Duration {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|(e, _)| e == what)
            .unwrap_or_else(|| panic!("no {what}"))
            .1
    }
}

const SEC: Duration = Duration::from_secs(1);

/// A plan whose step-down takes `step_down` (the reconciler stops that long
/// after it is told to) and whose edge drains for `edge_drain`.
fn plan(log: &Log, t0: Instant, step_down: Duration, edge_drain: Duration) -> Shutdown {
    let readiness = Arc::new(Readiness::default());
    let leader_stop = CancellationToken::new();
    let reconciler = {
        let (log, stop, r) = (log.clone(), leader_stop.clone(), Arc::clone(&readiness));
        async move {
            stop.cancelled().await;
            assert!(
                r.failing().contains(&"shutting_down".to_owned()),
                "/readyz fails before leader work stops"
            );
            tokio::time::sleep(step_down).await;
            log.push(t0, "reconciler stopped");
        }
        .boxed()
    };
    let mut leader = Stages::new(&["leader"]);
    let l = log.clone();
    leader.spawn(0, "claims", move |stop| async move {
        stop.cancelled().await;
        l.push(t0, "claims stopped");
        Ok(())
    });
    let elect_stop = CancellationToken::new();
    let elector = tokio::spawn({
        let (log, stop) = (log.clone(), elect_stop.clone());
        async move {
            stop.cancelled().await;
            log.push(t0, "lease released");
        }
    });
    let mut stages = Stages::new(&["api", "edge", "pki"]);
    for (i, name) in ["api", "edge", "pki"].into_iter().enumerate() {
        let l = log.clone();
        let drain = if name == "edge" {
            edge_drain
        } else {
            Duration::ZERO
        };
        stages.spawn(i, name, move |stop| async move {
            stop.cancelled().await;
            l.push(t0, &format!("{name} stopping"));
            tokio::time::sleep(drain).await;
            l.push(t0, &format!("{name} stopped"));
            Ok(())
        });
    }
    let watches = CancellationToken::new();
    let ops_stop = CancellationToken::new();
    let ops = {
        let (log, w, stop) = (log.clone(), watches.clone(), ops_stop.clone());
        async move {
            stop.cancelled().await;
            assert!(w.is_cancelled(), "watches stop with the ops server");
            log.push(t0, "ops stopped");
        }
        .boxed()
    };
    Shutdown {
        started: t0,
        readiness,
        leader_stop,
        reconciler,
        leader,
        request_timeout: 5 * SEC,
        elect_stop,
        elector,
        delay: 5 * SEC,
        drain_budget: 60 * SEC,
        stages,
        watches,
        ops_stop,
        ops: Some(ops),
    }
}

#[tokio::test(start_paused = true)]
async fn stops_in_order_and_returns() {
    let log = Log::default();
    let t0 = Instant::now();
    plan(&log, t0, SEC, 10 * SEC).run().await;
    assert_eq!(
        log.events(),
        [
            "claims stopped",
            "reconciler stopped",
            "lease released",
            "api stopping",
            "api stopped",
            "edge stopping",
            "edge stopped",
            "pki stopping",
            "pki stopped",
            "ops stopped",
        ]
    );
    assert_eq!(
        log.at("api stopping"),
        5 * SEC,
        "listeners close after the delay"
    );
}

#[tokio::test(start_paused = true)]
async fn a_slow_step_down_eats_into_the_delay_and_the_drain_budget() {
    let log = Log::default();
    let t0 = Instant::now();
    // Step-down takes 3 s of the 5 s delay; the edge would drain for 2 min.
    plan(&log, t0, 3 * SEC, 120 * SEC).run().await;
    assert_eq!(log.at("lease released"), 3 * SEC);
    assert_eq!(
        log.at("api stopping"),
        5 * SEC,
        "delay counted from the signal"
    );
    // The drain is cut at delay + budget from the signal, not from the end of step-down.
    let end = log.at("ops stopped");
    assert!(end >= 65 * SEC && end < 66 * SEC, "stopped at {end:?}");
    assert!(!log.events().contains(&"edge stopped".to_owned()));
}

#[tokio::test(start_paused = true)]
async fn a_step_down_longer_than_the_delay_drains_at_once() {
    let log = Log::default();
    let t0 = Instant::now();
    plan(&log, t0, 7 * SEC, Duration::ZERO).run().await;
    assert_eq!(log.at("api stopping"), 7 * SEC);
}

/// Stands in for a real component; [`Hooks::start`] is given its own runner.
struct Fake(&'static str);

impl Component for Fake {
    fn name(&self) -> &'static str {
        self.0
    }

    fn run(&self, _: AppContext, _: CancellationToken) -> BoxResult {
        Box::pin(std::future::pending())
    }
}

#[tokio::test(start_paused = true)]
async fn the_allocator_stops_before_the_lease_is_released() {
    let log = Log::default();
    let t0 = Instant::now();
    let fake = |name| Arc::new(Fake(name)) as Arc<dyn Component>;
    let hooks = Hooks {
        reconcile: |_| unreachable!("the plan stands in for the reconciler"),
        pki: fake("pki"),
        alloc: fake("alloc"),
        allocator: fake("allocator"),
        http: fake("http"),
        edge: fake("edge"),
    };
    let (leader, stages) = hooks.start(|c, stop| {
        let log = log.clone();
        Box::pin(async move {
            stop.cancelled().await;
            log.push(t0, &format!("{} stopped", c.name()));
            Ok(())
        })
    });
    let mut s = plan(&log, t0, SEC, Duration::ZERO);
    s.leader = leader;
    s.stages = stages;
    s.run().await;
    let events = log.events();
    let at = |e: &str| events.iter().position(|x| x == e).unwrap();
    assert!(at("allocator stopped") < at("lease released"), "{events:?}");
    for c in ["alloc", "http", "edge", "pki"] {
        assert!(
            at("lease released") < at(&format!("{c} stopped")),
            "{events:?}"
        );
    }
    assert_eq!(
        log.at("alloc stopped"),
        5 * SEC,
        "the API stops after the delay"
    );
}
