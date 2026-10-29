//! `buildkit_registry_*` metric families.

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;

use super::store::{GcReport, Stats};
use crate::runtime::metrics::Metrics;

type Labels = Vec<(&'static str, String)>;
type HistogramFamily = Family<Labels, Histogram, fn() -> Histogram>;

const BUCKETS: [f64; 14] = [
    0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0,
];

fn histogram() -> Histogram {
    Histogram::new(BUCKETS)
}

/// The cache registry's families.
#[derive(Clone, Debug)]
pub struct RegistryMetrics {
    requests: Family<Labels, Counter>,
    duration: HistogramFamily,
    denied: Family<Labels, Counter>,
    refused: Family<Labels, Counter>,
    received: Counter,
    evicted: Family<Labels, Counter>,
    deleted_blobs: Counter,
    stored_bytes: Gauge,
    blobs: Gauge,
    manifests: Gauge,
}

impl Default for RegistryMetrics {
    fn default() -> Self {
        Self {
            requests: Family::default(),
            duration: Family::new_with_constructor(histogram),
            denied: Family::default(),
            refused: Family::default(),
            received: Counter::default(),
            evicted: Family::default(),
            deleted_blobs: Counter::default(),
            stored_bytes: Gauge::default(),
            blobs: Gauge::default(),
            manifests: Gauge::default(),
        }
    }
}

impl RegistryMetrics {
    /// Fresh families registered in `metrics`.
    #[must_use]
    pub fn register(metrics: &Metrics) -> Self {
        let m = Self::default();
        metrics.with_registry(|r| {
            r.register(
                "registry_requests",
                "Cache registry requests by route, method and status",
                m.requests.clone(),
            );
            r.register(
                "registry_request_duration_seconds",
                "Cache registry request duration until the response head",
                m.duration.clone(),
            );
            r.register(
                "registry_denied",
                "Cache registry requests refused by the grant, by action",
                m.denied.clone(),
            );
            r.register(
                "registry_connections_refused",
                "Connections closed before a request: failed handshake or a client certificate that names no worker",
                m.refused.clone(),
            );
            r.register(
                "registry_received_bytes",
                "Blob and manifest bytes received in pushes",
                m.received.clone(),
            );
            r.register(
                "registry_gc_evicted_manifests",
                "Manifests evicted by garbage collection, by budget",
                m.evicted.clone(),
            );
            r.register(
                "registry_gc_deleted_blobs",
                "Blobs deleted by garbage collection",
                m.deleted_blobs.clone(),
            );
            r.register(
                "registry_stored_bytes",
                "Bytes of blobs in the store",
                m.stored_bytes.clone(),
            );
            r.register("registry_blobs", "Blobs in the store", m.blobs.clone());
            r.register(
                "registry_manifests",
                "Manifests in the store, over all repositories",
                m.manifests.clone(),
            );
        });
        m
    }

    /// One finished request.
    pub fn request(&self, route: &'static str, method: &str, status: u16, secs: f64) {
        self.requests
            .get_or_create(&vec![
                ("route", route.to_owned()),
                ("method", method.to_owned()),
                ("status", status.to_string()),
            ])
            .inc();
        self.duration
            .get_or_create(&vec![("route", route.to_owned())])
            .observe(secs);
    }

    pub fn denied(&self, action: &'static str) {
        self.denied
            .get_or_create(&vec![("action", action.to_owned())])
            .inc();
    }

    pub fn refused(&self, reason: &'static str) {
        self.refused
            .get_or_create(&vec![("reason", reason.to_owned())])
            .inc();
    }

    pub fn received(&self, n: u64) {
        self.received.inc_by(n);
    }

    pub fn gc(&self, r: &GcReport) {
        for (reason, n) in [("pool", r.evicted_pool), ("volume", r.evicted_volume)] {
            self.evicted
                .get_or_create(&vec![("budget", reason.to_owned())])
                .inc_by(n);
        }
        self.deleted_blobs.inc_by(r.deleted_blobs);
    }

    pub fn stats(&self, s: Stats) {
        let g = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        self.stored_bytes.set(g(s.bytes));
        self.blobs.set(g(s.blobs));
        self.manifests.set(g(s.manifests));
    }
}
