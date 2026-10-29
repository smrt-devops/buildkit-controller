//! The distribution API subset that `BuildKit`'s registry cache exporter and
//! importer use (through containerd's resolver):
//!
//! - `GET /v2/`
//! - `HEAD`/`GET /v2/<name>/blobs/<digest>`, with a single `Range`
//! - `POST /v2/<name>/blobs/uploads/`
//! - `PATCH` and `PUT ?digest=` on the upload's `Location`
//! - `HEAD`/`GET`/`PUT /v2/<name>/manifests/<reference>`
//!
//! Everything else gets 405 or 404 `UNSUPPORTED`. Every route checks the
//! caller's grant first: pull for reads, push for writes. A grant covers one
//! repository, so a cross-repository mount (`?mount=&from=`) is never allowed
//! and falls back to a plain upload, as in the reference registry.

use std::collections::HashMap;
use std::io::SeekFrom;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::Query;
use axum::http::header::{
    ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, HeaderName, RANGE,
    RETRY_AFTER,
};
use axum::http::{HeaderValue, Method, Request, Response, StatusCode};
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::{OwnedMutexGuard, Semaphore};
use tokio_util::io::ReaderStream;
use tracing::{error, info};

use super::auth::{Action, Authorizer};
use super::metrics::RegistryMetrics;
use super::store::{Digest, Reference, Store, StoreError, Upload};
use crate::pki::WorkerIdentity;

/// Largest manifest accepted (the reference registry's limit).
pub const MAX_MANIFEST: usize = 4 << 20;
/// Manifest bodies buffered at once, each up to [`MAX_MANIFEST`]. A PUT that
/// finds none free is answered 429 at once.
pub const MANIFEST_PUTS: usize = 16;
/// The whole manifest body must arrive within this, so a slow sender holds
/// one of the [`MANIFEST_PUTS`] for no longer.
pub const MANIFEST_BODY: Duration = Duration::from_secs(30);
/// Longest gap between two chunks of a request body.
pub const BODY_IDLE: Duration = Duration::from_mins(1);

const DIGEST_HEADER: &str = "docker-content-digest";
const UPLOAD_UUID: &str = "docker-upload-uuid";

/// The registry: store, grants and metrics.
pub struct Registry {
    pub store: Store,
    pub auth: Arc<dyn Authorizer>,
    pub metrics: RegistryMetrics,
    /// [`MANIFEST_PUTS`] permits.
    pub manifest_puts: Semaphore,
}

/// A distribution error response.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    header: Option<(HeaderName, String)>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            header: None,
        }
    }

    fn with(mut self, name: HeaderName, value: String) -> Self {
        self.header = Some((name, value));
        self
    }

    fn into_response(self) -> Response<Body> {
        let body = serde_json::json!({"errors": [{"code": self.code, "message": self.message}]});
        let mut r = Response::new(Body::from(body.to_string()));
        *r.status_mut() = self.status;
        r.headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some((name, value)) = self.header
            && let Ok(v) = HeaderValue::from_str(&value)
        {
            r.headers_mut().insert(name, v);
        }
        r
    }
}

fn denied() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "DENIED",
        "requested access to the resource is denied",
    )
}

fn unsupported() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "UNSUPPORTED",
        "the operation is unsupported",
    )
}

fn digest_invalid(m: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "DIGEST_INVALID", m)
}

fn upload_invalid(m: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "BLOB_UPLOAD_INVALID", m)
}

fn too_large(max: u64) -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "SIZE_INVALID",
        format!("larger than the {max} byte limit"),
    )
}

fn internal(e: &std::io::Error) -> ApiError {
    error!(error = %e, "cache store I/O");
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "UNKNOWN",
        "storage error",
    )
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::DigestMismatch(_) => digest_invalid(e.to_string()),
            StoreError::TooLarge(max) => too_large(max),
            StoreError::ManifestInvalid(_) => {
                Self::new(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", e.to_string())
            }
            StoreError::ManifestBlobUnknown(_) => Self::new(
                StatusCode::BAD_REQUEST,
                "MANIFEST_BLOB_UNKNOWN",
                e.to_string(),
            ),
            StoreError::UploadSpace => Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                "TOOMANYREQUESTS",
                e.to_string(),
            ),
            StoreError::Stalled => Self::new(
                StatusCode::REQUEST_TIMEOUT,
                "BLOB_UPLOAD_INVALID",
                e.to_string(),
            ),
            StoreError::Body(_) => upload_invalid(e.to_string()),
            StoreError::Io(io) => internal(&io),
        }
    }
}

impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        internal(&e)
    }
}

