//! API caller authentication and pool access decisions.
//!
//! Bearer tokens are never logged, stored or echoed in errors (README invariant 12).

mod access;
mod crypto;
mod oidc;
mod token_review;

#[cfg(test)]
pub(crate) mod tests;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};

pub use access::{AccessRule, AccessRules, RuleError};
pub use oidc::{OidcIssuer, REFETCH_INTERVAL, https_client, public_roots};
pub use token_review::{
    DEFAULT_MAX_CONCURRENT_REVIEWS, DEFAULT_MAX_REVIEWS_PER_IP, DEFAULT_RECENT_SUBJECT_TTL,
    DEFAULT_RESERVED_REVIEWS, KubeTokenReviewer, REVIEW_CACHE_TTL, ReviewError, TokenReviewer,
};

/// `Retry-After` seconds of a 503 from a busy, throttled or timed-out `TokenReview`.
pub const REVIEW_RETRY_AFTER_SECS: u64 = 2;

/// Largest accepted bearer token.
pub const MAX_TOKEN_BYTES: usize = 16 * 1024;

/// An authenticated API caller.
#[derive(Debug, Clone, PartialEq)]
pub enum Identity {
    Oidc {
        /// The configured issuer `url` (equal to the token's `iss`).
        issuer: String,
        subject: String,
        /// Every verified top-level claim.
        claims: Map<String, Value>,
    },
    ServiceAccount {
        namespace: String,
        name: String,
        uid: String,
        /// The pod a bound token was issued for, from the `TokenReview`.
        /// Pods that share a service account are different owners.
        pod_uid: Option<String>,
    },
    /// A `BuildKitClaim`, served by the leader. All
    /// three fields come from the API server, never from the claim spec.
    Claim {
        namespace: String,
        name: String,
        uid: String,
    },
}

impl Identity {
    /// Owner string: `oidc:<iss>#<sub>` (with the run claims appended when the
    /// token has them, see [`RUN_CLAIMS`]), `sa:<ns>/<name>` (with `#pod=<uid>`
    /// for a token bound to a pod) or
    /// `claim:<ns>/<name>#<uid>`. Only the same caller may release an allocation.
    #[must_use]
    pub fn owner(&self) -> String {
        match self {
            Self::Oidc {
                issuer,
                subject,
                claims,
            } => oidc_owner(issuer, subject, claims),
            Self::ServiceAccount {
                namespace,
                name,
                pod_uid,
                ..
            } => match pod_uid {
                Some(pod) => format!("sa:{namespace}/{name}#pod={pod}"),
                None => format!("sa:{namespace}/{name}"),
            },
            Self::Claim {
                namespace,
                name,
                uid,
            } => format!("claim:{namespace}/{name}#{uid}"),
        }
    }
}

/// Claims that tie a CI token to one run of one workflow (GitHub Actions sets
/// all three). The `sub` of a GitHub token is shared by every run of a
/// repository and ref, and by every `pull_request` run, so an owner built from
/// `iss#sub` alone would let one run release another's allocation.
pub const RUN_CLAIMS: [&str; 3] = ["run_id", "run_attempt", "job_workflow_ref"];

/// The OIDC owner string: `oidc:<iss>#<sub>`, then each present [`RUN_CLAIMS`]
/// claim as `#<name>=<value>` in the order of [`RUN_CLAIMS`]. The issuer, `sub`
/// and every value have `%` and `#` percent-encoded in both forms, so no two
/// callers produce the same string: without the escape in the plain form, sub
/// `x#run_id=1` with no run claims would equal sub `x` with `run_id` 1. Real
/// issuers contain neither character, so their owners read unchanged.
fn oidc_owner(issuer: &str, subject: &str, claims: &Map<String, Value>) -> String {
    let run: Vec<(&str, String)> = RUN_CLAIMS
        .iter()
        .filter_map(|&name| {
            let v = match claims.get(name)? {
                Value::String(s) if !s.is_empty() => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => return None,
            };
            Some((name, v))
        })
        .collect();
    let esc = |s: &str| s.replace('%', "%25").replace('#', "%23");
    let mut owner = format!("oidc:{}#{}", esc(issuer), esc(subject));
    for (name, v) in run {
        owner.push('#');
        owner.push_str(name);
        owner.push('=');
        owner.push_str(&esc(&v));
    }
    owner
}

