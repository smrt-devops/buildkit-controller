//! Reconciler metric families (leader only).

use std::sync::Arc;

use buildkit_controller_api::v1alpha1::BuildKitPoolStatus;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;

use super::exec::Report;
use super::pods::{Alloc, WorkerPod};
use crate::metrics_doc::{Def, Handle, Label, Labels, POOL, Role, register_table};
use crate::runtime::metrics::{Metrics, PoolSeries};

/// Pod counts of one pass that the pool status does not carry.
#[derive(Debug, Clone, Copy, Default)]
pub struct PodCounts {
    /// Pods with `alloc=draining`.
    pub draining: usize,
}

impl PodCounts {
    /// Count `workers`.
    #[must_use]
    pub fn of(workers: &[WorkerPod]) -> Self {
        Self {
            draining: workers
                .iter()
                .filter(|p| p.alloc == Alloc::Draining)
                .count(),
        }
    }
}

/// Histogram buckets, in seconds.
pub const BUCKETS: [f64; 16] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0,
];

const POD_STATES: &[&str] = &["warm", "starting", "allocated", "draining"];
const POD_OPS: &[&str] = &["create", "retire", "delete", "force_delete"];
const POD_OP_RESULTS: &[&str] = &["ok", "conflict", "error"];
const PASS_RESULTS: &[&str] = &["ok", "error"];
const CA_REASONS: &[&str] = &["missing", "invalid", "foreign", "expired"];

/// The reconciler's families. All gauges are the leader's, and go when its term ends.
pub const TABLE: [Def; 8] = [
    Def::gauge(
        "pool_pods",
        Role::Controller,
        &[
            Label::namespace(),
            Label::pool(),
            Label::closed("state", POD_STATES),
        ],
        "Worker pods of a pool by state. `warm` are free and ready, `starting` are not ready yet, `allocated` are held by a client, `draining` are being deleted",
    ),
    Def::gauge(
        "pool_desired_warm",
        Role::Controller,
        &POOL,
        "Free workers the pool is asked to keep now: scaling.min, or the active schedule window's floor (status.desiredWarm)",
    ),
    Def::gauge(
        "pool_waiting",
        Role::Controller,
        &POOL,
        "Claims waiting for a worker in the pool (status.waiting)",
    ),
    Def::gauge(
        "pool_max",
        Role::Controller,
        &POOL,
        "The pool's worker ceiling (scaling.max)",
    ),
    Def::counter(
        "pod_operations",
        Role::Controller,
        &[
            Label::closed("op", POD_OPS),
            Label::closed("result", POD_OP_RESULTS),
        ],
        "Worker pod writes by the reconciler. `conflict` means the pod was already gone or replaced. `force_delete` is for a pod stuck draining",
    ),
    Def::counter(
        "reconcile",
        Role::Controller,
        &[Label::closed("result", PASS_RESULTS)],
        "Pool reconcile passes. `error` when any step of the pass failed",
    ),
    Def::histogram(
        "reconcile_duration_seconds",
        Role::Controller,
        &[],
        &BUCKETS,
        "Duration of one pool reconcile pass",
    ),
    Def::counter(
        "ca_replacements",
        Role::Controller,
        &[Label::closed("reason", CA_REASONS)],
        "CA Secret creations and replacements by the leader. `missing`: created. `invalid`, `foreign` (not made by this install), `expired`: replaced after a backup",
    ),
];

/// `buildkit_pool_*`, `buildkit_pod_operations_total`, `buildkit_reconcile_*`,
/// `buildkit_ca_replacements_total`, `buildkit_releases_total` (the runtime's shared family).
#[derive(Debug, Clone)]
pub struct ReconcileMetrics {
    pool_pods: Family<Labels, Gauge>,
    desired_warm: Family<Labels, Gauge>,
    waiting: Family<Labels, Gauge>,
    max: Family<Labels, Gauge>,
    pod_ops: Family<Labels, Counter>,
    reconcile: Family<Labels, Counter>,
    duration: Histogram,
    ca_replacements: Family<Labels, Counter>,
    releases: Family<Labels, Counter>,
    held: crate::alloc::metrics::Hist,
}

impl ReconcileMetrics {
    /// Registers the families; call once per registry.
    #[must_use]
    pub fn register(metrics: &Metrics) -> Self {
        let m = Self {
            pool_pods: Family::default(),
            desired_warm: Family::default(),
            waiting: Family::default(),
            max: Family::default(),
            pod_ops: Family::default(),
            reconcile: Family::default(),
            duration: Histogram::new(BUCKETS),
            ca_replacements: Family::default(),
            releases: metrics.releases(),
            held: metrics.held(),
        };
        metrics.with_registry(|r| {
            register_table(
                r,
                Role::Controller,
                &TABLE,
                vec![
                    ("pool_pods", Handle::new(m.pool_pods.clone())),
                    ("pool_desired_warm", Handle::new(m.desired_warm.clone())),
                    ("pool_waiting", Handle::new(m.waiting.clone())),
                    ("pool_max", Handle::new(m.max.clone())),
                    ("pod_operations", Handle::new(m.pod_ops.clone())),
                    ("reconcile", Handle::new(m.reconcile.clone())),
                    (
                        "reconcile_duration_seconds",
                        Handle::new(m.duration.clone()),
                    ),
                    ("ca_replacements", Handle::new(m.ca_replacements.clone())),
                ],
            );
        });
        TABLE[4].seed(None, &m.pod_ops);
        TABLE[5].seed(None, &m.reconcile);
        TABLE[7].seed(None, &m.ca_replacements);
        metrics.track_pools(Arc::new(m.clone()));
        m
    }

