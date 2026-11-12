//! Metric family declarations.
//!
//! Every metrics module declares its families in one const table of [`Def`]s
//! (name, type, help, label names, and for each label its value set). The
//! module's `register()` walks that table, so a family exists in code only if
//! it is declared. The same tables render `docs/metrics.md` and let the
//! dashboard checks read the real names and labels.
//!
//! Counters and histograms whose labels are all closed sets start at 0: the
//! series without a pool at start-up, the per-pool series when a pool is first
//! seen ([`crate::runtime::metrics::Metrics::pool_seen`]).

use std::collections::HashMap;
use std::fmt;

use prometheus_client::encoding::{EncodeMetric, MetricEncoder};
use prometheus_client::metrics::MetricType;
use prometheus_client::metrics::family::{Family, MetricConstructor};
use prometheus_client::registry::{Metric, Registry};

/// Label pairs of one series, in declaration order.
pub type Labels = Vec<(&'static str, String)>;

/// The Prometheus type of a family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Only goes up.
    Counter,
    /// Goes up and down.
    Gauge,
    /// Observations in buckets.
    Histogram,
}

/// Which process role exports a family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The controller replicas.
    Controller,
    /// The cache registry pod.
    Registry,
    /// Both.
    Both,
}

impl Role {
    /// Whether a process of role `of` exports the family.
    #[must_use]
    pub fn includes(self, of: Role) -> bool {
        self == Role::Both || self == of
    }
}

/// The values a label can take.
#[derive(Debug, Clone, Copy)]
pub enum Values {
    /// Exactly these values; every one starts at 0.
    Closed(&'static [&'static str]),
    /// The pool's namespace or name (by the label's name); the series appear
    /// when the pool is first seen and go with it.
    Pool,
    /// Any value; these start at 0 and others appear on use.
    Seeded(&'static [&'static str]),
    /// Any value; series appear on use.
    Open,
}

/// One label of a family.
#[derive(Debug, Clone, Copy)]
pub struct Label {
    /// Label name.
    pub name: &'static str,
    /// Value set.
    pub values: Values,
}

impl Label {
    /// A label with a closed value set.
    #[must_use]
    pub const fn closed(name: &'static str, values: &'static [&'static str]) -> Self {
        Self {
            name,
            values: Values::Closed(values),
        }
    }

    /// The `namespace` label of a pool.
    #[must_use]
    pub const fn namespace() -> Self {
        Self {
            name: "namespace",
            values: Values::Pool,
        }
    }

    /// The `pool` label.
    #[must_use]
    pub const fn pool() -> Self {
        Self {
            name: "pool",
            values: Values::Pool,
        }
    }

    /// A label with an open value set.
    #[must_use]
    pub const fn open(name: &'static str) -> Self {
        Self {
            name,
            values: Values::Open,
        }
    }
}

/// `namespace` and `pool`, the first two labels of every per-pool family.
pub const POOL: [Label; 2] = [Label::namespace(), Label::pool()];

/// One declared family.
#[derive(Debug, Clone, Copy)]
pub struct Def {
    /// Name without the `buildkit_` prefix and without `_total`.
    pub name: &'static str,
    /// Type.
    pub kind: Kind,
    /// Help text: what the series means.
    pub help: &'static str,
    /// Who exports it.
    pub role: Role,
    /// Label names and value sets, in the order the code writes them.
    pub labels: &'static [Label],
    /// Histogram upper bounds in seconds; empty for other types.
    pub buckets: &'static [f64],
}

impl Def {
    /// A counter.
    #[must_use]
    pub const fn counter(
        name: &'static str,
        role: Role,
        labels: &'static [Label],
        help: &'static str,
    ) -> Self {
        Self {
            name,
            kind: Kind::Counter,
            help,
            role,
            labels,
            buckets: &[],
        }
    }

    /// A gauge.
    #[must_use]
    pub const fn gauge(
        name: &'static str,
        role: Role,
        labels: &'static [Label],
        help: &'static str,
    ) -> Self {
        Self {
            name,
            kind: Kind::Gauge,
            help,
            role,
            labels,
            buckets: &[],
        }
    }

    /// A histogram of seconds.
    #[must_use]
    pub const fn histogram(
        name: &'static str,
        role: Role,
        labels: &'static [Label],
        buckets: &'static [f64],
        help: &'static str,
    ) -> Self {
        Self {
            name,
            kind: Kind::Histogram,
            help,
            role,
            labels,
            buckets,
        }
    }