/// Authentication failure, mapped to the API error table.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Missing, malformed, unverifiable or rejected token.
    #[error("invalid token: {0}")]
    Invalid(&'static str),
    /// `iss` is not a configured issuer and service-account auth is off.
    #[error("unknown issuer")]
    UnknownIssuer,
    /// No usable JWKS for the issuer. It fails closed with a retryable 503:
    /// the token may be fine and the issuer unreachable.
    #[error("issuer keys unavailable")]
    KeysUnavailable,
    /// `TokenReview` timed out or failed with unknown outcome.
    #[error("upstream: {0}")]
    Upstream(String),
    /// Unexpected local or API failure.
    #[error("internal: {0}")]
    Internal(String),
}

impl AuthError {
    /// HTTP status (401 also carries `WWW-Authenticate: Bearer`).
    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            Self::Invalid(_) | Self::UnknownIssuer => 401,
            Self::KeysUnavailable | Self::Upstream(_) => 503,
            Self::Internal(_) => 500,
        }
    }

    /// The `error.code` of the response body.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self.status() {
            401 => "unauthenticated",
            503 => "upstream_error",
            _ => "internal",
        }
    }

    /// Why the token was refused, for the log: the fixed reason of
    /// [`Self::Invalid`] ("expired", "audience mismatch", "unknown kid", ...),
    /// otherwise [`Self::metric_result`]. It never holds token or claim data.
    #[must_use]
    pub fn detail(&self) -> &'static str {
        match self {
            Self::Invalid(reason) => reason,
            _ => self.metric_result(),
        }
    }

    /// `Retry-After` seconds of a 503: a `TokenReview` slot or an API server
    /// answer can come back within seconds, but after a failed JWKS fetch the
    /// issuer is not asked again for [`REFETCH_INTERVAL`], so an earlier retry
    /// gets the same answer.
    #[must_use]
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            Self::Upstream(_) => Some(REVIEW_RETRY_AFTER_SECS),
            Self::KeysUnavailable => Some(REFETCH_INTERVAL.as_secs()),
            Self::Invalid(_) | Self::UnknownIssuer | Self::Internal(_) => None,
        }
    }

    /// The `result` label of `buildkit_auth_total`.
    #[must_use]
    pub fn metric_result(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid",
            Self::UnknownIssuer => "unknown_issuer",
            Self::KeysUnavailable | Self::Upstream(_) | Self::Internal(_) => "error",
        }
    }
}

/// Extracts the token from an `Authorization` header value.
pub fn bearer_token(header: Option<&str>) -> Result<&str, AuthError> {
    let value = header.ok_or(AuthError::Invalid("missing authorization"))?;
    let (scheme, token) = value
        .split_once(' ')
        .ok_or(AuthError::Invalid("not a bearer token"))?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() {
        return Err(AuthError::Invalid("not a bearer token"));
    }
    check_size(token)?;
    Ok(token)
}

fn check_size(token: &str) -> Result<(), AuthError> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(AuthError::Invalid("token too large"));
    }
    Ok(())
}

/// The `auth` section of the config file.
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    pub oidc: Vec<OidcIssuer>,
    /// `Some(audiences)` when service-account auth is enabled.
    pub service_account_audiences: Option<Vec<String>>,
}

/// Outcome of one discovery + JWKS fetch (`buildkit_jwks_fetch_total`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JwksFetchResult {
    Ok,
    Error,
}

impl JwksFetchResult {
    /// The `result` label value.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
        }
    }
}

/// Called with the issuer `url` after every JWKS fetch attempt.
pub type JwksObserver = Arc<dyn Fn(&str, JwksFetchResult) + Send + Sync>;

