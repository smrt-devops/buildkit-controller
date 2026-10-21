//! OIDC bearer verification against a configured issuer.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::jwk::{
    AlgorithmParameters, EllipticCurve, Jwk, JwkSet, KeyAlgorithm, PublicKeyUse,
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde_json::{Map, Value};
use tokio::sync::watch;
use tokio::time::Instant;

use super::{AuthError, Identity, JwksFetchResult, JwksObserver, crypto};

/// Clock-skew allowance for `exp`, `nbf` and `iat`.
pub const LEEWAY_SECS: u64 = 60;
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const MIN_MAX_AGE: Duration = Duration::from_mins(5);
const MAX_MAX_AGE: Duration = Duration::from_hours(24);
const DEFAULT_MAX_AGE: Duration = Duration::from_hours(1);
/// Minimum gap between two JWKS fetches for one issuer.
pub const REFETCH_INTERVAL: Duration = Duration::from_secs(30);
/// Cached keys stay usable this long after they expire (`max-age` past the
/// last successful fetch), so a refresh that fails at expiry still has a grace period.
const STALE_LIMIT: Duration = Duration::from_hours(24);
const MAX_BODY: usize = 1 << 20;

/// One `auth.oidc[]` entry of the config file.
#[derive(Debug, Clone)]
pub struct OidcIssuer {
    /// Exact `iss` value; `https://` only.
    pub url: String,
    /// Accepted `aud` values; non-empty.
    pub audiences: Vec<String>,
    /// `None` = discover from `<url>/.well-known/openid-configuration`.
    pub jwks_url: Option<String>,
    /// Extra trusted CAs, added to the public roots for this issuer's fetches only.
    pub ca_certs: Vec<rustls_pki_types::CertificateDer<'static>>,
}

