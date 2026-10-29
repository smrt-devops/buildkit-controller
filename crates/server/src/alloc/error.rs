//! [`AllocError`]: one variant per API error code allocation can produce.

use buildkit_controller_api::dto::ErrorCode;

/// Failure of an allocation call of the front door.
///
/// Each variant maps to exactly one API error code ([`AllocError::code`]). The
/// codes the HTTP layer produces before calling it (`unauthenticated`,
/// `payload_too_large`, `unsupported_media_type`, and `invalid_request` for
/// undecodable JSON) have no variant here. Messages are safe to return to the
/// caller because they never contain tokens, keys or owners.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AllocError {
    /// HTTP 400, TTL or queue timeout out of range, bad `publicKey`.
    #[error("{0}")]
    InvalidRequest(String),
    /// HTTP 403, a caller that is not the owner.
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
    /// HTTP 503, the queue timeout ran out.
    #[error("{0}")]
    PoolExhausted(String),
    /// HTTP 503, the worker of a bound allocation is gone.
    #[error("{0}")]
    PodLost(String),
    /// HTTP 503, pool `Ready` is not True for a reason other than CA.
    #[error("pool is not ready")]
    NotReady,
    /// HTTP 503, CA never loaded or invalid.
    #[error("certificate authority unavailable")]
    CaUnavailable,
    /// HTTP 503 with `Retry-After`, Kubernetes API timeout or 5xx with unknown outcome.
    #[error("kubernetes API unavailable")]
    UpstreamError,
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
            Self::PoolExhausted(_) => ErrorCode::PoolExhausted,
            Self::PodLost(_) => ErrorCode::PodLost,
            Self::NotReady => ErrorCode::NotReady,
            Self::CaUnavailable => ErrorCode::CaUnavailable,
            Self::UpstreamError => ErrorCode::UpstreamError,
        }
    }

    /// The error for a finished claim's API code and condition message.
    #[must_use]
    pub fn of_claim(code: ErrorCode, message: String) -> Self {
        match code {
            ErrorCode::PoolExhausted => Self::PoolExhausted(message),
            ErrorCode::PodLost => Self::PodLost(message),
            ErrorCode::PoolNotFound => Self::PoolNotFound,
            ErrorCode::InvalidRequest => Self::InvalidRequest(message),
            ErrorCode::NotFound => Self::NotFound,
            _ => Self::Internal,
        }
    }
}
