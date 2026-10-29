//! Client tests against an in-process plaintext axum server.
#![cfg(feature = "client")]
#![allow(clippy::unwrap_used)]

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse as _;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use buildkit_controller_api::client::{Client, ClientError};
use buildkit_controller_api::dto::{CreateAllocationRequest, ErrorCode};
use serde_json::{Value, json};

async fn serve() -> String {
    let app = Router::new()
        .route(
            "/v1/allocations",
            post(|h: HeaderMap, Json(req): Json<Value>| async move {
                assert_eq!(h["authorization"], "Bearer tok");
                assert_eq!(
                    req,
                    json!({"namespace": "ci", "pool": "p", "publicKey": "PEM"})
                );
                (
                    StatusCode::CREATED,
                    Json(json!({
                        "id": "abc", "namespace": "ci", "pool": "p", "owner": "sa:ci/b",
                        "endpoint": "tcp://e:1", "serverName": "e", "caCert": null,
                        "clientCert": "C", "expiresAt": "2026-10-19T13:00:00Z"
                    })),
                )
            }),
        )
        .route(
            "/v1/allocations/{id}",
            delete(
                |axum::extract::Path(id): axum::extract::Path<String>| async move {
                    if id == "gone" {
                        (
                            StatusCode::NOT_FOUND,
                            Json(json!({"error": {"code": "not_found", "message": "gone"}})),
                        )
                            .into_response()
                    } else {
                        StatusCode::NO_CONTENT.into_response()
                    }
                },
            ),
        )
        .route(
            "/v1/pools",
            get(|| async {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [("retry-after", "5")],
                    Json(json!({"error": {"code": "pool_exhausted", "message": "full"}})),
                )
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn create_release_and_errors() {
    let base = serve().await;
    let c = Client::new(&base, "tok", None).unwrap();
    let r = c
        .create_allocation(&CreateAllocationRequest {
            namespace: "ci".into(),
            pool: "p".into(),
            ttl_seconds: None,
            queue_timeout_seconds: None,
            public_key: Some("PEM".into()),
            cache_key: None,
            cache: None,
        })
        .await
        .unwrap();
    assert_eq!(r.id, "abc");
    assert_eq!(r.ca_cert, None);
    assert_eq!(r.client_key, None);

    c.release_allocation("ok").await.unwrap();
    match c.release_allocation("gone").await.unwrap_err() {
        ClientError::Api { status, body, .. } => {
            assert_eq!(status, 404);
            assert_eq!(body.unwrap().error.code, ErrorCode::NotFound);
        }
        e => panic!("{e}"),
    }

    let e = c.list_pools().await.unwrap_err();
    assert!(e.is_retryable());
    match e {
        ClientError::Api {
            retry_after, body, ..
        } => {
            assert_eq!(retry_after, Some(std::time::Duration::from_secs(5)));
            assert_eq!(body.unwrap().error.code, ErrorCode::PoolExhausted);
        }
        e => panic!("{e}"),
    }
}

/// A pending allocation is polled until bound; a poll that fails with a
/// retryable 503 is tried again.
#[tokio::test]
async fn create_polls_a_pending_allocation() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let polls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&polls);
    let app = Router::new()
        .route(
            "/v1/allocations",
            post(|| async {
                (
                    StatusCode::ACCEPTED,
                    Json(json!({"id": "q1", "state": "pending", "position": 2})),
                )
            }),
        )
        .route(
            "/v1/allocations/{id}",
            get(
                move |axum::extract::Path(id): axum::extract::Path<String>,
                      axum::extract::RawQuery(q): axum::extract::RawQuery| {
                    let n = seen.fetch_add(1, Ordering::SeqCst);
                    async move {
                        assert_eq!((id.as_str(), q.as_deref()), ("q1", Some("wait=25s")));
                        match n {
                            0 => (
                                StatusCode::SERVICE_UNAVAILABLE,
                                [("retry-after", "0")],
                                Json(json!({"error": {"code": "upstream_error", "message": "x"}})),
                            )
                                .into_response(),
                            1 => (
                                StatusCode::ACCEPTED,
                                Json(json!({"id": "q1", "state": "pending", "position": 1})),
                            )
                                .into_response(),
                            _ => Json(json!({
                                "id": "q1", "namespace": "ci", "pool": "p", "owner": "sa:ci/b",
                                "endpoint": "tcp://e:1", "serverName": "e", "caCert": null,
                                "clientCert": "C", "expiresAt": "2026-10-19T13:00:00Z"
                            }))
                            .into_response(),
                        }
                    }
                },
            ),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let c = Client::new(&format!("http://{addr}"), "tok", None).unwrap();
    let req: CreateAllocationRequest =
        serde_json::from_value(json!({"namespace": "ci", "pool": "p", "publicKey": "PEM"}))
            .unwrap();
    let r = c.create_allocation(&req).await.unwrap();
    assert_eq!(r.id, "q1");
    assert_eq!(polls.load(Ordering::SeqCst), 3);
}