/// Builds the HTTPS-only client used for discovery and JWKS fetches.
///
/// Production passes the public roots ([`public_roots`]); tests pass a store
/// holding their own CA. Redirects are not followed.
pub fn https_client(roots: rustls::RootCertStore) -> Result<reqwest::Client, AuthError> {
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| AuthError::Internal(format!("tls config: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|e| AuthError::Internal(format!("http client: {e}")))
}

/// The Mozilla root set.
#[must_use]
pub fn public_roots() -> rustls::RootCertStore {
    rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    }
}

#[derive(Default)]
struct KeyCache {
    keys: Option<JwkSet>,
    fetched_at: Option<Instant>,
    max_age: Duration,
    last_kid_refetch: Option<Instant>,
    last_failure: Option<Instant>,
    /// Closes when the in-flight fetch has recorded its outcome.
    in_flight: Option<watch::Receiver<()>>,
}

impl KeyCache {
    /// Keys stay usable up to [`STALE_LIMIT`] past their `max-age`.
    fn usable(&self, now: Instant) -> bool {
        self.fetched_at
            .is_some_and(|t| now.duration_since(t) < self.max_age + STALE_LIMIT)
    }

    /// The first usable key with this `kid` whose use, kty, crv and alg agree
    /// with the token's `alg`.
    fn find(&self, kid: &str, alg: Algorithm, now: Instant) -> Option<Jwk> {
        if !self.usable(now) {
            return None;
        }
        self.keys
            .as_ref()?
            .keys
            .iter()
            .find(|k| k.common.key_id.as_deref() == Some(kid) && check_jwk(k, alg).is_ok())
            .cloned()
    }

    fn has_kid(&self, kid: &str) -> bool {
        self.keys.as_ref().is_some_and(|set| {
            set.keys
                .iter()
                .any(|k| k.common.key_id.as_deref() == Some(kid))
        })
    }

    /// The in-flight fetch, if one is still running. A closed channel means
    /// the fetch task has ended, even if it panicked.
    fn refreshing(&self) -> Option<watch::Receiver<()>> {
        self.in_flight
            .clone()
            .filter(|done| done.has_changed().is_ok())
    }

    /// Returns `Some(unknown_kid)` when a fetch is due. A fetch is due when the
    /// keys are past `max-age`, or when the kid is unknown and no unknown-kid
    /// refetch ran in the last [`REFETCH_INTERVAL`]. Nothing is due within
    /// [`REFETCH_INTERVAL`] of a failed fetch, so a down issuer does not get hammered.
    fn fetch_due(&self, found: bool, now: Instant) -> Option<bool> {
        let recent =
            |t: Option<Instant>| t.is_some_and(|t| now.duration_since(t) < REFETCH_INTERVAL);
        if recent(self.last_failure) {
            return None;
        }
        let expired = self
            .fetched_at
            .is_none_or(|t| now.duration_since(t) >= self.max_age);
        if expired {
            Some(false)
        } else if !found && !recent(self.last_kid_refetch) {
            Some(true)
        } else {
            None
        }
    }
}

/// Issuer config and key cache, shared with the detached fetch task.
struct Shared {
    cfg: OidcIssuer,
    /// Never held across an await.
    cache: Mutex<KeyCache>,
}

/// Verifier and key cache for one issuer.
pub(crate) struct IssuerVerifier {
    shared: Arc<Shared>,
    /// Set when the issuer has its own CAs; replaces the shared client.
    http: Option<reqwest::Client>,
    observer: Option<JwksObserver>,
}

impl IssuerVerifier {
    pub(crate) fn new(cfg: OidcIssuer) -> Self {
        Self {
            http: None,
            shared: Arc::new(Shared {
                cfg,
                cache: Mutex::default(),
            }),
            observer: None,
        }
    }

    pub(crate) fn set_http(&mut self, http: reqwest::Client) {
        self.http = Some(http);
    }

    pub(crate) fn set_observer(&mut self, observer: JwksObserver) {
        self.observer = Some(observer);
    }

    /// Test hook that ages the cached keys past `max-age` (still inside the stale limit).
    #[cfg(test)]
    pub(crate) fn expire(&self) {
        let mut cache = self.cache();
        let max_age = cache.max_age;
        cache.fetched_at = cache.fetched_at.and_then(|t| t.checked_sub(max_age));
    }

    fn cache(&self) -> MutexGuard<'_, KeyCache> {
        self.shared.cache()
    }

    pub(crate) async fn verify(
        &self,
        http: &reqwest::Client,
        token: &str,
    ) -> Result<Identity, AuthError> {
        let header = jsonwebtoken::decode_header(token)
            .map_err(|_| AuthError::Invalid("malformed header or unsupported alg"))?;
        if !crypto::ALLOWED_ALGS.contains(&header.alg) {
            return Err(AuthError::Invalid("alg not allowed"));
        }
        let kid = header
            .kid
            .as_deref()
            .ok_or(AuthError::Invalid("missing kid"))?;
        let http = self.http.as_ref().unwrap_or(http);
        let jwk = self.key(http, kid, header.alg).await?;
        let key = DecodingKey::from_jwk(&jwk).map_err(|_| AuthError::Invalid("bad jwk"))?;

        let mut v = Validation::new(header.alg);
        v.leeway = LEEWAY_SECS;
        v.validate_nbf = true;
        v.set_issuer(&[&self.shared.cfg.url]);
        v.set_audience(&self.shared.cfg.audiences);
        v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let claims = jsonwebtoken::decode::<Map<String, Value>>(token, &key, &v)
            .map_err(|e| AuthError::Invalid(reason(e.kind())))?
            .claims;

        if let Some(iat) = claims.get("iat") {
            let now = jsonwebtoken::get_current_timestamp();
            if iat.as_u64().is_none_or(|iat| iat > now + LEEWAY_SECS) {
                return Err(AuthError::Invalid("iat invalid or in the future"));
            }
        }
        let subject = match claims.get("sub") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => return Err(AuthError::Invalid("sub missing")),
        };
        Ok(Identity::Oidc {
            issuer: self.shared.cfg.url.clone(),
            subject,
            claims,
        })
    }

    /// Looks up the key for `kid` and `alg`, starting a fetch when one is due
    /// (see [`KeyCache::fetch_due`]). The fetch runs in a detached task, so a
    /// cancelled caller never strands it. A caller holding a usable cached key
    /// returns at once; a caller without one waits for the in-flight fetch
    /// instead of starting a second.
    async fn key(
        &self,
        http: &reqwest::Client,
        kid: &str,
        alg: Algorithm,
    ) -> Result<Jwk, AuthError> {
        let in_flight = {
            let mut cache = self.cache();
            let now = Instant::now();
            let found = cache.find(kid, alg, now);
            let mut in_flight = cache.refreshing();
            if in_flight.is_none()
                && let Some(unknown_kid) = cache.fetch_due(found.is_some(), now)
            {
                in_flight = Some(self.spawn_refresh(&mut cache, http, unknown_kid, now));
            }
            if let Some(jwk) = found {
                return Ok(jwk);
            }
            in_flight
        };
        if let Some(mut done) = in_flight {
            // Errs once the fetch task drops its sender, after recording its outcome.
            let _ = done.changed().await;
        }
        let cache = self.cache();
        let now = Instant::now();
        match cache.find(kid, alg, now) {
            Some(jwk) => Ok(jwk),
            None if cache.usable(now) => Err(AuthError::Invalid(if cache.has_kid(kid) {
                "alg does not match jwk"
            } else {
                "unknown kid"
            })),
            None => Err(AuthError::KeysUnavailable),
        }
    }

    /// Starts one discovery + JWKS fetch in a detached task (see
    /// [`Self::start_refresh`]). The caller holds the cache lock, so at most
    /// one fetch per issuer runs at a time.
    fn spawn_refresh(
        &self,
        cache: &mut KeyCache,
        http: &reqwest::Client,
        unknown_kid: bool,
        started: Instant,
    ) -> watch::Receiver<()> {
        let shared = Arc::clone(&self.shared);
        let http = http.clone();
        let fetch = async move { shared.fetch(&http).await };
        self.start_refresh(cache, unknown_kid, started, fetch)
    }

    /// Runs `fetch` in a detached task that records its outcome before
    /// reporting it to the observer. If the task unwinds first, a
    /// [`RefreshGuard`] records a failure, so the backoff still applies.
    fn start_refresh(
        &self,
        cache: &mut KeyCache,
        unknown_kid: bool,
        started: Instant,
        fetch: impl Future<Output = Result<(JwkSet, Duration), String>> + Send + 'static,
    ) -> watch::Receiver<()> {
        if unknown_kid {
            cache.last_kid_refetch = Some(started);
        }
        let (done, in_flight) = watch::channel(());
        cache.in_flight = Some(in_flight.clone());
        let mut guard = RefreshGuard {
            shared: Arc::clone(&self.shared),
            started,
            recorded: false,
            _done: done,
        };
        let observer = self.observer.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(FETCH_TIMEOUT, fetch)
                .await
                .unwrap_or_else(|_| Err("timed out".into()));
            let outcome = if result.is_ok() {
                JwksFetchResult::Ok
            } else {
                JwksFetchResult::Error
            };
            guard.shared.record(result, started);
            guard.recorded = true;
            if let Some(observe) = &observer {
                observe(&guard.shared.cfg.url, outcome);
            }
        });
        in_flight
    }
}

