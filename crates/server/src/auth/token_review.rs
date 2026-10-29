//! Service-account authentication via `TokenReview`.
//!
//! Every `TokenReview` is an API call made with the controller's own client, so
//! unauthenticated callers must not be able to drive them in bulk. Three bounds
//! apply before a review is sent:
//!
//! 1. [`sa_token_claims`]: the token must have the shape of a Kubernetes
//!    service-account JWT that is meant for this server, read without
//!    verification. Anything else is answered 401 locally.
//! 2. [`ReviewCache`]: a definitive answer is reused for a few seconds, keyed by
//!    the token's SHA-256 (never the token itself).
//! 3. [`ReviewSlots`] bound reviews in flight and per second, and a per-address
//!    cap (IPv6 peers grouped at /56, see [`review_peer`]) keeps one client from
//!    holding every slot. Over any bound the caller gets 503 `upstream_error` with
//!    `Retry-After` instead of a queued API call.

use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use k8s_openapi::api::authentication::v1::{
    TokenReview, TokenReviewSpec, TokenReviewStatus, UserInfo,
};
use serde_json::{Map, Value};
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::time::Instant;

use super::oidc::LEEWAY_SECS;
use super::{AuthError, Identity};

/// Reviews in flight per replica unless configured otherwise.
pub const DEFAULT_MAX_CONCURRENT_REVIEWS: usize = 32;
/// How long a definitive review answer is reused. A deleted pod's bound token
/// therefore stays usable for at most this long after the API server revokes it.
pub const REVIEW_CACHE_TTL: Duration = Duration::from_secs(10);
/// Cached identities per replica; a full map stops caching new ones.
const POSITIVE_ENTRIES: usize = 4096;
/// Cached refusals per replica, kept apart from identities.
const NEGATIVE_ENTRIES: usize = 1024;
/// `TokenReview` calls in flight per client address unless configured otherwise.
pub const DEFAULT_MAX_REVIEWS_PER_IP: usize = 4;
/// `TokenReview` calls a second. Legitimate callers reuse a token for its
/// cache lifetime, so this is far above their need and far below what a flood could send.
const REVIEWS_PER_SEC: u32 = 50;
/// Minimum gap between two sweeps of expired entries.
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);
/// Username prefix of every service-account token (`system:serviceaccount:<ns>:<name>`).
const SA_PREFIX: &str = "system:serviceaccount:";

/// Why a `TokenReview` call produced no answer.
#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    /// Timeout, transport failure, throttling or 5xx, so the outcome is unknown.
    #[error("tokenreview upstream failure: {0}")]
    Upstream(String),
    /// Any other unexpected API reply (e.g. 403 from missing RBAC).
    #[error("tokenreview unexpected reply: {0}")]
    Unexpected(String),
}

/// Posts a `TokenReview`; implemented by the kube client and by test fakes.
pub trait TokenReviewer: Send + Sync {
    /// Reviews `token` with `spec.audiences = audiences` and returns `status`.
    fn review(
        &self,
        token: &str,
        audiences: &[String],
    ) -> impl Future<Output = Result<TokenReviewStatus, ReviewError>> + Send;
}

/// [`TokenReviewer`] backed by the Kubernetes API.
#[derive(Clone)]
pub struct KubeTokenReviewer {
    api: kube::Api<TokenReview>,
    /// `kube.requestTimeout`: the kube client itself has no read timeout.
    timeout: Duration,
}

impl KubeTokenReviewer {
    #[must_use]
    pub fn new(client: kube::Client, timeout: Duration) -> Self {
        Self {
            api: kube::Api::all(client),
            timeout,
        }
    }
}

