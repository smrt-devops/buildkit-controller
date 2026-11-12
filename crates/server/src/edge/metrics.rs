//! Edge metric families.

use std::sync::Arc;

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;

use crate::metrics_doc::{Def, Handle, Label, Labels, Role, register_table};
use crate::pki::AllocationId;
use crate::runtime::metrics::{Metrics, PoolSeries};

/// A histogram family whose series share one bucket layout.
type Histograms = Family<Labels, Histogram, fn() -> Histogram>;

/// Buckets from 1 ms to about 4 s: 1 ms doubling over 13 buckets.
pub const BUCKETS: [f64; 13] = [
    0.001, 0.002, 0.004, 0.008, 0.016, 0.032, 0.064, 0.128, 0.256, 0.512, 1.024, 2.048, 4.096,
];

fn seconds() -> Histogram {
    Histogram::new(BUCKETS)
}

const TRANSPORTS: &[&str] = &["tcp", "tunnel"];
/// `result` values of `buildkit_edge_connections_total`.
const RESULTS: &[&str] = &[
    "ok",
    "tls_error",
    "bad_san",
    "pod_not_found",
    "alloc_mismatch",
    "expired",
    "pod_not_ready",
    "store_stale",
    "upstream_error",
    "allocation_limit",
    "namespace_limit",
    "connection_limit",
    "handshake_limit",
    "peer_handshake_limit",
];
/// The results counted before the allocation is known, with empty namespace and pool.
const UNATTRIBUTED: &[&str] = &[
    "tls_error",
    "bad_san",
    "handshake_limit",
    "peer_handshake_limit",
];
const DIRECTIONS: &[&str] = &["client_to_worker", "worker_to_client"];
const REVOCATIONS: &[&str] = &["released", "expired", "pod_gone", "drain"];

/// The edge's families. Each carries the `transport` the client came in by.
pub const TABLE: [Def; 6] = [
    Def::counter(
        "edge_connections",
        Role::Controller,
        &[
            Label::namespace(),
            Label::pool(),
            Label::closed("result", RESULTS),
            Label::closed("transport", TRANSPORTS),
        ],
        "Client connections to the edge by outcome. `ok`: proxied. `tls_error`, `bad_san`: the client certificate was refused before an allocation was known (empty namespace and pool). `pod_not_found`, `alloc_mismatch`, `expired`, `pod_not_ready`: the allocation's pod or lease failed a check. `store_stale`: a pool or pod watch erred since its last completed list. `upstream_error`: the worker could not be reached. `allocation_limit`, `namespace_limit`, `connection_limit`: a connection cap. `handshake_limit`, `peer_handshake_limit`: an unfinished handshake was evicted by a newer one",
    ),
    Def::gauge(
        "edge_active_connections",
        Role::Controller,
        &[
            Label::namespace(),
            Label::pool(),
            Label::closed("transport", TRANSPORTS),
        ],
        "Proxied connections open now",
    ),
    Def::histogram(
        "edge_handshake_seconds",
        Role::Controller,
        &[Label::closed("transport", TRANSPORTS)],
        &BUCKETS,
        "Client TLS handshake duration, including certificate verification",
    ),
    Def::histogram(
        "edge_upstream_connect_seconds",
        Role::Controller,
        &[Label::closed("transport", TRANSPORTS)],
        &BUCKETS,
        "Time from the end of the client handshake to the end of the upstream TLS handshake: the edge's own overhead",
    ),
    Def::counter(
        "edge_bytes",
        Role::Controller,
        &[
            Label::closed("direction", DIRECTIONS),
            Label::closed("transport", TRANSPORTS),
        ],
        "Bytes proxied between clients and workers",
    ),
    Def::counter(
        "edge_revocations",
        Role::Controller,
        &[
            Label::closed("reason", REVOCATIONS),
            Label::closed("transport", TRANSPORTS),
        ],
        "Live connections closed because their allocation ended. `released`: the pod's allocation changed. `expired`: the certificate's notAfter passed. `pod_gone`: the pod was deleted. `drain`: shutdown ran out of drain time",
    ),
];

/// How a client connection reached the edge: the `transport` label and log
/// field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Transport {
    /// The edge's own TCP listener.
    Tcp,
    /// A WebSocket on the API listener (`GET /v1/tunnel`).
    Tunnel,
}

impl Transport {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Tunnel => "tunnel",
        }
    }
}

/// `buildkit_edge_*` families; clone-cheap handles into the shared registry.
/// Every series this handle touches carries its [`Transport`] (`tcp` unless
/// set with [`EdgeMetrics::for_transport`]).
#[derive(Debug, Clone)]
pub struct EdgeMetrics {
    transport: Transport,
    connections: Family<Labels, Counter>,
    active: Family<Labels, Gauge>,
    handshake: Histograms,
    upstream_connect: Histograms,
    bytes: Family<Labels, Counter>,
    revocations: Family<Labels, Counter>,
}