/// Turns a bearer token into an [`Identity`].
pub struct Authenticator<R> {
    issuers: HashMap<String, oidc::IssuerVerifier>,
    sa_audiences: Option<Vec<String>>,
    http: reqwest::Client,
    reviewer: R,
    /// `TokenReview` answers by token hash.
    reviews: token_review::ReviewCache,
    /// Bounds `TokenReview` calls in flight, with a reserve for recently seen subjects.
    review_slots: token_review::ReviewSlots,
    /// Bounds `TokenReview` calls in flight per client address.
    review_peers: crate::edge::PerPeer,
}

impl<R: TokenReviewer> Authenticator<R> {
    /// Builds an authenticator that fetches JWKS with `http` (see [`https_client`]).
    pub fn new(cfg: AuthConfig, http: reqwest::Client, reviewer: R) -> Result<Self, AuthError> {
        if !crypto::install() {
            return Err(AuthError::Internal(
                "foreign jsonwebtoken crypto provider".into(),
            ));
        }
        let mut issuers = HashMap::new();
        for issuer in cfg.oidc {
            if !issuer.url.starts_with("https://") || issuer.audiences.is_empty() {
                return Err(AuthError::Internal(format!("bad issuer {}", issuer.url)));
            }
            if issuer
                .jwks_url
                .as_deref()
                .is_some_and(|u| !u.starts_with("https://"))
            {
                return Err(AuthError::Internal(format!(
                    "bad jwksUrl for {}",
                    issuer.url
                )));
            }
            let url = issuer.url.clone();
            let mut verifier = oidc::IssuerVerifier::new(issuer.clone());
            if !issuer.ca_certs.is_empty() {
                let mut roots = public_roots();
                for c in issuer.ca_certs {
                    roots.add(c).map_err(|e| {
                        AuthError::Internal(format!("bad CA certificate for {url}: {e}"))
                    })?;
                }
                verifier.set_http(https_client(roots)?);
            }
            if issuers.insert(url.clone(), verifier).is_some() {
                return Err(AuthError::Internal(format!("duplicate issuer {url}")));
            }
        }
        if cfg
            .service_account_audiences
            .as_ref()
            .is_some_and(Vec::is_empty)
        {
            return Err(AuthError::Internal(
                "empty service account audiences".into(),
            ));
        }
        Ok(Self {
            issuers,
            sa_audiences: cfg.service_account_audiences,
            http,
            reviewer,
            reviews: token_review::ReviewCache::default(),
            review_slots: token_review::ReviewSlots::new(
                DEFAULT_MAX_CONCURRENT_REVIEWS,
                DEFAULT_RESERVED_REVIEWS,
                DEFAULT_RECENT_SUBJECT_TTL,
            ),
            review_peers: crate::edge::PerPeer::new(DEFAULT_MAX_REVIEWS_PER_IP),
        })
    }

    /// Allows at most `n` (at least 1) `TokenReview` calls in flight; more callers get 503.
    #[must_use]
    pub fn with_max_concurrent_reviews(mut self, n: usize) -> Self {
        self.review_slots = self.review_slots.with_max(n);
        self
    }

    /// Holds `n` of the review slots back for service accounts that passed a review
    /// within `recent` (fewer than the total; a larger `n` is cut to fit). A token
    /// whose unverified `sub` names such an account may use them once the shared
    /// slots are taken, a quarter of them at a time per account. Forging a known
    /// `sub` wins no more than that share. The review decides who the caller is.
    #[must_use]
    pub fn with_reserved_reviews(mut self, n: usize, recent: Duration) -> Self {
        self.review_slots = self.review_slots.with_reserved(n).with_ttl(recent);
        self
    }

    /// Allows at most `n` `TokenReview` calls in flight for one client address
    /// (an IPv6 address counts as its /56; 0: no cap), so one source cannot hold every review slot.
    #[must_use]
    pub fn with_max_reviews_per_ip(mut self, n: usize) -> Self {
        self.review_peers = crate::edge::PerPeer::new(n);
        self
    }

    /// Reports every JWKS fetch attempt to `observer` (for `buildkit_jwks_fetch_total`).
    #[must_use]
    pub fn with_jwks_observer(
        mut self,
        observer: impl Fn(&str, JwksFetchResult) + Send + Sync + 'static,
    ) -> Self {
        let observer: JwksObserver = Arc::new(observer);
        for verifier in self.issuers.values_mut() {
            verifier.set_observer(Arc::clone(&observer));
        }
        self
    }

