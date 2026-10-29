//! Typed async client for the HTTP API (`client` feature).

use std::time::Duration;

use reqwest::header::{AUTHORIZATION, RETRY_AFTER};
use reqwest::{Certificate, StatusCode};

use crate::dto::{
    AllocationResponse, AllocationState, CreateAllocationRequest, ErrorBody, PendingAllocation,
    PoolList,
};

/// How long one poll of a pending allocation waits on the server.
pub const POLL_WAIT: Duration = Duration::from_secs(25);
/// Polls of a pending allocation that may fail in a row before the call gives up.
pub const POLL_RETRIES: u32 = 5;

/// What `GET /v1/allocations/{id}` found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AllocationStatus {
    /// `200`: bound.
    Bound(Box<AllocationResponse>),
    /// `202`: still waiting in the queue.
    Pending(PendingAllocation),
}

/// Client errors.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The client could not send the request or read the response.
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    /// The server answered with an error status.
    #[error("{status}: {message}")]
    Api {
        /// HTTP status.
        status: StatusCode,
        /// Parsed error body, when the server sent one.
        body: Option<ErrorBody>,
        /// Value of the `Retry-After` header, when present.
        retry_after: Option<Duration>,
        /// Human-readable message.
        message: String,
    },
    /// The client could not parse the server's CA certificate.
    #[error("invalid CA certificate: {0}")]
    BadCa(reqwest::Error),
}

impl ClientError {
    /// True when the request did not reach the server, or the server answered
    /// 503 with `Retry-After`. A caller may retry only in those cases, because
    /// allocation is not idempotent.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(e) => e.is_connect(),
            Self::Api {
                status,
                retry_after,
                ..
            } => *status == StatusCode::SERVICE_UNAVAILABLE && retry_after.is_some(),
            Self::BadCa(_) => false,
        }
    }

    /// The `Retry-After` of a server error.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Api { retry_after, .. } => *retry_after,
            Self::Transport(_) | Self::BadCa(_) => None,
        }
    }
}

/// Client for the buildkit-controller HTTP API.
#[derive(Clone, Debug)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    /// Creates a client for `base_url`, for example `https://bk.example.com:8443`, that sends a
    /// bearer `token`. `ca_pem` adds a trusted root for the serving certificate.
    pub fn new(base_url: &str, token: &str, ca_pem: Option<&[u8]>) -> Result<Self, ClientError> {
        let mut b = reqwest::Client::builder().timeout(Duration::from_mins(2));
        if let Some(pem) = ca_pem {
            b = b.add_root_certificate(Certificate::from_pem(pem).map_err(ClientError::BadCa)?);
        }
        Ok(Self {
            http: b.build()?,
            base: base_url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
        })
    }

    /// `POST /v1/allocations`, then `GET /v1/allocations/{id}?wait=25s` while
    /// the allocation is pending, until it is bound or fails.
    ///
    /// The HTTP API requires `public_key` (400 `invalid_request` without it), so the
    /// private key never leaves the caller. The POST is not idempotent, so retry it
    /// only when [`ClientError::is_retryable`] is true. A poll that fails that way is
    /// retried here, up to [`POLL_RETRIES`] times in a row.
    pub async fn create_allocation(
        &self,
        req: &CreateAllocationRequest,
    ) -> Result<AllocationResponse, ClientError> {
        let resp = self
            .http
            .post(self.url("/v1/allocations"))
            .header(AUTHORIZATION, self.bearer())
            .json(req)
            .send()
            .await?;
        let mut state = Self::state(resp).await?;
        let mut failures = 0;
        loop {
            let id = match state {
                AllocationStatus::Bound(r) => return Ok(*r),
                AllocationStatus::Pending(p) => p.id,
            };
            state = match self.get_allocation(&id, Some(POLL_WAIT)).await {
                Ok(s) => {
                    failures = 0;
                    s
                }
                Err(e) if e.is_retryable() && failures < POLL_RETRIES => {
                    failures += 1;
                    tokio::time::sleep(e.retry_after().unwrap_or(Duration::from_secs(1))).await;
                    AllocationStatus::Pending(PendingAllocation {
                        id,
                        state: AllocationState::Pending,
                        position: None,
                    })
                }
                Err(e) => return Err(e),
            };
        }
    }

    /// `GET /v1/allocations/{id}`, waiting up to `wait` (at most 25 s) while it is pending.
    pub async fn get_allocation(
        &self,
        id: &str,
        wait: Option<Duration>,
    ) -> Result<AllocationStatus, ClientError> {
        let mut url = self.url(&format!("/v1/allocations/{id}"));
        if let Some(w) = wait {
            url.push_str(&format!("?wait={}s", w.as_secs()));
        }
        let resp = self
            .http
            .get(url)
            .header(AUTHORIZATION, self.bearer())
            .send()
            .await?;
        Self::state(resp).await
    }

    async fn state(resp: reqwest::Response) -> Result<AllocationStatus, ClientError> {
        let resp = Self::check(resp).await?;
        Ok(if resp.status() == StatusCode::ACCEPTED {
            AllocationStatus::Pending(resp.json().await?)
        } else {
            AllocationStatus::Bound(Box::new(resp.json().await?))
        })
    }

    /// `DELETE /v1/allocations/{id}`. A 404 comes back as an error. Callers
    /// that treat a repeat release as success should match on it.
    pub async fn release_allocation(&self, id: &str) -> Result<(), ClientError> {
        let resp = self
            .http
            .delete(self.url(&format!("/v1/allocations/{id}")))
            .header(AUTHORIZATION, self.bearer())
            .send()
            .await?;
        Self::check(resp).await?;
        Ok(())
    }

    /// `GET /v1/pools`.
    pub async fn list_pools(&self) -> Result<PoolList, ClientError> {
        let resp = self
            .http
            .get(self.url("/v1/pools"))
            .header(AUTHORIZATION, self.bearer())
            .send()
            .await?;
        Ok(Self::check(resp).await?.json().await?)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }

    async fn check(resp: reqwest::Response) -> Result<reqwest::Response, ClientError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let retry_after = resp
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs);
        let text = resp.text().await.unwrap_or_default();
        let body = serde_json::from_str::<ErrorBody>(&text).ok();
        let message = body
            .as_ref()
            .map_or_else(|| text.clone(), |b| b.error.message.clone());
        Err(ClientError::Api {
            status,
            body,
            retry_after,
            message,
        })
    }
}
