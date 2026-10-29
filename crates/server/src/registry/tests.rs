//! Protocol, storage and garbage collection through [`handle`], and the mTLS
//! listener end to end.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use bytes::Bytes;

use super::store::MAX_UPLOADS_PER_OWNER;
use super::*;
use crate::pki::WorkerIdentity;

fn worker(pod: &str) -> WorkerIdentity {
    WorkerIdentity {
        namespace: "ns".parse().unwrap(),
        pool: "p".parse().unwrap(),
        pod: pod.to_owned(),
        nonce: "0123456789abcdef".to_owned(),
    }
}

fn grants() -> StaticGrants {
    let g = |repository: &str, push| Grant {
        repository: repository.to_owned(),
        push,
    };
    StaticGrants::default()
        .with("ns", "writer", g("ns/p/app", true))
        .with("ns", "reader", g("ns/p/app", false))
        .with("ns", "other", g("ns/p/other", true))
        .with("ns", "wide", g("ns/p", true))
}

struct Fixture {
    reg: Registry,
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bk-registry-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn registry(dir: &std::path::Path, max_blob: u64) -> Registry {
    Registry {
        store: Store::open(
            dir,
            StoreLimits {
                max_blob,
                max_upload_bytes: 4 * max_blob,
            },
        )
        .unwrap(),
        auth: Arc::new(grants()),
        metrics: RegistryMetrics::default(),
    }
}

fn fixture(name: &str) -> Fixture {
    fixture_max(name, 1 << 20)
}

fn fixture_max(name: &str, max_blob: u64) -> Fixture {
    let dir = dir(name);
    Fixture {
        reg: registry(&dir, max_blob),
        dir,
    }
}

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Resp {
    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }

    fn code(&self) -> String {
        let v: serde_json::Value = serde_json::from_slice(&self.body).unwrap_or_default();
        v["errors"][0]["code"].as_str().unwrap_or("").to_owned()
    }
}

async fn call(
    reg: &Registry,
    pod: &str,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Resp {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let r = handle(
        reg,
        &worker(pod),
        b.body(Body::from(body.to_vec())).unwrap(),
    )
    .await;
    let (parts, body) = r.into_parts();
    Resp {
        status: parts.status,
        headers: parts.headers,
        body: axum::body::to_bytes(body, usize::MAX).await.unwrap(),
    }
}

/// Pushes `data` into `repo` the way containerd does: POST, then one PUT.
async fn push_blob(reg: &Registry, pod: &str, repo: &str, data: &[u8]) -> Digest {
    let d = Digest::of(data);
    let r = call(
        reg,
        pod,
        Method::POST,
        &format!("/v2/{repo}/blobs/uploads/"),
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{:?}", r.body);
    // Go's url.Values encodes the colon.
    let put = format!(
        "{}?digest={}",
        r.header("location"),
        d.to_string().replace(':', "%3A")
    );
    let r = call(reg, pod, Method::PUT, &put, &[], data).await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    assert_eq!(r.header("docker-content-digest"), d.to_string());
    d
}

fn index_of(blobs: &[&Digest]) -> Vec<u8> {
    let manifests: Vec<_> = blobs
        .iter()
        .map(|d| serde_json::json!({"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": d.to_string(), "size": 1}))
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": manifests,
    }))
    .unwrap()
}

const INDEX: &str = "application/vnd.oci.image.index.v1+json";

async fn put_index(reg: &Registry, pod: &str, repo: &str, tag: &str, blobs: &[&Digest]) -> Resp {
    call(
        reg,
        pod,
        Method::PUT,
        &format!("/v2/{repo}/manifests/{tag}"),
        &[("content-type", INDEX)],
        &index_of(blobs),
    )
    .await
}

#[tokio::test]
async fn base_answers_only_a_granted_worker() {
    let f = fixture("base");
    let r = call(&f.reg, "writer", Method::GET, "/v2/", &[], b"").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("docker-distribution-api-version"), "registry/2.0");
    let r = call(&f.reg, "nobody", Method::GET, "/v2/", &[], b"").await;
    assert_eq!(
        (r.status, r.code().as_str()),
        (StatusCode::FORBIDDEN, "DENIED")
    );
}

