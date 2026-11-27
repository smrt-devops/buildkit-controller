//! Protocol, storage and garbage collection through [`handle`], and the mTLS
//! listener end to end.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use bytes::Bytes;

use object_store::ObjectStore;
use object_store::memory::InMemory;

use super::testing::Probe;
use super::*;
use crate::pki::WorkerIdentity;
use crate::registry::store::{MAX_UPLOADS_PER_OWNER, Parts};

fn worker(pod: &str) -> WorkerIdentity {
    WorkerIdentity {
        namespace: "ns".parse().unwrap(),
        pool: "p".parse().unwrap(),
        pod: pod.to_owned(),
        nonce: "0123456789abcdef".to_owned(),
    }
}

fn grants() -> StaticGrants {
    let g = |repository: &str, writes| Grant {
        repository: repository.to_owned(),
        writes,
    };
    let branch = |b: &str| Writes::Branch(b.to_owned());
    StaticGrants::default()
        .with("ns", "writer", g("ns/p/app", Writes::Scope))
        .with("ns", "reader", g("ns/p/app", Writes::Nothing))
        .with("ns", "other", g("ns/p/other", Writes::Scope))
        .with("ns", "wide", g("ns/p", Writes::Scope))
        .with("ns", "branch", g("ns/p/app", branch("ns/p/br")))
        .with("ns", "branch2", g("ns/p/app", branch("ns/p/br2")))
}

/// The two stores every test of the set runs on.
#[derive(Clone, Copy, Debug)]
enum Kind {
    /// `LocalFileSystem` on a directory, as the role opens a volume.
    Volume,
    /// `InMemory`, standing in for a bucket: uploads go through part buffers.
    Memory,
}

/// One store under test. Each [`Bed::os`] reopens it with the same
/// contents, as a restart of the role would.
struct Bed {
    kind: Kind,
    dir: PathBuf,
    mem: Arc<InMemory>,
}

impl Bed {
    fn new(kind: Kind, name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "bk-registry-{name}-{kind:?}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Self {
            kind,
            dir,
            mem: Arc::new(InMemory::new()),
        }
    }

    fn os(&self) -> Arc<dyn ObjectStore> {
        match self.kind {
            Kind::Volume => crate::registry::backend::open_volume(&self.dir).unwrap(),
            Kind::Memory => self.mem.clone(),
        }
    }

    fn parts(&self) -> Parts {
        match self.kind {
            Kind::Volume => Parts::Direct,
            Kind::Memory => Parts::Buffered,
        }
    }

    async fn registry(&self, max_blob: u64) -> Registry {
        self.registry_on(self.os(), limits(max_blob)).await
    }

    async fn registry_on(&self, os: Arc<dyn ObjectStore>, limits: StoreLimits) -> Registry {
        registry_with(Store::open(os, limits, self.parts()).await.unwrap())
    }
}