    /// The name as scraped, with prefix and `_total` for counters.
    #[must_use]
    pub fn exposed(&self) -> String {
        match self.kind {
            Kind::Counter => format!("buildkit_{}_total", self.name),
            Kind::Gauge | Kind::Histogram => format!("buildkit_{}", self.name),
        }
    }

    fn has_pool(&self) -> bool {
        self.labels.iter().any(|l| matches!(l.values, Values::Pool))
    }

    /// The label sets of the series that exist from the start. With `pool`
    /// `None`, the series of families without pool labels; with a pool, the
    /// series of that pool. Empty when a label has an open value set.
    #[must_use]
    pub fn series(&self, pool: Option<(&str, &str)>) -> Vec<Labels> {
        if pool.is_some() != self.has_pool() {
            return Vec::new();
        }
        let mut sets: Vec<Labels> = vec![Vec::new()];
        for label in self.labels {
            let values: Vec<String> = match (label.values, pool) {
                (Values::Closed(v) | Values::Seeded(v), _) => {
                    v.iter().map(|s| (*s).to_owned()).collect()
                }
                (Values::Pool, Some((ns, name))) => {
                    vec![if label.name == "namespace" { ns } else { name }.to_owned()]
                }
                (Values::Pool, None) | (Values::Open, _) => return Vec::new(),
            };
            sets = sets
                .into_iter()
                .flat_map(|set| {
                    values.iter().map(move |v| {
                        let mut next = set.clone();
                        next.push((label.name, v.clone()));
                        next
                    })
                })
                .collect();
        }
        sets
    }

    /// Creates every starting series of this family in `family`.
    pub fn seed<M, S>(&self, pool: Option<(&str, &str)>, family: &Family<Labels, M, S>)
    where
        S: MetricConstructor<M>,
    {
        for set in self.series(pool) {
            drop(family.get_or_create(&set));
        }
    }

    /// Removes the series of pool (`namespace`, `name`) from `family`.
    pub fn forget<M, S>(&self, namespace: &str, name: &str, family: &Family<Labels, M, S>)
    where
        S: MetricConstructor<M>,
    {
        for set in self.series(Some((namespace, name))) {
            family.remove(&set);
        }
    }
}

/// A metric of any type, to register it from a table.
#[derive(Debug)]
pub struct Handle(Box<dyn Metric>);

impl Handle {
    /// Wraps `metric`.
    #[must_use]
    pub fn new(metric: impl Metric) -> Self {
        Self(Box::new(metric))
    }
}

impl EncodeMetric for Handle {
    fn encode(&self, encoder: MetricEncoder) -> Result<(), fmt::Error> {
        self.0.encode(encoder)
    }

