//! Routes, authentication, error bodies.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{MatchedPath, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use buildkit_controller_api::dto::{
    AllocationResponse, CreateAllocationRequest, ErrorBody, ErrorCode, MAX_BODY_BYTES, PoolList,
    PoolSummary,
};
use buildkit_controller_api::v1alpha1::BuildKitPool;
use futures::future::BoxFuture;
use kube::runtime::reflector::Store;
use serde_json::{Map, Value};
use tracing::{info, warn};

/// Upload bound for the 16 KiB body, so a trickling client cannot hold a request open.
const BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// `Retry-After` on a 503 from authentication (`TokenReview` busy, throttled or timed out).
const AUTH_RETRY_AFTER_SECS: u64 = 2;

use super::body::CommitBody;
use super::listener::Stopping;
use super::metrics::HttpMetrics;
use crate::alloc::engine::access_rules;
use crate::alloc::{AllocError, AllocService, Allocation, Engine};
use crate::auth::{AuthError, Authenticator, Identity, TokenReviewer, bearer_token};

/// An allocation that retires its pod unless committed.
pub trait Grant: Send + Unpin + 'static {
    /// The 201 body.
    fn response(&self) -> &AllocationResponse;
    /// Disarm the orphan guard because the client has the response.
    fn commit(self);
}

impl Grant for Allocation {
    fn response(&self) -> &AllocationResponse {
        Allocation::response(self)
    }
    fn commit(self) {
        let _ = Allocation::commit(self);
    }
}

/// The allocation service as the API sees it.
pub trait Allocator: Send + Sync + 'static {
    /// What a successful allocate returns.
    type Grant: Grant;
    /// POST, after authentication.
    fn allocate<'a>(
        &'a self,
        who: &'a Identity,
        req: &'a CreateAllocationRequest,
    ) -> BoxFuture<'a, Result<Self::Grant, AllocError>>;
    /// Count a POST refused before [`Allocator::allocate`] in `buildkit_allocations_total`.
    /// Labels are always empty because those refusals happen before the body parsed (or
    /// before the caller authenticated), and request strings never become labels.
    fn rejected(&self, _code: ErrorCode) {}
    /// DELETE, after authentication.
    fn release<'a>(
        &'a self,
        who: &'a Identity,
        id: &'a str,
    ) -> BoxFuture<'a, Result<(), AllocError>>;
}

impl Allocator for AllocService {
    type Grant = Allocation;
    fn rejected(&self, code: ErrorCode) {
        self.count_rejection(code);
    }
    fn allocate<'a>(
        &'a self,
        who: &'a Identity,
        req: &'a CreateAllocationRequest,
    ) -> BoxFuture<'a, Result<Allocation, AllocError>> {
        Box::pin(AllocService::allocate(self, who, req))
    }
    fn release<'a>(
        &'a self,
        who: &'a Identity,
        id: &'a str,
    ) -> BoxFuture<'a, Result<(), AllocError>> {
        Box::pin(AllocService::release(self, who, id))
    }
}

impl Allocator for Engine {
    type Grant = Allocation;
    fn allocate<'a>(
        &'a self,
        who: &'a Identity,
        req: &'a CreateAllocationRequest,
    ) -> BoxFuture<'a, Result<Allocation, AllocError>> {
        Box::pin(Engine::allocate(self, who, req))
    }
    fn release<'a>(
        &'a self,
        who: &'a Identity,
        id: &'a str,
    ) -> BoxFuture<'a, Result<(), AllocError>> {
        Box::pin(Engine::release(self, who, id))
    }
}

/// Token verification as the API sees it.
pub trait Authenticate: Send + Sync + 'static {
    /// Authentication steps 1 to 3 for a request from `peer`.
    fn authenticate<'a>(
        &'a self,
        token: &'a str,
        peer: Option<std::net::IpAddr>,
    ) -> BoxFuture<'a, Result<Identity, AuthError>>;
}

