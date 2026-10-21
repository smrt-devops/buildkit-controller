//! Edge metric families.

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};

use crate::pki::AllocationId;
use crate::runtime::metrics::Metrics;

type Labels = Vec<(&'static str, String)>;

/// `buildkit_edge_*` families; clone-cheap handles into the shared registry.
#[derive(Debug, Clone)]
pub struct EdgeMetrics {
    connections: Family<Labels, Counter>,
    active: Family<Labels, Gauge>,
    handshake: Histogram,
    upstream_connect: Histogram,
    bytes: Family<Labels, Counter>,
    revocations: Family<Labels, Counter>,
}

impl EdgeMetrics {
    /// Registers the families; call once per registry.
    #[must_use]
    pub fn register(metrics: &Metrics) -> Self {
        let m = Self {
            connections: Family::default(),
            active: Family::default(),
            // Buckets run from 1 ms to about 4 s.
            handshake: Histogram::new(exponential_buckets(0.001, 2.0, 13)),
            upstream_connect: Histogram::new(exponential_buckets(0.001, 2.0, 13)),
            bytes: Family::default(),
            revocations: Family::default(),
        };
        metrics.with_registry(|r| {
            r.register(
                "edge_connections",
                "Edge connections by outcome",
                m.connections.clone(),
            );
            r.register(
                "edge_active_connections",
                "Open proxied connections",
                m.active.clone(),
            );
            r.register(
                "edge_handshake_seconds",
                "Client TLS handshake including verification",
                m.handshake.clone(),
            );
            r.register(
                "edge_upstream_connect_seconds",
                "Client handshake done to upstream TLS done",
                m.upstream_connect.clone(),
            );
            r.register("edge_bytes", "Bytes proxied, by direction", m.bytes.clone());
            r.register(
                "edge_revocations",
                "Live edge connections closed by revocation",
                m.revocations.clone(),
            );
        });
        m
    }

    /// Counts one connection outcome; `id` is `None` before a SAN parsed.
    pub fn outcome(&self, id: Option<&AllocationId>, result: &'static str) {
        self.connections.get_or_create(&labels(id, result)).inc();
    }

    /// Current count for one label set (tests, debugging).
    #[must_use]
    pub fn outcome_count(&self, id: Option<&AllocationId>, result: &'static str) -> u64 {
        self.connections.get_or_create(&labels(id, result)).get()
    }

    /// `buildkit_edge_active_connections{namespace,pool}`, for the proxy.
    #[must_use]
    pub fn active(&self, id: &AllocationId) -> Gauge {
        self.active
            .get_or_create(&vec![
                ("namespace", id.namespace.to_string()),
                ("pool", id.pool.to_string()),
            ])
            .clone()
    }

    pub(crate) fn handshake_done(&self, seconds: f64) {
        self.handshake.observe(seconds);
    }

    pub(crate) fn upstream_connected(&self, seconds: f64) {
        self.upstream_connect.observe(seconds);
    }

    /// `buildkit_edge_bytes_total{direction}`.
    #[must_use]
    pub fn bytes(&self, direction: Direction) -> Counter {
        self.bytes
            .get_or_create(&vec![("direction", direction.label().into())])
            .clone()
    }

    /// Counts one `buildkit_edge_revocations_total{reason}`.
    pub fn revoked(&self, reason: Revocation) {
        self.revocation(reason).inc();
    }

    /// Current revocation count for one reason (tests, debugging).
    #[must_use]
    pub fn revocation_count(&self, reason: Revocation) -> u64 {
        self.revocation(reason).get()
    }

    fn revocation(&self, reason: Revocation) -> Counter {
        self.revocations
            .get_or_create(&vec![("reason", reason.label().into())])
            .clone()
    }
}

fn labels(id: Option<&AllocationId>, result: &'static str) -> Labels {
    let (ns, pool) = id.map_or((String::new(), String::new()), |id| {
        (id.namespace.to_string(), id.pool.to_string())
    });
    vec![("namespace", ns), ("pool", pool), ("result", result.into())]
}

/// `direction` label of `buildkit_edge_bytes_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ClientToWorker,
    WorkerToClient,
}

impl Direction {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ClientToWorker => "client_to_worker",
            Self::WorkerToClient => "worker_to_client",
        }
    }
}

/// `reason` label of `buildkit_edge_revocations_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revocation {
    /// The pod's alloc annotation no longer names this allocation (release,
    /// re-claim or `draining`).
    Released,
    /// The client cert's `notAfter` passed.
    Expired,
    /// The pod was deleted or has a `deletionTimestamp`.
    PodGone,
    /// `shutdown.drainTimeout` passed and the connection was aborted.
    Drain,
}

impl Revocation {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Released => "released",
            Self::Expired => "expired",
            Self::PodGone => "pod_gone",
            Self::Drain => "drain",
        }
    }
}
