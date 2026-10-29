//! Allocation metric families.

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;

type Labels = Vec<(&'static str, String)>;

fn new_hist() -> Histogram {
    Histogram::new(exponential_buckets(0.05, 2.0, 14))
}

/// The families alloc owns; cheap to clone (shared handles).
#[derive(Clone, Debug)]
pub struct AllocMetrics {
    allocations: Family<Labels, Counter>,
    duration: Family<Labels, Histogram, fn() -> Histogram>,
    queue_wait: Family<Labels, Histogram, fn() -> Histogram>,
    conflicts: Family<Labels, Counter>,
    releases: Family<Labels, Counter>,
    cache: Family<Labels, Counter>,
}

impl Default for AllocMetrics {
    fn default() -> Self {
        Self {
            allocations: Family::default(),
            duration: Family::new_with_constructor(new_hist),
            queue_wait: Family::new_with_constructor(new_hist),
            conflicts: Family::default(),
            releases: Family::default(),
            cache: Family::default(),
        }
    }
}

fn pool(ns: &str, pool: &str) -> Labels {
    vec![("namespace", ns.to_owned()), ("pool", pool.to_owned())]
}

fn with(mut l: Labels, k: &'static str, v: &str) -> Labels {
    l.push((k, v.to_owned()));
    l
}

impl AllocMetrics {
    /// Families of its own, writing releases into the shared
    /// [`crate::runtime::metrics::Metrics::releases`] family.
    #[must_use]
    pub fn with_releases(releases: Family<Labels, Counter>) -> Self {
        Self {
            releases,
            ..Self::default()
        }
    }

    /// Register every family but `releases` (registered by the runtime;
    /// `buildkit_` prefix comes from the registry).
    pub fn register(&self, r: &mut Registry) {
        r.register(
            "allocations",
            "Allocation outcomes: refusals by the API, and how each claim ended",
            self.allocations.clone(),
        );
        r.register(
            "allocation_duration_seconds",
            "Successful allocation, claim creation to bind",
            self.duration.clone(),
        );
        r.register(
            "queue_wait_seconds",
            "Time claims spent queued, by how the wait ended",
            self.queue_wait.clone(),
        );
        r.register(
            "claim_conflicts",
            "Lost claim patches",
            self.conflicts.clone(),
        );
        r.register(
            "cache_allocations",
            "Allocations whose response carried cache flags, by tier",
            self.cache.clone(),
        );
    }

    /// `result` = `ok` or the API error code verbatim.
    pub fn allocation(&self, ns: &str, p: &str, result: &str) {
        self.allocations
            .get_or_create(&with(pool(ns, p), "result", result))
            .inc();
    }

    /// `path` = `warm` or `cold`.
    pub fn duration(&self, ns: &str, p: &str, path: &str, secs: f64) {
        self.duration
            .get_or_create(&with(pool(ns, p), "path", path))
            .observe(secs);
    }

    /// `result` = `ok` or `timeout`.
    pub fn queue_wait(&self, ns: &str, p: &str, result: &str, secs: f64) {
        self.queue_wait
            .get_or_create(&with(pool(ns, p), "result", result))
            .observe(secs);
    }

    /// A lost claim (422/409/404).
    pub fn conflict(&self, ns: &str, p: &str) {
        self.conflicts.get_or_create(&pool(ns, p)).inc();
    }

    /// `reason` = `released` (the claim was deleted or ended) or `pod_lost` (the claimed pod was
    /// terminating).
    pub fn release(&self, ns: &str, p: &str, reason: &str) {
        self.releases
            .get_or_create(&with(pool(ns, p), "reason", reason))
            .inc();
    }

    /// One `201` that carried cache flags; `tier` = `trusted` or `untrusted`.
    pub fn cache(&self, ns: &str, p: &str, tier: &str) {
        self.cache
            .get_or_create(&with(pool(ns, p), "tier", tier))
            .inc();
    }
}
