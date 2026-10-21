//! Request and response bodies of the HTTP API. JSON is camelCase.

use serde::{Deserialize, Serialize};

/// Largest accepted request body, in bytes.
pub const MAX_BODY_BYTES: usize = 16 * 1024;
/// Smallest `ttlSeconds` a request may ask for.
pub const MIN_TTL_SECONDS: u64 = 60;

/// `POST /v1/allocations` request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateAllocationRequest {
    /// Pool namespace (DNS-1123 label).
    pub namespace: String,
    /// Pool name (DNS-1123 label, at most 52 characters).
    pub pool: String,
    /// Requested lifetime. Defaults to the pool's `allocation.defaultTTL`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
    /// Longest time to wait in the queue. Defaults to the pool's `allocation.queueTimeout` and may not exceed `maxQueueTimeout`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_timeout_seconds: Option<u64>,
    /// PEM SPKI ECDSA P-256 public key. When set, the server returns no private key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    /// Cache key of both cache layers. Defaults to the pool's `registry.defaultKey`, else its name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<CacheKey>,
    /// Which cache layers to use. Defaults to `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheChoice>,
}

/// `cache` of `POST /v1/allocations`.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum CacheChoice {
    /// Every layer the pool enables.
    #[default]
    Auto,
    /// Only the registry cache; 400 when the pool has none.
    Registry,
    /// Only local volumes; 400 when the pool has none.
    Local,
    /// No cache layer.
    None,
}

/// `cache` of the `201` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheResponse {
    /// The effective cache key.
    pub key: String,
    /// buildx flags of the registry cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<crate::v1alpha1::RegistryCacheFlags>,
    /// Local volume outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<LocalCacheResult>,
}

/// `cache.local` of the `201` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalCacheResult {
    /// `hit`, `miss`, `spill`, `cold` or `uncached`, or
    /// `reused` when the request left out the local layer.
    pub result: String,
}

/// `cacheKey` of `POST /v1/allocations`: a Kubernetes label value of 1 to 63
/// characters (`^[A-Za-z0-9]([A-Za-z0-9._-]*[A-Za-z0-9])?$`). The allocator
/// uses it for the registry ref and the local volume preference.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CacheKey(String);

impl CacheKey {
    /// The key as sent.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CacheKey {
    type Error = crate::ApiError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        if crate::v1alpha1::is_cache_key(&s) {
            Ok(Self(s))
        } else {
            Err(crate::ApiError::Invalid {
                field: "cacheKey".into(),
                reason: "must be a label value: 1 to 63 of [A-Za-z0-9._-], starting and ending alphanumeric".into(),
            })
        }
    }
}

impl From<CacheKey> for String {
    fn from(k: CacheKey) -> Self {
        k.0
    }
}

/// `POST /v1/allocations` response (`201 Created`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AllocationResponse {
    /// Allocation id.
    pub id: String,
    /// Pool namespace.
    pub namespace: String,
    /// Pool name.
    pub pool: String,
    /// Caller identity, `oidc:<iss>#<sub>` or `sa:<ns>/<name>`.
    pub owner: String,
    /// Edge endpoint, for example `tcp://bk.example.com:8444`.
    pub endpoint: String,
    /// TLS server name.
    pub server_name: String,
    /// The client CA certificate to trust for the edge. `null` means use public roots.
    pub ca_cert: Option<String>,
    /// Allocation client certificate (PEM, no chain).
    pub client_cert: String,
    /// PKCS#8 PEM private key. The server omits it when the request carried a `publicKey`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key: Option<String>,
    /// RFC 3339 UTC expiry.
    pub expires_at: String,
    /// Cache flags and outcome; absent when no cache layer applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheResponse>,
}

/// One entry of `GET /v1/pools`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolSummary {
    /// Pool namespace.
    pub namespace: String,
    /// Pool name.
    pub name: String,
    /// True when the `Ready` condition is True.
    pub ready: bool,
    /// Number of free pods that are Ready.
    pub warm: i32,
    /// Number of free pods that are not Ready.
    pub starting: i32,
    /// Number of allocated pods.
    pub allocated: i32,
    /// Number of requests queued across replicas.
    pub waiting: i32,
    /// Value of `scaling.min`.
    pub min: i32,
    /// Value of `scaling.max`.
    pub max: i32,
    /// Value of `status.platforms`. Empty when unknown.
    pub platforms: Vec<String>,
    /// Edge endpoint.
    pub endpoint: String,
    /// TLS server name.
    pub server_name: String,
}