impl Drop for Bed {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Runs the test body once on each [`Kind`] of store, as
/// `<name>::volume` and `<name>::memory`, so both stores run one set.
macro_rules! store_test {
    ($(#[$attr:meta])* async fn $name:ident($kind:ident) $body:block) => {
        mod $name {
            #[allow(unused_imports)]
            use super::*;

            #[allow(clippy::too_many_lines)]
            async fn run($kind: Kind) $body

            $(#[$attr])*
            async fn volume() {
                run(Kind::Volume).await;
            }

            $(#[$attr])*
            async fn memory() {
                run(Kind::Memory).await;
            }
        }
    };
}

struct Fixture {
    reg: Registry,
    bed: Bed,
}

fn limits(max_blob: u64) -> StoreLimits {
    StoreLimits {
        max_blob,
        max_upload_bytes: 4 * max_blob,
    }
}

fn registry_with(store: Store) -> Registry {
    Registry {
        store,
        auth: Arc::new(grants()),
        metrics: RegistryMetrics::default(),
        manifest_puts: tokio::sync::Semaphore::new(crate::registry::api::MANIFEST_PUTS),
    }
}

async fn fixture(kind: Kind, name: &str) -> Fixture {
    fixture_max(kind, name, 1 << 20).await
}

async fn fixture_max(kind: Kind, name: &str, max_blob: u64) -> Fixture {
    let bed = Bed::new(kind, name);
    Fixture {
        reg: bed.registry(max_blob).await,
        bed,
    }
}

/// Files the volume at `dir` has staged for writes in progress.
fn staged(dir: &Path) -> usize {
    fn walk(d: &Path) -> usize {
        std::fs::read_dir(d).map_or(0, |rd| {
            rd.flatten()
                .map(|e| {
                    if e.path().is_dir() {
                        walk(&e.path())
                    } else {
                        usize::from(e.file_name().to_string_lossy().contains('#'))
                    }
                })
                .sum()
        })
    }
    walk(dir)
}

/// Whether the store holds a key that starts with `prefix`.
async fn has_key(reg: &Registry, prefix: &str) -> bool {
    reg.store
        .listed()
        .await
        .iter()
        .any(|k| k.starts_with(prefix))
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

store_test! {
#[tokio::test]
async fn base_answers_only_a_granted_worker(kind) {
    let f = fixture(kind, "base").await;
    let r = call(&f.reg, "writer", Method::GET, "/v2/", &[], b"").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("docker-distribution-api-version"), "registry/2.0");
    let r = call(&f.reg, "nobody", Method::GET, "/v2/", &[], b"").await;
    assert_eq!(
        (r.status, r.code().as_str()),
        (StatusCode::FORBIDDEN, "DENIED")
    );
}
}

store_test! {
#[tokio::test]
async fn chunked_upload_then_head_get_and_range(kind) {
    let f = fixture(kind, "chunked").await;
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
}

store_test! {
#[tokio::test]
async fn a_wrong_digest_stores_nothing(kind) {
    let f = fixture(kind, "mismatch").await;
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
    assert_eq!(staged(&f.bed.dir), 0);
    assert!(f.reg.store.listed().await.is_empty());
}
}

store_test! {
#[tokio::test]
async fn blobs_over_the_limit_are_refused_and_the_upload_ends(kind) {
    let f = fixture_max(kind, "large", 8).await;
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
    assert_eq!(staged(&f.bed.dir), 0);
}
}

store_test! {
#[tokio::test]
async fn manifests_need_their_blobs_and_resolve_by_tag_and_digest(kind) {
    let f = fixture(kind, "manifests").await;
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
}

store_test! {
#[tokio::test]
async fn every_route_enforces_the_grant(kind) {
    let f = fixture(kind, "grants").await;
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
}

store_test! {
#[tokio::test]
async fn blobs_are_per_repository_and_never_mount_across(kind) {
    let f = fixture(kind, "mount").await;
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
}

store_test! {
/// The branch caches table: an untrusted worker pulls the trusted scope and
/// pulls and pushes its own branch scope; a trusted worker never reaches a
/// branch scope; one branch never reaches another's.
#[tokio::test]
async fn branch_scopes_follow_the_table(kind) {
    let f = fixture(kind, "branch").await;
    let reg = &f.reg;
    let shared = push_blob(reg, "writer", "ns/p/app", b"base layer").await;
    assert_eq!(
        put_index(reg, "writer", "ns/p/app", "main", &[&shared])
            .await
            .status,
        StatusCode::CREATED
    );
    let status = |pod: &'static str, m: Method, uri: String| async move {
        call(reg, pod, m, &uri, &[], b"").await.status
    };
    // Pull the trusted scope, either tier.
    for pod in ["writer", "reader", "branch"] {
        let s = status(pod, Method::GET, "/v2/ns/p/app/manifests/main".into()).await;
        assert_eq!(s, StatusCode::OK, "{pod}");
    }
    // Push the trusted scope: trusted only.
    for pod in ["reader", "branch"] {
        let s = status(pod, Method::POST, "/v2/ns/p/app/blobs/uploads/".into()).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{pod}");
        let r = put_index(reg, pod, "ns/p/app", "main", &[&shared]).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{pod}");
    }
    // Pull and push its own branch scope: untrusted, and only its own.
    let own = push_blob(reg, "branch", "ns/p/br", b"branch layer").await;
    let again = push_blob(reg, "branch", "ns/p/br", b"base layer").await;
    assert_eq!(
        put_index(reg, "branch", "ns/p/br", "main", &[&own, &again])
            .await
            .status,
        StatusCode::CREATED
    );
    let s = status("branch", Method::GET, "/v2/ns/p/br/manifests/main".into()).await;
    assert_eq!(s, StatusCode::OK);
    for pod in ["writer", "reader", "branch2", "other", "wide"] {
        for (m, uri) in [
            (Method::GET, "/v2/ns/p/br/manifests/main".to_owned()),
            (Method::HEAD, format!("/v2/ns/p/br/blobs/{own}")),
            (Method::POST, "/v2/ns/p/br/blobs/uploads/".to_owned()),
        ] {
            assert_eq!(
                status(pod, m.clone(), uri.clone()).await,
                StatusCode::FORBIDDEN,
                "{pod} {m} {uri}"
            );
        }
        let r = put_index(reg, pod, "ns/p/br", "main", &[&shared]).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{pod}");
    }
    // A mount is a plain upload, and a branch manifest cannot reference a
    // blob that only another branch holds.
    let s = status(
        "branch2",
        Method::POST,
        format!("/v2/ns/p/br2/blobs/uploads/?mount={own}&from=ns%2Fp%2Fbr"),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let r = put_index(reg, "branch2", "ns/p/br2", "main", &[&own]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.body);
    // The layer both scopes hold is stored once.
    let markers = reg
        .store
        .listed()
        .await
        .into_iter()
        .filter(|k| k.starts_with("blobs/"))
        .count();
    assert_eq!(markers, 4, "two layers and two manifests");
}
}

store_test! {
/// A read of a branch scope that lacks a manifest or blob is answered from
/// the caller's trusted scope: HEAD and GET, tags and digests, ranges. Never
/// from another branch or repository, never for a push, and it writes
/// nothing into the branch scope.
#[tokio::test]
async fn a_branch_scope_reads_through_to_its_trusted_scope(kind) {
    let f = fixture(kind, "fallback").await;
    let reg = &f.reg;
    let shared = push_blob(reg, "writer", "ns/p/app", b"base layer").await;
    let r = put_index(reg, "writer", "ns/p/app", "main", &[&shared]).await;
    assert_eq!(r.status, StatusCode::CREATED);
    let index = r.header("docker-content-digest").to_owned();
    let elsewhere = push_blob(reg, "other", "ns/p/other", b"other repository").await;
    let theirs = push_blob(reg, "branch2", "ns/p/br2", b"other branch").await;
    let get = |pod: &'static str,
               m: Method,
               uri: String,
               h: &'static [(&'static str, &'static str)]| async move {
        call(reg, pod, m, &uri, h, b"").await
    };
    for m in [Method::HEAD, Method::GET] {
        for t in ["main", index.as_str()] {
            let r = get(
                "branch",
                m.clone(),
                format!("/v2/ns/p/br/manifests/{t}"),
                &[],
            )
            .await;
            assert_eq!(r.status, StatusCode::OK, "{m} {t}");
            assert_eq!(r.header("docker-content-digest"), index, "{m} {t}");
        }
        let r = get(
            "branch",
            m.clone(),
            format!("/v2/ns/p/br/blobs/{shared}"),
            &[],
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{m}");
    }
    let r = get(
        "branch",
        Method::GET,
        format!("/v2/ns/p/br/blobs/{shared}"),
        &[("range", "bytes=5-9")],
    )
    .await;
    assert_eq!(r.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(&r.body[..], b"layer");
    // Never another branch's or another repository's content.
    for d in [&elsewhere, &theirs] {
        let r = get("branch", Method::GET, format!("/v2/ns/p/br/blobs/{d}"), &[]).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{d}");
    }
    let r = put_index(reg, "branch2", "ns/p/br2", "main", &[&theirs]).await;
    assert_eq!(r.status, StatusCode::CREATED);
    let r = get(
        "branch",
        Method::GET,
        "/v2/ns/p/br/manifests/main".into(),
        &[],
    )
    .await;
    assert_eq!(
        r.header("docker-content-digest"),
        index,
        "not branch2's tag"
    );
    // A trusted or read-only worker gets no fallback, and a fallback read
    // created nothing in the branch scope.
    for pod in ["writer", "reader"] {
        let r = get(pod, Method::GET, "/v2/ns/p/br/manifests/main".into(), &[]).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{pod}");
    }
    assert!(reg.store.links("ns/p/br").is_empty());
    assert!(!has_key(reg, "repos/ns/p/br/").await);
    // A push names the branch scope only. A manifest may reference what the
    // trusted scope holds, and the branch then links it itself.
    let own = push_blob(reg, "branch", "ns/p/br", b"branch layer").await;
    let r = put_index(reg, "branch", "ns/p/br", "main", &[&shared, &own]).await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    let mine = r.header("docker-content-digest").to_owned();
    assert_ne!(mine, index);
    let r = get(
        "branch",
        Method::GET,
        "/v2/ns/p/br/manifests/main".into(),
        &[],
    )
    .await;
    assert_eq!(
        r.header("docker-content-digest"),
        mine,
        "its own manifest wins"
    );
    let r = get(
        "writer",
        Method::HEAD,
        format!("/v2/ns/p/app/blobs/{own}"),
        &[],
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::NOT_FOUND,
        "nothing reached the trusted scope"
    );
}
}

store_test! {
/// A branch manifest may reference a blob it has not uploaded only when the
/// caller's own trusted scope holds it. A blob that only another repository,
/// or another branch holds is refused, and a
/// refused push links nothing, not even the references it could borrow.
#[tokio::test]
async fn a_branch_manifest_borrows_only_from_its_own_trusted_scope(kind) {
    let f = fixture(kind, "borrow").await;
    let reg = &f.reg;
    let shared = push_blob(reg, "writer", "ns/p/app", b"base layer").await;
    let elsewhere = push_blob(reg, "other", "ns/p/other", b"other repository").await;
    let theirs = push_blob(reg, "branch2", "ns/p/br2", b"other branch").await;
    let link = |d: &Digest| reg.store.links("ns/p/br").contains(d);
    for d in [&elsewhere, &theirs] {
        for refs in [vec![d], vec![&shared, d]] {
            let r = put_index(reg, "branch", "ns/p/br", "main", &refs).await;
            assert_eq!(r.status, StatusCode::BAD_REQUEST, "{d}");
            assert_eq!(r.code(), "MANIFEST_BLOB_UNKNOWN", "{d}");
            assert!(!link(d), "{d}");
            assert!(!link(&shared), "{d}");
        }
    }
    let r = call(
        reg,
        "branch",
        Method::GET,
        "/v2/ns/p/br/manifests/main",
        &[],
        b"",
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::NOT_FOUND,
        "no branch manifest was stored"
    );
    // Only the caller's own trusted scope lends; branch2 borrows the same
    // layer from the same trusted scope into its own branch only.
    let r = put_index(reg, "branch", "ns/p/br", "main", &[&shared]).await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    assert!(link(&shared));
    assert!(!reg.store.links("ns/p/br2").contains(&shared));
}
}

store_test! {
#[tokio::test]
async fn names_and_routes_outside_the_subset_are_refused(kind) {
    let f = fixture(kind, "routes").await;
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
}

store_test! {
#[tokio::test]
async fn the_store_survives_a_restart(kind) {
    let bed = Bed::new(kind, "restart");
    let d = {
        let reg = bed.registry(1 << 20).await;
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
    let reg = bed.registry(1 << 20).await;
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
    assert_eq!(staged(&bed.dir), 0);
    let s = reg.store.stats();
    assert_eq!((s.blobs, s.manifests), (2, 1));
}
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

store_test! {
#[tokio::test]
async fn one_worker_cannot_hold_every_upload_slot(kind) {
    let f = fixture(kind, "slots").await;
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
}

store_test! {
#[tokio::test]
async fn open_uploads_are_bounded_in_bytes_per_worker_and_in_total(kind) {
    let bed = Bed::new(kind, "upload-bytes");
    // One worker may hold max(12 / 4, 8) = 8 bytes; all of them 12.
    let limits = StoreLimits {
        max_blob: 8,
        max_upload_bytes: 12,
    };
    let reg = bed.registry_on(bed.os(), limits).await;
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
}
}

store_test! {
#[tokio::test]
async fn idle_uploads_expire(kind) {
    let f = fixture(kind, "expire").await;
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
    assert_eq!(staged(&f.bed.dir), 0);
}
}

store_test! {
#[tokio::test]
async fn gc_sweeps_unreferenced_blobs_and_evicts_over_budget(kind) {
    let f = fixture(kind, "gc").await;
    let reg = &f.reg;
    let kept = push_blob(reg, "writer", "ns/p/app", b"referenced").await;
    let orphan = push_blob(reg, "writer", "ns/p/app", b"orphan").await;
    assert_eq!(
        put_index(reg, "writer", "ns/p/app", "t", &[&kept])
            .await
            .status,
        StatusCode::CREATED
    );
    let stored = async |d: &Digest| has_key(reg, &format!("blobs/sha256/{}/", d.hex())).await;
    let big = FixedBudgets {
        pool: u64::MAX,
        total: u64::MAX,
    };

    // Within the grace period a fresh link holds even an unreferenced blob.
    let r = reg.store.gc(&big, UPLOAD_TTL).await;
    assert_eq!(r.deleted_blobs, 0);
    let r = reg.store.gc(&big, Duration::ZERO).await;
    assert_eq!((r.deleted_blobs, r.freed_bytes), (1, 6));
    assert!(!stored(&orphan).await && stored(&kept).await);
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
    assert!(!stored(&kept).await);
    assert!(!has_key(reg, "repos/ns/p/app/_tags/t/").await);
    assert_eq!(reg.store.keys(), reg.store.listed().await);
    assert_eq!(reg.store.stats(), Stats::default());
}
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

store_test! {
#[tokio::test]
#[allow(clippy::too_many_lines, reason = "one end-to-end scenario")]
async fn the_listener_admits_only_worker_client_certs(kind) {
    use rcgen::ExtendedKeyUsagePurpose::{ClientAuth, ServerAuth};
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let bed = Bed::new(kind, "tls");
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
        Arc::new(bed.registry(1 << 20).await),
        Limits::default(),
        stop.clone(),
    ));

    let client = |identity: Option<(String, String)>| {
        let mut b = reqwest::Client::builder()
            .tls_certs_only([reqwest::Certificate::from_pem(ca.pem().as_bytes()).unwrap()]);
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
}
}

store_test! {
/// Pod names are reused: the next incarnation of a pod does not inherit the
/// uploads, and so the quota, of the previous one.
#[tokio::test]
async fn a_new_incarnation_of_a_pod_has_its_own_upload_quota(kind) {
    let f = fixture(kind, "incarnation").await;
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
}

store_test! {
/// Workers that drip manifest bodies cannot hold up another worker's
/// manifest push: it is answered 429 at once while they hold
/// every permit, and they are cut off at `MANIFEST_BODY`.
#[tokio::test(start_paused = true)]
async fn slow_manifest_pushes_cannot_block_others(kind) {
    use crate::registry::api::{MANIFEST_BODY, MANIFEST_PUTS};
    use futures::StreamExt;
    let bed = Bed::new(kind, "slow-manifest");
    let reg = Arc::new(bed.registry(1 << 20).await);
    let d = push_blob(&reg, "writer", "ns/p/app", b"layer").await;
    let mut drips = Vec::new();
    for _ in 0..MANIFEST_PUTS {
        let reg = Arc::clone(&reg);
        drips.push(tokio::spawn(async move {
            let body =
                futures::stream::once(async { Ok::<_, std::io::Error>(Bytes::from_static(b"{")) })
                    .chain(futures::stream::pending());
            let req = Request::builder()
                .method(Method::PUT)
                .uri("/v2/ns/p/app/manifests/slow")
                .header("content-type", INDEX)
                .body(Body::from_stream(body))
                .unwrap();
            handle(&reg, &worker("writer"), req).await.status()
        }));
    }
    while reg.manifest_puts.available_permits() > 0 {
        tokio::task::yield_now().await;
    }
    let started = tokio::time::Instant::now();
    let r = put_index(&reg, "other", "ns/p/other", "t", &[]).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.header("retry-after"), "1");
    assert_eq!(started.elapsed(), Duration::ZERO);
    for d in drips {
        assert_eq!(d.await.unwrap(), StatusCode::REQUEST_TIMEOUT);
    }
    assert!(started.elapsed() <= MANIFEST_BODY + Duration::from_secs(1));
    let r = put_index(&reg, "writer", "ns/p/app", "t", &[&d]).await;
    assert_eq!(r.status, StatusCode::CREATED);
}
}

const BIG: FixedBudgets = FixedBudgets {
    pool: u64::MAX,
    total: u64::MAX,
};

/// Runs a collection in the background and returns once it has applied its
/// plan to the index, while the probe holds its deletes.
async fn collect_held(
    reg: &Arc<Registry>,
    probe: &Probe,
    budgets: FixedBudgets,
    applied: impl Fn(Stats) -> bool,
) -> tokio::task::JoinHandle<crate::registry::store::GcReport> {
    probe.hold_deletes();
    let gc = tokio::spawn({
        let reg = Arc::clone(reg);
        async move { reg.store.gc(&budgets, Duration::ZERO).await }
    });
    while !applied(reg.store.stats()) {
        tokio::task::yield_now().await;
    }
    gc
}

store_test! {
/// Collection removes digest D from the index, and a re-upload of D lands
/// before the collection deletes D's old objects. The re-upload writes fresh
/// keys, so the old deletes do not touch it.
#[tokio::test]
async fn gc_racing_a_reupload_of_a_blob_keeps_the_index_and_store_agreeing(kind) {
    let bed = Bed::new(kind, "race-blob");
    let probe = Arc::new(Probe::new(bed.os()));
    let reg = Arc::new(bed.registry_on(probe.clone(), limits(1 << 20)).await);
    let d = push_blob(&reg, "writer", "ns/p/app", b"layer").await;
    let gc = collect_held(&reg, &probe, BIG, |s| s.blobs == 0).await;
    push_blob(&reg, "writer", "ns/p/app", b"layer").await;
    probe.release_deletes();
    assert_eq!(gc.await.unwrap().deleted_blobs, 1);
    assert_eq!(reg.store.keys(), reg.store.listed().await);
    let r = call(
        &reg,
        "reader",
        Method::GET,
        &format!("/v2/ns/p/app/blobs/{d}"),
        &[],
        b"",
    )
    .await;
    assert_eq!(&r.body[..], b"layer");
}
}

store_test! {
/// The same for a manifest and its tag: collection evicts them, the push
/// comes again before the old record and tag marker are deleted, and the
/// new ones survive, in the index, in the store and after a restart.
#[tokio::test]
async fn gc_racing_a_repush_of_a_manifest_and_tag_keeps_the_index_and_store_agreeing(kind) {
    let bed = Bed::new(kind, "race-manifest");
    let probe = Arc::new(Probe::new(bed.os()));
    let reg = Arc::new(bed.registry_on(probe.clone(), limits(1 << 20)).await);
    let l = push_blob(&reg, "writer", "ns/p/app", b"layer").await;
    let r = put_index(&reg, "writer", "ns/p/app", "t", &[&l]).await;
    assert_eq!(r.status, StatusCode::CREATED);
    let none = FixedBudgets {
        pool: 0,
        total: u64::MAX,
    };
    let gc = collect_held(&reg, &probe, none, |s| s.manifests == 0 && s.blobs == 0).await;
    push_blob(&reg, "writer", "ns/p/app", b"layer").await;
    let r = put_index(&reg, "writer", "ns/p/app", "t", &[&l]).await;
    assert_eq!(r.status, StatusCode::CREATED);
    probe.release_deletes();
    assert_eq!(gc.await.unwrap().evicted_pool, 1);
    assert_eq!(reg.store.keys(), reg.store.listed().await);
    let get = async |reg: &Registry| {
        call(
            reg,
            "reader",
            Method::GET,
            "/v2/ns/p/app/manifests/t",
            &[],
            b"",
        )
        .await
    };
    assert_eq!(&get(&reg).await.body[..], &index_of(&[&l])[..]);
    drop(reg);
    let reg = bed.registry(1 << 20).await;
    assert_eq!(&get(&reg).await.body[..], &index_of(&[&l])[..]);
    assert_eq!(reg.store.keys(), reg.store.listed().await);
}
}

store_test! {
/// Unscripted: collections and re-uploads of one digest run at once, again
/// and again, and the index and the store agree after each round.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_and_reuploads_running_at_once_keep_the_index_and_store_agreeing(kind) {
    let f = fixture(kind, "race-free").await;
    let reg = &f.reg;
    for _ in 0..20 {
        let (_, d) = tokio::join!(
            reg.store.gc(&BIG, Duration::ZERO),
            push_blob(reg, "writer", "ns/p/app", b"layer")
        );
        let r = put_index(reg, "writer", "ns/p/app", "t", &[&d]).await;
        assert!(
            [StatusCode::CREATED, StatusCode::BAD_REQUEST].contains(&r.status),
            "{:?}",
            r.body
        );
        assert_eq!(reg.store.keys(), reg.store.listed().await);
    }
}
}

store_test! {
/// At open, a data object no marker names is an orphan, and keys that name
/// nothing stored go too: a marker without data, a second marker for a
/// stored digest, a record of a missing manifest, a tag of a missing
/// manifest. The first collection, which the role runs at start, deletes
/// them; what the store holds is still served.
#[tokio::test]
async fn an_orphan_data_object_is_deleted_at_open(kind) {
    use object_store::{ObjectStoreExt, PutPayload};
    let bed = Bed::new(kind, "orphan");
    let d = {
        let reg = bed.registry(1 << 20).await;
        let d = push_blob(&reg, "writer", "ns/p/app", b"kept").await;
        let r = put_index(&reg, "writer", "ns/p/app", "t", &[&d]).await;
        assert_eq!(r.status, StatusCode::CREATED);
        d
    };
    let id = || crate::registry::layout::Id::random().unwrap();
    let missing = Digest::of(b"missing");
    let os = bed.os();
    let (orphan, extra) = (id(), id());
    let junk = [
        format!("data/{orphan}"),
        format!("data/{extra}"),
        format!("blobs/sha256/{}/{}", missing.hex(), id()),
        format!("blobs/sha256/{}/{extra}", d.hex()),
        format!("repos/ns/p/app/_manifests/{}/{}", missing.hex(), id()),
        format!("repos/ns/p/app/_tags/t2/{}/{}", missing.hex(), id()),
    ];
    for k in &junk {
        // A second marker of a stored digest names the same verified bytes.
        let body = if k.starts_with("data/") { "kept" } else { "" };
        os.put(&k.as_str().into(), PutPayload::from(body))
            .await
            .unwrap();
    }
    let reg = bed.registry(1 << 20).await;
    let listed = reg.store.listed().await;
    assert!(junk.iter().all(|k| listed.contains(k)));
    reg.store.gc(&BIG, UPLOAD_TTL).await;
    let listed = reg.store.listed().await;
    // Of two markers for one digest the open keeps one, either one.
    let markers_of_d = listed
        .iter()
        .filter(|k| k.starts_with(&format!("blobs/sha256/{}/", d.hex())))
        .count();
    assert_eq!(markers_of_d, 1, "{listed:?}");
    let duplicate = [
        format!("data/{extra}"),
        format!("blobs/sha256/{}/{extra}", d.hex()),
    ];
    assert!(
        !junk
            .iter()
            .filter(|k| !duplicate.contains(k))
            .any(|k| listed.contains(k)),
        "{listed:?}"
    );
    assert_eq!(reg.store.keys(), listed);
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
    let s = reg.store.stats();
    assert_eq!((s.blobs, s.manifests), (2, 1));
}
}

store_test! {
/// The open lists the store and reads each manifest record once: never a
/// manifest body, a blob or a tag.
#[tokio::test]
async fn the_open_reads_no_manifest_body_and_no_tag(kind) {
    let bed = Bed::new(kind, "open-gets");
    {
        let reg = bed.registry(1 << 20).await;
        let a = push_blob(&reg, "writer", "ns/p/app", b"a").await;
        let b = push_blob(&reg, "writer", "ns/p/app", b"b").await;
        for (tag, refs) in [("t1", vec![&a]), ("t2", vec![&a, &b])] {
            let r = put_index(&reg, "writer", "ns/p/app", tag, &refs).await;
            assert_eq!(r.status, StatusCode::CREATED);
        }
        let r = put_index(&reg, "branch", "ns/p/br", "t1", &[&a]).await;
        assert_eq!(r.status, StatusCode::CREATED);
    }
    let probe = Arc::new(Probe::new(bed.os()));
    let reg = bed.registry_on(probe.clone(), limits(1 << 20)).await;
    let gets = probe.gets();
    assert_eq!(gets.len(), 3, "{gets:?}");
    assert!(gets.iter().all(|k| k.contains("/_manifests/")), "{gets:?}");
    // Links came back from the records: the branch reads its borrowed layer.
    let r = call(
        &reg,
        "branch",
        Method::GET,
        &format!("/v2/ns/p/br/blobs/{}", Digest::of(b"a")),
        &[],
        b"",
    )
    .await;
    assert_eq!(&r.body[..], b"a");
    let r = call(
        &reg,
        "reader",
        Method::GET,
        "/v2/ns/p/app/manifests/t2",
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
}
}

/// A 0.4 volume: its files sit where 0.5 needs directories. The open removes
/// every file without a 0.5 key shape before it serves, so a push of the same
/// digest and tag succeeds.
#[tokio::test]
async fn the_0_4_tree_is_swept_before_serving() {
    let bed = Bed::new(Kind::Volume, "sweep");
    let dir = &bed.dir;
    let d = Digest::of(b"layer");
    let old = [
        format!("blobs/sha256/{}", d.hex()),
        format!("repos/ns/p/app/_links/{}", d.hex()),
        format!("repos/ns/p/app/_manifests/{}", d.hex()),
        "repos/ns/p/app/_tags/t".to_owned(),
        "uploads/0123".to_owned(),
        "tmp/gc-0123".to_owned(),
        format!("data/{}#7", crate::registry::layout::Id::random().unwrap()),
    ];
    for f in &old {
        let p = dir.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"0.4").unwrap();
    }
    let reg = bed.registry(1 << 20).await;
    for f in &old {
        assert!(!dir.join(f).exists(), "{f}");
    }
    for gone in ["uploads", "tmp", "repos"] {
        assert!(!dir.join(gone).exists(), "{gone}");
    }
    assert_eq!(push_blob(&reg, "writer", "ns/p/app", b"layer").await, d);
    let r = put_index(&reg, "writer", "ns/p/app", "t", &[&d]).await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    assert_eq!(reg.store.keys(), reg.store.listed().await);
}

store_test! {
/// A store that refuses writes answers 503 with a retry hint, and the upload
/// leaves nothing behind.
#[tokio::test]
async fn a_failing_store_answers_503_and_keeps_nothing(kind) {
    let bed = Bed::new(kind, "unavailable");
    let probe = Arc::new(Probe::new(bed.os()));
    let reg = bed.registry_on(probe.clone(), limits(1 << 20)).await;
    probe.fail_puts(true);
    let loc = open_upload(&reg, "writer", "ns/p/app")
        .await
        .header("location")
        .to_owned();
    let d = Digest::of(b"data");
    let r = call(
        &reg,
        "writer",
        Method::PUT,
        &format!("{loc}?digest={d}"),
        &[],
        b"data",
    )
    .await;
    assert_eq!(
        (r.status, r.code().as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, "UNKNOWN")
    );
    assert_eq!(r.header("retry-after"), "5");
    let r = put_index(&reg, "writer", "ns/p/app", "t", &[]).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(reg.store.listed().await.is_empty());
    assert_eq!(staged(&bed.dir), 0);
}
}

/// Another incarnation of the writer pod: its own owner of uploads.
fn incarnation(n: usize) -> WorkerIdentity {
    WorkerIdentity {
        nonce: format!("{n:016x}"),
        ..worker("writer")
    }
}

async fn send(
    reg: &Registry,
    who: &WorkerIdentity,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Body,
) -> axum::http::Response<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    handle(reg, who, b.body(body).unwrap()).await
}

/// Opens an upload as `who` and returns its location.
async fn open_as(reg: &Registry, who: &WorkerIdentity) -> String {
    let r = send(
        reg,
        who,
        Method::POST,
        "/v2/ns/p/app/blobs/uploads/",
        &[],
        Body::empty(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    r.headers()["location"].to_str().unwrap().to_owned()
}

async fn patch_as(
    reg: &Registry,
    who: &WorkerIdentity,
    loc: &str,
    data: &'static [u8],
) -> StatusCode {
    send(reg, who, Method::PATCH, loc, &[], Body::from(data))
        .await
        .status()
}

/// On a bucket an upload takes a part buffer at its first byte. One worker
/// holds at most a quarter of them. Past that, or with none free, a request
/// gets 429 with `Retry-After` before its body is read, and its upload stays
/// open for the client to repeat the request. An upload idle for
/// `PART_IDLE` gives its buffer back, at the next request that wants one
/// and at each collection.
#[tokio::test(start_paused = true)]
async fn part_buffers_are_bounded_per_worker_and_in_total() {
    use crate::registry::store::{PART_IDLE, PART_PERMITS, PART_PERMITS_PER_OWNER};
    use std::sync::atomic::{AtomicBool, Ordering};
    let f = fixture(Kind::Memory, "permits").await;
    let reg = &f.reg;
    let first = incarnation(0);
    let mut held = Vec::new();
    for _ in 0..PART_PERMITS_PER_OWNER {
        let loc = open_as(reg, &first).await;
        assert_eq!(
            patch_as(reg, &first, &loc, b"x").await,
            StatusCode::ACCEPTED
        );
        held.push(loc);
    }
    // Over the worker's share: refused before the body is read.
    let extra = open_as(reg, &first).await;
    let read = Arc::new(AtomicBool::new(false));
    let body = Body::from_stream(futures::stream::once({
        let read = Arc::clone(&read);
        async move {
            read.store(true, Ordering::Relaxed);
            Ok::<_, std::io::Error>(Bytes::from_static(b"y"))
        }
    }));
    let r = send(reg, &first, Method::PATCH, &extra, &[], body).await;
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.headers()["retry-after"], "1");
    assert!(!read.load(Ordering::Relaxed));
    // Other workers take the rest, and then none is free for anyone.
    for n in 1..PART_PERMITS / PART_PERMITS_PER_OWNER {
        let who = incarnation(n);
        for _ in 0..PART_PERMITS_PER_OWNER {
            let loc = open_as(reg, &who).await;
            assert_eq!(patch_as(reg, &who, &loc, b"x").await, StatusCode::ACCEPTED);
        }
    }
    let late = incarnation(99);
    let waiting = open_as(reg, &late).await;
    assert_eq!(
        patch_as(reg, &late, &waiting, b"z").await,
        StatusCode::TOO_MANY_REQUESTS
    );
    // A request that declares no body needs no buffer.
    let r = send(
        reg,
        &late,
        Method::PATCH,
        &waiting,
        &[("content-length", "0")],
        Body::empty(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    // A finished upload gives its buffer back, and the refused upload,
    // still open and empty, takes it: it holds only the repeated byte.
    let d = Digest::of(b"x");
    let put = format!("{}?digest={d}", held[0]);
    let r = send(reg, &first, Method::PUT, &put, &[], Body::empty()).await;
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(
        patch_as(reg, &first, &extra, b"y").await,
        StatusCode::ACCEPTED
    );
    let put = format!("{extra}?digest={}", Digest::of(b"y"));
    let r = send(reg, &first, Method::PUT, &put, &[], Body::empty()).await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let other = incarnation(50);
    let loc = open_as(reg, &other).await;
    assert_eq!(
        patch_as(reg, &other, &loc, b"x").await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        patch_as(reg, &late, &waiting, b"z").await,
        StatusCode::TOO_MANY_REQUESTS
    );
    // Workers reaped mid-export hold their buffers for PART_IDLE at most.
    tokio::time::advance(PART_IDLE).await;
    assert_eq!(
        patch_as(reg, &late, &waiting, b"z").await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        patch_as(reg, &first, &held[1], b"x").await,
        StatusCode::NOT_FOUND
    );
    tokio::time::advance(PART_IDLE).await;
    assert_eq!(reg.store.expire_uploads(UPLOAD_TTL).await, 1);
    assert_eq!(
        patch_as(reg, &late, &waiting, b"z").await,
        StatusCode::NOT_FOUND
    );
}

store_test! {
/// A blob that fills parts goes as a multipart upload; on a bucket a blob
/// that never fills one goes as one put. On the volume every chunk is a part.
#[tokio::test]
async fn large_blobs_go_in_parts_and_small_ones_whole(kind) {
    let bed = Bed::new(kind, "parts");
    let probe = Arc::new(Probe::new(bed.os()));
    let reg = bed.registry_on(probe.clone(), limits(16 << 20)).await;
    push_blob(&reg, "writer", "ns/p/app", b"small").await;
    let small = probe.multiparts();
    let big: Vec<u8> = (0..11 << 20).map(|i: u32| (i % 251) as u8).collect();
    let d = Digest::of(&big);
    let r = open_upload(&reg, "writer", "ns/p/app").await;
    let loc = r.header("location").to_owned();
    let (head, tail) = big.split_at(7 << 20);
    let r = call(&reg, "writer", Method::PATCH, &loc, &[], head).await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    let put = format!("{loc}?digest={d}");
    let r = call(&reg, "writer", Method::PUT, &put, &[], tail).await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    let uri = format!("/v2/ns/p/app/blobs/{d}");
    let r = call(&reg, "reader", Method::GET, &uri, &[], b"").await;
    assert!(r.body[..] == big[..]);
    let want = match kind {
        Kind::Memory => (0, 1),
        Kind::Volume => (1, 2),
    };
    assert_eq!((small, probe.multiparts()), want);
    assert_eq!(reg.store.keys(), reg.store.listed().await);
}
}

store_test! {
/// A store that fails every call: the open retries with backoff, and
/// `registry_store` stays not ready until the first open that succeeds.
/// After it, a manifest GET gets 503 with `Retry-After`, while HEAD answers
/// from the index.
#[tokio::test(start_paused = true)]
async fn a_failing_store_keeps_the_registry_not_ready_and_answers_503(kind) {
    use crate::runtime::readiness::Readiness;
    let bed = Bed::new(kind, "outage");
    let probe = Arc::new(Probe::new(bed.os()));
    let l = {
        let reg = bed.registry_on(probe.clone(), limits(1 << 20)).await;
        let l = push_blob(&reg, "writer", "ns/p/app", b"layer").await;
        let r = put_index(&reg, "writer", "ns/p/app", "t", &[&l]).await;
        assert_eq!(r.status, StatusCode::CREATED);
        l
    };
    probe.fail_all(true);
    let readiness = Readiness::default();
    let flag = readiness.register("registry_store");
    let stop = tokio_util::sync::CancellationToken::new();
    let open = tokio::spawn({
        let (os, parts): (Arc<dyn ObjectStore>, _) = (probe.clone(), bed.parts());
        let (flag, stop) = (flag.clone(), stop.clone());
        async move { crate::registry::open_store(os, limits(1 << 20), parts, &flag, &stop).await }
    });
    tokio::time::sleep(Duration::from_mins(2)).await;
    assert!(!open.is_finished());
    assert_eq!(readiness.failing(), ["registry_store"]);
    probe.fail_all(false);
    let reg = registry_with(open.await.unwrap().unwrap());
    assert!(readiness.failing().is_empty());
    probe.fail_all(true);
    let r = call(&reg, "reader", Method::GET, "/v2/ns/p/app/manifests/t", &[], b"").await;
    assert_eq!(
        (r.status, r.code().as_str(), r.header("retry-after")),
        (StatusCode::SERVICE_UNAVAILABLE, "UNKNOWN", "5")
    );
    let blob = format!("/v2/ns/p/app/blobs/{l}");
    let r = call(&reg, "reader", Method::GET, &blob, &[], b"").await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    for uri in ["/v2/ns/p/app/manifests/t", blob.as_str()] {
        let r = call(&reg, "reader", Method::HEAD, uri, &[], b"").await;
        assert_eq!(r.status, StatusCode::OK, "{uri}");
    }
    // Shutdown ends an open that is still retrying.
    let open = tokio::spawn({
        let os: Arc<dyn ObjectStore> = probe.clone();
        let (flag, stop, parts) = (flag.clone(), stop.clone(), bed.parts());
        async move { crate::registry::open_store(os, limits(1 << 20), parts, &flag, &stop).await }
    });
    stop.cancel();
    assert!(open.await.unwrap().is_none());
}
}

store_test! {
/// A collection dies between its delete rounds. A fresh open on the same
/// store, and the first collection, which the role runs at start, leave no
/// object that no marker, manifest or record reaches.
#[tokio::test]
async fn a_crash_between_delete_rounds_leaves_nothing_a_restart_cannot_clean(kind) {
    for rounds in 1..=2 {
        let bed = Bed::new(kind, &format!("crash-{rounds}"));
        let probe = Arc::new(Probe::new(bed.os()));
        let reg = bed.registry_on(probe.clone(), limits(1 << 20)).await;
        let l = push_blob(&reg, "writer", "ns/p/app", b"layer").await;
        let r = put_index(&reg, "writer", "ns/p/app", "t", &[&l]).await;
        assert_eq!(r.status, StatusCode::CREATED);
        probe.crash_after_deletes(rounds);
        let none = FixedBudgets {
            pool: 0,
            total: u64::MAX,
        };
        assert_eq!(reg.store.gc(&none, Duration::ZERO).await.evicted_pool, 1);
        assert!(!reg.store.listed().await.is_empty(), "round {rounds}");
        drop(reg);
        let reg = bed.registry(1 << 20).await;
        assert!(reg.store.keys().is_subset(&reg.store.listed().await));
        reg.store.gc(&BIG, UPLOAD_TTL).await;
        let listed = reg.store.listed().await;
        assert!(listed.is_empty(), "round {rounds}: {listed:?}");
        assert_eq!(reg.store.stats(), Stats::default());
    }
}
}

store_test! {
/// A pull sets the last-used time in memory; the record in the store gets
/// it at the next collection, and not before.
#[tokio::test]
async fn a_pull_reaches_the_record_at_the_next_collection(kind) {
    let f = fixture(kind, "last-used").await;
    let reg = &f.reg;
    let l = push_blob(reg, "writer", "ns/p/app", b"layer").await;
    assert_eq!(
        put_index(reg, "writer", "ns/p/app", "t", &[&l]).await.status,
        StatusCode::CREATED
    );
    reg.store.set_last_used(1);
    reg.store.gc(&BIG, UPLOAD_TTL).await;
    assert_eq!(reg.store.recorded_times().await, [1]);
    let r = call(reg, "reader", Method::GET, "/v2/ns/p/app/manifests/t", &[], b"").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(reg.store.recorded_times().await, [1]);
    reg.store.gc(&BIG, UPLOAD_TTL).await;
    let times = reg.store.recorded_times().await;
    assert!(times.len() == 1 && times[0] > 1, "{times:?}");
    assert_eq!(reg.store.keys(), reg.store.listed().await);
}
}
