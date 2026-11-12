//! `buildkit_registry_*` metric families.

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;

use super::store::{GcReport, Stats};
use crate::metrics_doc::{Def, Handle, Label, Labels, Role, register_table};
use crate::runtime::metrics::Metrics;

type HistogramFamily = Family<Labels, Histogram, fn() -> Histogram>;

/// Request duration buckets, in seconds.
pub const BUCKETS: [f64; 14] = [
    0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0,
];

const ROUTES: &[&str] = &["base", "blob", "upload", "manifest", "other"];
const METHODS: &[&str] = &["GET", "HEAD", "POST", "PATCH", "PUT", "DELETE", "other"];
const ACTIONS: &[&str] = &["pull", "push"];
const REFUSALS: &[&str] = &[
    "tls_error",
    "handshake_timeout",
    "bad_certificate",
    "client_auth_eku",
    "bad_san",
];
const BUDGETS: &[&str] = &["pool", "volume"];

/// The cache registry's families.
pub const TABLE: [Def; 10] = [
    Def::counter(
        "registry_requests",
        Role::Registry,
        &[
            Label::closed("route", ROUTES),
            Label::closed("method", METHODS),
            Label::open("status"),
        ],
        "Registry requests by matched route, method and HTTP status. `other` is any unmatched path or method",
    ),
    Def::histogram(
        "registry_request_duration_seconds",
        Role::Registry,
        &[Label::closed("route", ROUTES)],
        &BUCKETS,
        "Registry request duration until the response head",
    ),
    Def::counter(
        "registry_denied",
        Role::Registry,
        &[Label::closed("action", ACTIONS)],
        "Requests refused by the worker's grant, by the action asked for. Denied `push` is expected for untrusted allocations",
    ),
    Def::counter(
        "registry_connections_refused",
        Role::Registry,
        &[Label::closed("reason", REFUSALS)],
        "Connections closed before a request: a failed or timed-out handshake, or a client certificate that names no worker",
    ),
    Def::counter(
        "registry_received_bytes",
        Role::Registry,
        &[],
        "Blob and manifest bytes received in pushes",
    ),
    Def::counter(
        "registry_gc_evicted_manifests",
        Role::Registry,
        &[Label::closed("budget", BUDGETS)],
        "Manifests evicted by garbage collection to fit a budget: `pool` is a pool's share, `volume` is the whole store",
    ),
    Def::counter(
        "registry_gc_deleted_blobs",
        Role::Registry,
        &[],
        "Blobs deleted by garbage collection",
    ),
    Def::gauge(
        "registry_stored_bytes",
        Role::Registry,
        &[],
        "Bytes of blobs in the store. Compare it with the store budget (`--max-size`)",
    ),
    Def::gauge("registry_blobs", Role::Registry, &[], "Blobs in the store"),
    Def::gauge(
        "registry_manifests",
        Role::Registry,
        &[],
        "Manifests in the store, over all repositories",
    ),
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
            register_table(
                r,
                Role::Registry,
                &TABLE,
                vec![
                    ("registry_requests", Handle::new(m.requests.clone())),
                    (
                        "registry_request_duration_seconds",
                        Handle::new(m.duration.clone()),
                    ),
                    ("registry_denied", Handle::new(m.denied.clone())),
                    (
                        "registry_connections_refused",
                        Handle::new(m.refused.clone()),
                    ),
                    ("registry_received_bytes", Handle::new(m.received.clone())),
                    (
                        "registry_gc_evicted_manifests",
                        Handle::new(m.evicted.clone()),
                    ),
                    (
                        "registry_gc_deleted_blobs",
                        Handle::new(m.deleted_blobs.clone()),
                    ),
                    ("registry_stored_bytes", Handle::new(m.stored_bytes.clone())),
                    ("registry_blobs", Handle::new(m.blobs.clone())),
                    ("registry_manifests", Handle::new(m.manifests.clone())),
                ],
            );
        });
        TABLE[1].seed(None, &m.duration);
        TABLE[2].seed(None, &m.denied);
        TABLE[3].seed(None, &m.refused);
        TABLE[5].seed(None, &m.evicted);
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
