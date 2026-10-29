#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::needless_pass_by_value
)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use buildkit_controller_api::claim::{BuildKitClaim, BuildKitClaimStatus};
use buildkit_controller_api::dto::{
    AllocationResponse, AllocationState, CreateAllocationRequest, ErrorCode, PendingAllocation,
};
use buildkit_controller_api::v1alpha1::BuildKitPool;
use futures::future::BoxFuture;
use k8s_openapi::api::authentication::v1::{TokenReviewStatus, UserInfo};
use kube::runtime::reflector::{self, Store};
use kube::runtime::watcher;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::sign::CertifiedKey;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, broadcast, watch};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use super::api::{Allocator, ApiState, EdgeInfo, router};
use super::listener::Limits;
use super::metrics::HttpMetrics;
use crate::alloc::backend::{Backend, PatchOutcome, Write};
use crate::alloc::metrics::AllocMetrics;
use crate::alloc::{AllocError, AllocService, CertSource, Front, Outcome};
use crate::auth::{
    AuthConfig, Authenticator, Identity, OidcIssuer, ReviewError, TokenReviewer, https_client,
};
use crate::pki::{CaBundle, UserServingCert};
use crate::render::fixtures;
use crate::runtime::metrics::Metrics;
use crate::runtime::stores::ClaimSubscription;

const NS: &str = fixtures::POOL_NAMESPACE;
const ISSUER: &str = "https://issuer.invalid";

/// Test shorthand: `sa.<ns>.<name>` names a service account. [`SaJwt`] turns it
/// into an SA-shaped JWT carrying the shorthand in a `label` claim, and the
/// reviewer accepts a token whose label has that form.
struct Reviewer;

/// Wraps the real authenticator, turning `sa.*` shorthands into SA-shaped JWTs.
struct SaJwt(Authenticator<Reviewer>);

impl super::api::Authenticate for SaJwt {
    fn authenticate<'a>(
        &'a self,
        token: &'a str,
        peer: Option<std::net::IpAddr>,
    ) -> BoxFuture<'a, Result<Identity, crate::auth::AuthError>> {
        let token = if token.starts_with("sa.") {
            sa_jwt(token)
        } else {
            token.to_owned()
        };
        Box::pin(async move { self.0.authenticate_from(&token, peer).await })
    }
}

/// An unsigned token of service-account shape for audience `buildkit-controller`.
fn sa_jwt(label: &str) -> String {
    use base64::Engine as _;
    let b64 = |v: Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;
    let claims = json!({
        "sub": "system:serviceaccount:x:y",
        "aud": ["buildkit-controller"],
        "exp": exp,
        "label": label,
    });
    format!("{}.{}.c2ln", b64(json!({"alg": "RS256"})), b64(claims))
}

impl TokenReviewer for Reviewer {
    fn review(
        &self,
        token: &str,
        audiences: &[String],
    ) -> impl Future<Output = Result<TokenReviewStatus, ReviewError>> + Send {
        let label = jsonwebtoken::dangerous::insecure_decode_claims::<Value>(token)
            .ok()
            .and_then(|c| c.get("label").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default();
        let user = label
            .strip_prefix("sa.")
            .and_then(|r| r.split_once('.'))
            .map(|(ns, n)| format!("system:serviceaccount:{ns}:{n}"));
        let status = TokenReviewStatus {
            authenticated: Some(user.is_some()),
            audiences: Some(audiences.to_vec()),
            user: user.map(|u| UserInfo {
                username: Some(u),
                uid: Some("uid".into()),
                ..UserInfo::default()
            }),
            ..TokenReviewStatus::default()
        };
        async move { Ok(status) }
    }
}

/// `Fake::calls` before any allocation.
const PENDING: u8 = 0;

enum Next {
    Bound,
    Pending,
    Fail(AllocError),
    /// Never resolves; signals `started`, marks `cancelled` when dropped.
    Hang,
    /// Binds after the delay.
    Slow(Duration),
}

struct Fake {
    next: Mutex<Option<Next>>,
    release: Mutex<Result<(), AllocError>>,
    get: Mutex<Result<Outcome, AllocError>>,
    /// Allocations made.
    calls: Arc<AtomicU8>,
    started: Arc<Notify>,
    cancelled: Arc<AtomicU8>,
    seen: Mutex<Vec<String>>,
    waits: Mutex<Vec<Duration>>,
    rejected: Arc<Mutex<Vec<ErrorCode>>>,
}

impl Fake {
    fn new(next: Next) -> Self {
        Self {
            next: Mutex::new(Some(next)),
            release: Mutex::new(Ok(())),
            get: Mutex::new(Err(AllocError::NotFound)),
            calls: Arc::new(AtomicU8::new(PENDING)),
            started: Arc::new(Notify::new()),
            cancelled: Arc::new(AtomicU8::new(0)),
            seen: Mutex::new(Vec::new()),
            waits: Mutex::new(Vec::new()),
            rejected: Arc::default(),
        }
    }
}

struct SetOnDrop(Arc<AtomicU8>);
impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(1, Ordering::SeqCst);
    }
}

const ID: &str = "k3q7m2x4b5n6p7r2s3t4v5w6y7";

fn response(req: &CreateAllocationRequest, owner: String) -> AllocationResponse {
    AllocationResponse {
        id: ID.into(),
        namespace: req.namespace.clone(),
        pool: req.pool.clone(),
        owner,
        endpoint: "tcp://bk.example.com:8444".into(),
        server_name: "bk.example.com".into(),
        ca_cert: None,
        client_cert: "-----BEGIN CERTIFICATE-----".into(),
        client_key: None,
        expires_at: "2026-10-19T13:00:00Z".into(),
        cache: None,
    }
}

fn pending(position: u32) -> Outcome {
    Outcome::Pending(PendingAllocation {
        id: ID.into(),
        state: AllocationState::Pending,
        position: Some(position),
    })
}

impl Allocator for Fake {
    fn allocate<'a>(
        &'a self,
        who: &'a Identity,
        req: &'a CreateAllocationRequest,
    ) -> BoxFuture<'a, Result<Outcome, AllocError>> {
        self.seen.lock().unwrap().push(who.owner());
        let next = self.next.lock().unwrap().take().unwrap_or(Next::Bound);
        Box::pin(async move {
            let bound = || {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(Outcome::Bound(Box::new(response(req, who.owner()))))
            };
            match next {
                Next::Bound => bound(),
                Next::Pending => {
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    Ok(pending(3))
                }
                Next::Fail(e) => Err(e),
                Next::Slow(d) => {
                    tokio::time::sleep(d).await;
                    bound()
                }
                Next::Hang => {
                    let _guard = SetOnDrop(Arc::clone(&self.cancelled));
                    self.started.notify_one();
                    std::future::pending().await
                }
            }
        })
    }
    fn get<'a>(
        &'a self,
        who: &'a Identity,
        _: &'a str,
        wait: Duration,
    ) -> BoxFuture<'a, Result<Outcome, AllocError>> {
        self.seen.lock().unwrap().push(who.owner());
        self.waits.lock().unwrap().push(wait);
        let r = self.get.lock().unwrap().clone();
        Box::pin(async move { r })
    }
    fn rejected(&self, code: ErrorCode) {
        self.rejected.lock().unwrap().push(code);
    }
    fn release<'a>(&'a self, _: &'a Identity, _: &'a str) -> BoxFuture<'a, Result<(), AllocError>> {
        let r = self.release.lock().unwrap().clone();
        Box::pin(async move { r })
    }
}