/// A parsed request path.
#[derive(Debug, PartialEq, Eq)]
enum Route<'a> {
    Base,
    Blob(&'a str, &'a str),
    Uploads(&'a str),
    Upload(&'a str, &'a str),
    Manifest(&'a str, &'a str),
}

impl Route<'_> {
    fn label(&self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Blob(..) => "blob",
            Self::Uploads(..) | Self::Upload(..) => "upload",
            Self::Manifest(..) => "manifest",
        }
    }

    fn repository(&self) -> Option<&str> {
        match self {
            Self::Base => None,
            Self::Blob(n, _) | Self::Uploads(n) | Self::Upload(n, _) | Self::Manifest(n, _) => {
                Some(n)
            }
        }
    }
}

/// Splits on the last marker: a repository name may hold `blobs` or
/// `manifests` as a component, the digest, upload id or reference never a `/`.
fn route(path: &str) -> Option<Route<'_>> {
    if path == "/v2/" || path == "/v2" {
        return Some(Route::Base);
    }
    let rest = path.strip_prefix("/v2/")?;
    if let Some(name) = rest
        .strip_suffix("/blobs/uploads/")
        .or_else(|| rest.strip_suffix("/blobs/uploads"))
    {
        return Some(Route::Uploads(name));
    }
    let tail = |marker: &str| {
        rest.rsplit_once(marker)
            .filter(|(_, t)| !t.is_empty() && !t.contains('/'))
    };
    if let Some((name, id)) = tail("/blobs/uploads/") {
        return Some(Route::Upload(name, id));
    }
    if let Some((name, d)) = tail("/blobs/") {
        return Some(Route::Blob(name, d));
    }
    tail("/manifests/").map(|(name, r)| Route::Manifest(name, r))
}

/// The reference grammar, components `[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*`,
/// at most 255 bytes, plus this registry's rule of at least three components
/// (`<namespace>/<pool>/<scope>`).
fn valid_repository(name: &str) -> bool {
    fn component(c: &str) -> bool {
        let b = c.as_bytes();
        let alnum = |x: &u8| x.is_ascii_lowercase() || x.is_ascii_digit();
        if !b.first().is_some_and(alnum) || !b.last().is_some_and(alnum) {
            return false;
        }
        c.split(|x: char| x.is_ascii_lowercase() || x.is_ascii_digit())
            .all(|sep| matches!(sep, "" | "." | "_" | "__") || sep.bytes().all(|x| x == b'-'))
    }
    name.len() <= 255 && name.split('/').count() >= 3 && name.split('/').all(component)
}

fn method_label(m: &Method) -> &'static str {
    match *m {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::POST => "POST",
        Method::PATCH => "PATCH",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        _ => "other",
    }
}

/// Serves one request from `peer`.
pub async fn handle(reg: &Registry, peer: &WorkerIdentity, req: Request<Body>) -> Response<Body> {
    let started = Instant::now();
    let method = method_label(req.method());
    let path = req.uri().path().to_owned();
    let route = route(&path);
    let label = route.as_ref().map_or("other", Route::label);
    let result = match route {
        None => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "UNSUPPORTED",
            "no such route",
        )),
        Some(r) => dispatch(reg, peer, r, req).await,
    };
    let mut resp = result.unwrap_or_else(ApiError::into_response);
    resp.headers_mut().insert(
        "docker-distribution-api-version",
        HeaderValue::from_static("registry/2.0"),
    );
    reg.metrics.request(
        label,
        method,
        resp.status().as_u16(),
        started.elapsed().as_secs_f64(),
    );
    resp
}

impl Registry {
    fn authorize(&self, peer: &WorkerIdentity, repo: &str, action: Action) -> Result<(), ApiError> {
        if self.allowed(peer, repo, action) {
            return Ok(());
        }
        self.metrics.denied(action.label());
        info!(
            peer = %peer.dns_name(),
            repository = repo,
            action = action.label(),
            "cache request denied"
        );
        Err(denied())
    }

    fn allowed(&self, peer: &WorkerIdentity, repo: &str, action: Action) -> bool {
        self.auth
            .grant(peer)
            .is_some_and(|g| g.allows(repo, action))
    }

    fn upload(&self, repo: &str, id: &str) -> Result<OwnedMutexGuard<Upload>, ApiError> {
        match self.store.upload(repo, id) {
            Ok(Some(u)) => Ok(u),
            Ok(None) => Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "BLOB_UPLOAD_UNKNOWN",
                "blob upload unknown to registry",
            )),
            Err(()) => Err(ApiError::new(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "BLOB_UPLOAD_INVALID",
                "another request is writing this upload",
            )),
        }
    }
}

