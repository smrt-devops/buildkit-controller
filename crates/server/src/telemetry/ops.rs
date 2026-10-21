//! The plaintext ops server: `/metrics`, `/healthz`, `/readyz`.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use tokio_util::sync::CancellationToken;

use crate::runtime::metrics::Metrics;
use crate::runtime::readiness::Readiness;

/// State shared by the handlers.
#[derive(Clone)]
pub struct OpsState {
    /// Registry.
    pub metrics: Arc<Metrics>,
    /// Checks.
    pub readiness: Arc<Readiness>,
}

/// The ops router.
pub fn router(state: OpsState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .with_state(state)
}

async fn readyz(State(s): State<OpsState>) -> impl IntoResponse {
    let failing = s.readiness.failing();
    if failing.is_empty() {
        (StatusCode::OK, "ok\n".to_owned())
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("not ready: {}\n", failing.join(", ")),
        )
    }
}

async fn metrics(State(s): State<OpsState>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        s.metrics.render(),
    )
}

/// Bind the ops listener, so a bind failure surfaces at startup.
pub async fn bind(addr: SocketAddr) -> anyhow::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("binding ops listener {addr}: {e}"))
}

/// Serve on `listener` until `stop` fires.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: OpsState,
    stop: CancellationToken,
) -> anyhow::Result<()> {
    tracing::info!(addr = ?listener.local_addr().ok(), "ops server listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move { stop.cancelled().await })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    async fn get_path(app: &Router, path: &str) -> (StatusCode, String) {
        let r = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn readyz_follows_checks_and_shutdown() {
        let readiness = Arc::new(Readiness::default());
        let pods = readiness.register("pod_store");
        let app = router(OpsState {
            metrics: Arc::new(Metrics::new()),
            readiness: Arc::clone(&readiness),
        });
        assert_eq!(get_path(&app, "/healthz").await.0, StatusCode::OK);
        let (code, body) = get_path(&app, "/readyz").await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("pod_store"), "{body}");
        pods.set();
        assert_eq!(get_path(&app, "/readyz").await.0, StatusCode::OK);
        readiness.set_shutting_down();
        let (code, body) = get_path(&app, "/readyz").await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("shutting_down"));
        assert_eq!(get_path(&app, "/healthz").await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn metrics_expose_the_leader_gauge() {
        let m = Arc::new(Metrics::new());
        m.watch_error("pod");
        let app = router(OpsState {
            metrics: m,
            readiness: Arc::new(Readiness::default()),
        });
        let (code, body) = get_path(&app, "/metrics").await;
        assert_eq!(code, StatusCode::OK);
        assert!(body.contains("buildkit_leader 0"), "{body}");
        assert!(
            body.contains("buildkit_watch_errors_total{kind=\"pod\"} 1"),
            "{body}"
        );
    }
}