/// Owned by the refresh task. On drop, it records a failure if the task
/// unwound before recording a result. Then it closes the in-flight channel,
/// because fields drop after [`Drop::drop`] runs.
struct RefreshGuard {
    shared: Arc<Shared>,
    started: Instant,
    recorded: bool,
    _done: watch::Sender<()>,
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        if !self.recorded {
            self.shared
                .record(Err("refresh task panicked".into()), self.started);
        }
    }
}

impl Shared {
    fn cache(&self) -> MutexGuard<'_, KeyCache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn record(&self, result: Result<(JwkSet, Duration), String>, started: Instant) {
        let mut cache = self.cache();
        match result {
            Ok((set, max_age)) => {
                cache.keys = Some(set);
                cache.fetched_at = Some(Instant::now());
                cache.max_age = max_age;
            }
            Err(e) => {
                cache.last_failure = Some(started);
                tracing::warn!(issuer = %self.cfg.url, error = %e, "jwks fetch failed");
            }
        }
    }

    /// Discovery plus JWKS; the caller bounds both with one [`FETCH_TIMEOUT`].
    async fn fetch(&self, http: &reqwest::Client) -> Result<(JwkSet, Duration), String> {
        let jwks_url = if let Some(url) = &self.cfg.jwks_url {
            url.clone()
        } else {
            self.discover(http).await?
        };
        let (body, max_age) = get(http, &jwks_url).await?;
        let set = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
        Ok((set, max_age))
    }

    /// Reads `jwks_uri` from the discovery document, whose `issuer` must equal ours.
    async fn discover(&self, http: &reqwest::Client) -> Result<String, String> {
        let url = format!(
            "{}/.well-known/openid-configuration",
            self.cfg.url.trim_end_matches('/')
        );
        let (body, _) = get(http, &url).await?;
        let doc: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
        if doc.get("issuer").and_then(Value::as_str) != Some(self.cfg.url.as_str()) {
            return Err("discovery issuer mismatch".into());
        }
        doc.get("jwks_uri")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "discovery without jwks_uri".to_owned())
    }
}

async fn get(http: &reqwest::Client, url: &str) -> Result<(Vec<u8>, Duration), String> {
    let mut resp = http.get(url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("http {}", resp.status()));
    }
    let max_age = cache_max_age(
        resp.headers()
            .get(reqwest::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
    );
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        if body.len() + chunk.len() > MAX_BODY {
            return Err("response too large".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, max_age))
}

/// `Cache-Control: max-age` clamped to 5 min..24 h; 1 h when absent.
fn cache_max_age(header: Option<&str>) -> Duration {
    header
        .into_iter()
        .flat_map(|h| h.split(','))
        .find_map(|d| d.trim().strip_prefix("max-age=")?.parse::<u64>().ok())
        .map_or(DEFAULT_MAX_AGE, |s| {
            Duration::from_secs(s).clamp(MIN_MAX_AGE, MAX_MAX_AGE)
        })
}