    /// `buildkit_ca_replacements_total` of a pool pass or the CA pass.
    pub fn record_ca(&self, report: &Report) {
        for reason in &report.ca_replacements {
            self.ca_replacements
                .get_or_create(&vec![("reason", (*reason).into())])
                .inc();
        }
    }

    /// Sets `buildkit_pool_max` to the pool's `scaling.max`.
    pub fn set_max(&self, ns: &str, pool: &str, max: i32) {
        self.max
            .get_or_create(&vec![
                ("namespace", ns.to_owned()),
                ("pool", pool.to_owned()),
            ])
            .set(i64::from(max));
    }

    /// Records one pass.
    pub fn pass(
        &self,
        ns: &str,
        pool: &str,
        status: Option<&BuildKitPoolStatus>,
        counts: PodCounts,
        report: &Report,
        seconds: f64,
    ) {
        let p = |extra: Option<(&'static str, String)>| -> Labels {
            let mut l = vec![("namespace", ns.to_owned()), ("pool", pool.to_owned())];
            l.extend(extra);
            l
        };
        if let Some(s) = status {
            for (state, v) in [
                ("warm", s.warm),
                ("starting", s.starting),
                ("allocated", s.allocated),
            ] {
                self.pool_pods
                    .get_or_create(&p(Some(("state", state.into()))))
                    .set(i64::from(v.unwrap_or(0)));
            }
            self.desired_warm
                .get_or_create(&p(None))
                .set(i64::from(s.desired_warm.unwrap_or(0)));
            self.waiting
                .get_or_create(&p(None))
                .set(i64::from(s.waiting.unwrap_or(0)));
        }
        self.pool_pods
            .get_or_create(&p(Some(("state", "draining".into()))))
            .set(i64::try_from(counts.draining).unwrap_or(i64::MAX));
        for (op, result) in &report.pod_ops {
            self.pod_ops
                .get_or_create(&vec![("op", (*op).into()), ("result", (*result).into())])
                .inc();
        }
        self.record_ca(report);
        for reason in &report.releases {
            self.releases
                .get_or_create(&p(Some(("reason", (*reason).into()))))
                .inc();
        }
        for (reason, held) in &report.held {
            self.held
                .get_or_create(&p(Some(("reason", (*reason).into()))))
                .observe(*held);
        }
        let result = if report.errors.is_empty() {
            "ok"
        } else {
            "error"
        };
        self.reconcile
            .get_or_create(&vec![("result", result.into())])
            .inc();
        self.duration.observe(seconds);
    }

    /// Drops every leader-only per-pool gauge (the term ended).
    pub fn clear_pool_gauges(&self) {
        self.pool_pods.clear();
        self.desired_warm.clear();
        self.waiting.clear();
        self.max.clear();
    }

    /// Drops a deleted pool's gauges.
    pub fn forget(&self, ns: &str, pool: &str) {
        TABLE[0].forget(ns, pool, &self.pool_pods);
        TABLE[1].forget(ns, pool, &self.desired_warm);
        TABLE[2].forget(ns, pool, &self.waiting);
        TABLE[3].forget(ns, pool, &self.max);
    }
}

impl PoolSeries for ReconcileMetrics {
    /// The gauges are the leader's and appear when it writes them.
    fn seed(&self, _namespace: &str, _pool: &str) {}

    fn forget(&self, namespace: &str, pool: &str) {
        ReconcileMetrics::forget(self, namespace, pool);
    }
}

/// Clears the leader-only per-pool gauges when dropped, so a replica that
/// lost the term stops exporting them (no double count after a failover).
pub struct TermGauges(pub ReconcileMetrics);