impl<R: TokenReviewer + Send + Sync + 'static> Authenticate for Authenticator<R> {
    fn authenticate<'a>(
        &'a self,
        token: &'a str,
        peer: Option<std::net::IpAddr>,
    ) -> BoxFuture<'a, Result<Identity, AuthError>> {
        Box::pin(Authenticator::authenticate_from(self, token, peer))
    }
}

/// `edge.*` values echoed by the API.
#[derive(Clone, Debug)]
pub struct EdgeInfo {
    /// `edge.endpoint`.
    pub endpoint: String,
    /// `edge.serverName`.
    pub server_name: String,
    /// `edge.allocationDomain` is set.
    pub per_allocation_endpoints: bool,
}

/// Shared request state.
pub struct ApiState<A> {
    /// Allocation service.
    pub alloc: A,
    /// Authenticator.
    pub auth: Arc<dyn Authenticate>,
    /// Configured OIDC issuer urls (for the `method` label only).
    pub oidc_issuers: HashSet<String>,
    /// `auth.serviceAccount.enabled`.
    pub service_accounts: bool,
    /// The Pool store (watched scope).
    pub pools: Store<BuildKitPool>,
    /// Edge values.
    pub edge: EdgeInfo,
    /// Metrics.
    pub metrics: HttpMetrics,
    /// Bounds the per-request authentication refusal log.
    pub refusals: LogSampler,
}

/// Lets [`LogSampler::PER_SECOND`] events a second through to the log and counts
/// the rest, so a flood of bad tokens cannot flood the log. `buildkit_auth_total`
/// still counts every refusal.
#[derive(Default)]
pub struct LogSampler {
    /// Window start, lines logged in it, events suppressed since the last line.
    state: std::sync::Mutex<(Option<Instant>, u32, u64)>,
}

impl LogSampler {
    /// Lines logged per one-second window.
    pub const PER_SECOND: u32 = 10;

    /// `Some(suppressed)` when this event may be logged, carrying the number of
    /// events suppressed since the previous logged one; `None` to drop it.
    pub fn admit(&self, now: Instant) -> Option<u64> {
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if s.0
            .is_none_or(|t| now.duration_since(t) >= std::time::Duration::from_secs(1))
        {
            s.0 = Some(now);
            s.1 = 0;
        }
        if s.1 >= Self::PER_SECOND {
            s.2 += 1;
            return None;
        }
        s.1 += 1;
        Some(std::mem::take(&mut s.2))
    }
}

/// Remote address of the connection, inserted by the listener.
#[derive(Clone, Copy, Debug)]
pub struct Peer(pub SocketAddr);

/// A non-2xx answer with its API error body.
#[derive(Debug)]
pub struct ApiError {
    code: ErrorCode,
    message: String,
    /// Overrides the code's own `Retry-After`.
    retry_after: Option<u64>,
}

impl ApiError {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retry_after: None,
        }
    }
}

impl From<AllocError> for ApiError {
    fn from(e: AllocError) -> Self {
        Self::new(e.code(), e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.code.http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = serde_json::to_vec(&ErrorBody::new(self.code, self.message)).unwrap_or_default();
        let mut r = (status, body).into_response();
        let h = r.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        if self.code == ErrorCode::Unauthenticated {
            h.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        if let Some(s) = self.retry_after.or(self.code.retry_after_seconds()) {
            h.insert(header::RETRY_AFTER, HeaderValue::from(s));
        }
        r
    }
}

/// The `/v1` router; any other path or method is 404 `not_found`.
pub fn router<A: Allocator>(state: Arc<ApiState<A>>) -> Router {
    Router::new()
        .route("/v1/allocations", post(create::<A>).fallback(not_found))
        .route(
            "/v1/allocations/{id}",
            delete(release::<A>).fallback(not_found),
        )
        .route("/v1/pools", get(pools::<A>).fallback(not_found))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            observe::<A>,
        ))
        .with_state(state)
}