async fn dispatch(
    reg: &Registry,
    peer: &WorkerIdentity,
    route: Route<'_>,
    req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    let m = req.method().clone();
    let read = m == Method::GET || m == Method::HEAD;
    if let Some(name) = route.repository()
        && !valid_repository(name)
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "NAME_INVALID",
            "invalid repository name",
        ));
    }
    match route {
        Route::Base if read => {
            if reg.auth.grant(peer).is_none() {
                reg.metrics.denied(Action::Pull.label());
                return Err(denied());
            }
            Ok(json_ok("{}"))
        }
        Route::Blob(name, d) if read => {
            let d = d
                .parse::<Digest>()
                .map_err(|()| digest_invalid("unsupported digest"))?;
            reg.authorize(peer, name, Action::Pull)?;
            let range = req.headers().get(RANGE).cloned();
            get_blob(reg, name, &d, range.as_ref(), m == Method::HEAD).await
        }
        Route::Uploads(name) if m == Method::POST => {
            reg.authorize(peer, name, Action::Push)?;
            start_upload(reg, peer, name, query(&req)).await
        }
        Route::Upload(name, id) if m == Method::PATCH => {
            reg.authorize(peer, name, Action::Push)?;
            patch(reg, name, id, req).await
        }
        Route::Upload(name, id) if m == Method::PUT => {
            reg.authorize(peer, name, Action::Push)?;
            put_upload(reg, name, id, req).await
        }
        Route::Manifest(name, r) if read || m == Method::PUT => {
            let r = r.parse::<Reference>().map_err(|()| {
                ApiError::new(StatusCode::BAD_REQUEST, "TAG_INVALID", "invalid reference")
            })?;
            if read {
                reg.authorize(peer, name, Action::Pull)?;
                get_manifest(reg, name, &r, m == Method::HEAD).await
            } else {
                reg.authorize(peer, name, Action::Push)?;
                put_manifest(reg, name, &r, req).await
            }
        }
        _ => Err(unsupported()),
    }
}

fn json_ok(body: &'static str) -> Response<Body> {
    let mut r = Response::new(Body::from(body));
    r.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

/// A response with `status`, headers and an empty body.
fn empty(status: StatusCode, headers: &[(&str, String)]) -> Result<Response<Body>, ApiError> {
    let mut b = Response::builder().status(status);
    for (k, v) in headers {
        b = b.header(*k, v);
    }
    b.body(Body::empty())
        .map_err(|e| internal(&std::io::Error::other(e)))
}

/// One `bytes=` range, inclusive, within `size`. `Ok(None)` serves the whole
/// blob: no header, a malformed one, or several ranges.
fn parse_range(header: Option<&HeaderValue>, size: u64) -> Result<Option<(u64, u64)>, ApiError> {
    let Some(spec) = header
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("bytes="))
        .filter(|s| !s.contains(','))
    else {
        return Ok(None);
    };
    let Some((a, b)) = spec.trim().split_once('-') else {
        return Ok(None);
    };
    let unsatisfiable = || {
        ApiError::new(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "BLOB_UNKNOWN",
            "range not satisfiable",
        )
        .with(CONTENT_RANGE, format!("bytes */{size}"))
    };
    let (start, end) = match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(s), Ok(e)) => (s, e.min(size.saturating_sub(1))),
        (Ok(s), Err(_)) if b.is_empty() => (s, size.saturating_sub(1)),
        (Err(_), Ok(n)) if a.is_empty() && n > 0 => {
            (size.saturating_sub(n), size.saturating_sub(1))
        }
        _ => return Ok(None),
    };
    if size == 0 || start > end || start >= size {
        return Err(unsatisfiable());
    }
    Ok(Some((start, end)))
}

async fn get_blob(
    reg: &Registry,
    name: &str,
    d: &Digest,
    range: Option<&HeaderValue>,
    head: bool,
) -> Result<Response<Body>, ApiError> {
    let unknown = || {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "BLOB_UNKNOWN",
            "blob unknown to registry",
        )
    };
    let (path, size) = reg.store.blob(name, d).await.ok_or_else(unknown)?;
    let range = if head {
        None
    } else {
        parse_range(range, size)?
    };
    let (status, start, len) = match range {
        Some((s, e)) => (StatusCode::PARTIAL_CONTENT, s, e - s + 1),
        None => (StatusCode::OK, 0, size),
    };
    let mut b = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/octet-stream")
        .header(CONTENT_LENGTH, len)
        .header(DIGEST_HEADER, d.to_string())
        .header(ETAG, format!("\"{d}\""))
        .header(ACCEPT_RANGES, "bytes");
    if let Some((s, e)) = range {
        b = b.header(CONTENT_RANGE, format!("bytes {s}-{e}/{size}"));
    }
    let body = if head {
        Body::empty()
    } else {
        // Garbage collection may delete the file between lookup and open;
        // an open file stays readable after deletion.
        let mut f = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(unknown()),
            Err(e) => return Err(e.into()),
        };
        if start > 0 {
            f.seek(SeekFrom::Start(start)).await?;
        }
        Body::from_stream(ReaderStream::new(f.take(len)))
    };
    b.body(body)
        .map_err(|e| internal(&std::io::Error::other(e)))
}

