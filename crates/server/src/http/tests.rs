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
use buildkit_controller_api::dto::{AllocationResponse, CreateAllocationRequest, ErrorCode};
use buildkit_controller_api::v1alpha1::BuildKitPool;
use futures::future::BoxFuture;
use http_body::Body as _;
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

use super::api::{Allocator, ApiState, EdgeInfo, Grant, router};
use super::body::CommitBody;
use super::listener::Limits;
use super::metrics::HttpMetrics;
use crate::alloc::backend::{Backend, CreateOutcome, PatchOutcome};
use crate::alloc::metrics::AllocMetrics;
use crate::alloc::queue::{MAX_WAITERS, Queues};
use crate::alloc::{AllocError, AllocService, CertSource, Engine, EngineConfig};
use crate::auth::{
    AuthConfig, Authenticator, Identity, OidcIssuer, ReviewError, TokenReviewer, https_client,
};
use crate::pki::{CaBundle, UserServingCert};
use crate::render::fixtures;
use crate::runtime::metrics::Metrics;
use crate::runtime::stores::PodSubscription;

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

const PENDING: u8 = 0;
const COMMITTED: u8 = 1;
const RETIRED: u8 = 2;

struct FakeGrant(AllocationResponse, Arc<AtomicU8>);

impl Grant for FakeGrant {
    fn response(&self) -> &AllocationResponse {
        &self.0
    }
    fn commit(self) {
        self.1.store(COMMITTED, Ordering::SeqCst);
    }
}

impl Drop for FakeGrant {
    fn drop(&mut self) {
        let _ = self
            .1
            .compare_exchange(PENDING, RETIRED, Ordering::SeqCst, Ordering::SeqCst);
    }
}

enum Next {
    Grant,
    Fail(AllocError),
    /// Never resolves; signals `started`, marks `cancelled` when dropped.
    Hang,
    /// Grants after the delay.
    Slow(Duration),
}

struct Fake {
    next: Mutex<Option<Next>>,
    release: Mutex<Result<(), AllocError>>,
    grant: Arc<AtomicU8>,
    started: Arc<Notify>,
    cancelled: Arc<AtomicU8>,
    seen: Mutex<Vec<String>>,
    rejected: Arc<Mutex<Vec<ErrorCode>>>,
}

