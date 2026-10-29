//! The metrics registry. The runtime owns the families
//! it needs; other modules add theirs with [`Metrics::with_registry`].

use std::sync::Mutex;

use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use tokio::sync::watch;

use crate::election::LeaderState;

type KindLabel = Vec<(&'static str, &'static str)>;

/// `namespace`, `pool` and one more label.
pub type PoolLabels = Vec<(&'static str, String)>;

/// Shared registry plus the runtime's own families.
#[derive(Debug)]
pub struct Metrics {
    registry: Mutex<Registry>,
    leader: Gauge,
    leader_transitions: Counter,
    watch_errors: Family<KindLabel, Counter>,
    namespace_unlisted: Family<PoolLabels, Gauge>,
    ca_valid: Gauge,
    cert_load_errors: Family<KindLabel, Counter>,
    cert_not_after: Family<KindLabel, Gauge>,
    releases: Family<PoolLabels, Counter>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// A registry with prefix `buildkit` and the runtime families registered.
    #[must_use]
    pub fn new() -> Self {
        let mut registry = Registry::with_prefix("buildkit");
        let m = Self {
            registry: Mutex::new(Registry::default()),
            leader: Gauge::default(),
            leader_transitions: Counter::default(),
            watch_errors: Family::default(),
            namespace_unlisted: Family::default(),
            ca_valid: Gauge::default(),
            cert_load_errors: Family::default(),
            cert_not_after: Family::default(),
            releases: Family::default(),
        };
        registry.register("leader", "1 while this replica leads", m.leader.clone());
        registry.register(
            "leader_transitions",
            "Leadership gained or lost by this replica",
            m.leader_transitions.clone(),
        );
        registry.register(
            "watch_errors",
            "Watcher error items",
            m.watch_errors.clone(),
        );
        registry.register(
            "watch_namespace_unlisted",
            "1 while a watch carries on without a namespace it cannot list",
            m.namespace_unlisted.clone(),
        );
        registry.register(
            "ca_valid",
            "1 when the CA Secret is usable",
            m.ca_valid.clone(),
        );
        registry.register(
            "cert_load_errors",
            "Certificate load failures",
            m.cert_load_errors.clone(),
        );
        registry.register(
            "cert_not_after_seconds",
            "notAfter (Unix seconds) of the CAs and this replica's in-memory leaves",
            m.cert_not_after.clone(),
        );
        // Alloc and reconcile share this family, so it has two writers.
        registry.register(
            "releases",
            "Retirements of allocated pods",
            m.releases.clone(),
        );
        Self {
            registry: Mutex::new(registry),
            ..m
        }
    }

    /// Run `f` with the registry, to register more families (use prefix-free names; `buildkit_` is added).
    pub fn with_registry<T>(&self, f: impl FnOnce(&mut Registry) -> T) -> T {
        f(&mut self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// Count a watcher `Err` item; `kind` is `pool`, `pod`, `ca_secret`, `serving_secret`, `lease` or `controller`.
    pub fn watch_error(&self, kind: &'static str) {
        self.watch_errors.get_or_create(&vec![("kind", kind)]).inc();
    }

    /// Set `buildkit_watch_namespace_unlisted{kind,namespace}`; `kind` is `pool` or `pod`.
    pub fn set_namespace_unlisted(&self, kind: &'static str, namespace: &str, unlisted: bool) {
        self.namespace_unlisted
            .get_or_create(&vec![
                ("kind", kind.to_owned()),
                ("namespace", namespace.to_owned()),
            ])
            .set(i64::from(unlisted));
    }

    /// Count a certificate load failure; `cert` is `ca` or `edge_serving`.
    pub fn cert_load_error(&self, cert: &'static str) {
        self.cert_load_errors
            .get_or_create(&vec![("cert", cert)])
            .inc();
    }

    /// Set `buildkit_cert_not_after_seconds{kind}`.
    pub fn set_cert_not_after(&self, kind: crate::pki::CertKind, not_after: time::OffsetDateTime) {
        self.cert_not_after
            .get_or_create(&vec![("kind", kind.label())])
            .set(not_after.unix_timestamp());
    }

    /// `buildkit_cert_not_after_seconds`, for the leader's `kind="worker"` series.
    #[must_use]
    pub fn cert_not_after(&self) -> Family<Vec<(&'static str, &'static str)>, Gauge> {
        self.cert_not_after.clone()
    }

    /// `buildkit_releases_total{namespace,pool,reason}`, shared by alloc and reconcile.
    #[must_use]
    pub fn releases(&self) -> Family<PoolLabels, Counter> {
        self.releases.clone()
    }

    /// Set `buildkit_ca_valid`.
    pub fn set_ca_valid(&self, valid: bool) {
        self.ca_valid.set(i64::from(valid));
    }

    /// Reflect leadership into `buildkit_leader` and `buildkit_leader_transitions_total`.
    pub async fn follow_leader(&self, mut rx: watch::Receiver<LeaderState>) {
        let mut was = false;
        loop {
            let now = rx.borrow_and_update().is_leader();
            self.leader.set(i64::from(now));
            if now != was {
                self.leader_transitions.inc();
                was = now;
            }
            if rx.changed().await.is_err() {
                break;
            }
        }
    }

    /// Prometheus text exposition.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        let r = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if encode(&mut out, &r).is_err() {
            out.clear();
        }
        out
    }
}
