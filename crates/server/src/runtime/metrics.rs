//! The metrics registry. The runtime owns the families
//! it needs; other modules add theirs with [`Metrics::with_registry`].

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use buildkit_controller_api::v1alpha1::BuildKitPool;
use kube::ResourceExt;
use kube::runtime::reflector::Store;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use tokio::sync::watch;

use crate::election::LeaderState;
use crate::metrics_doc::{Def, Handle, Label, Labels, Role, register_table};

/// `namespace`, `pool` and one more label.
pub type PoolLabels = Labels;

/// `buildkit_releases_total{reason}` values, written by the allocator
/// (`released`, `allocation_idle`, `pod_lost`) and the reconciler (the rest).
pub const RELEASE_REASONS: &[&str] = &[
    "released",
    "allocation_idle",
    "pod_lost",
    "expired",
    "failed",
    "start_timeout",
    "ca_replaced",
];

/// The `buildkit_releases_total` family, written by alloc and reconcile.
pub const RELEASES: Def = Def::counter(
    "releases",
    Role::Controller,
    &[
        Label::namespace(),
        Label::pool(),
        Label::closed("reason", RELEASE_REASONS),
    ],
    "Worker pods retired after an allocation. `released`: the client or its claim ended it. `allocation_idle`: a delivered claim went unused for the pool's idle timeout. `pod_lost`: the pod was terminating when claimed. `expired`: the allocation TTL ran out. `failed`: the pod exited. `start_timeout`: the pod was not up within startTimeout. `ca_replaced`: a hard CA replacement made it unreachable",
);

const WATCH_KINDS: &[&str] = &[
    "pool",
    "pod",
    "claim",
    "ca_secret",
    "serving_secret",
    "lease",
    "controller",
];
const CERT_LOAD: &[&str] = &["ca", "edge_serving"];
const CERT_KINDS: &[&str] = &["client_ca", "internal_ca", "edge_serving", "edge_client"];

/// The runtime's families.
pub const TABLE: [Def; 8] = [
    Def::gauge(
        "leader",
        Role::Controller,
        &[],
        "1 while this replica holds the leader Lease",
    ),
    Def::counter(
        "leader_transitions",
        Role::Controller,
        &[],
        "Times this replica gained or lost the Lease",
    ),
    Def::counter(
        "watch_errors",
        Role::Both,
        &[Label::closed("kind", WATCH_KINDS)],
        "Errors from a Kubernetes watch, by what it watches. The watch retries by itself",
    ),
    Def::gauge(
        "watch_namespace_unlisted",
        Role::Both,
        &[Label::open("kind"), Label::open("namespace")],
        "1 while a pool or pod watch carries on without a namespace it cannot list",
    ),
    Def::gauge(
        "ca_valid",
        Role::Controller,
        &[],
        "1 when the CA Secret loads and is usable. A CA whose notBefore is in the future still counts",
    ),
    Def::counter(
        "cert_load_errors",
        Role::Controller,
        &[Label::closed("cert", CERT_LOAD)],
        "Failures to load the CA bundle or the edge serving certificate",
    ),
    Def::gauge(
        "cert_not_after_seconds",
        Role::Controller,
        &[Label::closed("kind", CERT_KINDS)],
        "Expiry (Unix seconds) of the client CA, the internal CA, and this replica's edge serving and edge client certificates. Worker certificates are per pod and have no series",
    ),
    RELEASES,
];

/// Receives the pools the process sees, to create and drop their series.
pub trait PoolSeries: Send + Sync {
    /// A pool exists: create its starting series.
    fn seed(&self, namespace: &str, pool: &str);
    /// The pool is gone: drop its series.
    fn forget(&self, namespace: &str, pool: &str);
}

#[derive(Default)]
struct Pools {
    seen: HashSet<(String, String)>,
    /// Pools that were seen and are gone. A late write recreates their series, so
    /// every scrape forgets them again. Bounded by the pools ever deleted.
    gone: HashSet<(String, String)>,
    hooks: Vec<Arc<dyn PoolSeries>>,
}

/// Shared registry plus the runtime's own families.
pub struct Metrics {
    registry: Mutex<Registry>,
    leader: Gauge,
    leader_transitions: Counter,
    watch_errors: Family<Labels, Counter>,
    namespace_unlisted: Family<Labels, Gauge>,
    ca_valid: Gauge,
    cert_load_errors: Family<Labels, Counter>,
    cert_not_after: Family<Labels, Gauge>,
    releases: Family<PoolLabels, Counter>,
    held: crate::alloc::metrics::Hist,
    pools: Mutex<Pools>,
    pool_source: OnceLock<Store<BuildKitPool>>,
}

