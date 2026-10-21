//! Reconciler metric families (leader only).

use buildkit_controller_api::v1alpha1::{BuildKitPool, BuildKitPoolStatus};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;

use super::exec::Report;
use super::pods::{Alloc, WorkerPod};
use crate::runtime::metrics::Metrics;

type Labels = Vec<(&'static str, String)>;

/// Pod counts of one pass that the pool status does not carry.
#[derive(Debug, Clone, Copy, Default)]
pub struct PodCounts {
    /// Pods with `alloc=draining`.
    pub draining: usize,
    /// Pods in cached slots, each of which mounts its slot's volume; `None` when `cache.local` is unset.
    pub volumes_in_use: Option<usize>,
}

impl PodCounts {
    /// Count `workers` of `pool`. A pod in a slot at or above the limit mounts
    /// no volume.
    #[must_use]
    pub fn of(pool: &BuildKitPool, workers: &[WorkerPod]) -> Self {
        let limit = crate::render::cache_limit(pool);
        Self {
            draining: workers
                .iter()
                .filter(|p| p.alloc == Alloc::Draining)
                .count(),
            volumes_in_use: pool.spec.workers.cache.local.is_some().then(|| {
                workers
                    .iter()
                    .filter(|p| p.slot.is_some_and(|s| s < limit))
                    .count()
            }),
        }
    }
}

/// Histogram buckets, in seconds.
pub const BUCKETS: [f64; 16] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0,
];

/// `buildkit_pool_*`, `buildkit_pod_operations_total`, `buildkit_reconcile_*`,
/// `buildkit_ca_replacements_total`, `buildkit_releases_total` (the runtime's shared family).
#[derive(Debug, Clone)]
pub struct ReconcileMetrics {
    pool_pods: Family<Labels, Gauge>,
    desired_warm: Family<Labels, Gauge>,
    waiting: Family<Labels, Gauge>,
    pod_ops: Family<Labels, Counter>,
    /// `buildkit_cache_volume_operations_total{op,result}`.
    volume_ops: Family<Labels, Counter>,
    /// `buildkit_cache_volumes_in_use{namespace,pool}`.
    volumes_in_use: Family<Labels, Gauge>,
    reconcile: Family<Labels, Counter>,
    duration: Histogram,
    ca_replacements: Family<Labels, Counter>,
    releases: Family<Labels, Counter>,
}

impl ReconcileMetrics {
    /// Registers the families; call once per registry.
    #[must_use]
    pub fn register(metrics: &Metrics) -> Self {
        let m = Self {
            pool_pods: Family::default(),
            desired_warm: Family::default(),
            waiting: Family::default(),
            pod_ops: Family::default(),
            volume_ops: Family::default(),
            volumes_in_use: Family::default(),
            reconcile: Family::default(),
            duration: Histogram::new(BUCKETS),
            ca_replacements: Family::default(),
            releases: metrics.releases(),
        };
        metrics.with_registry(|r| {
            r.register("pool_pods", "Pods of a pool by state", m.pool_pods.clone());
            r.register(
                "pool_desired_warm",
                "status.desiredWarm",
                m.desired_warm.clone(),
            );
            r.register("pool_waiting", "status.waiting", m.waiting.clone());
            r.register(
                "pod_operations",
                "Pod creates, retires and deletes",
                m.pod_ops.clone(),
            );
            r.register(
                "cache_volume_operations",
                "Cache volume creates and deletes",
                m.volume_ops.clone(),
            );
            r.register(
                "cache_volumes_in_use",
                "Cache volumes mounted by a pod of the pool",
                m.volumes_in_use.clone(),
            );
            r.register("reconcile", "Pool passes by result", m.reconcile.clone());
            r.register(
                "reconcile_duration_seconds",
                "One pool pass",
                m.duration.clone(),
            );
            r.register(
                "ca_replacements",
                "CA Secret creations and replacements",
                m.ca_replacements.clone(),
            );
        });
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
        match counts.volumes_in_use {
            Some(n) => {
                self.volumes_in_use
                    .get_or_create(&p(None))
                    .set(i64::try_from(n).unwrap_or(i64::MAX));
            }
            None => {
                self.volumes_in_use.remove(&p(None));
            }
        }
        for (op, result) in &report.pod_ops {
            self.pod_ops
                .get_or_create(&vec![("op", (*op).into()), ("result", (*result).into())])
                .inc();
        }
        for (op, result) in &report.volume_ops {
            self.volume_ops
                .get_or_create(&vec![("op", (*op).into()), ("result", (*result).into())])
                .inc();
        }
        self.record_ca(report);
        for reason in &report.releases {
            self.releases
                .get_or_create(&p(Some(("reason", (*reason).into()))))
                .inc();
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
        self.volumes_in_use.clear();
    }

    /// Drops a deleted pool's gauges.
    pub fn forget(&self, ns: &str, pool: &str) {
        let base = || vec![("namespace", ns.to_owned()), ("pool", pool.to_owned())];
        for state in ["warm", "starting", "allocated", "draining"] {
            let mut l = base();
            l.push(("state", state.into()));
            self.pool_pods.remove(&l);
        }
        self.desired_warm.remove(&base());
        self.waiting.remove(&base());
        self.volumes_in_use.remove(&base());
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
        let alloc = AllocMetrics::with_releases(m.releases());
        m.with_registry(|r| alloc.register(r));
        let rec = ReconcileMetrics::register(&m);
        alloc.release("ns", "p", "released");
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
                    PodCounts {
                        draining: 1,
                        volumes_in_use: Some(1),
                    },
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
            "buildkit_cache_volumes_in_use{",
        ] {
            assert!(!text.contains(g), "{g} survived the term:\n{text}");
        }
        assert!(text.contains("buildkit_reconcile_total{"), "counters stay");
    }

    #[test]
    fn volumes_in_use_follows_the_pass_and_goes_with_cache_local() {
        let m = Metrics::new();
        let rec = ReconcileMetrics::register(&m);
        let sample = "buildkit_cache_volumes_in_use{namespace=\"ns\",pool=\"p\"}";
        let counts = |n| PodCounts {
            draining: 0,
            volumes_in_use: n,
        };
        rec.pass(
            "ns",
            "p",
            Some(&status()),
            counts(Some(3)),
            &Report::default(),
            0.0,
        );
        assert!(
            m.render().contains(&format!("{sample} 3")),
            "{}",
            m.render()
        );
        rec.pass(
            "ns",
            "p",
            Some(&status()),
            counts(Some(1)),
            &Report::default(),
            0.0,
        );
        assert!(
            m.render().contains(&format!("{sample} 1")),
            "{}",
            m.render()
        );
        // cache.local was removed from the pool: the series goes.
        rec.pass(
            "ns",
            "p",
            Some(&status()),
            counts(None),
            &Report::default(),
            0.0,
        );
        assert!(!m.render().contains(sample), "{}", m.render());
        rec.pass(
            "ns",
            "p",
            Some(&status()),
            counts(Some(2)),
            &Report::default(),
            0.0,
        );
        rec.forget("ns", "p");
        assert!(!m.render().contains(sample), "{}", m.render());
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