impl Drop for TermGauges {
    fn drop(&mut self) {
        self.0.clear_pool_gauges();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use buildkit_controller_api::v1alpha1::BuildKitPoolStatus;

    use super::{PodCounts, ReconcileMetrics, TermGauges};
    use crate::alloc::metrics::AllocMetrics;
    use crate::reconcile::exec::Report;
    use crate::runtime::metrics::Metrics;

    fn status() -> BuildKitPoolStatus {
        BuildKitPoolStatus {
            warm: Some(2),
            desired_warm: Some(2),
            ..BuildKitPoolStatus::default()
        }
    }

    #[test]
    fn alloc_and_reconcile_share_one_releases_family() {
        let m = Metrics::new();
        let alloc = AllocMetrics::with_releases(m.releases(), m.held());
        alloc.register(&m);
        let rec = ReconcileMetrics::register(&m);
        alloc.release("ns", "p", "released", None);
        let report = Report {
            releases: vec!["expired"],
            ..Report::default()
        };
        rec.pass(
            "ns",
            "p",
            Some(&status()),
            PodCounts::default(),
            &report,
            0.01,
        );
        let text = m.render();
        // Declare every family once, because a duplicate fails the scrape.
        let mut seen = BTreeSet::new();
        for l in text.lines().filter(|l| l.starts_with("# TYPE ")) {
            assert!(seen.insert(l.to_owned()), "duplicate family: {l}\n{text}");
        }
        assert!(seen.contains("# TYPE buildkit_releases counter"), "{text}");
        for reason in ["released", "expired"] {
            let sample = format!(
                "buildkit_releases_total{{namespace=\"ns\",pool=\"p\",reason=\"{reason}\"}} 1"
            );
            assert!(text.contains(&sample), "{sample} missing:\n{text}");
        }
        assert!(text.contains("le=\"0.0025\""), "spec buckets:\n{text}");
        assert!(text.contains("le=\"120.0\""), "spec buckets:\n{text}");
    }

    #[tokio::test]
    async fn pool_gauges_clear_when_the_term_ends() {
        let m = Metrics::new();
        let rec = ReconcileMetrics::register(&m);
        let term = tokio::spawn({
            let rec = rec.clone();
            async move {
                let _gauges = TermGauges(rec.clone());
                rec.pass(
                    "ns",
                    "p",
                    Some(&status()),
                    PodCounts { draining: 1 },
                    &Report::default(),
                    0.01,
                );
                std::future::pending::<()>().await;
            }
        });
        tokio::task::yield_now().await;
        while !m.render().contains("buildkit_pool_pods{") {
            tokio::task::yield_now().await;
        }
        term.abort();
        let _ = term.await;
        let text = m.render();
        for g in [
            "buildkit_pool_pods{",
            "buildkit_pool_desired_warm{",
            "buildkit_pool_waiting{",
        ] {
            assert!(!text.contains(g), "{g} survived the term:\n{text}");
        }
        assert!(text.contains("buildkit_reconcile_total{"), "counters stay");
    }

    /// A retire by the reconciler records the hold time of the allocation it ended.
    #[test]
    fn an_expired_allocation_observes_how_long_it_was_held() {
        let m = Metrics::new();
        let rec = ReconcileMetrics::register(&m);
        let alloc = AllocMetrics::with_releases(m.releases(), m.held());
        alloc.register(&m);
        let report = Report {
            releases: vec!["expired"],
            held: vec![("expired", 10.0)],
            ..Report::default()
        };
        rec.pass(
            "ns",
            "p",
            Some(&status()),
            PodCounts::default(),
            &report,
            0.01,
        );
        let text = m.render();
        let line = |suffix: &str| {
            let prefix = format!(
                "buildkit_allocation_held_seconds_{suffix}{{namespace=\"ns\",pool=\"p\",reason=\"expired\"}} "
            );
            text.lines()
                .find_map(|l| l.strip_prefix(prefix.as_str()))
                .map(|v| v.parse::<f64>().unwrap())
        };
        assert_eq!(line("count"), Some(1.0), "{text}");
        assert_eq!(line("sum"), Some(10.0), "{text}");
    }

    #[test]
    fn pool_max_is_set_per_pool_and_goes_with_the_pool() {
        let m = Metrics::new();
        let rec = ReconcileMetrics::register(&m);
        m.pool_seen("ns", "p");
        rec.set_max("ns", "p", 4);
        assert!(
            m.render()
                .contains("buildkit_pool_max{namespace=\"ns\",pool=\"p\"} 4"),
            "{}",
            m.render()
        );
        m.pool_gone("ns", "p");
        assert!(!m.render().contains("buildkit_pool_max"), "{}", m.render());
        rec.set_max("ns", "p", 2);
        rec.clear_pool_gauges();
        assert!(
            !m.render().contains("buildkit_pool_max"),
            "term end clears it"
        );
    }

    /// Per-pod worker certs live no longer than an allocation or the pod, so
    /// no `kind="worker"` expiry is exported. A forgotten pool drops its gauges.
    #[test]
    fn forgotten_pools_drop_gauges_and_no_worker_cert_series() {
        let m = Metrics::new();
        let rec = ReconcileMetrics::register(&m);
        let r = Report::default();
        rec.pass("ns", "a", Some(&status()), PodCounts::default(), &r, 0.0);
        rec.pass("ns", "b", Some(&status()), PodCounts::default(), &r, 0.0);
        rec.forget("ns", "b");
        let text = m.render();
        assert!(!text.contains("pool=\"b\""), "{text}");
        assert!(text.contains("pool=\"a\""), "{text}");
        assert!(!text.contains("kind=\"worker\""), "{text}");
    }
}