impl std::fmt::Debug for Metrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Metrics").finish_non_exhaustive()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// A controller registry with prefix `buildkit`: the runtime families
    /// registered, their starting series created.
    #[must_use]
    pub fn new() -> Self {
        Self::for_role(Role::Controller)
    }

    /// The registry of the cache registry pod: only the runtime families that
    /// role writes.
    #[must_use]
    pub fn for_registry() -> Self {
        Self::for_role(Role::Registry)
    }

    fn for_role(role: Role) -> Self {
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
            held: Family::new_with_constructor(crate::alloc::metrics::held_hist),
            pools: Mutex::default(),
            pool_source: OnceLock::new(),
        };
        register_table(
            &mut registry,
            role,
            &TABLE,
            vec![
                ("leader", Handle::new(m.leader.clone())),
                (
                    "leader_transitions",
                    Handle::new(m.leader_transitions.clone()),
                ),
                ("watch_errors", Handle::new(m.watch_errors.clone())),
                (
                    "watch_namespace_unlisted",
                    Handle::new(m.namespace_unlisted.clone()),
                ),
                ("ca_valid", Handle::new(m.ca_valid.clone())),
                ("cert_load_errors", Handle::new(m.cert_load_errors.clone())),
                (
                    "cert_not_after_seconds",
                    Handle::new(m.cert_not_after.clone()),
                ),
                ("releases", Handle::new(m.releases.clone())),
            ],
        );
        if role == Role::Controller {
            TABLE[2].seed(None, &m.watch_errors);
            TABLE[5].seed(None, &m.cert_load_errors);
        }
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

    /// Have `series` told about every pool seen, now and later.
    pub fn track_pools(&self, series: Arc<dyn PoolSeries>) {
        let mut p = self.lock_pools();
        for (ns, pool) in &p.seen {
            series.seed(ns, pool);
        }
        p.hooks.push(series);
    }

    /// A pool exists. The first call per pool creates its starting series.
    pub fn pool_seen(&self, namespace: &str, pool: &str) {
        let mut p = self.lock_pools();
        p.gone.remove(&(namespace.to_owned(), pool.to_owned()));
        if p.seen.insert((namespace.to_owned(), pool.to_owned())) {
            for h in &p.hooks {
                h.seed(namespace, pool);
            }
        }
    }

    /// A pool is gone: drop its series.
    pub fn pool_gone(&self, namespace: &str, pool: &str) {
        let mut p = self.lock_pools();
        if p.seen.remove(&(namespace.to_owned(), pool.to_owned())) {
            p.gone.insert((namespace.to_owned(), pool.to_owned()));
            for h in &p.hooks {
                h.forget(namespace, pool);
            }
        }
    }

    /// Follow `pools`: every scrape starts the series of new pools and drops
    /// those of deleted ones.
    pub fn follow_pools(&self, pools: Store<BuildKitPool>) {
        let _ = self.pool_source.set(pools);
    }

    fn sync_pools(&self) {
        let Some(store) = self.pool_source.get() else {
            return;
        };
        let now: HashSet<(String, String)> = store
            .state()
            .iter()
            .filter_map(|p| Some((p.namespace()?, p.name_any())))
            .collect();
        let gone: Vec<(String, String)> =
            self.lock_pools().seen.difference(&now).cloned().collect();
        for (ns, pool) in gone {
            self.pool_gone(&ns, &pool);
        }
        for (ns, pool) in &now {
            self.pool_seen(ns, pool);
        }
        let p = self.lock_pools();
        for (ns, pool) in p.gone.difference(&now) {
            for h in &p.hooks {
                h.forget(ns, pool);
            }
        }
    }

    fn lock_pools(&self) -> std::sync::MutexGuard<'_, Pools> {
        self.pools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Count a watcher `Err` item; `kind` is `pool`, `pod`, `ca_secret`, `serving_secret`, `lease` or `controller`.
    pub fn watch_error(&self, kind: &'static str) {
        self.watch_errors
            .get_or_create(&vec![("kind", kind.to_owned())])
            .inc();
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
            .get_or_create(&vec![("cert", cert.to_owned())])
            .inc();
    }

    /// Set `buildkit_cert_not_after_seconds{kind}`.
    pub fn set_cert_not_after(&self, kind: crate::pki::CertKind, not_after: time::OffsetDateTime) {
        self.cert_not_after
            .get_or_create(&vec![("kind", kind.label().to_owned())])
            .set(not_after.unix_timestamp());
    }

    /// `buildkit_allocation_held_seconds{namespace,pool,reason}`, shared by alloc and reconcile.
    #[must_use]
    pub fn held(&self) -> crate::alloc::metrics::Hist {
        self.held.clone()
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
        self.sync_pools();
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