async fn start_upload(
    reg: &Registry,
    peer: &WorkerIdentity,
    name: &str,
    q: HashMap<String, String>,
) -> Result<Response<Body>, ApiError> {
    if q.contains_key("digest") {
        return Err(ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "UNSUPPORTED",
            "single-request uploads are not served; PUT to the upload location",
        ));
    }
    // The full identity, nonce included: a later pod of the same name starts
    // with a clean quota.
    let owner = peer.dns_name();
    let id = reg.store.start_upload(name, &owner).await?.ok_or_else(|| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "TOOMANYREQUESTS",
            "too many uploads in progress",
        )
    })?;
    empty(
        StatusCode::ACCEPTED,
        &[
            ("location", format!("/v2/{name}/blobs/uploads/{id}")),
            ("range", "0-0".into()),
            (UPLOAD_UUID, id),
            ("content-length", "0".into()),
        ],
    )
}

fn query(req: &Request<Body>) -> HashMap<String, String> {
    Query::<HashMap<String, String>>::try_from_uri(req.uri())
        .map(|q| q.0)
        .unwrap_or_default()
}

/// `0-<last byte>`, or `0-0` for nothing yet.
fn range_header(size: u64) -> String {
    format!("0-{}", size.saturating_sub(1))
}

/// Streams the request body into the upload; aborts the upload on error.
async fn receive(
    reg: &Registry,
    id: &str,
    mut up: OwnedMutexGuard<Upload>,
    req: Request<Body>,
) -> Result<OwnedMutexGuard<Upload>, ApiError> {
    let before = up.size();
    let declared = req
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let max = reg.store.max_blob();
    if declared.is_some_and(|n| n > max.saturating_sub(before)) {
        reg.store.abort(id, up).await;
        return Err(too_large(max));
    }
    let body = req.into_body().into_data_stream();
    if let Err(e) = reg.store.append(&mut up, body, BODY_IDLE).await {
        reg.store.abort(id, up).await;
        return Err(e.into());
    }
    reg.metrics.received(up.size() - before);
    Ok(up)
}

async fn patch(
    reg: &Registry,
    name: &str,
    id: &str,
    req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    let up = reg.upload(name, id)?;
    if let Some(cr) = req.headers().get(CONTENT_RANGE) {
        let start = cr
            .to_str()
            .ok()
            .map(|s| s.trim_start_matches("bytes "))
            .and_then(|s| s.split_once('-'))
            .and_then(|(a, _)| a.parse::<u64>().ok());
        if start != Some(up.size()) {
            return Err(ApiError::new(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "BLOB_UPLOAD_INVALID",
                "chunk does not start at the end of the upload",
            )
            .with(axum::http::header::RANGE, range_header(up.size())));
        }
    }
    let up = receive(reg, id, up, req).await?;
    empty(
        StatusCode::ACCEPTED,
        &[
            ("location", format!("/v2/{name}/blobs/uploads/{id}")),
            ("range", range_header(up.size())),
            (UPLOAD_UUID, id.to_owned()),
            ("content-length", "0".into()),
        ],
    )
}

async fn put_upload(
    reg: &Registry,
    name: &str,
    id: &str,
    req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    let d = query(&req)
        .get("digest")
        .and_then(|d| d.parse::<Digest>().ok())
        .ok_or_else(|| digest_invalid("missing or unsupported digest"))?;
    let up = reg.upload(name, id)?;
    let up = receive(reg, id, up, req).await?;
    reg.store.finish(id, up, &d).await?;
    empty(
        StatusCode::CREATED,
        &[
            ("location", format!("/v2/{name}/blobs/{d}")),
            (DIGEST_HEADER, d.to_string()),
            ("content-length", "0".into()),
        ],
    )
}