fn pool(name: &str, ns: &str, rule: Value) -> BuildKitPool {
    let mut p = fixtures::pool(
        name,
        json!({
            "template": {"spec": {"containers": [{"name": "buildkitd", "image": "moby/buildkit:v0.26.3"}]}},
            "scaling": {"min": 1, "max": 4},
            "access": {"rules": [rule]},
        }),
    )
    .unwrap();
    p.metadata.namespace = Some(ns.into());
    p.status = Some(
        serde_json::from_value(
            json!({"warm": 2, "allocated": 1, "platforms": ["linux/amd64"],
            "conditions": [{"type": "Ready", "status": "True", "reason": "Ready", "message": "",
            "lastTransitionTime": "2026-01-01T00:00:00Z"}]}),
        )
        .unwrap(),
    );
    p
}

fn pools() -> Store<BuildKitPool> {
    let (store, mut writer) = reflector::store();
    let mut b = pool("b", NS, json!({"serviceAccount": {}}));
    b.spec.allocation.queue_timeout = serde_json::from_value(json!("5m")).unwrap();
    for p in [
        b,
        pool("a", NS, json!({"serviceAccount": {}})),
        pool(
            "c",
            "other",
            json!({"serviceAccount": {"namespace": "other"}}),
        ),
    ] {
        writer.apply_watcher_event(&watcher::Event::Apply(p));
    }
    store
}

/// API state whose OIDC issuer is [`ISSUER`], which never resolves.
fn state<A: Allocator>(alloc: A, metrics: &Metrics) -> Arc<ApiState<A>> {
    state_with(alloc, metrics, ISSUER, rustls::RootCertStore::empty())
}

/// API state trusting the OIDC issuer at `issuer`, reached with `roots`.
fn state_with<A: Allocator>(
    alloc: A,
    metrics: &Metrics,
    issuer: &str,
    roots: rustls::RootCertStore,
) -> Arc<ApiState<A>> {
    let auth = Authenticator::new(
        AuthConfig {
            oidc: vec![OidcIssuer {
                url: issuer.into(),
                audiences: vec!["buildkit-controller".into()],
                jwks_url: None,
                ca_certs: Vec::new(),
            }],
            service_account_audiences: Some(vec!["buildkit-controller".into()]),
        },
        https_client(roots).unwrap(),
        Reviewer,
    )
    .unwrap();
    Arc::new(ApiState {
        alloc,
        auth: Arc::new(SaJwt(auth)),
        oidc_issuers: HashSet::from([issuer.to_owned()]),
        service_accounts: true,
        pools: pools(),
        edge: EdgeInfo {
            endpoint: "tcp://bk.example.com:8444".into(),
            server_name: "bk.example.com".into(),
            per_allocation_endpoints: true,
        },
        metrics: HttpMetrics::register(metrics),
        refusals: super::api::LogSampler::default(),
    })
}

async fn call(app: &Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let r = app.clone().oneshot(req).await.unwrap();
    let (parts, body) = r.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
    (
        parts.status,
        parts.headers,
        String::from_utf8(bytes.to_vec()).unwrap(),
    )
}

fn post(token: Option<&str>, ctype: &str, body: impl Into<Body>) -> Request<Body> {
    let mut b = Request::post("/v1/allocations").header(header::CONTENT_TYPE, ctype);
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(body.into()).unwrap()
}

fn get(path: &str, token: &str) -> Request<Body> {
    Request::get(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

const OK_BODY: &str = r#"{"namespace":"render-verify","pool":"a","publicKey":"pk"}"#;
const ALICE: &str = "sa.render-verify.alice";

#[tokio::test]
async fn unauthenticated_is_401_with_www_authenticate_and_exact_body() {
    let metrics = Metrics::new();
    let app = router(state(Fake::new(Next::Bound), &metrics));
    for token in [None, Some("garbage"), Some("sa.")] {
        let (st, h, body) = call(&app, post(token, "application/json", OK_BODY)).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{token:?}");
        assert_eq!(h[header::WWW_AUTHENTICATE], "Bearer");
        assert_eq!(h[header::CONTENT_TYPE], "application/json");
        assert_eq!(
            body,
            r#"{"error":{"code":"unauthenticated","message":"missing or invalid bearer token"}}"#
        );
    }
    let text = metrics.render();
    assert!(
        text.contains(r#"buildkit_http_requests_total{route="/v1/allocations",status="401"} 3"#),
        "{text}"
    );
    assert!(
        text.contains(r#"buildkit_auth_total{method="serviceaccount",result="invalid"} 2"#),
        "{text}"
    );
}

fn b64(v: &Value) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
}

/// The issuer's keys load, so each token fails on its signature or `alg`.
#[tokio::test]
async fn forged_or_unsigned_jwt_is_401() {
    use crate::auth::tests::{Signer, TestIssuer, claims};
    let key = Signer::rsa("k", "RS256");
    let iss = TestIssuer::start(&[&key]).await;
    let fake = Fake::new(Next::Bound);
    let grant = Arc::clone(&fake.calls);
    let app = router(state_with(
        fake,
        &Metrics::new(),
        &iss.url,
        iss.roots.clone(),
    ));
    let valid = key.token(&claims(&iss.url));
    let body = valid.split('.').nth(1).unwrap().to_owned();
    let unsigned = format!("{}.{body}.", b64(&json!({"alg": "none", "typ": "JWT"})));
    let garbage = format!(
        "{}.{body}.c2lnbmF0dXJl",
        b64(&json!({"alg": "RS256", "kid": "k"}))
    );
    // A real signature over other claims.
    let mut other = claims(&iss.url);
    other["sub"] = json!("repo:smrt-devops/other:ref:refs/heads/main");
    let mut swapped: Vec<String> = valid.split('.').map(str::to_owned).collect();
    swapped[1] = key.token(&other).split('.').nth(1).unwrap().to_owned();
    let swapped = swapped.join(".");
    let hs = format!(
        "{}.{body}.c2lnbmF0dXJl",
        b64(&json!({"alg": "HS256", "kid": "k"}))
    );
    for t in [unsigned, garbage, swapped, hs] {
        let (st, h, _) = call(&app, post(Some(&t), "application/json", OK_BODY)).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        assert_eq!(h[header::WWW_AUTHENTICATE], "Bearer");
    }
    assert_eq!(
        grant.load(Ordering::SeqCst),
        PENDING,
        "allocator never reached"
    );
    // The same issuer's validly signed token gets through, so the keys loaded.
    let (st, _, text) = call(&app, post(Some(&valid), "application/json", OK_BODY)).await;
    assert_ne!(st, StatusCode::UNAUTHORIZED, "{text}");
    assert_ne!(grant.load(Ordering::SeqCst), PENDING);
}

/// An issuer whose keys cannot be fetched is a retryable 503, not a 401 that
/// reads as a bad token. `Retry-After` covers the refetch backoff, since an
/// earlier retry cannot change the answer.
#[tokio::test]
async fn unreachable_issuer_is_503_with_retry_after() {
    let fake = Fake::new(Next::Bound);
    let grant = Arc::clone(&fake.calls);
    let app = router(state(fake, &Metrics::new()));
    let claims = b64(
        &json!({"iss": ISSUER, "sub": "x", "aud": "buildkit-controller", "exp": 4_102_444_800_u64}),
    );
    let t = format!(
        "{}.{claims}.c2lnbmF0dXJl",
        b64(&json!({"alg": "RS256", "kid": "k"}))
    );
    for _ in 0..2 {
        let (st, h, text) = call(&app, post(Some(&t), "application/json", OK_BODY)).await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{text}");
        assert_eq!(
            h[header::RETRY_AFTER],
            crate::auth::REFETCH_INTERVAL.as_secs().to_string()
        );
        assert!(h.get(header::WWW_AUTHENTICATE).is_none());
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["error"]["code"], "upstream_error", "{text}");
        assert_eq!(v["error"]["message"], "token issuer keys unavailable");
    }
    assert_eq!(grant.load(Ordering::SeqCst), PENDING);
}

struct NoBackend;
impl Backend for NoBackend {
    fn patch_pod(&self, _: &str, _: &str, _: Value) -> BoxFuture<'static, PatchOutcome> {
        Box::pin(async { PatchOutcome::Unknown })
    }
    fn delete_pod(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: Option<u32>,
    ) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn create_claim(&self, _: BuildKitClaim) -> BoxFuture<'static, Write> {
        Box::pin(async { Write::Unknown })
    }
    fn get_claim(
        &self,
        _: &str,
        _: &str,
    ) -> BoxFuture<'static, Result<Option<BuildKitClaim>, Write>> {
        Box::pin(async { Ok(None) })
    }
    fn delete_claim(&self, _: &str, _: &str, _: &str) -> BoxFuture<'static, Write> {
        Box::pin(async { Write::Gone })
    }
    fn delete_claim_at(&self, _: &str, _: &str, _: &str, _: &str) -> BoxFuture<'static, Write> {
        Box::pin(async { Write::Gone })
    }
    fn annotate_claim(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: std::collections::BTreeMap<String, String>,
    ) -> BoxFuture<'static, Write> {
        Box::pin(async { Write::Gone })
    }
    fn patch_claim_status(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &BuildKitClaimStatus,
    ) -> BoxFuture<'static, Write> {
        Box::pin(async { Write::Unknown })
    }
    fn get_secret(
        &self,
        _: &str,
        _: &str,
    ) -> BoxFuture<'static, Result<Option<k8s_openapi::api::core::v1::Secret>, Write>> {
        Box::pin(async { Ok(None) })
    }
    fn create_secret(&self, _: k8s_openapi::api::core::v1::Secret) -> BoxFuture<'static, Write> {
        Box::pin(async { Write::Unknown })
    }
    fn delete_secret(&self, _: &str, _: &str, _: &str) -> BoxFuture<'static, Write> {
        Box::pin(async { Write::Gone })
    }
}

