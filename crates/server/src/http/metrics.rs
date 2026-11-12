//! HTTP API metric families.

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::Histogram;

use crate::auth::JwksFetchResult;
use crate::metrics_doc::{Def, Handle, Label, Labels, Role, Values, register_table};
use crate::runtime::metrics::Metrics;

type HistogramFamily = Family<Labels, Histogram, fn() -> Histogram>;

/// Bucket set, in seconds.
pub const BUCKETS: [f64; 16] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0,
];

/// Route templates of `buildkit_http_requests_total{route}`.
const ROUTES: &[&str] = &[
    "/v1/allocations",
    "/v1/allocations/{id}",
    "/v1/pools",
    "/v1/tunnel",
    "other",
];
/// Statuses that start at 0; others appear on use.
const STATUSES: &[&str] = &[
    "200", "201", "202", "400", "401", "403", "404", "409", "429", "503",
];
const AUTH_METHODS: &[&str] = &["oidc", "serviceaccount"];
const AUTH_RESULTS: &[&str] = &["ok", "invalid", "unknown_issuer", "error"];
const JWKS_RESULTS: &[&str] = &["ok", "error"];

/// The HTTP API's families.
pub const TABLE: [Def; 4] = [
    Def::counter(
        "http_requests",
        Role::Controller,
        &[
            Label::closed("route", ROUTES),
            Label {
                name: "status",
                values: Values::Seeded(STATUSES),
            },
        ],
        "API requests by matched route template and HTTP status. `other` is any unmatched path",
    ),
    Def::histogram(
        "http_request_duration_seconds",
        Role::Controller,
        &[Label::closed("route", ROUTES)],
        &BUCKETS,
        "API request duration until the response head. A tunnel request ends when the tunnel closes",
    ),
    Def::counter(
        "auth",
        Role::Controller,
        &[
            Label::closed("method", AUTH_METHODS),
            Label::closed("result", AUTH_RESULTS),
        ],
        "Bearer token checks. `invalid`: bad or expired token. `unknown_issuer`: no configured issuer matches. `error`: the keys or the TokenReview call were unavailable",
    ),
    Def::counter(
        "jwks_fetch",
        Role::Controller,
        &[Label::open("issuer"), Label::closed("result", JWKS_RESULTS)],
        "Fetches of an OIDC issuer's signing keys, by configured issuer URL",
    ),
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
    /// Fresh families registered in `metrics`, their starting series created.
    #[must_use]
    pub fn register(metrics: &Metrics) -> Self {
        let m = Self::default();
        metrics.with_registry(|r| {
            register_table(
                r,
                Role::Controller,
                &TABLE,
                vec![
                    ("http_requests", Handle::new(m.requests.clone())),
                    (
                        "http_request_duration_seconds",
                        Handle::new(m.duration.clone()),
                    ),
                    ("auth", Handle::new(m.auth.clone())),
                    ("jwks_fetch", Handle::new(m.jwks.clone())),
                ],
            );
        });
        TABLE[0].seed(None, &m.requests);
        TABLE[1].seed(None, &m.duration);
        TABLE[2].seed(None, &m.auth);
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