async fn not_found() -> ApiError {
    ApiError::new(ErrorCode::NotFound, "not found")
}

async fn observe<A: Allocator>(
    State(s): State<Arc<ApiState<A>>>,
    req: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let route = match req
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
    {
        Some("/v1/allocations") => "/v1/allocations",
        Some("/v1/allocations/{id}") => "/v1/allocations/{id}",
        Some("/v1/pools") => "/v1/pools",
        _ => "other",
    };
    let r = next.run(req).await;
    s.metrics
        .request(route, r.status().as_u16(), started.elapsed().as_secs_f64());
    r
}

fn peer_addr(req: &Request) -> Option<SocketAddr> {
    req.extensions().get::<Peer>().map(|p| p.0)
}

fn peer(req: &Request) -> String {
    peer_addr(req).map_or_else(String::new, |p| p.to_string())
}

/// `oidc` when `iss` names a configured issuer or service accounts are off,
/// else `serviceaccount` (mirrors the dispatch in [`Authenticator`]).
fn method(s: &ApiState<impl Allocator>, token: &str) -> &'static str {
    let iss = jsonwebtoken::dangerous::insecure_decode_claims::<Map<String, Value>>(token)
        .ok()
        .and_then(|c| c.get("iss").and_then(Value::as_str).map(str::to_owned));
    if iss.is_some_and(|i| s.oidc_issuers.contains(&i)) || !s.service_accounts {
        "oidc"
    } else {
        "serviceaccount"
    }
}

async fn authenticate<A: Allocator>(
    s: &ApiState<A>,
    headers: &HeaderMap,
    addr: Option<SocketAddr>,
) -> Result<Identity, ApiError> {
    let peer = addr.map_or_else(String::new, |p| p.to_string());
    let peer = peer.as_str();
    let header = headers
        .get(header::AUTHORIZATION)
        .map(|v| v.to_str().unwrap_or(""));
    let r = match bearer_token(header) {
        Ok(token) => {
            let method = method(s, token);
            let r = s.auth.authenticate(token, addr.map(|a| a.ip())).await;
            s.metrics.auth(
                method,
                r.as_ref().map_or_else(AuthError::metric_result, |_| "ok"),
            );
            r
        }
        Err(e) => Err(e),
    };
    r.map_err(|e| {
        let code = match e.status() {
            401 => ErrorCode::Unauthenticated,
            503 => ErrorCode::UpstreamError,
            _ => ErrorCode::Internal,
        };
        if let Some(suppressed) = s.refusals.admit(Instant::now()) {
            if code == ErrorCode::Unauthenticated {
                info!(
                    peer,
                    result = "unauthenticated",
                    reason = e.metric_result(),
                    suppressed,
                    "request refused"
                );
            } else {
                warn!(peer, result = code.as_str(), error = %e, suppressed, "authentication failed");
            }
        }
        let message = match code {
            ErrorCode::Unauthenticated => "missing or invalid bearer token",
            ErrorCode::UpstreamError => "token review unavailable",
            _ => "internal error",
        };
        let mut e = ApiError::new(code, message);
        if code == ErrorCode::UpstreamError {
            e.retry_after = Some(AUTH_RETRY_AFTER_SECS);
        }
        e
    })
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"))
}

async fn read_body(headers: &HeaderMap, body: Body) -> Result<CreateAllocationRequest, ApiError> {
    if !is_json(headers) {
        return Err(ApiError::new(
            ErrorCode::UnsupportedMediaType,
            "Content-Type must be application/json",
        ));
    }
    let too_large = || ApiError::new(ErrorCode::PayloadTooLarge, "request body over 16 KiB");
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_BODY_BYTES as u64) {
        return Err(too_large());
    }
    let bytes = tokio::time::timeout(BODY_TIMEOUT, axum::body::to_bytes(body, MAX_BODY_BYTES))
        .await
        .map_err(|_| {
            ApiError::new(
                ErrorCode::InvalidRequest,
                "request body not received in time",
            )
        })?
        .map_err(|_| too_large())?;
    serde_json::from_slice(&bytes).map_err(|e| {
        ApiError::new(
            ErrorCode::InvalidRequest,
            format!("invalid request body: {e}"),
        )
    })
}

