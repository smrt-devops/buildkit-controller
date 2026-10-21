//! HTTP API metric families.

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::Histogram;

use crate::auth::JwksFetchResult;
use crate::runtime::metrics::Metrics;

type Labels = Vec<(&'static str, String)>;
type HistogramFamily = Family<Labels, Histogram, fn() -> Histogram>;

/// bucket set, in seconds.
const BUCKETS: [f64; 16] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0,
];

fn histogram() -> Histogram {
    Histogram::new(BUCKETS)
}

/// `buildkit_http_*`, `buildkit_auth_total` and `buildkit_jwks_fetch_total`.
#[derive(Clone, Debug)]
pub struct HttpMetrics {
    requests: Family<Labels, Counter>,
    duration: HistogramFamily,
    auth: Family<Labels, Counter>,
    jwks: Family<Labels, Counter>,
}

impl Default for HttpMetrics {
    fn default() -> Self {
        Self {
            requests: Family::default(),
            duration: Family::new_with_constructor(histogram),
            auth: Family::default(),
            jwks: Family::default(),
        }
    }
}

impl HttpMetrics {
    /// Fresh families registered in `metrics`.
    #[must_use]
    pub fn register(metrics: &Metrics) -> Self {
        let m = Self::default();
        metrics.with_registry(|r| {
            r.register(
                "http_requests",
                "HTTP API requests by route template and status",
                m.requests.clone(),
            );
            r.register(
                "http_request_duration_seconds",
                "HTTP API request duration until the response head",
                m.duration.clone(),
            );
            r.register("auth", "bearer token authentications", m.auth.clone());
            r.register("jwks_fetch", "OIDC JWKS fetch attempts", m.jwks.clone());
        });
        m
    }

    /// One finished request on `route` (a template, or `other`).
    pub fn request(&self, route: &'static str, status: u16, secs: f64) {
        self.requests
            .get_or_create(&vec![
                ("route", route.to_owned()),
                ("status", status.to_string()),
            ])
            .inc();
        self.duration
            .get_or_create(&vec![("route", route.to_owned())])
            .observe(secs);
    }

    /// One authentication; `method` = `oidc` or `serviceaccount`.
    pub fn auth(&self, method: &'static str, result: &'static str) {
        self.auth
            .get_or_create(&vec![
                ("method", method.to_owned()),
                ("result", result.to_owned()),
            ])
            .inc();
    }

    /// One JWKS fetch attempt for the configured `issuer` url.
    pub fn jwks(&self, issuer: &str, result: JwksFetchResult) {
        self.jwks
            .get_or_create(&vec![
                ("issuer", issuer.to_owned()),
                ("result", result.label().to_owned()),
            ])
            .inc();
    }
}