    fn metric_type(&self) -> MetricType {
        self.0.metric_type()
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Registers every family of `table` that a process of `role` exports, taking
/// its handle from `handles` by name. A declared family without a handle, or
/// a handle without a declaration, is a bug caught by the first test run.
pub fn register_table(
    r: &mut Registry,
    role: Role,
    table: &[Def],
    handles: Vec<(&'static str, Handle)>,
) {
    let mut by_name: HashMap<&str, Handle> = handles.into_iter().collect();
    for def in table {
        match by_name.remove(def.name) {
            Some(h) if def.role.includes(role) => r.register(def.name, def.help, h),
            Some(_) => {}
            None => debug_assert!(false, "no handle for family {}", def.name),
        }
    }
    debug_assert!(
        by_name.is_empty(),
        "undeclared families: {:?}",
        by_name.keys()
    );
}

/// Every declared family, both roles, in the order of the reference.
#[cfg(any(test, feature = "fixtures"))]
#[must_use]
pub fn families() -> Vec<Def> {
    use crate::{alloc, edge, http, net, reconcile, registry, runtime};
    [
        &runtime::metrics::TABLE[..],
        &reconcile::metrics::TABLE[..],
        &alloc::metrics::TABLE[..],
        &alloc::leader::CLAIM_TABLE[..],
        &http::metrics::TABLE[..],
        &edge::metrics::TABLE[..],
        &net::proxy_protocol::TABLE[..],
        &registry::metrics::TABLE[..],
    ]
    .concat()
}

/// `docs/metrics.md`, rendered from [`families`].
#[cfg(any(test, feature = "fixtures"))]
#[must_use]
pub fn render_markdown() -> String {
    use std::fmt::Write as _;

    let mut out = String::from(
        "<!-- Generated by `cargo xtask crdgen`; do not edit. -->\n\n# Metrics reference\n\n\
Both processes serve Prometheus text on `GET /metrics` of their ops port. The controller \
replicas export the first group, the cache registry pod the second. Names are as scraped: \
counters end in `_total`.\n\n\
The `namespace` and `pool` labels name a pool; they are empty when the pool is not yet known, \
for example after a failed TLS handshake. A label shown with a list of values takes only those \
values. Counters and histograms whose labels all have such lists, and the per-pool ones once the \
pool exists, start at 0 so that `rate()` and `increase()` see the first event. A label shown as \
`any` takes values that appear on use. No metric carries a per-allocation or per-pod label, so \
cardinality stays bounded by the number of pools.\n",
    );
    let all = families();
    for (title, role) in [
        ("Controller metrics", Role::Controller),
        ("Cache registry metrics", Role::Registry),
    ] {
        let _ = write!(
            out,
            "\n## {title}\n\n| Name | Type | Labels | Meaning |\n|---|---|---|---|\n"
        );
        for d in all.iter().filter(|d| d.role.includes(role)) {
            let kind = match d.kind {
                Kind::Counter => "counter",
                Kind::Gauge => "gauge",
                Kind::Histogram => "histogram",
            };
            let _ = writeln!(
                out,
                "| `{}` | {kind} | {} | {} |",
                d.exposed(),
                labels_cell(d),
                d.help
            );
        }
    }
    out.push_str(
        "\n## Histogram buckets\n\nUpper bounds in seconds.\n\n| Name | Buckets |\n|---|---|\n",
    );
    for d in all.iter().filter(|d| d.kind == Kind::Histogram) {
        let buckets: Vec<String> = d.buckets.iter().map(f64::to_string).collect();
        let _ = writeln!(out, "| `{}` | {} |", d.exposed(), buckets.join(", "));
    }
    out
}

#[cfg(any(test, feature = "fixtures"))]
fn labels_cell(d: &Def) -> String {
    if d.labels.is_empty() {
        return "none".to_owned();
    }
    d.labels
        .iter()
        .map(|l| match l.values {
            Values::Closed(v) => {
                let v: Vec<String> = v.iter().map(|s| format!("`{s}`")).collect();
                format!("`{}`: {}", l.name, v.join(", "))
            }
            Values::Pool => format!("`{}`", l.name),
            Values::Seeded(v) => {
                let v: Vec<String> = v.iter().map(|s| format!("`{s}`")).collect();
                format!("`{}`: any, starting with {}", l.name, v.join(", "))
            }
            Values::Open => format!("`{}`: any", l.name),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Kind, Role, families, render_markdown};
    use crate::alloc::metrics::AllocMetrics;
    use crate::edge::metrics::EdgeMetrics;
    use crate::http::metrics::HttpMetrics;
    use crate::net::proxy_protocol::ProxyMetrics;
    use crate::reconcile::metrics::ReconcileMetrics;
    use crate::registry::RegistryMetrics;
    use crate::runtime::metrics::Metrics;

    /// A controller registry with every module's families, proxy protocol on.
    fn controller() -> Metrics {
        let m = Metrics::new();
        AllocMetrics::with_releases(m.releases()).register(&m);
        let _ = ReconcileMetrics::register(&m);
        let _ = EdgeMetrics::register(&m);
        let _ = HttpMetrics::register(&m);
        let _ = ProxyMetrics::register(&m, "api");
        let _ = ProxyMetrics::register(&m, "edge");
        crate::alloc::leader::register_claims_for_test(&m);
        m
    }

    fn repo_file(rel: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(rel);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    fn samples_of(text: &str, pool: &str) -> usize {
        let needle = format!("pool=\"{pool}\"");
        text.lines()
            .filter(|l| !l.starts_with('#') && l.contains(&needle))
            .count()
    }

    #[test]
    fn a_pool_starts_at_zero_before_any_allocation_and_goes_with_the_pool() {
        let m = controller();
        let before = m.render();
        assert!(!before.contains("pool=\"p\""), "{before}");
        m.pool_seen("ns", "p");
        m.pool_seen("ns", "p");
        let text = m.render();
        for line in [
            "buildkit_allocations_total{namespace=\"ns\",pool=\"p\",result=\"ok\"} 0",
            "buildkit_edge_connections_total{namespace=\"ns\",pool=\"p\",result=\"ok\",transport=\"tunnel\"} 0",
            "buildkit_releases_total{namespace=\"ns\",pool=\"p\",reason=\"released\"} 0",
        ] {
            assert!(text.contains(line), "{line} missing:\n{text}");
        }
        let series: usize = families()
            .iter()
            .map(|d| d.series(Some(("ns", "p"))).len())
            .sum();
        println!(
            "per-pool series: {series}, sample lines: {}",
            samples_of(&text, "p")
        );
        assert!(series > 0);
        m.pool_gone("ns", "p");
        let after = m.render();
        assert!(!after.contains("pool=\"p\""), "{after}");
        let sorted = |t: &str| {
            let mut v: Vec<String> = t.lines().map(str::to_owned).collect();
            v.sort();
            v
        };
        assert_eq!(
            sorted(&after),
            sorted(&before),
            "nothing of the pool is left"
        );
    }

    #[test]
    fn closed_counters_and_histograms_without_a_pool_start_at_zero() {
        let text = controller().render();
        for d in families()
            .iter()
            .filter(|d| d.role.includes(Role::Controller))
        {
            if d.kind == Kind::Gauge || d.series(None).is_empty() {
                continue;
            }
            let ty = match d.kind {
                Kind::Counter => format!("# TYPE buildkit_{} counter", d.name),
                _ => format!("# TYPE buildkit_{} histogram", d.name),
            };
            assert!(text.contains(&ty), "{ty} missing:\n{text}");
        }
        assert!(
            text.contains(
                "buildkit_http_requests_total{route=\"/v1/allocations\",status=\"429\"} 0"
            )
        );
        assert!(text.contains("buildkit_reconcile_total{result=\"error\"} 0"));
    }

    #[test]
    fn the_registry_role_exports_only_what_it_writes() {
        let m = Metrics::for_registry();
        let reg = RegistryMetrics::register(&m);
        reg.request("blob", "GET", 200, 0.01);
        let text = m.render();
        for gone in [
            "buildkit_leader",
            "buildkit_ca_valid",
            "buildkit_cert_",
            "buildkit_watch_",
            "buildkit_releases",
            "buildkit_pool_",
        ] {
            assert!(!text.contains(gone), "{gone} exported:\n{text}");
        }
        assert!(text.contains("buildkit_registry_requests_total{"), "{text}");
        assert!(
            text.contains("buildkit_registry_denied_total{action=\"push\"} 0"),
            "{text}"
        );
        assert!(text.contains("buildkit_registry_stored_bytes 0"), "{text}");
    }

    #[test]
    fn family_names_are_unique_and_help_is_plain() {
        let all = families();
        let mut names: Vec<String> = all.iter().map(super::Def::exposed).collect();
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate family name");
        for d in &all {
            assert!(
                !d.help.is_empty() && !d.help.contains('|') && !d.help.contains('—'),
                "{}",
                d.name
            );
            if d.kind == Kind::Histogram {
                assert!(d.buckets.windows(2).all(|w| w[0] < w[1]), "{}", d.name);
            }
        }
    }

    #[test]
    fn exponential_bucket_tables_match_their_formula() {
        use prometheus_client::metrics::histogram::exponential_buckets;
        let eq = |table: &[f64], start: f64, n: u16| {
            let want: Vec<f64> = exponential_buckets(start, 2.0, n).collect();
            assert_eq!(table, want.as_slice());
        };
        eq(&crate::alloc::metrics::BUCKETS, 0.05, 14);
        eq(&crate::edge::metrics::BUCKETS, 0.001, 13);
    }

    #[test]
    fn docs_metrics_md_is_current() {
        assert_eq!(
            repo_file("docs/metrics.md"),
            render_markdown(),
            "docs/metrics.md is stale; run `cargo xtask crdgen`"
        );
    }

    /// Every `buildkit_*` name in the operations guide is a declared family
    /// (or a `_bucket`, `_sum` or `_count` series of a histogram).
    #[test]
    fn operations_md_names_only_declared_families() {
        let known: Vec<(String, Kind)> = families().iter().map(|d| (d.exposed(), d.kind)).collect();
        let text = repo_file("docs/operations.md");
        let mut found = 0;
        for token in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            if !token.starts_with("buildkit_") || token.ends_with('_') {
                continue;
            }
            found += 1;
            let ok = known.iter().any(|(name, kind)| {
                token == name
                    || (*kind == Kind::Histogram
                        && ["_bucket", "_sum", "_count"]
                            .iter()
                            .any(|s| token == format!("{name}{s}")))
            });
            // The binary and chart names share the prefix; they are not metrics.
            let not_a_metric = ["buildkit_controller", "buildkit_registry"].contains(&token);
            assert!(
                ok || not_a_metric,
                "`{token}` in docs/operations.md is not a declared family"
            );
        }
        assert!(found > 10, "found only {found} names");
    }
}