struct NoCa;
impl CertSource for NoCa {
    fn bundle(&self) -> Option<Arc<CaBundle>> {
        None
    }
    fn ca_cert_pem(&self, _: &CaBundle) -> Option<String> {
        None
    }
}

/// The real front door over the test pools, with no API server and no CA.
fn front() -> Front {
    let (claims, _) = reflector::store();
    let (tx, _) = broadcast::channel(4);
    Front::new(
        Arc::new(NoBackend),
        pools(),
        claims,
        Box::new(move || ClaimSubscription::new(tx.subscribe())),
        Arc::new(NoCa),
        "bk-system".into(),
        HashSet::from(["bk-system".to_owned()]),
        AllocMetrics::default(),
    )
}

/// [`Front`] as the router's allocator.
impl Allocator for Front {
    fn allocate<'a>(
        &'a self,
        who: &'a Identity,
        req: &'a CreateAllocationRequest,
    ) -> BoxFuture<'a, Result<Outcome, AllocError>> {
        Box::pin(Front::allocate(self, who, req))
    }
    fn get<'a>(
        &'a self,
        who: &'a Identity,
        id: &'a str,
        wait: Duration,
    ) -> BoxFuture<'a, Result<Outcome, AllocError>> {
        Box::pin(Front::get(self, who, id, wait))
    }
    fn release<'a>(
        &'a self,
        who: &'a Identity,
        id: &'a str,
    ) -> BoxFuture<'a, Result<(), AllocError>> {
        Box::pin(Front::release(self, who, id))
    }
}

#[tokio::test]
async fn no_access_is_identical_to_a_missing_pool() {
    let app = router(state(front(), &Metrics::new()));
    let denied = call(
        &app,
        post(Some("sa.elsewhere.mallory"), "application/json", OK_BODY),
    )
    .await;
    let missing = call(
        &app,
        post(
            Some(ALICE),
            "application/json",
            r#"{"namespace":"render-verify","pool":"zz","publicKey":"pk"}"#,
        ),
    )
    .await;
    assert_eq!(denied.0, StatusCode::NOT_FOUND);
    assert_eq!(
        denied.2,
        r#"{"error":{"code":"pool_not_found","message":"pool not found"}}"#
    );
    assert_eq!((denied.0, &denied.2), (missing.0, &missing.2));
    assert_eq!(denied.1.get(header::RETRY_AFTER), None);
    // The pool exists for an allowed caller, so the next step (CA) answers.
    let (st, _, body) = call(&app, post(Some(ALICE), "application/json", OK_BODY)).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("ca_unavailable"), "{body}");
}

/// `TokenReview` busy, throttled or timed out.
struct ReviewDown;

impl super::api::Authenticate for ReviewDown {
    fn authenticate<'a>(
        &'a self,
        _: &'a str,
        _: Option<std::net::IpAddr>,
    ) -> BoxFuture<'a, Result<Identity, crate::auth::AuthError>> {
        Box::pin(async {
            Err(crate::auth::AuthError::Upstream(
                "tokenreview saturated".into(),
            ))
        })
    }
}

#[tokio::test]
async fn auth_upstream_failure_is_503_with_retry_after() {
    let metrics = Metrics::new();
    let mut s = Arc::into_inner(state(Fake::new(Next::Bound), &metrics)).unwrap();
    s.auth = Arc::new(ReviewDown);
    let app = router(Arc::new(s));
    let (st, h, body) = call(&app, post(Some(ALICE), "application/json", OK_BODY)).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(h[header::RETRY_AFTER], "2");
    assert!(body.contains("upstream_error"), "{body}");
}