async fn create<A: Allocator>(State(s): State<Arc<ApiState<A>>>, req: Request) -> Response {
    // A stopping replica takes no new allocation; the engine refuses queued ones.
    if req
        .extensions()
        .get::<Stopping>()
        .is_some_and(|s| s.0.is_cancelled())
    {
        s.alloc.rejected(ErrorCode::ShuttingDown);
        return ApiError::from(AllocError::ShuttingDown).into_response();
    }
    let addr = peer_addr(&req);
    let peer = peer(&req);
    let (parts, body) = req.into_parts();
    let pre = async {
        let who = authenticate(&s, &parts.headers, addr).await?;
        Ok::<_, ApiError>((who, read_body(&parts.headers, body).await?))
    }
    .await;
    let (who, body) = match pre {
        Ok(v) => v,
        Err(e) => {
            s.alloc.rejected(e.code);
            return e.into_response();
        }
    };
    let r = s.alloc.allocate(&who, &body).await.map_err(ApiError::from);
    match r {
        Ok(grant) => match serde_json::to_vec(grant.response()) {
            Ok(json) => {
                let mut res = Response::new(Body::new(CommitBody::new(json.into(), grant)));
                *res.status_mut() = StatusCode::CREATED;
                let h = res.headers_mut();
                h.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                res
            }
            Err(e) => {
                warn!(peer, error = %e, "serializing the allocation");
                ApiError::new(ErrorCode::Internal, "internal error").into_response()
            }
        },
        Err(e) => e.into_response(),
    }
}

async fn release<A: Allocator>(
    State(s): State<Arc<ApiState<A>>>,
    id: Result<Path<String>, PathRejection>,
    req: Request,
) -> Result<StatusCode, ApiError> {
    let who = authenticate(&s, req.headers(), peer_addr(&req)).await?;
    // An undecodable id (bad percent-encoding) names no allocation.
    let Ok(Path(id)) = id else {
        return Err(AllocError::NotFound.into());
    };
    s.alloc.release(&who, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn summary(p: &BuildKitPool, edge: &EdgeInfo) -> Option<PoolSummary> {
    let st = p.status.clone().unwrap_or_default();
    Some(PoolSummary {
        namespace: p.metadata.namespace.clone()?,
        name: p.metadata.name.clone()?,
        ready: st
            .conditions
            .iter()
            .any(|c| c.type_ == "Ready" && c.status == "True"),
        warm: st.warm.unwrap_or(0),
        starting: st.starting.unwrap_or(0),
        allocated: st.allocated.unwrap_or(0),
        waiting: st.waiting.unwrap_or(0),
        min: p.spec.scaling.min,
        max: p.spec.scaling.max,
        platforms: st.platforms,
        endpoint: edge.endpoint.clone(),
        server_name: edge.server_name.clone(),
    })
}

async fn pools<A: Allocator>(
    State(s): State<Arc<ApiState<A>>>,
    req: Request,
) -> Result<Response, ApiError> {
    let who = authenticate(&s, req.headers(), peer_addr(&req)).await?;
    let mut pools: Vec<PoolSummary> = s
        .pools
        .state()
        .iter()
        .filter(|p| p.metadata.uid.is_some())
        .filter(|p| {
            let ns = p.metadata.namespace.as_deref().unwrap_or_default();
            access_rules(p).is_some_and(|r| r.allows(&who, ns))
        })
        .filter_map(|p| summary(p, &s.edge))
        .collect();
    pools.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    let list = PoolList {
        per_allocation_endpoints: s.edge.per_allocation_endpoints,
        pools,
    };
    let mut r = axum::Json(list).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(r)
}