impl Fake {
    fn new(next: Next) -> Self {
        Self {
            next: Mutex::new(Some(next)),
            release: Mutex::new(Ok(())),
            grant: Arc::new(AtomicU8::new(PENDING)),
            started: Arc::new(Notify::new()),
            cancelled: Arc::new(AtomicU8::new(0)),
            seen: Mutex::new(Vec::new()),
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

fn response(req: &CreateAllocationRequest, owner: String) -> AllocationResponse {
    AllocationResponse {
        id: "k3q7m2x4b5n6p7r2s3t4v5w6y7".into(),
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

impl Allocator for Fake {
    type Grant = FakeGrant;
    fn allocate<'a>(
        &'a self,
        who: &'a Identity,
        req: &'a CreateAllocationRequest,
    ) -> BoxFuture<'a, Result<FakeGrant, AllocError>> {
        self.seen.lock().unwrap().push(who.owner());
        let next = self.next.lock().unwrap().take().unwrap_or(Next::Grant);
        Box::pin(async move {
            match next {
                Next::Grant => Ok(FakeGrant(
                    response(req, who.owner()),
                    Arc::clone(&self.grant),
                )),
                Next::Fail(e) => Err(e),
                Next::Slow(d) => {
                    tokio::time::sleep(d).await;
                    Ok(FakeGrant(
                        response(req, who.owner()),
                        Arc::clone(&self.grant),
                    ))
                }
                Next::Hang => {
                    let _guard = SetOnDrop(Arc::clone(&self.cancelled));
                    self.started.notify_one();
                    std::future::pending().await
                }
            }
        })
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
    for p in [
        pool("b", NS, json!({"serviceAccount": {}})),
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

fn state<A: Allocator>(alloc: A, metrics: &Metrics) -> Arc<ApiState<A>> {
    let auth = Authenticator::new(
        AuthConfig {
            oidc: vec![OidcIssuer {
                url: ISSUER.into(),
                audiences: vec!["buildkit-controller".into()],
                jwks_url: None,
                ca_certs: Vec::new(),
            }],
            service_account_audiences: Some(vec!["buildkit-controller".into()]),
        },
        https_client(rustls::RootCertStore::empty()).unwrap(),
        Reviewer,
    )
    .unwrap();
    Arc::new(ApiState {
        alloc,
        auth: Arc::new(SaJwt(auth)),
        oidc_issuers: HashSet::from([ISSUER.to_owned()]),
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
    let app = router(state(Fake::new(Next::Grant), &metrics));
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

#[tokio::test]
async fn forged_or_unsigned_jwt_is_401() {
    let fake = Fake::new(Next::Grant);
    let grant = Arc::clone(&fake.grant);
    let app = router(state(fake, &Metrics::new()));
    let claims = b64(
        &json!({"iss": ISSUER, "sub": "x", "aud": "buildkit-controller", "exp": 4_102_444_800_u64}),
    );
    let unsigned = format!("{}.{claims}.", b64(&json!({"alg": "none", "typ": "JWT"})));
    let forged = format!(
        "{}.{claims}.c2lnbmF0dXJl",
        b64(&json!({"alg": "RS256", "kid": "k"}))
    );
    let hs = format!("{}.{claims}.c2lnbmF0dXJl", b64(&json!({"alg": "HS256"})));
    for t in [unsigned, forged, hs] {
        let (st, h, _) = call(&app, post(Some(&t), "application/json", OK_BODY)).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        assert_eq!(h[header::WWW_AUTHENTICATE], "Bearer");
    }
    assert_eq!(
        grant.load(Ordering::SeqCst),
        PENDING,
        "allocator never reached"
    );
}

struct NoBackend;
impl Backend for NoBackend {
    fn patch_pod(&self, _: &str, _: &str, _: Value) -> BoxFuture<'static, PatchOutcome> {
        Box::pin(async { PatchOutcome::Unknown })
    }
    fn create_pod(
        &self,
        _: &str,
        _: k8s_openapi::api::core::v1::Pod,
    ) -> BoxFuture<'static, CreateOutcome> {
        Box::pin(async { CreateOutcome::Exists })
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
    fn apply_waiters(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: i32,
        _: &str,
    ) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn get_pvc(
        &self,
        _: &str,
        _: &str,
    ) -> BoxFuture<'static, Result<Option<k8s_openapi::api::core::v1::PersistentVolumeClaim>, String>>
    {
        Box::pin(async { Ok(None) })
    }
    fn create_pvc(
        &self,
        _: &str,
        _: k8s_openapi::api::core::v1::PersistentVolumeClaim,
    ) -> BoxFuture<'static, crate::alloc::backend::PvcCreate> {
        Box::pin(async { crate::alloc::backend::PvcCreate::Created })
    }
    fn delete_pvc(&self, _: &str, _: &str, _: &str) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn annotate_pvc(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
    ) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
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

fn engine() -> Engine {
    let (pods, _) = reflector::store();
    let (tx, _) = broadcast::channel(4);
    let m = AllocMetrics::default();
    Engine::new(
        Arc::new(NoBackend),
        pools(),
        pods,
        Box::new(move || PodSubscription::new(tx.subscribe())),
        Arc::new(NoCa),
        EngineConfig {
            endpoint: "tcp://bk.example.com:8444".into(),
            server_name: "bk.example.com".into(),
            allocation_domain: None,
        },
        m.clone(),
        Arc::new(Queues::new(MAX_WAITERS, m)),
    )
}

#[tokio::test]
async fn no_access_is_identical_to_a_missing_pool() {
    let app = router(state(engine(), &Metrics::new()));
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
            r#"{"namespace":"render-verify","pool":"zz"}"#,
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
    let mut s = Arc::into_inner(state(Fake::new(Next::Grant), &metrics)).unwrap();
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
        AllocError::StartTimeout,
        AllocError::PodLost,
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
        let want = matches!(code, ErrorCode::PoolExhausted | ErrorCode::StartTimeout)
            .then(|| "5".to_owned());
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
        let fake = Fake::new(Next::Grant);
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
    let app = router(state(Fake::new(Next::Grant), &Metrics::new()));
    let req = Request::delete("/v1/allocations/x")
        .body(Body::empty())
        .unwrap();
    assert_eq!(call(&app, req).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn body_rules_415_413_400_then_201() {
    let app = router(state(Fake::new(Next::Grant), &Metrics::new()));
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
async fn grant_commits_only_after_the_body_is_consumed() {
    let fake = Fake::new(Next::Grant);
    let grant = Arc::clone(&fake.grant);
    let app = router(state(fake, &Metrics::new()));
    let r = app
        .clone()
        .oneshot(post(Some(ALICE), "application/json", OK_BODY))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(
        grant.load(Ordering::SeqCst),
        PENDING,
        "head sent, body not yet written"
    );
    let _ = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    assert_eq!(grant.load(Ordering::SeqCst), COMMITTED);

    // The client drops after the data frame but before the end, so the allocation retires and does not commit.
    let state = Arc::new(AtomicU8::new(PENDING));
    let req = CreateAllocationRequest {
        namespace: "n".into(),
        pool: "p".into(),
        ttl_seconds: None,
        queue_timeout_seconds: None,
        public_key: None,
        cache_key: None,
        cache: None,
    };
    let mut body = CommitBody::new(
        "{}".into(),
        FakeGrant(response(&req, "o".into()), Arc::clone(&state)),
    );
    assert!(!body.is_end_stream());
    let waker = futures::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    assert!(matches!(
        std::pin::Pin::new(&mut body).poll_frame(&mut cx),
        std::task::Poll::Ready(Some(Ok(_)))
    ));
    drop(body);
    assert_eq!(state.load(Ordering::SeqCst), RETIRED);
}

#[tokio::test]
async fn pools_lists_accessible_pools_sorted() {
    let app = router(state(Fake::new(Next::Grant), &Metrics::new()));
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
            "endpoint": "tcp://bk.example.com:8444", "serverName": "bk.example.com"})
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
    let app = router(state(Fake::new(Next::Grant), &metrics));
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
    let (addr, tx, stop) = spawn_api(Fake::new(Next::Grant), None).await;
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
/// was open before gets 503 `shutting_down` with `Retry-After` and
/// `Connection: close`, not a reset, and never reaches the allocator. New
/// connections are refused.
#[tokio::test]
async fn shutdown_answers_keep_alive_allocations_with_503() {
    let (k, c) = serving("api.test");
    let fake = Fake::new(Next::Grant);
    let rejected = Arc::clone(&fake.rejected);
    let grant = Arc::clone(&fake.grant);
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
    assert!(out.starts_with("HTTP/1.1 503"), "{out}");
    assert!(out.contains("retry-after: 1\r\n"), "{out}");
    assert!(out.contains("connection: close\r\n"), "{out}");
    assert!(out.contains(r#""code":"shutting_down""#), "{out}");
    assert_eq!(*rejected.lock().unwrap(), [ErrorCode::ShuttingDown]);
    assert_eq!(grant.load(Ordering::SeqCst), PENDING, "no grant was made");
}

#[tokio::test]
async fn full_201_over_tls_commits() {
    let (k, c) = serving("api.test");
    let fake = Fake::new(Next::Grant);
    let grant = Arc::clone(&fake.grant);
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
    assert_eq!(grant.load(Ordering::SeqCst), COMMITTED);
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
    let app = router(state(Fake::new(Next::Grant), &Metrics::new()));
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
    let (addr, _tx, stop) = spawn_api_with(Fake::new(Next::Grant), Some(k), limits).await;
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
    let (addr, _tx, stop) = spawn_api_with(Fake::new(Next::Grant), Some(k), limits).await;
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
    let (addr, _tx, stop) = spawn_api_with(Fake::new(Next::Grant), Some(k), limits).await;
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
    let (addr, _tx, stop) = spawn_api_with(Fake::new(Next::Grant), Some(k), limits).await;
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
    let (addr, _tx, stop) = spawn_api_with(Fake::new(Next::Grant), Some(k), limits).await;
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

impl crate::reconcile::spawn::WorkerSecrets for NoBackend {
    fn create_worker_secret(
        &self,
        _: &k8s_openapi::api::core::v1::Secret,
    ) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async { Err("no backend".into()) })
    }
    fn reown_worker_secret(
        &self,
        _: &str,
        _: &str,
        _: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
    ) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async { Err("no backend".into()) })
    }
    fn delete_worker_secret(&self, _: &str, _: &str) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async { Err("no backend".into()) })
    }
}
