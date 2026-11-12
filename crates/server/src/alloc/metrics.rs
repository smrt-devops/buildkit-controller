//! Allocation metric families.

use std::sync::Arc;

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::Histogram;

use crate::metrics_doc::{Def, Handle, Label, Labels, POOL, Role, register_table};
use crate::runtime::metrics::{Metrics, PoolSeries, RELEASE_REASONS, RELEASES};

/// Allocation histogram buckets, in seconds: 0.05 s doubling over 14 buckets.
pub const BUCKETS: [f64; 14] = [
    0.05, 0.1, 0.2, 0.4, 0.8, 1.6, 3.2, 6.4, 12.8, 25.6, 51.2, 102.4, 204.8, 409.6,
];

/// Hold-time buckets, in seconds: 1 s to 8 h.
pub const HELD_BUCKETS: [f64; 14] = [
    1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0, 14400.0,
    28800.0,
];

/// `buildkit_allocations_total{result}`: `ok` and every API error code.
pub const RESULTS: &[&str] = &[
    "ok",
    "invalid_request",
    "unauthenticated",
    "forbidden",
    "pool_not_found",
    "not_found",
    "payload_too_large",
    "unsupported_media_type",
    "internal",
    "pool_exhausted",
    "pod_lost",
    "not_ready",
    "ca_unavailable",
    "upstream_error",
];

const PATHS: &[&str] = &["warm", "cold"];
const QUEUE_RESULTS: &[&str] = &["ok", "timeout"];
const TIERS: &[&str] = &["trusted", "untrusted"];

/// The families alloc owns, and `buildkit_releases_total` through the runtime's.
pub const TABLE: [Def; 6] = [
    Def::counter(
        "allocations",
        Role::Controller,
        &[
            Label::namespace(),
            Label::pool(),
            Label::closed("result", RESULTS),
        ],
        "Allocation outcomes. The API replica counts the requests it refuses, with the error code as `result`. The leader counts how each claim ends: `ok` when it binds, `pool_exhausted` at its queue timeout, and so on. A poll or a 202 counts nothing. `pool_not_found` carries empty namespace and pool",
    ),
    Def::histogram(
        "allocation_duration_seconds",
        Role::Controller,
        &[
            Label::namespace(),
            Label::pool(),
            Label::closed("path", PATHS),
        ],
        &BUCKETS,
        "Time from claim creation to bind, for claims that bound. `warm`: the leader bound it on its first look. `cold`: it waited for a worker",
    ),
    Def::histogram(
        "queue_wait_seconds",
        Role::Controller,
        &[
            Label::namespace(),
            Label::pool(),
            Label::closed("result", QUEUE_RESULTS),
        ],
        &BUCKETS,
        "Time a claim spent queued, from its creation. `ok`: it bound. `timeout`: it ran out of queue time",
    ),
    Def::histogram(
        "allocation_held_seconds",
        Role::Controller,
        &[
            Label::namespace(),
            Label::pool(),
            Label::closed("reason", RELEASE_REASONS),
        ],
        &HELD_BUCKETS,
        "How long a worker was held, from its bind to its retirement, by the same reasons as `buildkit_releases_total`. Use it to size scaling.max and the TTLs",
    ),
    Def::counter(
        "claim_conflicts",
        Role::Controller,
        &POOL,
        "Claim patches the leader lost to another writer (HTTP 422, 409 or 404)",
    ),
    Def::counter(
        "cache_allocations",
        Role::Controller,
        &[
            Label::namespace(),
            Label::pool(),
            Label::closed("tier", TIERS),
        ],
        "Claims that bound with cache flags, by cache tier",
    ),
];

fn new_hist() -> Histogram {
    Histogram::new(BUCKETS)
}

pub(crate) fn held_hist() -> Histogram {
    Histogram::new(HELD_BUCKETS)
}

pub(crate) type Hist = Family<Labels, Histogram, fn() -> Histogram>;

/// The families alloc owns; cheap to clone (shared handles).
#[derive(Clone, Debug)]
pub struct AllocMetrics {
    allocations: Family<Labels, Counter>,
    duration: Hist,
    queue_wait: Hist,
    held: Hist,
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
            held: Family::new_with_constructor(held_hist),
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
    pub fn with_releases(releases: Family<Labels, Counter>, held: Hist) -> Self {
        Self {
            held,
            releases,
            ..Self::default()
        }
    }

    /// Register every family but `releases` (registered by the runtime) and
    /// start tracking pools for the per-pool series.
    pub fn register(&self, metrics: &Metrics) {
        metrics.with_registry(|r| {
            register_table(
                r,
                Role::Controller,
                &TABLE,
                vec![
                    ("allocations", Handle::new(self.allocations.clone())),
                    (
                        "allocation_duration_seconds",
                        Handle::new(self.duration.clone()),
                    ),
                    ("queue_wait_seconds", Handle::new(self.queue_wait.clone())),
                    ("allocation_held_seconds", Handle::new(self.held.clone())),
                    ("claim_conflicts", Handle::new(self.conflicts.clone())),
                    ("cache_allocations", Handle::new(self.cache.clone())),
                ],
            );
        });
        metrics.track_pools(Arc::new(self.clone()));
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

    /// `reason` = `released` (the claim was deleted or ended), `allocation_idle`
    /// (its delivered claim went unused) or `pod_lost` (the claimed pod was
    /// terminating). `held` is how long the worker was held, when known.
    pub fn release(&self, ns: &str, p: &str, reason: &str, held: Option<f64>) {
        self.releases
            .get_or_create(&with(pool(ns, p), "reason", reason))
            .inc();
        if let Some(secs) = held {
            self.held
                .get_or_create(&with(pool(ns, p), "reason", reason))
                .observe(secs);
        }
    }

    /// Releases counted for `reason` (tests).
    #[must_use]
    #[cfg(test)]
    pub fn released(&self, ns: &str, p: &str, reason: &str) -> u64 {
        self.releases
            .get_or_create(&with(pool(ns, p), "reason", reason))
            .get()
    }

    /// One `201` that carried cache flags; `tier` = `trusted` or `untrusted`.
    pub fn cache(&self, ns: &str, p: &str, tier: &str) {
        self.cache
            .get_or_create(&with(pool(ns, p), "tier", tier))
            .inc();
    }
}

impl PoolSeries for AllocMetrics {
    fn seed(&self, ns: &str, p: &str) {
        let at = Some((ns, p));
        TABLE[0].seed(at, &self.allocations);
        TABLE[1].seed(at, &self.duration);
        TABLE[2].seed(at, &self.queue_wait);
        TABLE[3].seed(at, &self.held);
        TABLE[4].seed(at, &self.conflicts);
        TABLE[5].seed(at, &self.cache);
        RELEASES.seed(at, &self.releases);
    }

    fn forget(&self, ns: &str, p: &str) {
        TABLE[0].forget(ns, p, &self.allocations);
        TABLE[1].forget(ns, p, &self.duration);
        TABLE[2].forget(ns, p, &self.queue_wait);
        TABLE[3].forget(ns, p, &self.held);
        TABLE[4].forget(ns, p, &self.conflicts);
        TABLE[5].forget(ns, p, &self.cache);
        RELEASES.forget(ns, p, &self.releases);
    }
}