impl EdgeMetrics {
    /// Registers the families; call once per registry.
    #[must_use]
    pub fn register(metrics: &Metrics) -> Self {
        let m = Self {
            transport: Transport::Tcp,
            connections: Family::default(),
            active: Family::default(),
            handshake: Family::new_with_constructor(seconds),
            upstream_connect: Family::new_with_constructor(seconds),
            bytes: Family::default(),
            revocations: Family::default(),
        };
        metrics.with_registry(|r| {
            register_table(
                r,
                Role::Controller,
                &TABLE,
                vec![
                    ("edge_connections", Handle::new(m.connections.clone())),
                    ("edge_active_connections", Handle::new(m.active.clone())),
                    ("edge_handshake_seconds", Handle::new(m.handshake.clone())),
                    (
                        "edge_upstream_connect_seconds",
                        Handle::new(m.upstream_connect.clone()),
                    ),
                    ("edge_bytes", Handle::new(m.bytes.clone())),
                    ("edge_revocations", Handle::new(m.revocations.clone())),
                ],
            );
        });
        TABLE[2].seed(None, &m.handshake);
        TABLE[3].seed(None, &m.upstream_connect);
        TABLE[4].seed(None, &m.bytes);
        TABLE[5].seed(None, &m.revocations);
        for transport in TRANSPORTS {
            for result in UNATTRIBUTED {
                drop(m.connections.get_or_create(&vec![
                    ("namespace", String::new()),
                    ("pool", String::new()),
                    ("result", (*result).to_owned()),
                    ("transport", (*transport).to_owned()),
                ]));
            }
        }
        metrics.track_pools(Arc::new(m.clone()));
        m
    }

    /// The same families, labelled with `transport`.
    #[must_use]
    pub fn for_transport(&self, transport: Transport) -> Self {
        Self {
            transport,
            ..self.clone()
        }
    }

    fn with_transport(&self, mut labels: Labels) -> Labels {
        labels.push(("transport", self.transport.label().into()));
        labels
    }

    /// Counts one connection outcome; `id` is `None` before a SAN parsed.
    pub fn outcome(&self, id: Option<&AllocationId>, result: &'static str) {
        self.connections
            .get_or_create(&self.labels(id, result))
            .inc();
    }

    /// Current count for one label set (tests, debugging).
    #[must_use]
    pub fn outcome_count(&self, id: Option<&AllocationId>, result: &'static str) -> u64 {
        self.connections
            .get_or_create(&self.labels(id, result))
            .get()
    }

    /// `buildkit_edge_active_connections{namespace,pool,transport}`, for the proxy.
    #[must_use]
    pub fn active(&self, id: &AllocationId) -> Gauge {
        self.active
            .get_or_create(&self.with_transport(vec![
                ("namespace", id.namespace.to_string()),
                ("pool", id.pool.to_string()),
            ]))
            .clone()
    }

    pub(crate) fn handshake_done(&self, seconds: f64) {
        self.handshake
            .get_or_create(&self.with_transport(Vec::new()))
            .observe(seconds);
    }

    pub(crate) fn upstream_connected(&self, seconds: f64) {
        self.upstream_connect
            .get_or_create(&self.with_transport(Vec::new()))
            .observe(seconds);
    }

    /// `buildkit_edge_bytes_total{direction,transport}`.
    #[must_use]
    pub fn bytes(&self, direction: Direction) -> Counter {
        self.bytes
            .get_or_create(&self.with_transport(vec![("direction", direction.label().into())]))
            .clone()
    }

    /// Counts one `buildkit_edge_revocations_total{reason,transport}`.
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
            .get_or_create(&self.with_transport(vec![("reason", reason.label().into())]))
            .clone()
    }

    fn labels(&self, id: Option<&AllocationId>, result: &'static str) -> Labels {
        let (ns, pool) = id.map_or((String::new(), String::new()), |id| {
            (id.namespace.to_string(), id.pool.to_string())
        });
        self.with_transport(vec![
            ("namespace", ns),
            ("pool", pool),
            ("result", result.into()),
        ])
    }
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
    /// The drain budget (`shutdown.drainTimeout`, or the longest connection
    /// lifetime) ran out and the connection was aborted.
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

impl PoolSeries for EdgeMetrics {
    fn seed(&self, namespace: &str, pool: &str) {
        TABLE[0].seed(Some((namespace, pool)), &self.connections);
        TABLE[1].seed(Some((namespace, pool)), &self.active);
    }

    fn forget(&self, namespace: &str, pool: &str) {
        TABLE[0].forget(namespace, pool, &self.connections);
        TABLE[1].forget(namespace, pool, &self.active);
    }
}