#[tokio::test]
async fn alloc_errors_map_to_spec_status_code_and_retry_after() {
    for e in [
        AllocError::InvalidRequest("ttlSeconds out of range".into()),
        AllocError::PoolNotFound,
        AllocError::Internal,
        AllocError::PoolExhausted("render-verify/a".into()),
        AllocError::PodLost("the worker pod is gone".into()),
        AllocError::NotReady,
        AllocError::CaUnavailable,
        AllocError::UpstreamError,
    ] {
        let code = e.code();
        let metrics = Metrics::new();
        let app = router(state(Fake::new(Next::Fail(e.clone())), &metrics));
        let (st, h, body) = call(&app, post(Some(ALICE), "application/json", OK_BODY)).await;
        assert_eq!(st.as_u16(), code.http_status(), "{code:?}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v,
            json!({"error": {"code": code.as_str(), "message": e.to_string()}})
        );
        let retry = h
            .get(header::RETRY_AFTER)
            .map(|v| v.to_str().unwrap().to_owned());
        let want = match code {
            ErrorCode::PoolExhausted => Some("5".to_owned()),
            ErrorCode::UpstreamError => Some("2".to_owned()),
            _ => None,
        };
        assert_eq!(retry, want, "{code:?}");
        let line = format!(
            r#"buildkit_http_requests_total{{route="/v1/allocations",status="{}"}} 1"#,
            st.as_u16()
        );
        assert!(metrics.render().contains(&line));
    }
}

