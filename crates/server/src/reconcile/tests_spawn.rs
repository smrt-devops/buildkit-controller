//! [`super::spawn`] against a recording API server: the requests a real
//! `kube::Client` sends to create, re-own and delete a worker Secret.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use hyper::{Request, Response, StatusCode};
use k8s_openapi::api::core::v1::Pod;
use kube::client::Body;
use serde_json::{Value, json};
use time::OffsetDateTime;

use super::ops::{KubeClient, KubeOps};
use super::spawn::{self, Worker};
use crate::pki::CaBundle;
use crate::render::fixtures;

/// One request: method, path, content type and JSON body.
#[derive(Clone, Debug)]
struct Call {
    method: String,
    path: String,
    content_type: String,
    body: Value,
}

/// Answers every request from `reply` and records it.
fn server(reply: fn(&Call) -> (StatusCode, Value)) -> (KubeClient, Arc<Mutex<Vec<Call>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let log = calls.clone();
    let svc = tower::service_fn(move |req: Request<Body>| {
        let log = log.clone();
        async move {
            let (parts, body) = req.into_parts();
            let bytes = body.collect_bytes().await.unwrap();
            let call = Call {
                method: parts.method.to_string(),
                path: parts.uri.path().to_owned(),
                content_type: parts
                    .headers
                    .get("content-type")
                    .map(|v| v.to_str().unwrap().to_owned())
                    .unwrap_or_default(),
                body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            };
            let (status, out) = reply(&call);
            log.lock().unwrap().push(call);
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(Bytes::from(out.to_string())))
                    .unwrap(),
            )
        }
    });
    (KubeClient(kube::Client::new(svc, "default")), calls)
}

fn status(code: u16, reason: &str) -> Value {
    json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "code": code, "reason": reason, "message": reason})
}

/// Echoes creates (a pod gets a uid), accepts patches and deletes.
fn happy(c: &Call) -> (StatusCode, Value) {
    match (c.method.as_str(), c.path.ends_with("/pods")) {
        ("POST", true) => {
            let mut p = c.body.clone();
            p["metadata"]["uid"] = json!("pod-uid-1");
            (StatusCode::CREATED, p)
        }
        ("POST", false) => (StatusCode::CREATED, c.body.clone()),
        ("PATCH", _) => (
            StatusCode::OK,
            json!({"kind": "Secret", "apiVersion": "v1", "metadata": {"name": "x"}}),
        ),
        ("DELETE", _) => (
            StatusCode::OK,
            json!({"kind": "Status", "apiVersion": "v1", "status": "Success"}),
        ),
        _ => (StatusCode::FORBIDDEN, status(403, "Forbidden")),
    }
}

/// Like [`happy`], but the pod's slot name is taken.
fn slot_taken(c: &Call) -> (StatusCode, Value) {
    if c.method == "POST" && c.path.ends_with("/pods") {
        return (StatusCode::CONFLICT, status(409, "AlreadyExists"));
    }
    happy(c)
}

/// The pod create hits a `ResourceQuota` (403).
fn quota_exceeded(c: &Call) -> (StatusCode, Value) {
    if c.method == "POST" && c.path.ends_with("/pods") {
        return (StatusCode::FORBIDDEN, status(403, "Forbidden"));
    }
    happy(c)
}

/// The pod create fails with a 503: the pod may exist.
fn unavailable(c: &Call) -> (StatusCode, Value) {
    if c.method == "POST" && c.path.ends_with("/pods") {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            status(503, "ServiceUnavailable"),
        );
    }
    happy(c)
}

/// The Secret create is refused.
fn secret_refused(c: &Call) -> (StatusCode, Value) {
    if c.method == "POST" && c.path.ends_with("/secrets") {
        return (StatusCode::FORBIDDEN, status(403, "Forbidden"));
    }
    happy(c)
}

fn worker() -> Worker {
    let pool = fixtures::pool("p", json!({})).unwrap();
    let now = OffsetDateTime::now_utc();
    let ca = CaBundle::generate(now).unwrap();
    spawn::prepare(&pool, &ca, 0, None, now, time::Duration::days(30)).unwrap()
}

async fn run(ops: &KubeClient, w: Worker) -> Result<super::ops::OpResult<Pod>, String> {
    let ns = w.pod.metadata.namespace.clone().unwrap();
    spawn::spawn(ops, w, |pod| async move { ops.create_pod(&ns, &pod).await }).await
}