/// The JWK must be a signing key whose type, curve and declared `alg` agree
/// with the token's `alg`.
fn check_jwk(jwk: &Jwk, alg: Algorithm) -> Result<(), AuthError> {
    use Algorithm as A;
    if jwk
        .common
        .public_key_use
        .as_ref()
        .is_some_and(|u| *u != PublicKeyUse::Signature)
    {
        return Err(AuthError::Invalid("jwk not for signing"));
    }
    if let Some(declared) = jwk.common.key_algorithm
        && key_alg(declared) != Some(alg)
    {
        return Err(AuthError::Invalid("alg does not match jwk"));
    }
    let consistent = match (&jwk.algorithm, alg) {
        (
            AlgorithmParameters::RSA(_),
            A::RS256 | A::RS384 | A::RS512 | A::PS256 | A::PS384 | A::PS512,
        ) => true,
        (AlgorithmParameters::EllipticCurve(p), A::ES256) => p.curve == EllipticCurve::P256,
        (AlgorithmParameters::EllipticCurve(p), A::ES384) => p.curve == EllipticCurve::P384,
        (AlgorithmParameters::OctetKeyPair(p), A::EdDSA) => p.curve == EllipticCurve::Ed25519,
        _ => false,
    };
    if consistent {
        Ok(())
    } else {
        Err(AuthError::Invalid("alg does not match jwk"))
    }
}

fn key_alg(k: KeyAlgorithm) -> Option<Algorithm> {
    use KeyAlgorithm as K;
    Some(match k {
        K::RS256 => Algorithm::RS256,
        K::RS384 => Algorithm::RS384,
        K::RS512 => Algorithm::RS512,
        K::PS256 => Algorithm::PS256,
        K::PS384 => Algorithm::PS384,
        K::PS512 => Algorithm::PS512,
        K::ES256 => Algorithm::ES256,
        K::ES384 => Algorithm::ES384,
        K::EdDSA => Algorithm::EdDSA,
        _ => return None,
    })
}

fn reason(kind: &ErrorKind) -> &'static str {
    match kind {
        ErrorKind::ExpiredSignature => "expired",
        ErrorKind::ImmatureSignature => "not yet valid",
        ErrorKind::InvalidAudience => "audience mismatch",
        ErrorKind::InvalidIssuer => "issuer mismatch",
        ErrorKind::InvalidSignature => "bad signature",
        ErrorKind::MissingRequiredClaim(_) => "missing required claim",
        _ => "invalid token",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_age_is_clamped() {
        let h = |s| cache_max_age(Some(s));
        assert_eq!(cache_max_age(None), DEFAULT_MAX_AGE);
        assert_eq!(h("no-store"), DEFAULT_MAX_AGE);
        assert_eq!(h("public, max-age=10"), MIN_MAX_AGE);
        assert_eq!(h("max-age=7200"), Duration::from_hours(2));
        assert_eq!(h("max-age=999999"), MAX_MAX_AGE);
    }

    /// With `max-age` at its 24 h ceiling, keys due for refresh still have a
    /// full grace period; they fail closed only `STALE_LIMIT` past expiry.
    #[test]
    fn stale_grace_starts_at_expiry() {
        let t = Instant::now();
        let cache = KeyCache {
            fetched_at: Some(t),
            max_age: MAX_MAX_AGE,
            ..KeyCache::default()
        };
        assert!(cache.usable(t + MAX_MAX_AGE));
        assert!(cache.usable(t + MAX_MAX_AGE + STALE_LIMIT - Duration::from_secs(1)));
        assert!(!cache.usable(t + MAX_MAX_AGE + STALE_LIMIT));
    }

    fn panicking_fetch() -> Result<(JwkSet, Duration), String> {
        panic!("fetch panicked")
    }

    #[tokio::test]
    async fn panicking_fetch_still_starts_the_backoff() {
        let verifier = IssuerVerifier::new(OidcIssuer {
            url: "https://issuer.test".into(),
            audiences: vec!["aud".into()],
            jwks_url: None,
            ca_certs: Vec::new(),
        });
        let mut done = {
            let mut cache = verifier.cache();
            let fetch = async { panicking_fetch() };
            verifier.start_refresh(&mut cache, false, Instant::now(), fetch)
        };
        let _ = done.changed().await;
        let cache = verifier.cache();
        assert!(cache.last_failure.is_some());
        assert!(cache.refreshing().is_none());
        assert_eq!(cache.fetch_due(false, Instant::now()), None);
    }
}