async fn get_manifest(
    reg: &Registry,
    name: &str,
    r: &Reference,
    head: bool,
) -> Result<Response<Body>, ApiError> {
    let unknown = || {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "MANIFEST_UNKNOWN",
            "manifest unknown",
        )
    };
    let m = reg.store.manifest(name, r).await.ok_or_else(unknown)?;
    let body = if head {
        Body::empty()
    } else {
        match tokio::fs::read(&m.path).await {
            Ok(b) => Body::from(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(unknown()),
            Err(e) => return Err(e.into()),
        }
    };
    Response::builder()
        .header(CONTENT_TYPE, &m.media_type)
        .header(CONTENT_LENGTH, m.size)
        .header(DIGEST_HEADER, m.digest.to_string())
        .header(ETAG, format!("\"{}\"", m.digest))
        .body(body)
        .map_err(|e| internal(&std::io::Error::other(e)))
}

async fn put_manifest(
    reg: &Registry,
    name: &str,
    r: &Reference,
    req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    let media = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned);
    let _permit = reg.manifest_puts.try_acquire().map_err(|_| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "TOOMANYREQUESTS",
            "too many manifest pushes in progress",
        )
        .with(RETRY_AFTER, "1".to_owned())
    })?;
    let mut body = req.into_body().into_data_stream();
    let mut buf = Vec::new();
    let read = async {
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| upload_invalid(e.to_string()))?;
            if buf.len() + chunk.len() > MAX_MANIFEST {
                return Err(too_large(MAX_MANIFEST as u64));
            }
            buf.extend_from_slice(&chunk);
        }
        Ok::<(), ApiError>(())
    };
    tokio::time::timeout(MANIFEST_BODY, read)
        .await
        .map_err(|_| ApiError::from(StoreError::Stalled))??;
    reg.metrics.received(buf.len() as u64);
    let d = reg
        .store
        .put_manifest(name, r, media.as_deref(), &buf)
        .await?;
    empty(
        StatusCode::CREATED,
        &[
            ("location", format!("/v2/{name}/manifests/{d}")),
            (DIGEST_HEADER, d.to_string()),
            ("content-length", "0".into()),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_split_on_the_last_marker() {
        assert_eq!(route("/v2/"), Some(Route::Base));
        assert_eq!(
            route("/v2/a/b/c/blobs/uploads/"),
            Some(Route::Uploads("a/b/c"))
        );
        assert_eq!(
            route("/v2/a/b/c/blobs/uploads/xyz"),
            Some(Route::Upload("a/b/c", "xyz"))
        );
        assert_eq!(
            route("/v2/a/blobs/c/blobs/sha256:00"),
            Some(Route::Blob("a/blobs/c", "sha256:00"))
        );
        assert_eq!(
            route("/v2/a/b/c/manifests/latest"),
            Some(Route::Manifest("a/b/c", "latest"))
        );
        assert_eq!(route("/v2/a/b/c/tags/list"), None);
        assert_eq!(route("/v2/_catalog"), None);
        assert_eq!(route("/v1/a/b/c/manifests/x"), None);
    }

    #[test]
    fn repository_names_follow_the_grammar() {
        for ok in ["ns/pool/app", "ns/pool/a.b_c__d--e/f", "a/b/c/d"] {
            assert!(valid_repository(ok), "{ok}");
        }
        for bad in [
            "ns/pool",
            "ns/pool/App",
            "ns/pool/../x",
            "ns/pool/.x",
            "ns//pool/x",
            "ns/pool/x-",
            "ns/pool/a___b",
            "ns/pool/a%2fb",
            "ns/pool/_links",
        ] {
            assert!(!valid_repository(bad), "{bad}");
        }
        assert!(!valid_repository(&format!("a/b/{}", "c".repeat(260))));
    }

    #[test]
    fn ranges_parse() {
        let h = |s: &'static str| HeaderValue::from_static(s);
        assert_eq!(parse_range(None, 10).unwrap(), None);
        assert_eq!(parse_range(Some(&h("bytes=2-")), 10).unwrap(), Some((2, 9)));
        assert_eq!(
            parse_range(Some(&h("bytes=2-4")), 10).unwrap(),
            Some((2, 4))
        );
        assert_eq!(
            parse_range(Some(&h("bytes=2-99")), 10).unwrap(),
            Some((2, 9))
        );
        assert_eq!(parse_range(Some(&h("bytes=-3")), 10).unwrap(), Some((7, 9)));
        assert_eq!(parse_range(Some(&h("bytes=0-1,4-5")), 10).unwrap(), None);
        assert_eq!(parse_range(Some(&h("items=0-1")), 10).unwrap(), None);
        let e = parse_range(Some(&h("bytes=10-")), 10).unwrap_err();
        assert_eq!(e.status, StatusCode::RANGE_NOT_SATISFIABLE);
        assert!(parse_range(Some(&h("bytes=5-2")), 10).is_err());
    }
}