impl TokenReviewer for KubeTokenReviewer {
    async fn review(
        &self,
        token: &str,
        audiences: &[String],
    ) -> Result<TokenReviewStatus, ReviewError> {
        let review = TokenReview {
            spec: TokenReviewSpec {
                token: Some(token.to_owned()),
                audiences: Some(audiences.to_vec()),
            },
            ..TokenReview::default()
        };
        // The code summarises errors without their Display, because kube errors can echo
        // the request body, which holds the bearer token.
        let params = kube::api::PostParams::default();
        let create = self.api.create(&params, &review);
        match tokio::time::timeout(self.timeout, create).await {
            Err(_) => Err(ReviewError::Upstream("timeout".into())),
            Ok(Ok(reply)) => reply
                .status
                .ok_or_else(|| ReviewError::Unexpected("no status".into())),
            Ok(Err(kube::Error::Api(status))) => Err(classify(status.code)),
            Ok(Err(_)) => Err(ReviewError::Upstream("transport".into())),
        }
    }
}

/// 408, 429 (API priority and fairness) and 5xx are transient; other codes are not.
fn classify(code: u16) -> ReviewError {
    if code >= 500 || code == 408 || code == 429 {
        ReviewError::Upstream(format!("http {code}"))
    } else {
        ReviewError::Unexpected(format!("http {code}"))
    }
}

/// The `exp` of `token` if it has the shape of a service-account JWT for this
/// server, read without verification: three segments, an asymmetric `alg`
/// (never `none` or HS*), a `sub` of `system:serviceaccount:<ns>:<name>`, an
/// `exp` not past (with [`LEEWAY_SECS`]), and an `aud` that names one of
/// `audiences`. The API server signs every bound and projected token this way,
/// and a review of any other token cannot succeed: the `aud` check would fail
/// in [`identity_from_status`], or the username check would. The `iss` is not
/// checked, because the API server may accept several issuers.
pub(crate) fn sa_token_claims(token: &str, audiences: &[String], now_secs: u64) -> Option<u64> {
    let mut parts = token.split('.');
    let (header, payload, signature) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || signature.is_empty() {
        return None;
    }
    let header = decode_object(header)?;
    let alg = header.get("alg").and_then(Value::as_str)?;
    if !["RS", "PS", "ES"].iter().any(|f| alg.starts_with(f)) && alg != "EdDSA" {
        return None;
    }
    let claims = decode_object(payload)?;
    let sub = claims.get("sub").and_then(Value::as_str)?;
    if !sub.strip_prefix(SA_PREFIX)?.contains(':') {
        return None;
    }
    let exp = claims.get("exp").and_then(Value::as_u64)?;
    if exp.saturating_add(LEEWAY_SECS) <= now_secs {
        return None;
    }
    let names_ours = |a: &Value| a.as_str().is_some_and(|a| audiences.iter().any(|x| x == a));
    let ours = match claims.get("aud")? {
        Value::Array(list) => list.iter().any(names_ours),
        one => names_ours(one),
    };
    ours.then_some(exp)
}

fn decode_object(segment: &str) -> Option<Map<String, Value>> {
    let bytes = B64.decode(segment).ok()?;
    match serde_json::from_slice(&bytes).ok()? {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

/// The cache key: SHA-256 of the token, so the cache never holds a credential.
pub(crate) fn token_key(token: &str) -> [u8; 32] {
    let d = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
    let mut key = [0; 32];
    key.copy_from_slice(d.as_ref());
    key
}

/// Short-lived positive and negative `TokenReview` answers, keyed by [`token_key`].
///
/// Identities and refusals live in separate, separately bounded maps. Anyone
/// can mint unique tokens of SA shape, so refusals can arrive in bulk; they
/// fill only their own map and never evict or crowd out a cached identity.
#[derive(Default)]
pub(crate) struct ReviewCache {
    positive: Mutex<Bounded<Identity>>,
    negative: Mutex<Bounded<&'static str>>,
}

/// Entries that expire, at most `cap` of them.
struct Bounded<T> {
    entries: HashMap<[u8; 32], (Instant, T)>,
    last_sweep: Option<Instant>,
}

impl<T> Default for Bounded<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            last_sweep: None,
        }
    }
}