/// `GET /v1/pools` response.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolList {
    /// True when `edge.allocationDomain` is set. Multi-node builders require it.
    pub per_allocation_endpoints: bool,
    /// Pools visible to the caller, sorted by namespace then name.
    pub pools: Vec<PoolSummary>,
}

/// Machine-readable error codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// HTTP 400 for bad JSON, an unknown field, a TTL out of range or a bad `publicKey`.
    InvalidRequest,
    /// HTTP 401 for a missing or invalid token.
    Unauthenticated,
    /// HTTP 403 when a caller releases an allocation it does not own. A caller that
    /// matches no access rule gets [`ErrorCode::PoolNotFound`] instead, so callers cannot probe for pools.
    Forbidden,
    /// HTTP 404 when the pool is not in the store, is outside the watched scope, or no access rule matches the caller.
    PoolNotFound,
    /// HTTP 404 for an unknown, released or expired allocation, or an unknown path.
    NotFound,
    /// HTTP 413 when the body is over 16 KiB.
    PayloadTooLarge,
    /// HTTP 415 when `Content-Type` is not `application/json`.
    UnsupportedMediaType,
    /// HTTP 500 for an unexpected error.
    Internal,
    /// HTTP 503 when the queue timeout expires, the queue timeout is `0s`, or the replica queue is full.
    PoolExhausted,
    /// HTTP 503 when a cold pod is not Ready within `startTimeout`.
    StartTimeout,
    /// HTTP 503 when a cold pod is deleted or changed while the request waits.
    PodLost,
    /// HTTP 503 when pool `Ready` is not True for a reason other than the CA.
    NotReady,
    /// HTTP 503 when the CA never loaded or is invalid.
    CaUnavailable,
    /// HTTP 503 when a Kubernetes API call times out or returns 5xx and the outcome is unknown.
    UpstreamError,
}

impl ErrorCode {
    /// HTTP status that the server returns for the code.
    #[must_use]
    pub const fn http_status(self) -> u16 {
        match self {
            Self::InvalidRequest => 400,
            Self::Unauthenticated => 401,
            Self::Forbidden => 403,
            Self::PoolNotFound | Self::NotFound => 404,
            Self::PayloadTooLarge => 413,
            Self::UnsupportedMediaType => 415,
            Self::Internal => 500,
            Self::PoolExhausted
            | Self::StartTimeout
            | Self::PodLost
            | Self::NotReady
            | Self::CaUnavailable
            | Self::UpstreamError => 503,
        }
    }

    /// Whether the response carries `Retry-After: 5`.
    #[must_use]
    pub const fn retry_after_seconds(self) -> Option<u64> {
        match self {
            Self::PoolExhausted | Self::StartTimeout => Some(5),
            _ => None,
        }
    }
}

/// The `error` object of an error body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDetail {
    /// Machine-readable code.
    pub code: ErrorCode,
    /// Human-readable message.
    pub message: String,
}

/// Body of every non-2xx response, for example `{"error": {"code": ..., "message": ...}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// The error.
    pub error: ErrorDetail,
}

impl ErrorBody {
    /// Build an error body.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            error: ErrorDetail {
                code,
                message: message.into(),
            },
        }
    }
}

impl ErrorCode {
    /// Every code, in the order of the API error table.
    pub const ALL: [ErrorCode; 14] = [
        Self::InvalidRequest,
        Self::Unauthenticated,
        Self::Forbidden,
        Self::PoolNotFound,
        Self::NotFound,
        Self::PayloadTooLarge,
        Self::UnsupportedMediaType,
        Self::Internal,
        Self::PoolExhausted,
        Self::StartTimeout,
        Self::PodLost,
        Self::NotReady,
        Self::CaUnavailable,
        Self::UpstreamError,
    ];

    /// The stable wire string. It is also the metrics `result` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::Unauthenticated => "unauthenticated",
            Self::Forbidden => "forbidden",
            Self::PoolNotFound => "pool_not_found",
            Self::NotFound => "not_found",
            Self::PayloadTooLarge => "payload_too_large",
            Self::UnsupportedMediaType => "unsupported_media_type",
            Self::Internal => "internal",
            Self::PoolExhausted => "pool_exhausted",
            Self::StartTimeout => "start_timeout",
            Self::PodLost => "pod_lost",
            Self::NotReady => "not_ready",
            Self::CaUnavailable => "ca_unavailable",
            Self::UpstreamError => "upstream_error",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