#[tokio::test]
async fn chunked_upload_then_head_get_and_range() {
    let f = fixture("chunked");
    let reg = &f.reg;
    let r = call(
        reg,
        "writer",
        Method::POST,
        "/v2/ns/p/app/blobs/uploads/",
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    assert_eq!(r.header("range"), "0-0");
    let loc = r.header("location").to_owned();
    assert!(loc.starts_with("/v2/ns/p/app/blobs/uploads/"), "{loc}");
    assert_eq!(
        r.header("docker-upload-uuid"),
        loc.rsplit('/').next().unwrap()
    );

    let r = call(reg, "writer", Method::PATCH, &loc, &[], b"hello ").await;
    assert_eq!((r.status, r.header("range")), (StatusCode::ACCEPTED, "0-5"));
    // A chunk that does not start at the end is refused, and the upload survives.
    let r = call(
        reg,
        "writer",
        Method::PATCH,
        &loc,
        &[("content-range", "0-4")],
        b"world",
    )
    .await;
    assert_eq!(r.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(r.header("range"), "0-5");
    let r = call(
        reg,
        "writer",
        Method::PATCH,
        &loc,
        &[("content-range", "6-10")],
        b"world",
    )
    .await;
    assert_eq!(
        (r.status, r.header("range")),
        (StatusCode::ACCEPTED, "0-10")
    );

    let d = Digest::of(b"hello world");
    let r = call(
        reg,
        "writer",
        Method::PUT,
        &format!("{loc}?digest={d}"),
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.header("location"), format!("/v2/ns/p/app/blobs/{d}"));
    assert_eq!(r.header("docker-content-digest"), d.to_string());
    // The upload is gone.
    let r = call(reg, "writer", Method::PATCH, &loc, &[], b"x").await;
    assert_eq!(
        (r.status, r.code().as_str()),
        (StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN")
    );

    let blob = format!("/v2/ns/p/app/blobs/{d}");
    let r = call(reg, "reader", Method::HEAD, &blob, &[], b"").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("content-length"), "11");
    assert_eq!(r.header("docker-content-digest"), d.to_string());
    assert!(r.body.is_empty());
    let r = call(reg, "reader", Method::GET, &blob, &[], b"").await;
    assert_eq!(&r.body[..], b"hello world");
    let r = call(
        reg,
        "reader",
        Method::GET,
        &blob,
        &[("range", "bytes=6-")],
        b"",
    )
    .await;
    assert_eq!(r.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(r.header("content-range"), "bytes 6-10/11");
    assert_eq!(&r.body[..], b"world");
    let r = call(
        reg,
        "reader",
        Method::GET,
        &blob,
        &[("range", "bytes=11-")],
        b"",
    )
    .await;
    assert_eq!(r.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(r.header("content-range"), "bytes */11");
}

#[tokio::test]
async fn a_wrong_digest_stores_nothing() {
    let f = fixture("mismatch");
    let r = call(
        &f.reg,
        "writer",
        Method::POST,
        "/v2/ns/p/app/blobs/uploads/",
        &[],
        b"",
    )
    .await;
    let claimed = Digest::of(b"something else");
    let put = format!("{}?digest={claimed}", r.header("location"));
    let r = call(&f.reg, "writer", Method::PUT, &put, &[], b"payload").await;
    assert_eq!(
        (r.status, r.code().as_str()),
        (StatusCode::BAD_REQUEST, "DIGEST_INVALID")
    );
    for d in [claimed, Digest::of(b"payload")] {
        let r = call(
            &f.reg,
            "writer",
            Method::HEAD,
            &format!("/v2/ns/p/app/blobs/{d}"),
            &[],
            b"",
        )
        .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
    }
    assert_eq!(std::fs::read_dir(f.dir.join("uploads")).unwrap().count(), 0);
    assert_eq!(
        std::fs::read_dir(f.dir.join("blobs/sha256"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn blobs_over_the_limit_are_refused_and_the_upload_ends() {
    let f = fixture_max("large", 8);
    for headers in [&[][..], &[("content-length", "10")][..]] {
        let r = call(
            &f.reg,
            "writer",
            Method::POST,
            "/v2/ns/p/app/blobs/uploads/",
            &[],
            b"",
        )
        .await;
        let loc = r.header("location").to_owned();
        let r = call(
            &f.reg,
            "writer",
            Method::PATCH,
            &loc,
            headers,
            b"0123456789",
        )
        .await;
        assert_eq!(
            (r.status, r.code().as_str()),
            (StatusCode::PAYLOAD_TOO_LARGE, "SIZE_INVALID")
        );
        let r = call(&f.reg, "writer", Method::PATCH, &loc, &[], b"0").await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
    }
    assert_eq!(std::fs::read_dir(f.dir.join("uploads")).unwrap().count(), 0);
}

#[tokio::test]
async fn manifests_need_their_blobs_and_resolve_by_tag_and_digest() {
    let f = fixture("manifests");
    let reg = &f.reg;
    let layer = Digest::of(b"layer");
    let r = put_index(reg, "writer", "ns/p/app", "cache", &[&layer]).await;
    assert_eq!(
        (r.status, r.code().as_str()),
        (StatusCode::BAD_REQUEST, "MANIFEST_BLOB_UNKNOWN")
    );

    push_blob(reg, "writer", "ns/p/app", b"layer").await;
    let r = put_index(reg, "writer", "ns/p/app", "cache", &[&layer]).await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    let d = Digest::of(&index_of(&[&layer]));
    assert_eq!(r.header("docker-content-digest"), d.to_string());
    assert_eq!(r.header("location"), format!("/v2/ns/p/app/manifests/{d}"));

    let r = call(
        reg,
        "reader",
        Method::HEAD,
        "/v2/ns/p/app/manifests/cache",
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("content-type"), INDEX);
    assert_eq!(r.header("docker-content-digest"), d.to_string());
    assert_eq!(
        r.header("content-length"),
        index_of(&[&layer]).len().to_string()
    );
    let r = call(
        reg,
        "reader",
        Method::GET,
        &format!("/v2/ns/p/app/manifests/{d}"),
        &[],
        b"",
    )
    .await;
    assert_eq!(&r.body[..], &index_of(&[&layer])[..]);
    // The manifest is a blob of the repository too.
    let r = call(
        reg,
        "reader",
        Method::HEAD,
        &format!("/v2/ns/p/app/blobs/{d}"),
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);

    let r = call(
        reg,
        "reader",
        Method::GET,
        "/v2/ns/p/app/manifests/missing",
        &[],
        b"",
    )
    .await;
    assert_eq!(
        (r.status, r.code().as_str()),
        (StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN")
    );
    let r = put_index(
        reg,
        "writer",
        "ns/p/app",
        &Digest::of(b"x").to_string(),
        &[&layer],
    )
    .await;
    assert_eq!(
        (r.status, r.code().as_str()),
        (StatusCode::BAD_REQUEST, "DIGEST_INVALID")
    );
    let r = call(
        reg,
        "writer",
        Method::PUT,
        "/v2/ns/p/app/manifests/bad",
        &[],
        b"{}",
    )
    .await;
    assert_eq!(
        (r.status, r.code().as_str()),
        (StatusCode::BAD_REQUEST, "MANIFEST_INVALID")
    );
}

#[tokio::test]
async fn every_route_enforces_the_grant() {
    let f = fixture("grants");
    let reg = &f.reg;
    let d = push_blob(reg, "writer", "ns/p/app", b"data").await;
    let r = put_index(reg, "writer", "ns/p/app", "t", &[&d]).await;
    assert_eq!(r.status, StatusCode::CREATED);
    // Below the granted repository is outside the grant.
    assert_eq!(
        open_upload(reg, "writer", "ns/p/app/sub").await.status,
        StatusCode::FORBIDDEN
    );

    // A read-only grant pulls but never pushes.
    let blob = format!("/v2/ns/p/app/blobs/{d}");
    assert_eq!(
        call(reg, "reader", Method::GET, &blob, &[], b"")
            .await
            .status,
        StatusCode::OK
    );
    let manifest = "/v2/ns/p/app/manifests/t";
    assert_eq!(
        call(reg, "reader", Method::GET, manifest, &[], b"")
            .await
            .status,
        StatusCode::OK
    );
    let denied = [
        call(
            reg,
            "reader",
            Method::POST,
            "/v2/ns/p/app/blobs/uploads/",
            &[],
            b"",
        )
        .await,
        put_index(reg, "reader", "ns/p/app", "t2", &[&d]).await,
    ];
    for r in &denied {
        assert_eq!(
            (r.status, r.code().as_str()),
            (StatusCode::FORBIDDEN, "DENIED")
        );
    }
    // A writer's upload cannot be continued by a reader.
    let r = call(
        reg,
        "writer",
        Method::POST,
        "/v2/ns/p/app/blobs/uploads/",
        &[],
        b"",
    )
    .await;
    let loc = r.header("location").to_owned();
    for m in [Method::PATCH, Method::PUT] {
        let r = call(reg, "reader", m, &format!("{loc}?digest={d}"), &[], b"x").await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
    }

    // Another scope gets nothing, whatever the reference.
    for (m, uri) in [
        (Method::GET, blob.clone()),
        (Method::HEAD, manifest.to_owned()),
        (Method::POST, "/v2/ns/p/app/blobs/uploads/".to_owned()),
    ] {
        let r = call(reg, "other", m, &uri, &[], b"").await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{uri}");
    }
    let r = call(
        reg,
        "writer",
        Method::GET,
        "/v2/ns/p/other/manifests/t",
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = call(reg, "nobody", Method::GET, &blob, &[], b"").await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn blobs_are_per_repository_and_never_mount_across() {
    let f = fixture("mount");
    let reg = &f.reg;
    let d = push_blob(reg, "other", "ns/p/other", b"secret layer").await;
    // Known by digest elsewhere is not visible here.
    let here = format!("/v2/ns/p/app/blobs/{d}");
    assert_eq!(
        call(reg, "writer", Method::HEAD, &here, &[], b"")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // A grant covers one repository, so a mount always falls back to an upload.
    let mount = format!("/v2/ns/p/app/blobs/uploads/?mount={d}&from=ns%2Fp%2Fother");
    let r = call(reg, "writer", Method::POST, &mount, &[], b"").await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    assert_eq!(
        call(reg, "writer", Method::HEAD, &here, &[], b"")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // A grant on the pool does not reach the repositories below it.
    let r = call(reg, "wide", Method::POST, &mount, &[], b"").await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = call(reg, "wide", Method::HEAD, &here, &[], b"").await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn names_and_routes_outside_the_subset_are_refused() {
    let f = fixture("routes");
    let reg = &f.reg;
    for (m, uri, status, code) in [
        (
            Method::GET,
            "/v2/ns/p/APP/manifests/t",
            StatusCode::BAD_REQUEST,
            "NAME_INVALID",
        ),
        (
            Method::GET,
            "/v2/ns/p/../manifests/t",
            StatusCode::BAD_REQUEST,
            "NAME_INVALID",
        ),
        (
            Method::GET,
            "/v2/ns/p/manifests/t",
            StatusCode::BAD_REQUEST,
            "NAME_INVALID",
        ),
        (
            Method::GET,
            "/v2/ns/p/app/manifests/bad:ref",
            StatusCode::BAD_REQUEST,
            "TAG_INVALID",
        ),
        (
            Method::GET,
            "/v2/ns/p/app/blobs/sha512:00",
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
        ),
        (
            Method::GET,
            "/v2/_catalog",
            StatusCode::NOT_FOUND,
            "UNSUPPORTED",
        ),
        (
            Method::GET,
            "/v2/ns/p/app/tags/list",
            StatusCode::NOT_FOUND,
            "UNSUPPORTED",
        ),
        (
            Method::DELETE,
            "/v2/ns/p/app/manifests/t",
            StatusCode::METHOD_NOT_ALLOWED,
            "UNSUPPORTED",
        ),
        (
            Method::GET,
            "/v2/ns/p/app/blobs/uploads/x",
            StatusCode::METHOD_NOT_ALLOWED,
            "UNSUPPORTED",
        ),
    ] {
        let r = call(reg, "writer", m, uri, &[], b"").await;
        assert_eq!((r.status, r.code().as_str()), (status, code), "{uri}");
    }
    let d = Digest::of(b"x");
    let r = call(
        reg,
        "writer",
        Method::POST,
        &format!("/v2/ns/p/app/blobs/uploads/?digest={d}"),
        &[],
        b"x",
    )
    .await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn the_store_survives_a_restart() {
    let dir = dir("restart");
    let d = {
        let reg = registry(&dir, 1 << 20);
        let d = push_blob(&reg, "writer", "ns/p/app", b"kept").await;
        assert_eq!(
            put_index(&reg, "writer", "ns/p/app", "t", &[&d])
                .await
                .status,
            StatusCode::CREATED
        );
        // An unfinished upload does not survive.
        call(
            &reg,
            "writer",
            Method::POST,
            "/v2/ns/p/app/blobs/uploads/",
            &[],
            b"",
        )
        .await;
        d
    };
    let reg = registry(&dir, 1 << 20);
    let r = call(
        &reg,
        "reader",
        Method::GET,
        "/v2/ns/p/app/manifests/t",
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = call(
        &reg,
        "reader",
        Method::GET,
        &format!("/v2/ns/p/app/blobs/{d}"),
        &[],
        b"",
    )
    .await;
    assert_eq!(&r.body[..], b"kept");
    assert_eq!(std::fs::read_dir(dir.join("uploads")).unwrap().count(), 0);
    let s = reg.store.stats().await;
    assert_eq!((s.blobs, s.manifests), (2, 1));
    std::fs::remove_dir_all(dir).unwrap();
}

async fn open_upload(reg: &Registry, pod: &str, repo: &str) -> Resp {
    call(
        reg,
        pod,
        Method::POST,
        &format!("/v2/{repo}/blobs/uploads/"),
        &[],
        b"",
    )
    .await
}

#[tokio::test]
async fn one_worker_cannot_hold_every_upload_slot() {
    let f = fixture("slots");
    let reg = &f.reg;
    for _ in 0..MAX_UPLOADS_PER_OWNER {
        assert_eq!(
            open_upload(reg, "writer", "ns/p/app").await.status,
            StatusCode::ACCEPTED
        );
    }
    let r = open_upload(reg, "writer", "ns/p/app").await;
    assert_eq!(
        (r.status, r.code().as_str()),
        (StatusCode::TOO_MANY_REQUESTS, "TOOMANYREQUESTS")
    );
    assert_eq!(
        open_upload(reg, "other", "ns/p/other").await.status,
        StatusCode::ACCEPTED
    );
    // Expiry gives the slots back.
    reg.store.expire_uploads(Duration::ZERO).await;
    assert_eq!(
        open_upload(reg, "writer", "ns/p/app").await.status,
        StatusCode::ACCEPTED
    );
}

#[tokio::test]
async fn open_uploads_are_bounded_in_bytes_per_worker_and_in_total() {
    let dir = dir("upload-bytes");
    // One worker may hold max(12 / 4, 8) = 8 bytes; all of them 12.
    let reg = Registry {
        store: Store::open(
            &dir,
            StoreLimits {
                max_blob: 8,
                max_upload_bytes: 12,
            },
        )
        .unwrap(),
        auth: Arc::new(grants()),
        metrics: RegistryMetrics::default(),
    };
    let reg = &reg;
    let patch = async |pod: &str, repo: &str, data: &[u8]| {
        let loc = open_upload(reg, pod, repo)
            .await
            .header("location")
            .to_owned();
        let r = call(reg, pod, Method::PATCH, &loc, &[], data).await;
        (loc, r)
    };
    let (first, r) = patch("writer", "ns/p/app", b"12345678").await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    // Over the worker's share: refused, and that upload ends.
    let (loc, r) = patch("writer", "ns/p/app", b"9").await;
    assert_eq!(
        (r.status, r.code().as_str()),
        (StatusCode::TOO_MANY_REQUESTS, "TOOMANYREQUESTS")
    );
    assert_eq!(
        call(reg, "writer", Method::PATCH, &loc, &[], b"")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // Over the total: another worker is refused too.
    let (_, r) = patch("other", "ns/p/other", b"12345").await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    let (_, r) = patch("other", "ns/p/other", b"1234").await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    // A finished upload gives its bytes back.
    let d = Digest::of(b"12345678");
    let r = call(
        reg,
        "writer",
        Method::PUT,
        &format!("{first}?digest={d}"),
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);
    // 4 held by the other worker plus 8 is exactly the total.
    let (_, r) = patch("writer", "ns/p/app", b"abcdefgh").await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn idle_uploads_expire() {
    let f = fixture("expire");
    let r = call(
        &f.reg,
        "writer",
        Method::POST,
        "/v2/ns/p/app/blobs/uploads/",
        &[],
        b"",
    )
    .await;
    let loc = r.header("location").to_owned();
    assert_eq!(f.reg.store.expire_uploads(UPLOAD_TTL).await, 0);
    assert_eq!(f.reg.store.expire_uploads(Duration::ZERO).await, 1);
    let r = call(&f.reg, "writer", Method::PATCH, &loc, &[], b"x").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(std::fs::read_dir(f.dir.join("uploads")).unwrap().count(), 0);
}

#[tokio::test]
async fn gc_sweeps_unreferenced_blobs_and_evicts_over_budget() {
    let f = fixture("gc");
    let reg = &f.reg;
    let kept = push_blob(reg, "writer", "ns/p/app", b"referenced").await;
    let orphan = push_blob(reg, "writer", "ns/p/app", b"orphan").await;
    assert_eq!(
        put_index(reg, "writer", "ns/p/app", "t", &[&kept])
            .await
            .status,
        StatusCode::CREATED
    );
    let blob_file = |d: &Digest| f.dir.join("blobs/sha256").join(d.hex());
    let big = FixedBudgets {
        pool: u64::MAX,
        total: u64::MAX,
    };

    // Within the grace period a fresh link holds even an unreferenced blob.
    let r = reg.store.gc(&big, UPLOAD_TTL).await;
    assert_eq!(r.deleted_blobs, 0);
    let r = reg.store.gc(&big, Duration::ZERO).await;
    assert_eq!((r.deleted_blobs, r.freed_bytes), (1, 6));
    assert!(!blob_file(&orphan).exists() && blob_file(&kept).exists());
    let head = |d: &Digest| format!("/v2/ns/p/app/blobs/{d}");
    assert_eq!(
        call(reg, "reader", Method::HEAD, &head(&orphan), &[], b"")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(reg, "reader", Method::HEAD, &head(&kept), &[], b"")
            .await
            .status,
        StatusCode::OK
    );

    // Over the pool budget the manifest goes, then everything it held.
    let none = FixedBudgets {
        pool: 0,
        total: u64::MAX,
    };
    let r = reg.store.gc(&none, Duration::ZERO).await;
    assert_eq!((r.evicted_pool, r.deleted_blobs), (1, 2));
    let r = call(
        reg,
        "reader",
        Method::HEAD,
        "/v2/ns/p/app/manifests/t",
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(!blob_file(&kept).exists());
    assert!(!f.dir.join("repos/ns/p/app/_tags/t").exists());
    assert_eq!(reg.store.stats().await, Stats::default());
}

/// Certificates for the TLS test, signed by one test CA.
mod certs {
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa,
        KeyPair, KeyUsagePurpose,
    };

    pub struct Ca(CertifiedIssuer<'static, KeyPair>);

    impl Ca {
        pub fn new() -> Self {
            let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
            p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
            Self(CertifiedIssuer::self_signed(p, KeyPair::generate().unwrap()).unwrap())
        }

        pub fn pem(&self) -> String {
            self.0.pem()
        }

        /// (cert PEM, key PEM) with one DNS SAN `name` and EKU `eku`.
        pub fn leaf(&self, name: &str, eku: ExtendedKeyUsagePurpose) -> (String, String) {
            let key = KeyPair::generate().unwrap();
            let mut p = CertificateParams::new(vec![name.to_owned()]).unwrap();
            p.extended_key_usages = vec![eku];
            (
                p.signed_by(&key, &self.0).unwrap().pem(),
                key.serialize_pem(),
            )
        }
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines, reason = "one end-to-end scenario")]
async fn the_listener_admits_only_worker_client_certs() {
    use rcgen::ExtendedKeyUsagePurpose::{ClientAuth, ServerAuth};
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let dir = dir("tls");
    let ca = certs::Ca::new();
    let (crt, key) = ca.leaf("localhost", ServerAuth);
    let tls = server_config(
        vec![CertificateDer::from_pem_slice(crt.as_bytes()).unwrap()],
        PrivateKeyDer::from_pem_slice(key.as_bytes()).unwrap(),
        CertificateDer::from_pem_slice(ca.pem().as_bytes()).unwrap(),
    )
    .unwrap();
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = tcp.local_addr().unwrap().port();
    let stop = tokio_util::sync::CancellationToken::new();
    let (_tx, rx) = tokio::sync::watch::channel(tls);
    let server = tokio::spawn(listener::serve(
        tcp,
        rx,
        Arc::new(registry(&dir, 1 << 20)),
        Limits::default(),
        stop.clone(),
    ));

    let client = |identity: Option<(String, String)>| {
        let mut b = reqwest::Client::builder()
            .use_rustls_tls()
            .tls_built_in_root_certs(false)
            .add_root_certificate(reqwest::Certificate::from_pem(ca.pem().as_bytes()).unwrap());
        if let Some((c, k)) = identity {
            b = b.identity(reqwest::Identity::from_pem(format!("{c}{k}").as_bytes()).unwrap());
        }
        b.build().unwrap()
    };
    let url = |p: &str| format!("https://localhost:{port}{p}");
    let writer = client(Some(ca.leaf(
        "0123456789abcdef.writer.p.ns.worker.buildkit.internal",
        ClientAuth,
    )));

    assert_eq!(writer.get(url("/v2/")).send().await.unwrap().status(), 200);
    // A blob through the socket: HEAD keeps its Content-Length.
    let data = b"over the wire".to_vec();
    let d = Digest::of(&data);
    let r = writer
        .post(url("/v2/ns/p/app/blobs/uploads/"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let loc = r.headers()["location"].to_str().unwrap().to_owned();
    let r = writer
        .put(url(&format!(
            "{loc}?digest={}",
            d.to_string().replace(':', "%3A")
        )))
        .body(data.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let r = writer
        .head(url(&format!("/v2/ns/p/app/blobs/{d}")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["content-length"],
        data.len().to_string().as_str()
    );
    let r = writer
        .get(url(&format!("/v2/ns/p/app/blobs/{d}")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.bytes().await.unwrap(), data);

    let other_ca = certs::Ca::new();
    for (name, c) in [
        ("no client cert", client(None)),
        (
            "serverAuth only",
            client(Some(ca.leaf(
                "0123456789abcdef.writer.p.ns.worker.buildkit.internal",
                ServerAuth,
            ))),
        ),
        (
            "not a worker",
            client(Some(ca.leaf("edge.example.com", ClientAuth))),
        ),
        (
            "another CA",
            client(Some(other_ca.leaf(
                "0123456789abcdef.writer.p.ns.worker.buildkit.internal",
                ClientAuth,
            ))),
        ),
    ] {
        assert!(
            c.get(url("/v2/")).send().await.is_err(),
            "{name} was admitted"
        );
    }

    stop.cancel();
    server.await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

/// Pod names are reused: the next incarnation of a pod does not inherit the
/// uploads, and so the quota, of the previous one.
#[tokio::test]
async fn a_new_incarnation_of_a_pod_has_its_own_upload_quota() {
    let f = fixture("incarnation");
    let reg = &f.reg;
    for _ in 0..MAX_UPLOADS_PER_OWNER {
        assert_eq!(
            open_upload(reg, "writer", "ns/p/app").await.status,
            StatusCode::ACCEPTED
        );
    }
    let next = WorkerIdentity {
        nonce: "fedcba9876543210".to_owned(),
        ..worker("writer")
    };
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v2/ns/p/app/blobs/uploads/")
        .body(Body::empty())
        .unwrap();
    assert_eq!(handle(reg, &next, req).await.status(), StatusCode::ACCEPTED);
}