    /// Authenticates a bearer token with no client address.
    pub async fn authenticate(&self, token: &str) -> Result<Identity, AuthError> {
        self.authenticate_from(token, None).await
    }

    /// Authenticates a bearer token from `peer`, which the per-address review cap counts.
    pub async fn authenticate_from(
        &self,
        token: &str,
        peer: Option<std::net::IpAddr>,
    ) -> Result<Identity, AuthError> {
        check_size(token)?;
        // Untrusted peek at `iss`, used only to choose the verification path.
        let iss = jsonwebtoken::dangerous::insecure_decode_claims::<Map<String, Value>>(token)
            .ok()
            .and_then(|c| c.get("iss").and_then(Value::as_str).map(str::to_owned));
        if let Some(verifier) = iss.as_deref().and_then(|i| self.issuers.get(i)) {
            return verifier.verify(&self.http, token).await;
        }
        let Some(audiences) = &self.sa_audiences else {
            return Err(AuthError::UnknownIssuer);
        };
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let Some((exp, subject)) = token_review::sa_token_claims(token, audiences, now_secs) else {
            return Err(AuthError::Invalid("not a service account token"));
        };
        let key = token_review::token_key(token);
        let now = tokio::time::Instant::now();
        if let Some(hit) = self.reviews.get(&key, now) {
            return hit;
        }
        let peer_slot = peer.map(|ip| self.review_peers.try_take(token_review::review_peer(ip)));
        // An address over its cap may still use the reserve for a subject seen
        // recently: behind a gateway every client shares one address, and the
        // reserve exists for exactly those callers. Its per-subject share bounds them.
        let (_peer_slot, _slot) = match peer_slot {
            Some(None) => match self.review_slots.acquire_reserved(&subject, now) {
                Some(s) => (None, s),
                None => {
                    return Err(AuthError::Upstream(
                        "tokenreview busy for this address".into(),
                    ));
                }
            },
            p => match self.review_slots.acquire(&subject, now) {
                Some(s) => (p.flatten(), s),
                None => return Err(AuthError::Upstream("tokenreview saturated".into())),
            },
        };
        let status = self
            .reviewer
            .review(token, audiences)
            .await
            .map_err(|e| match e {
                ReviewError::Upstream(m) => AuthError::Upstream(m),
                ReviewError::Unexpected(m) => AuthError::Internal(m),
            })?;
        let r = token_review::identity_from_status(&status, audiences);
        if let Ok(Identity::ServiceAccount {
            namespace, name, ..
        }) = &r
        {
            self.review_slots
                .note_authenticated(&format!("{namespace}/{name}"), now);
        }
        // A positive answer never outlives the token's own `exp`.
        let left = Duration::from_secs(exp.saturating_sub(now_secs));
        let ttl = if r.is_ok() {
            REVIEW_CACHE_TTL.min(left)
        } else {
            REVIEW_CACHE_TTL
        };
        self.reviews.put(key, &r, now + ttl);
        r
    }
}

/// The controller's own projected service-account token. Its default audience
/// is the API server's.
pub const OWN_TOKEN_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";

/// The `aud` values of `own_token` (read without verification) that also appear
/// in `sa_audiences`. Any overlap makes every pod's default token, and legacy
/// Secret tokens, valid API callers, so startup refuses it.
#[must_use]
pub fn api_audience_overlap(own_token: &str, sa_audiences: &[String]) -> Vec<String> {
    let claims =
        jsonwebtoken::dangerous::insecure_decode_claims::<Map<String, Value>>(own_token.trim())
            .unwrap_or_default();
    let own: Vec<&str> = match claims.get("aud") {
        Some(Value::Array(list)) => list.iter().filter_map(Value::as_str).collect(),
        Some(Value::String(one)) => vec![one.as_str()],
        _ => Vec::new(),
    };
    sa_audiences
        .iter()
        .filter(|a| own.contains(&a.as_str()))
        .cloned()
        .collect()
}