#[tokio::test]
async fn release_is_204_owner_only_403_and_404() {
    for (r, want, code) in [
        (Ok(()), StatusCode::NO_CONTENT, None),
        (
            Err(AllocError::Forbidden),
            StatusCode::FORBIDDEN,
            Some("forbidden"),
        ),
        (
            Err(AllocError::NotFound),
            StatusCode::NOT_FOUND,
            Some("not_found"),
        ),
    ] {
        let fake = Fake::new(Next::Bound);
        *fake.release.lock().unwrap() = r;
        let app = router(state(fake, &Metrics::new()));
        let req = Request::delete("/v1/allocations/k3q7m2x4b5n6p7r2s3t4v5w6y7")
            .header(header::AUTHORIZATION, format!("Bearer {ALICE}"))
            .body(Body::empty())
            .unwrap();
        let (st, _, body) = call(&app, req).await;
        assert_eq!(st, want);
        match code {
            None => assert!(body.is_empty()),
            Some(c) => assert!(body.contains(&format!(r#""code":"{c}""#)), "{body}"),
        }
    }
    let app = router(state(Fake::new(Next::Bound), &Metrics::new()));
    let req = Request::delete("/v1/allocations/x")
        .body(Body::empty())
        .unwrap();
    assert_eq!(call(&app, req).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn body_rules_415_413_400_then_201() {
    let app = router(state(Fake::new(Next::Bound), &Metrics::new()));
    let cases: Vec<(Request<Body>, StatusCode, &str)> = vec![
        (
            post(Some(ALICE), "text/plain", OK_BODY),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
        ),
        (
            post(Some(ALICE), "application/json", vec![b' '; 16 * 1024 + 1]),
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
        ),
        (
            post(
                Some(ALICE),
                "application/json",
                r#"{"namespace":"a","pool":"b","extra":1}"#,
            ),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            post(Some(ALICE), "application/json", "{"),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            post(Some(ALICE), "application/json", r#"{"pool":"b"}"#),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
    ];
    for (req, want, code) in cases {
        let (st, _, body) = call(&app, req).await;
        assert_eq!(st, want, "{body}");
        assert!(
            body.starts_with(&format!(r#"{{"error":{{"code":"{code}","message":"#)),
            "{body}"
        );
    }
    // Declared Content-Length over the limit is refused before reading.
    let req = Request::post("/v1/allocations")
        .header(header::AUTHORIZATION, format!("Bearer {ALICE}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, "999999")
        .body(Body::from(OK_BODY))
        .unwrap();
    assert_eq!(call(&app, req).await.0, StatusCode::PAYLOAD_TOO_LARGE);
    // Exactly 16 KiB of valid JSON is accepted.
    let pad = " ".repeat(16 * 1024 - OK_BODY.len());
    let (st, h, body) = call(
        &app,
        post(
            Some(ALICE),
            "application/json; charset=utf-8",
            format!("{OK_BODY}{pad}"),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(h[header::CACHE_CONTROL], "no-store");
    let r: AllocationResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(r.owner, "sa:render-verify/alice");
    assert!(!body.contains("clientKey"));
}

#[tokio::test]
async fn pools_lists_accessible_pools_sorted() {
    let app = router(state(Fake::new(Next::Bound), &Metrics::new()));
    let (st, h, body) = call(&app, get("/v1/pools", ALICE)).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(h[header::CONTENT_TYPE], "application/json");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["perAllocationEndpoints"], true);
    let names: Vec<&str> = v["pools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a", "b"]);
    assert_eq!(
        v["pools"][0],
        json!({"namespace": NS, "name": "a", "ready": true, "warm": 2, "starting": 0, "allocated": 1,
            "waiting": 0, "min": 1, "max": 4, "platforms": ["linux/amd64"],
            "endpoint": "tcp://bk.example.com:8444", "serverName": "bk.example.com",
            "queueTimeoutSeconds": 120, "maxQueueTimeoutSeconds": 600,
            "defaultTtlSeconds": 3600, "maxTtlSeconds": 86400})
    );
    assert_eq!(
        v["pools"][1]["queueTimeoutSeconds"], 300,
        "from the pool spec"
    );
    let (_, _, body) = call(&app, get("/v1/pools", "sa.other.bob")).await;
    assert!(
        body.contains(r#""name":"c""#) && !body.contains(r#""name":"a""#),
        "{body}"
    );
    let (st, _, body) = call(&app, get("/v1/pools", "sa.nowhere.eve")).await;
    assert_eq!(
        (st, body.as_str()),
        (
            StatusCode::OK,
            r#"{"perAllocationEndpoints":true,"pools":[]}"#
        )
    );
}

#[tokio::test]
async fn other_paths_and_methods_are_404_not_found() {
    let metrics = Metrics::new();
    let app = router(state(Fake::new(Next::Bound), &metrics));
    for req in [
        get("/nope", ALICE),
        get("/v1/allocations", ALICE),
        get("/healthz", ALICE),
    ] {
        let (st, _, body) = call(&app, req).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(
            body,
            r#"{"error":{"code":"not_found","message":"not found"}}"#
        );
    }
    let text = metrics.render();
    assert!(
        text.contains(r#"buildkit_http_requests_total{route="other",status="404"} 2"#),
        "{text}"
    );
    assert!(
        text.contains(r#"buildkit_http_requests_total{route="/v1/allocations",status="404"} 1"#),
        "{text}"
    );
}

// ---- real TLS ----

fn serving(name: &str) -> (Arc<CertifiedKey>, CertificateDer<'static>) {
    let c = rcgen::generate_simple_self_signed(vec![name.to_owned()]).unwrap();
    let crt = c.cert.pem();
    let key = c.signing_key.serialize_pem();
    let s = UserServingCert::from_tls_secret(
        crt.as_bytes(),
        key.as_bytes(),
        name,
        time::OffsetDateTime::now_utc(),
    )
    .unwrap();
    (s.certified_key(), c.cert.der().clone())
}

async fn spawn_api<A: Allocator>(
    alloc: A,
    key: Option<Arc<CertifiedKey>>,
) -> (
    std::net::SocketAddr,
    watch::Sender<Option<Arc<CertifiedKey>>>,
    CancellationToken,
) {
    spawn_api_with(alloc, key, Limits::default()).await
}

async fn spawn_api_with<A: Allocator>(
    alloc: A,
    key: Option<Arc<CertifiedKey>>,
    limits: Limits,
) -> (
    std::net::SocketAddr,
    watch::Sender<Option<Arc<CertifiedKey>>>,
    CancellationToken,
) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let (tx, rx) = watch::channel(key);
    let stop = CancellationToken::new();
    tokio::spawn(super::serve(
        l,
        router(state(alloc, &Metrics::new())),
        rx,
        limits,
        stop.clone(),
    ));
    (addr, tx, stop)
}

async fn dial(
    addr: std::net::SocketAddr,
    trust: &CertificateDer<'static>,
) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    dial_alpn(addr, trust, &[b"h2", b"http/1.1"]).await
}

async fn dial_alpn(
    addr: std::net::SocketAddr,
    trust: &CertificateDer<'static>,
    alpn: &[&[u8]],
) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(trust.clone()).unwrap();
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let tcp = TcpStream::connect(addr).await?;
    tokio_rustls::TlsConnector::from(Arc::new(cfg))
        .connect(ServerName::try_from("api.test").unwrap(), tcp)
        .await
}

#[tokio::test]
async fn tls_alpn_and_hot_swapped_serving_cert() {
    let (k1, c1) = serving("api.test");
    let (k2, c2) = serving("api.test");
    let (addr, tx, stop) = spawn_api(Fake::new(Next::Bound), None).await;
    assert!(
        dial(addr, &c1).await.is_err(),
        "no cert yet: connection closed"
    );
    tx.send_replace(Some(k1));
    let s = dial(addr, &c1).await.unwrap();
    assert_eq!(s.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    assert!(dial(addr, &c2).await.is_err());
    tx.send_replace(Some(k2));
    assert!(
        dial(addr, &c2).await.is_ok(),
        "new connections get the rotated cert"
    );
    assert!(dial(addr, &c1).await.is_err());
    stop.cancel();
}

#[tokio::test]
async fn client_disconnect_mid_allocation_cancels_it() {
    let (k, c) = serving("api.test");
    let fake = Fake::new(Next::Hang);
    let (started, cancelled) = (Arc::clone(&fake.started), Arc::clone(&fake.cancelled));
    let (addr, _tx, stop) = spawn_api(fake, Some(k)).await;
    let mut s = dial_alpn(addr, &c, &[b"http/1.1"]).await.unwrap();
    let req = format!(
        "POST /v1/allocations HTTP/1.1\r\nhost: api.test\r\nauthorization: Bearer {ALICE}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{OK_BODY}",
        OK_BODY.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    assert_eq!(cancelled.load(Ordering::SeqCst), 0);
    drop(s);
    tokio::time::timeout(Duration::from_secs(5), async {
        while cancelled.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("allocation future dropped after the client left");
    stop.cancel();
}

/// After shutdown, an allocation sent on an HTTP/1 keep-alive connection that
/// was open before is still served, with `Connection: close`: the claim it
/// creates outlives the replica. New connections are refused.
#[tokio::test]
async fn shutdown_still_answers_keep_alive_allocations() {
    let (k, c) = serving("api.test");
    let fake = Fake::new(Next::Bound);
    let rejected = Arc::clone(&fake.rejected);
    let grant = Arc::clone(&fake.calls);
    let (addr, _tx, stop) = spawn_api(fake, Some(k)).await;
    let mut s = dial_alpn(addr, &c, &[b"http/1.1"]).await.unwrap();
    stop.cancel();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(dial(addr, &c).await.is_err(), "new connections are refused");
    let req = format!(
        "POST /v1/allocations HTTP/1.1\r\nhost: api.test\r\nauthorization: Bearer {ALICE}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{OK_BODY}",
        OK_BODY.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_string(&mut out))
        .await
        .expect("the server closes after the response")
        .unwrap();
    assert!(out.starts_with("HTTP/1.1 201"), "{out}");
    assert!(out.contains("connection: close\r\n"), "{out}");
    assert!(rejected.lock().unwrap().is_empty());
    assert_eq!(grant.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn full_201_over_tls() {
    let (k, c) = serving("api.test");
    let fake = Fake::new(Next::Bound);
    let grant = Arc::clone(&fake.calls);
    let (addr, _tx, stop) = spawn_api(fake, Some(k)).await;
    let mut s = dial_alpn(addr, &c, &[b"http/1.1"]).await.unwrap();
    let req = format!(
        "POST /v1/allocations HTTP/1.1\r\nhost: api.test\r\nauthorization: Bearer {ALICE}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{OK_BODY}",
        OK_BODY.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out).await;
    assert!(out.starts_with("HTTP/1.1 201"), "{out}");
    assert!(out.contains("cache-control: no-store"));
    assert_eq!(grant.load(Ordering::SeqCst), 1);
    stop.cancel();
}

#[tokio::test]
async fn pre_allocate_refusals_are_counted_once_each() {
    let fake = Fake::new(Next::Fail(AllocError::NotReady));
    let rejected = Arc::clone(&fake.rejected);
    let app = router(state(fake, &Metrics::new()));
    for req in [
        post(None, "application/json", OK_BODY),
        post(Some(ALICE), "text/plain", OK_BODY),
        post(Some(ALICE), "application/json", vec![b' '; 16 * 1024 + 1]),
        post(Some(ALICE), "application/json", "{"),
        post(
            Some(ALICE),
            "application/json",
            r#"{"namespace":"a","pool":"b","x":1}"#,
        ),
        post(
            Some(ALICE),
            "application/json",
            r#"{"namespace":"render-verify","pool":"a"}"#,
        ),
        // Reaches the allocator, which counts its own outcome.
        post(Some(ALICE), "application/json", OK_BODY),
    ] {
        let _ = call(&app, req).await;
    }
    assert_eq!(
        *rejected.lock().unwrap(),
        [
            ErrorCode::Unauthenticated,
            ErrorCode::UnsupportedMediaType,
            ErrorCode::PayloadTooLarge,
            ErrorCode::InvalidRequest,
            ErrorCode::InvalidRequest,
            ErrorCode::InvalidRequest,
        ]
    );
}

#[tokio::test]
async fn alloc_service_counts_rejections_with_empty_labels() {
    let metrics = Metrics::new();
    let m = AllocMetrics::default();
    metrics.with_registry(|r| m.register(r));
    let app = router(state(
        AllocService::with_metrics(watch::channel(crate::certs::CertMaterial::empty()).1, m),
        &metrics,
    ));
    let _ = call(&app, post(None, "application/json", OK_BODY)).await;
    let _ = call(&app, post(Some(ALICE), "application/json", "{")).await;
    let text = metrics.render();
    for code in ["unauthenticated", "invalid_request"] {
        let line =
            format!(r#"buildkit_allocations_total{{namespace="",pool="",result="{code}"}} 1"#);
        assert!(text.contains(&line), "{line}\n{text}");
    }
}

#[tokio::test]
async fn delete_with_undecodable_id_authenticates_first() {
    let app = router(state(Fake::new(Next::Bound), &Metrics::new()));
    let req = Request::delete("/v1/allocations/%FF")
        .body(Body::empty())
        .unwrap();
    let (st, h, _) = call(&app, req).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(h[header::WWW_AUTHENTICATE], "Bearer");
    let req = Request::delete("/v1/allocations/%FF")
        .header(header::AUTHORIZATION, format!("Bearer {ALICE}"))
        .body(Body::empty())
        .unwrap();
    let (st, _, body) = call(&app, req).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        r#"{"error":{"code":"not_found","message":"allocation not found"}}"#
    );
}

async fn closed_within(s: &mut tokio_rustls::client::TlsStream<TcpStream>, d: Duration) -> bool {
    let mut buf = [0u8; 256];
    tokio::time::timeout(d, async {
        loop {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn idle_after_tls_is_closed_for_h1_and_h2() {
    let (k, c) = serving("api.test");
    let limits = Limits {
        idle_timeout: Duration::from_millis(200),
        ..Limits::default()
    };
    let (addr, _tx, stop) = spawn_api_with(Fake::new(Next::Bound), Some(k), limits).await;
    for alpn in [&b"h2"[..], &b"http/1.1"[..]] {
        let mut s = dial_alpn(addr, &c, &[alpn]).await.unwrap();
        assert_eq!(s.get_ref().1.alpn_protocol(), Some(alpn));
        assert!(
            closed_within(&mut s, Duration::from_secs(3)).await,
            "{alpn:?}"
        );
    }
    // A partial HTTP/2 preface does not count as a request.
    let mut s = dial_alpn(addr, &c, &[b"h2"]).await.unwrap();
    s.write_all(b"PRI * HTTP/2.0\r\n").await.unwrap();
    assert!(closed_within(&mut s, Duration::from_secs(3)).await);
    stop.cancel();
}

#[tokio::test]
async fn connection_cap_is_enforced() {
    let (k, c) = serving("api.test");
    let limits = Limits {
        max_connections: 1,
        ..Limits::default()
    };
    let (addr, _tx, stop) = spawn_api_with(Fake::new(Next::Bound), Some(k), limits).await;
    let first = dial(addr, &c).await.unwrap();
    assert!(dial(addr, &c).await.is_err(), "second connection refused");
    drop(first);
    let again = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if dial(addr, &c).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(again.is_ok(), "slot freed after the first closed");
    stop.cancel();
}

/// One client address cannot hold more than `max_connections_per_ip`, even
/// while the global cap has room.
#[tokio::test]
async fn per_address_connection_cap_is_enforced() {
    let (k, c) = serving("api.test");
    let limits = Limits {
        max_connections: 64,
        max_connections_per_ip: 2,
        ..Limits::default()
    };
    let (addr, _tx, stop) = spawn_api_with(Fake::new(Next::Bound), Some(k), limits).await;
    let first = dial(addr, &c).await.unwrap();
    let _second = dial(addr, &c).await.unwrap();
    assert!(dial(addr, &c).await.is_err(), "third connection refused");
    drop(first);
    let again = tokio::time::timeout(Duration::from_secs(3), async {
        while dial(addr, &c).await.is_err() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(again.is_ok(), "slot freed after the first closed");
    stop.cancel();
}

/// An HTTP/2 client connection over TLS; the task ends when the server closes it.
async fn h2_client(
    addr: std::net::SocketAddr,
    trust: &CertificateDer<'static>,
) -> (
    hyper::client::conn::http2::SendRequest<Body>,
    tokio::task::JoinHandle<()>,
) {
    let s = dial_alpn(addr, trust, &[b"h2"]).await.unwrap();
    let (send, conn) = hyper::client::conn::http2::handshake(
        hyper_util::rt::TokioExecutor::new(),
        hyper_util::rt::TokioIo::new(s),
    )
    .await
    .unwrap();
    (send, tokio::spawn(async move { drop(conn.await) }))
}

fn h2_post(token: &str) -> Request<Body> {
    Request::post("https://api.test/v1/allocations")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(OK_BODY))
        .unwrap()
}

#[tokio::test]
async fn h2_idle_after_its_streams_finish_is_closed() {
    let (k, c) = serving("api.test");
    let limits = Limits {
        idle_timeout: Duration::from_millis(300),
        ..Limits::default()
    };
    let (addr, _tx, stop) = spawn_api_with(Fake::new(Next::Bound), Some(k), limits).await;
    let (mut send, conn) = h2_client(addr, &c).await;
    // Busy for longer than the idle bound, one stream at a time.
    for _ in 0..4 {
        let req = Request::get("https://api.test/v1/pools")
            .body(Body::empty())
            .unwrap();
        let resp = send.send_request(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        drop(resp);
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    assert!(!conn.is_finished(), "requests re-arm the idle bound");
    // Now the connection is idle. The server closes it, though the client answers every PING.
    tokio::time::timeout(Duration::from_secs(3), conn)
        .await
        .expect("idle HTTP/2 connection closed")
        .unwrap();
    stop.cancel();
}

#[tokio::test]
async fn h2_queued_post_is_never_cut_by_the_idle_bound() {
    let (k, c) = serving("api.test");
    let limits = Limits {
        idle_timeout: Duration::from_millis(200),
        ..Limits::default()
    };
    let fake = Fake::new(Next::Hang);
    let (started, cancelled) = (Arc::clone(&fake.started), Arc::clone(&fake.cancelled));
    let (addr, _tx, stop) = spawn_api_with(fake, Some(k), limits).await;
    let (mut send, conn) = h2_client(addr, &c).await;
    // A live handle, so only the server can end the connection.
    let mut held = send.clone();
    let pending = tokio::spawn(async move { send.send_request(h2_post(ALICE)).await });
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(cancelled.load(Ordering::SeqCst), 0, "queued POST kept");
    assert!(
        !conn.is_finished(),
        "connection kept while a stream is open"
    );
    assert!(!pending.is_finished());
    // The client gives up (RST_STREAM): the stream ends, the bound re-arms.
    pending.abort();
    tokio::time::timeout(Duration::from_secs(3), conn)
        .await
        .expect("idle once the last stream ended")
        .unwrap();
    assert_eq!(cancelled.load(Ordering::SeqCst), 1);
    assert!(held.ready().await.is_err(), "closed by the server");
    stop.cancel();
}

#[tokio::test]
async fn h2_peer_with_a_zero_window_cannot_hold_the_connection() {
    let (k, c) = serving("api.test");
    let limits = Limits {
        idle_timeout: Duration::from_millis(200),
        ..Limits::default()
    };
    let (addr, _tx, stop) = spawn_api_with(Fake::new(Next::Bound), Some(k), limits).await;
    let s = dial_alpn(addr, &c, &[b"h2"]).await.unwrap();
    // With SETTINGS_INITIAL_WINDOW_SIZE=0 and no WINDOW_UPDATE, no body byte can be sent.
    let (mut send, conn) =
        hyper::client::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
            .initial_stream_window_size(0)
            .handshake::<_, Body>(hyper_util::rt::TokioIo::new(s))
            .await
            .unwrap();
    let conn = tokio::spawn(async move { drop(conn.await) });
    let req = Request::get("https://api.test/v1/pools")
        .body(Body::empty())
        .unwrap();
    let resp = send.send_request(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // The 401 body stays undelivered; the connection is closed regardless.
    tokio::time::timeout(Duration::from_secs(3), conn)
        .await
        .expect("closed although the response body never left")
        .unwrap();
    assert!(send.ready().await.is_err(), "closed by the server");
    drop(resp);
    stop.cancel();
}

#[tokio::test]
async fn a_request_outliving_the_idle_bound_keeps_its_connection() {
    let (k, c) = serving("api.test");
    let idle = Duration::from_millis(200);
    let limits = Limits {
        idle_timeout: idle,
        ..Limits::default()
    };
    // The deadline elapses while the request is open and races `notified()` when
    // it resolves; repeat so an unbiased select would lose at least once.
    for _ in 0..12 {
        let fake = Fake::new(Next::Slow(idle * 2));
        let (addr, _tx, stop) = spawn_api_with(fake, Some(Arc::clone(&k)), limits).await;
        let (mut send, conn) = h2_client(addr, &c).await;
        let resp = send.send_request(h2_post(ALICE)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = axum::body::to_bytes(Body::new(resp.into_body()), 1 << 20)
            .await
            .unwrap();
        assert!(!body.is_empty());
        // A fresh idle bound starts at the response, not an already elapsed one.
        tokio::time::sleep(idle / 2).await;
        assert!(!conn.is_finished(), "closed right after the response");
        // And the bound still applies afterwards.
        tokio::time::timeout(Duration::from_secs(3), conn)
            .await
            .expect("idle again, so closed")
            .unwrap();
        stop.cancel();
    }
}

/// A flood of refusals logs at most `PER_SECOND` lines a second, and the next
/// logged line reports how many were dropped.
#[test]
fn refusal_log_is_sampled() {
    use super::api::LogSampler;
    let s = LogSampler::default();
    let t = std::time::Instant::now();
    let logged = (0..1000).filter(|_| s.admit(t).is_some()).count();
    assert_eq!(logged, LogSampler::PER_SECOND as usize);
    let later = t + Duration::from_secs(1);
    assert_eq!(
        s.admit(later),
        Some(1000 - u64::from(LogSampler::PER_SECOND))
    );
    assert_eq!(s.admit(later), Some(0));
}

/// A PROXY v2 header over TCP/IPv4 from `src` to 10.0.0.1:8443.
fn proxy_header(src: std::net::SocketAddrV4) -> Vec<u8> {
    let mut h = b"\r\n\r\n\0\r\nQUIT\n".to_vec();
    h.extend_from_slice(&[0x21, 0x11, 0, 12]);
    h.extend_from_slice(&src.ip().octets());
    h.extend_from_slice(&[10, 0, 0, 1]);
    h.extend_from_slice(&src.port().to_be_bytes());
    h.extend_from_slice(&8443u16.to_be_bytes());
    h
}

/// The API listener with PROXY protocol trusted from `cidrs`, serving
/// `GET /peer`, which answers with the [`super::api::Peer`] the handlers see.
async fn spawn_proxied(
    cidrs: &[&str],
    limits: Limits,
) -> (
    std::net::SocketAddr,
    CertificateDer<'static>,
    crate::net::ProxyMetrics,
    CancellationToken,
) {
    let (k, c) = serving("api.test");
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let (tx, rx) = watch::channel(Some(k));
    std::mem::forget(tx);
    let metrics = crate::net::ProxyMetrics::register(&Metrics::new(), "api");
    let proxy = crate::net::ProxyProtocol::trusting(
        cidrs.iter().map(|c| c.parse().unwrap()).collect(),
        Duration::from_secs(2),
        metrics.clone(),
    );
    let app = Router::new().route(
        "/peer",
        axum::routing::get(
            |axum::Extension(p): axum::Extension<super::api::Peer>| async move { p.0.to_string() },
        ),
    );
    let stop = CancellationToken::new();
    tokio::spawn(super::serve_with_proxy(
        l,
        app,
        rx,
        limits,
        proxy,
        stop.clone(),
    ));
    (addr, c, metrics, stop)
}

/// TLS (HTTP/1.1) to `addr`, after writing `prefix` on the bare TCP stream.
async fn dial_prefixed(
    addr: std::net::SocketAddr,
    trust: &CertificateDer<'static>,
    prefix: &[u8],
) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(trust.clone()).unwrap();
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    let mut tcp = TcpStream::connect(addr).await?;
    tcp.write_all(prefix).await?;
    let tls = tokio_rustls::TlsConnector::from(Arc::new(cfg))
        .connect(ServerName::try_from("api.test").unwrap(), tcp);
    tokio::time::timeout(Duration::from_secs(5), tls)
        .await
        .map_err(|_| std::io::Error::other("TLS handshake timed out"))?
}

/// `GET /peer` on an open connection.
async fn peer_seen(s: &mut tokio_rustls::client::TlsStream<TcpStream>) -> String {
    s.write_all(b"GET /peer HTTP/1.1\r\nhost: api.test\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    out.split("\r\n\r\n").nth(1).unwrap().to_owned()
}

/// Behind a trusted gateway the handlers see the PROXY source, not the gateway.
#[tokio::test]
async fn proxied_connection_is_the_header_source() {
    let (addr, c, m, stop) = spawn_proxied(&["127.0.0.0/8"], Limits::default()).await;
    let src = "192.0.2.7:51000".parse().unwrap();
    let mut s = dial_prefixed(addr, &c, &proxy_header(src)).await.unwrap();
    assert_eq!(peer_seen(&mut s).await, "192.0.2.7:51000");
    assert_eq!(m.count("proxied"), 1);
    stop.cancel();
}

/// `max_connections_per_ip` counts PROXY sources: two clients behind one
/// gateway address each get their own slot, and one client is still capped.
#[tokio::test]
async fn per_address_cap_counts_the_proxy_source() {
    let limits = Limits {
        max_connections_per_ip: 1,
        ..Limits::default()
    };
    let (addr, c, _m, stop) = spawn_proxied(&["127.0.0.0/8"], limits).await;
    let a = "192.0.2.7:1000".parse().unwrap();
    let b = "192.0.2.8:1000".parse().unwrap();
    let _held = dial_prefixed(addr, &c, &proxy_header(a)).await.unwrap();
    let mut other = dial_prefixed(addr, &c, &proxy_header(b))
        .await
        .expect("another client behind the same gateway address gets a slot");
    assert_eq!(peer_seen(&mut other).await, "192.0.2.8:1000");
    let again = "192.0.2.7:2000".parse().unwrap();
    assert!(
        dial_prefixed(addr, &c, &proxy_header(again)).await.is_err(),
        "the first client's second connection is over its cap"
    );
    stop.cancel();
}

/// A trusted source that sends no header is closed, never served as itself.
#[tokio::test]
async fn trusted_source_without_a_header_is_closed() {
    let (addr, c, m, stop) = spawn_proxied(&["127.0.0.0/8"], Limits::default()).await;
    assert!(dial_prefixed(addr, &c, &[]).await.is_err());
    // The ClientHello fails the signature check.
    tokio::time::timeout(Duration::from_secs(3), async {
        while m.count("bad_signature") == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    stop.cancel();
}

/// An untrusted source is direct: its forged header goes to TLS and fails the
/// handshake, and without one it is served as its own address.
#[tokio::test]
async fn untrusted_source_cannot_forge_its_address() {
    let (addr, c, m, stop) = spawn_proxied(&["10.0.0.0/8"], Limits::default()).await;
    let forged = proxy_header("203.0.113.9:1".parse().unwrap());
    assert!(dial_prefixed(addr, &c, &forged).await.is_err());
    let mut s = dial_prefixed(addr, &c, &[]).await.unwrap();
    let seen = peer_seen(&mut s).await;
    assert!(seen.starts_with("127.0.0.1:"), "{seen}");
    for result in ["proxied", "bad_signature"] {
        assert_eq!(m.count(result), 0, "{result}");
    }
    stop.cancel();
}

/// The HTTP API never generates a private key: a request without
/// `publicKey` is a 400 before the allocator runs.
#[tokio::test]
async fn a_request_without_public_key_is_400_and_never_allocates() {
    let fake = Fake::new(Next::Bound);
    let rejected = Arc::clone(&fake.rejected);
    let state = state(fake, &Metrics::new());
    let app = router(Arc::clone(&state));
    for body in [
        r#"{"namespace":"render-verify","pool":"a"}"#,
        r#"{"namespace":"render-verify","pool":"a","publicKey":null}"#,
    ] {
        let (st, h, body) = call(&app, post(Some(ALICE), "application/json", body)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(h.get(header::RETRY_AFTER), None);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v,
            json!({"error": {"code": "invalid_request", "message": super::api::PUBLIC_KEY_REQUIRED}})
        );
    }
    assert!(state.alloc.seen.lock().unwrap().is_empty(), "allocator ran");
    assert_eq!(
        *rejected.lock().unwrap(),
        [ErrorCode::InvalidRequest, ErrorCode::InvalidRequest]
    );
    // An unauthenticated request still gets 401, not the body error.
    let (st, _, _) = call(
        &app,
        post(
            None,
            "application/json",
            r#"{"namespace":"render-verify","pool":"a"}"#,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

/// A POST whose claim is still waiting after the create wait is a 202 with
/// the id and the place in the queue.
#[tokio::test]
async fn pending_allocation_is_202_with_id_and_position() {
    let app = router(state(Fake::new(Next::Pending), &Metrics::new()));
    let (st, h, body) = call(&app, post(Some(ALICE), "application/json", OK_BODY)).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    assert_eq!(h[header::CACHE_CONTROL], "no-store");
    assert_eq!(h[header::CONTENT_TYPE], "application/json");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v, json!({"id": ID, "state": "pending", "position": 3}));
}

/// `GET /v1/allocations/{id}`: 200 bound, 202 pending, the error of a failed
/// allocation; `wait` reaches the allocator capped at 25 s.
#[tokio::test]
async fn get_answers_bound_pending_and_failed() {
    let req: CreateAllocationRequest =
        serde_json::from_value(json!({"namespace": NS, "pool": "a"})).unwrap();
    let bound = Outcome::Bound(Box::new(response(&req, "sa:render-verify/alice".into())));
    let path = format!("/v1/allocations/{ID}");
    for (r, query, want, wait) in [
        (Ok(bound), "", StatusCode::OK, 0),
        (Ok(pending(1)), "?wait=25s", StatusCode::ACCEPTED, 25),
        (Ok(pending(1)), "?wait=1m", StatusCode::ACCEPTED, 25),
        (Ok(pending(1)), "?x=1&wait=3s", StatusCode::ACCEPTED, 3),
        (
            Err(AllocError::PoolExhausted("no worker became free".into())),
            "?wait=25s",
            StatusCode::SERVICE_UNAVAILABLE,
            25,
        ),
        (Err(AllocError::Forbidden), "", StatusCode::FORBIDDEN, 0),
    ] {
        let fake = Fake::new(Next::Bound);
        *fake.get.lock().unwrap() = r.clone();
        let s = state(fake, &Metrics::new());
        let app = router(Arc::clone(&s));
        let (st, h, body) = call(&app, get(&format!("{path}{query}"), ALICE)).await;
        assert_eq!(st, want, "{query}: {body}");
        assert_eq!(*s.alloc.waits.lock().unwrap(), [Duration::from_secs(wait)]);
        assert_eq!(*s.alloc.seen.lock().unwrap(), ["sa:render-verify/alice"]);
        match r {
            Ok(Outcome::Bound(b)) => {
                assert_eq!(
                    serde_json::from_str::<AllocationResponse>(&body).unwrap(),
                    *b
                );
            }
            Ok(Outcome::Pending(p)) => {
                assert_eq!(serde_json::from_str::<PendingAllocation>(&body).unwrap(), p);
            }
            Err(e) => {
                let v: Value = serde_json::from_str(&body).unwrap();
                assert_eq!(v["error"]["code"], e.code().as_str());
                if e.code() == ErrorCode::PoolExhausted {
                    assert_eq!(h[header::RETRY_AFTER], "5");
                }
            }
        }
    }
    // A wait that is not a duration is refused before the allocator runs.
    let s = state(Fake::new(Next::Bound), &Metrics::new());
    let app = router(Arc::clone(&s));
    let (st, _, body) = call(&app, get(&format!("{path}?wait=soon"), ALICE)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(s.alloc.waits.lock().unwrap().is_empty());
    // Unauthenticated first.
    let req = Request::get(&path).body(Body::empty()).unwrap();
    assert_eq!(call(&app, req).await.0, StatusCode::UNAUTHORIZED);
    assert!(s.alloc.seen.lock().unwrap().is_empty());
}

/// A release that meets a transient API server failure is a 503 the clients
/// retry: it carries `Retry-After`.
#[tokio::test]
async fn release_upstream_failure_is_503_with_retry_after() {
    let fake = Fake::new(Next::Bound);
    *fake.release.lock().unwrap() = Err(AllocError::UpstreamError);
    let app = router(state(fake, &Metrics::new()));
    let req = Request::delete(format!("/v1/allocations/{ID}"))
        .header(header::AUTHORIZATION, format!("Bearer {ALICE}"))
        .body(Body::empty())
        .unwrap();
    let (st, h, body) = call(&app, req).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(h[header::RETRY_AFTER], "2");
    assert!(body.contains(r#""code":"upstream_error""#), "{body}");
}
