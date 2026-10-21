//! Typed async client for the HTTP API (`client` feature).

use std::time::Duration;

use reqwest::header::{AUTHORIZATION, RETRY_AFTER};
use reqwest::{Certificate, StatusCode};

use crate::dto::{AllocationResponse, CreateAllocationRequest, ErrorBody, PoolList};

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

    /// `POST /v1/allocations`.
    ///
    /// The GitHub Action and the curl recipe always set `public_key`, so the private
    /// key never leaves the caller. The field stays optional on the wire for other
    /// clients. The call is not idempotent, so retry only when
    /// [`ClientError::is_retryable`] is true.
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
        Ok(Self::check(resp).await?.json().await?)
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
