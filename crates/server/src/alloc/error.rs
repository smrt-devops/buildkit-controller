//! [`AllocError`]: one variant per API error code the allocation service can produce.

use buildkit_controller_api::dto::ErrorCode;

/// Failure of [`super::AllocService::allocate`] or [`super::AllocService::release`].
///
/// Each variant maps to exactly one API error code ([`AllocError::code`]). The
/// codes the HTTP layer produces before calling this service
/// (`unauthenticated`, `payload_too_large`, `unsupported_media_type`, and
/// `invalid_request` for undecodable JSON) have no variant here. Messages are
/// safe to return to the caller because they never contain tokens, keys or owners.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AllocError {
    /// HTTP 400, TTL or queue timeout out of range, bad `publicKey`.
    #[error("{0}")]
    InvalidRequest(String),
    /// HTTP 403, release by a caller that is not the owner.
    #[error("allocation is owned by another identity")]
    Forbidden,
    /// HTTP 404, pool absent, outside the watched scope, or no access rule matches.
    #[error("pool not found")]
    PoolNotFound,
    /// HTTP 404, allocation unknown, released or expired.
    #[error("allocation not found")]
    NotFound,
    /// HTTP 500, unexpected error (logged with detail; the message stays generic).
    #[error("internal error")]
    Internal,
    /// HTTP 503, queue timeout expired, queue timeout `0s`, or the replica queue is full.
    #[error("no warm pod and no free slot in {0}")]
    PoolExhausted(String),
    /// HTTP 503, cold pod not Ready within `startTimeout`.
    #[error("worker pod did not start in time")]
    StartTimeout,
    /// HTTP 503, cold pod deleted or changed while waiting.
    #[error("worker pod was lost while starting")]
    PodLost,
    /// HTTP 503, pool `Ready` is not True for a reason other than CA.
    #[error("pool is not ready")]
    NotReady,
    /// HTTP 503, CA never loaded or invalid.
    #[error("certificate authority unavailable")]
    CaUnavailable,
    /// HTTP 503 with `Retry-After`, too many releases are waiting for their pod.
    #[error("too many releases in progress")]
    ReleaseBusy,
    /// HTTP 503, Kubernetes API timeout or 5xx with unknown outcome.
    #[error("kubernetes API unavailable")]
    UpstreamError,
    /// HTTP 503 with `Retry-After`, the replica is shutting down. Returned only
    /// before a pod was claimed for the request.
    #[error("replica is shutting down; retry")]
    ShuttingDown,
}

impl AllocError {
    /// The API error code (also the metrics `result` label, via [`ErrorCode::as_str`]).
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::InvalidRequest(_) => ErrorCode::InvalidRequest,
            Self::Forbidden => ErrorCode::Forbidden,
            Self::PoolNotFound => ErrorCode::PoolNotFound,
            Self::NotFound => ErrorCode::NotFound,
            Self::Internal => ErrorCode::Internal,
            // `ReleaseBusy` uses the API error 503 code that carries `Retry-After`.
            Self::PoolExhausted(_) | Self::ReleaseBusy => ErrorCode::PoolExhausted,
            Self::StartTimeout => ErrorCode::StartTimeout,
            Self::PodLost => ErrorCode::PodLost,
            Self::NotReady => ErrorCode::NotReady,
            Self::CaUnavailable => ErrorCode::CaUnavailable,
            Self::UpstreamError => ErrorCode::UpstreamError,
            Self::ShuttingDown => ErrorCode::ShuttingDown,
        }
    }
}
