//! [`super::spawn`] against a recording API server: the requests a real
//! `kube::Client` sends to create a worker pod and then its Secret.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use hyper::{Request, Response, StatusCode};
use k8s_openapi::api::core::v1::Pod;
use kube::client::Body;
use serde_json::{Value, json};
use time::OffsetDateTime;

use super::ops::{KubeClient, KubeOps};
use super::spawn::{self, PodCreated, Spawned, Worker};
use crate::pki::CaBundle;
use crate::render::{ANN_WORKER_NONCE, fixtures};

/// One request: method, path and JSON body.
#[derive(Clone, Debug)]
struct Call {
    method: String,
    path: String,
    body: Value,
}

type Reply = Arc<dyn Fn(&Call) -> (StatusCode, Value) + Send + Sync>;

/// Answers every request from `reply` and records it.
fn server(reply: Reply) -> (KubeClient, Arc<Mutex<Vec<Call>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let log = calls.clone();
    let svc = tower::service_fn(move |req: Request<Body>| {
        let (log, reply) = (log.clone(), Arc::clone(&reply));
        async move {
            let (parts, body) = req.into_parts();
            let bytes = body.collect_bytes().await.unwrap();
            let call = Call {
                method: parts.method.to_string(),
                path: parts.uri.path().to_owned(),
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

fn is_pod_post(c: &Call) -> bool {
    c.method == "POST" && c.path.ends_with("/pods")
}

/// Echoes creates (a pod gets a uid).
fn happy(c: &Call) -> (StatusCode, Value) {
    match c.method.as_str() {
        "POST" if is_pod_post(c) => {
            let mut p = c.body.clone();
            p["metadata"]["uid"] = json!("pod-uid-1");
            (StatusCode::CREATED, p)
        }
        "POST" => (StatusCode::CREATED, c.body.clone()),
        _ => (StatusCode::FORBIDDEN, status(403, "Forbidden")),
    }
}

/// Like [`happy`], but the pod create answers `code`.
fn pod_create_fails(code: u16, reason: &'static str) -> Reply {
    Arc::new(move |c: &Call| {
        if is_pod_post(c) {
            return (StatusCode::from_u16(code).unwrap(), status(code, reason));
        }
        happy(c)
    })
}

/// The pod create answers 503; the read back answers `get`.
fn lost_create(get: impl Fn() -> (StatusCode, Value) + Send + Sync + 'static) -> Reply {
    let fail = pod_create_fails(503, "ServiceUnavailable");
    Arc::new(move |c: &Call| {
        if c.method == "GET" {
            return get();
        }
        fail(c)
    })
}

fn pod_with_nonce(nonce: &str) -> Value {
    json!({"apiVersion": "v1", "kind": "Pod", "metadata": {
        "name": "p-0", "uid": "pod-uid-2", "annotations": {ANN_WORKER_NONCE: nonce}}})
}

fn worker() -> Worker {
    let pool = fixtures::pool("p", json!({})).unwrap();
    let now = OffsetDateTime::now_utc();
    let ca = CaBundle::generate(now).unwrap();
    spawn::prepare(&pool, &ca, 0, now, time::Duration::days(30), None).unwrap()
}

fn nonce(w: &Worker) -> String {
    w.pod.metadata.annotations.as_ref().unwrap()[ANN_WORKER_NONCE].clone()
}

async fn run(ops: &KubeClient, w: Worker) -> Spawned<super::ops::OpResult<Pod>> {
    let ns = w.pod.metadata.namespace.clone().unwrap();
    Box::pin(spawn::spawn(ops, w, |pod| async move {
        ops.create_pod(&ns, &pod).await
    }))
    .await
}

fn summary(calls: &[Call]) -> Vec<String> {
    calls
        .iter()
        .map(|c| format!("{} {}", c.method, c.path))
        .collect()
}

fn pod_owner(uid: &str) -> Value {
    json!([{"apiVersion": "v1", "kind": "Pod", "name": "p-0", "uid": uid, "controller": true}])
}

#[tokio::test]
async fn the_secret_is_created_after_the_pod_and_owned_by_it() {
    let (ops, calls) = server(Arc::new(happy));
    let s = run(&ops, worker()).await;
    assert!(s.out.is_ok() && s.secret.is_ok());
    assert_eq!(
        s.pod,
        PodCreated::Yes {
            name: "p-0".into(),
            uid: "pod-uid-1".into()
        }
    );
    let calls = calls.lock().unwrap().clone();
    let ns = fixtures::POOL_NAMESPACE;
    assert_eq!(
        summary(&calls),
        [
            format!("POST /api/v1/namespaces/{ns}/pods"),
            format!("POST /api/v1/namespaces/{ns}/secrets"),
        ]
    );
    // The pool never owns it, not even for a moment.
    assert_eq!(
        calls[1].body["metadata"]["ownerReferences"],
        pod_owner("pod-uid-1")
    );
}

#[tokio::test]
async fn a_refused_pod_create_creates_no_secret() {
    // 409 slot taken, 403 quota: nothing exists, so there is nothing to clean up.
    for (code, reason) in [(409, "AlreadyExists"), (403, "Forbidden")] {
        let (ops, calls) = server(pod_create_fails(code, reason));
        let s = run(&ops, worker()).await;
        assert!(s.out.is_err());
        assert_eq!(s.pod, PodCreated::No);
        assert_eq!(calls.lock().unwrap().len(), 1, "{code}");
    }
}

#[tokio::test]
async fn a_refused_secret_create_reports_the_pod_that_cannot_start() {
    let refuse: Reply = Arc::new(|c: &Call| {
        if c.method == "POST" && c.path.ends_with("/secrets") {
            return (StatusCode::FORBIDDEN, status(403, "Forbidden"));
        }
        happy(c)
    });
    let (ops, calls) = server(refuse);
    let s = run(&ops, worker()).await;
    assert!(s.out.is_ok());
    assert!(matches!(s.pod, PodCreated::Yes { .. }));
    assert!(s.secret.is_err());
    assert_eq!(calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn an_unknown_create_is_read_back_before_any_secret() {
    let w = worker();
    let n = nonce(&w);
    let found = pod_with_nonce(&n);
    let (ops, calls) = server(lost_create(move || (StatusCode::OK, found.clone())));
    let s = run(&ops, w).await;
    assert!(s.out.is_err());
    assert_eq!(
        s.pod,
        PodCreated::Yes {
            name: "p-0".into(),
            uid: "pod-uid-2".into()
        }
    );
    assert!(s.secret.is_ok());
    let calls = calls.lock().unwrap().clone();
    let ns = fixtures::POOL_NAMESPACE;
    assert_eq!(
        summary(&calls),
        [
            format!("POST /api/v1/namespaces/{ns}/pods"),
            format!("GET /api/v1/namespaces/{ns}/pods/p-0"),
            format!("POST /api/v1/namespaces/{ns}/secrets"),
        ]
    );
    assert_eq!(
        calls[2].body["metadata"]["ownerReferences"],
        pod_owner("pod-uid-2")
    );
}

#[tokio::test]
async fn an_unknown_create_without_its_pod_creates_no_secret() {
    let cases: [(&str, Reply, PodCreated); 3] = [
        (
            "absent",
            lost_create(|| (StatusCode::NOT_FOUND, status(404, "NotFound"))),
            PodCreated::No,
        ),
        (
            "another pod of the slot",
            lost_create(|| (StatusCode::OK, pod_with_nonce("00000000000000ff"))),
            PodCreated::No,
        ),
        (
            "read failed",
            lost_create(|| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    status(503, "ServiceUnavailable"),
                )
            }),
            PodCreated::Unknown,
        ),
    ];
    for (what, reply, want) in cases {
        let (ops, calls) = server(reply);
        let s = run(&ops, worker()).await;
        assert_eq!(s.pod, want, "{what}");
        let calls = summary(&calls.lock().unwrap());
        assert!(
            calls.iter().all(|c| !c.ends_with("/secrets")),
            "{what}: {calls:?}"
        );
    }
}

#[test]
fn the_pod_mounts_the_secret_named_for_its_nonce() {
    let w = worker();
    let nonce = nonce(&w);
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