fn summary(calls: &[Call]) -> Vec<String> {
    calls
        .iter()
        .map(|c| format!("{} {}", c.method, c.path))
        .collect()
}

#[tokio::test]
async fn reown_moves_the_secret_to_the_pod_without_a_read() {
    let (ops, calls) = server(happy);
    let w = worker();
    let (secret, pool_uid) = (
        w.secret.metadata.name.clone().unwrap(),
        "2f6c1f4e-3b0a-4c43-9a55-5d4f0c7b1a10",
    );
    let owners = w.secret.metadata.owner_references.clone().unwrap();
    assert_eq!(
        (owners[0].kind.as_str(), owners[0].uid.as_str()),
        ("BuildKitPool", pool_uid)
    );
    assert!(run(&ops, w).await.unwrap().is_ok());
    let calls = calls.lock().unwrap().clone();
    let ns = fixtures::POOL_NAMESPACE;
    assert_eq!(
        summary(&calls),
        [
            format!("POST /api/v1/namespaces/{ns}/secrets"),
            format!("POST /api/v1/namespaces/{ns}/pods"),
            format!("PATCH /api/v1/namespaces/{ns}/secrets/{secret}"),
        ]
    );
    let patch = &calls[2];
    // A merge patch replaces the list whole, so the pool owner goes without a GET.
    assert_eq!(patch.content_type, "application/merge-patch+json");
    assert_eq!(
        patch.body,
        json!({"metadata": {"ownerReferences": [{
            "apiVersion": "v1", "kind": "Pod", "name": "p-0", "uid": "pod-uid-1", "controller": true,
        }]}})
    );
}

#[tokio::test]
async fn a_slot_taken_create_deletes_its_own_secret() {
    let (ops, calls) = server(slot_taken);
    let w = worker();
    let secret = w.secret.metadata.name.clone().unwrap();
    assert!(run(&ops, w).await.unwrap().is_err());
    let calls = summary(&calls.lock().unwrap());
    let ns = fixtures::POOL_NAMESPACE;
    assert_eq!(
        calls.last().unwrap(),
        &format!("DELETE /api/v1/namespaces/{ns}/secrets/{secret}")
    );
    assert!(
        calls
            .iter()
            .all(|c| !c.starts_with("GET") && !c.starts_with("PATCH")),
        "{calls:?}"
    );
}

#[tokio::test]
async fn a_refused_secret_create_creates_no_pod() {
    let (ops, calls) = server(secret_refused);
    assert!(run(&ops, worker()).await.is_err());
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[test]
fn the_pod_mounts_the_secret_named_for_its_nonce() {
    let w = worker();
    let ann = w.pod.metadata.annotations.as_ref().unwrap();
    let nonce = &ann[crate::render::ANN_WORKER_NONCE];
    let secret = w.secret.metadata.name.clone().unwrap();
    assert_eq!(secret, format!("p-0-{nonce}"));
    let vols = w.pod.spec.as_ref().unwrap().volumes.as_ref().unwrap();
    assert!(
        vols.iter()
            .any(|v| v.secret.as_ref().and_then(|s| s.secret_name.as_deref())
                == Some(secret.as_str()))
    );
    // Two pods of the same slot never share a Secret.
    assert_ne!(worker().secret.metadata.name, Some(secret));
}

#[tokio::test]
async fn a_quota_refused_create_deletes_its_own_secret() {
    // A quota-blocked pool would otherwise keep one valid worker key per retry.
    let (ops, calls) = server(quota_exceeded);
    let w = worker();
    let secret = w.secret.metadata.name.clone().unwrap();
    assert!(run(&ops, w).await.unwrap().is_err());
    let calls = summary(&calls.lock().unwrap());
    let ns = fixtures::POOL_NAMESPACE;
    assert_eq!(
        calls.last().unwrap(),
        &format!("DELETE /api/v1/namespaces/{ns}/secrets/{secret}"),
        "{calls:?}"
    );
}

#[tokio::test]
async fn an_unknown_create_outcome_keeps_the_secret() {
    let (ops, calls) = server(unavailable);
    assert!(run(&ops, worker()).await.unwrap().is_err());
    let calls = summary(&calls.lock().unwrap());
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls.iter().all(|c| !c.starts_with("DELETE")), "{calls:?}");
}