impl<T: Clone> Bounded<T> {
    fn get(&self, key: &[u8; 32], now: Instant) -> Option<T> {
        let (until, v) = self.entries.get(key)?;
        (now < *until).then(|| v.clone())
    }

    /// Inserts unless full. A full map is swept of expired entries at most once
    /// per [`SWEEP_INTERVAL`]; if that frees nothing, the entry is not cached.
    fn put(&mut self, cap: usize, key: [u8; 32], v: T, until: Instant, now: Instant) {
        if self.entries.len() >= cap && !self.entries.contains_key(&key) {
            if self
                .last_sweep
                .is_some_and(|t| now.duration_since(t) < SWEEP_INTERVAL)
            {
                return;
            }
            self.last_sweep = Some(now);
            self.entries.retain(|_, (u, _)| now < *u);
            if self.entries.len() >= cap {
                return;
            }
        }
        self.entries.insert(key, (until, v));
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ReviewCache {
    /// The cached answer for `key`, if it has not expired.
    pub(crate) fn get(&self, key: &[u8; 32], now: Instant) -> Option<Result<Identity, AuthError>> {
        if let Some(id) = lock(&self.positive).get(key, now) {
            return Some(Ok(id));
        }
        lock(&self.negative)
            .get(key, now)
            .map(|reason| Err(AuthError::Invalid(reason)))
    }

    /// Caches a definitive answer until `until`. Upstream failures and internal
    /// errors are never cached.
    pub(crate) fn put(&self, key: [u8; 32], answer: &Result<Identity, AuthError>, until: Instant) {
        let now = Instant::now();
        match answer {
            Ok(id) => lock(&self.positive).put(POSITIVE_ENTRIES, key, id.clone(), until, now),
            Err(AuthError::Invalid(reason)) => {
                lock(&self.negative).put(NEGATIVE_ENTRIES, key, *reason, until, now);
            }
            Err(_) => {}
        }
    }

    /// Cached identities (tests).
    #[cfg(test)]
    pub(crate) fn positives(&self) -> usize {
        lock(&self.positive).entries.len()
    }

    /// Cached refusals (tests).
    #[cfg(test)]
    pub(crate) fn negatives(&self) -> usize {
        lock(&self.negative).entries.len()
    }
}

/// The address the per-peer review cap counts: the client's /56 group, as for
/// the API's connection cap ([`crate::net::client_group`]).
pub(crate) fn review_peer(ip: IpAddr) -> IpAddr {
    crate::net::client_group(ip)
}

/// Reviews in flight and per second, for every uncached token alike.
///
/// Slots bound concurrency, not rate: a refused review takes about a
/// millisecond, so a few slots kept busy with forged tokens would still send
/// thousands of reviews a second. The token bucket caps them at
/// [`REVIEWS_PER_SEC`]. A forged token can name any `sub`, so no subject gets
/// slots or rate of its own: under a flood a legitimate caller with an
/// uncached token gets the retryable 503, and cached tokens are unaffected.
pub(crate) struct ReviewSlots {
    slots: Semaphore,
    rate: Bucket,
}

/// A token bucket: `per_sec` tokens a second, at most `burst` saved up.
struct Bucket {
    per_sec: f64,
    burst: f64,
    /// Tokens, and when they were counted.
    state: Mutex<(f64, Option<Instant>)>,
}

impl Bucket {
    fn new(per_sec: f64) -> Self {
        let burst = per_sec.max(1.0);
        Self {
            per_sec,
            burst,
            state: Mutex::new((burst, None)),
        }
    }

    /// Takes one token if there is one.
    fn take(&self, now: Instant) -> bool {
        let mut s = lock(&self.state);
        let (tokens, at) = *s;
        let earned = at.map_or(0.0, |t| now.duration_since(t).as_secs_f64() * self.per_sec);
        let tokens = (tokens + earned).min(self.burst);
        if tokens < 1.0 {
            *s = (tokens, Some(now));
            return false;
        }
        *s = (tokens - 1.0, Some(now));
        true
    }
}

impl ReviewSlots {
    /// `max` reviews in flight (at least 1).
    pub(crate) fn new(max: usize) -> Self {
        Self {
            slots: Semaphore::new(max.max(1)),
            rate: Bucket::new(f64::from(REVIEWS_PER_SEC)),
        }
    }

    /// A slot for one review, or `None` when every slot is busy or the rate is spent.
    pub(crate) fn acquire(&self, now: Instant) -> Option<SemaphorePermit<'_>> {
        let permit = self.slots.try_acquire().ok()?;
        self.rate.take(now).then_some(permit)
    }
}

/// Turns a review status into an [`Identity`], or `Invalid`.
pub(crate) fn identity_from_status(
    status: &TokenReviewStatus,
    audiences: &[String],
) -> Result<Identity, AuthError> {
    if status.authenticated != Some(true) {
        return Err(AuthError::Invalid("tokenreview: not authenticated"));
    }
    let granted = status.audiences.as_deref().unwrap_or_default();
    if !granted.iter().any(|a| audiences.contains(a)) {
        return Err(AuthError::Invalid("tokenreview: audience mismatch"));
    }
    let user = status.user.as_ref();
    let username = user.and_then(|u| u.username.as_deref()).unwrap_or_default();
    let Some((namespace, name)) = username
        .strip_prefix(SA_PREFIX)
        .and_then(|rest| rest.split_once(':'))
        .filter(|(ns, n)| !ns.is_empty() && !n.is_empty() && !n.contains(':'))
    else {
        return Err(AuthError::Invalid("tokenreview: not a service account"));
    };
    Ok(Identity::ServiceAccount {
        namespace: namespace.to_owned(),
        name: name.to_owned(),
        uid: user.and_then(|u| u.uid.clone()).unwrap_or_default(),
        pod_uid: user.and_then(pod_uid),
    })
}

/// The `extra` key in which the API server names the pod a bound token was issued for.
const EXTRA_POD_UID: &str = "authentication.kubernetes.io/pod-uid";

/// The pod uid of a bound token's review: one value of hex digits and dashes.
fn pod_uid(user: &UserInfo) -> Option<String> {
    match user.extra.as_ref()?.get(EXTRA_POD_UID)?.as_slice() {
        [uid] if !uid.is_empty() && uid.chars().all(|c| c.is_ascii_hexdigit() || c == '-') => {
            Some(uid.clone())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttling_and_timeouts_are_transient() {
        for code in [408, 429, 500, 503] {
            assert!(matches!(classify(code), ReviewError::Upstream(_)), "{code}");
        }
        for code in [400, 401, 403, 404, 422] {
            assert!(
                matches!(classify(code), ReviewError::Unexpected(_)),
                "{code}"
            );
        }
    }

    /// A `TokenReview` against an API server that accepts and never answers
    /// gives up after the configured timeout.
    #[tokio::test]
    async fn review_honours_the_request_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hold = tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let (s, _) = listener.accept().await.unwrap();
                held.push(s);
            }
        });
        let mut cfg = kube::Config::new(format!("http://{addr}").parse().unwrap());
        cfg.read_timeout = None;
        let client = kube::Client::try_from(cfg).unwrap();
        let reviewer = KubeTokenReviewer::new(client, Duration::from_millis(300));
        let started = std::time::Instant::now();
        let r = tokio::time::timeout(
            Duration::from_secs(5),
            reviewer.review("t", &["a".to_owned()]),
        )
        .await
        .expect("review must not hang");
        assert!(matches!(r, Err(ReviewError::Upstream(m)) if m == "timeout"));
        assert!(started.elapsed() < Duration::from_secs(2));
        hold.abort();
    }
}
