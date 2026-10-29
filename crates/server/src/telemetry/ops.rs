//! The plaintext ops server: `/metrics`, `/healthz`, `/readyz`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::runtime::listen;
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
    let degraded = s.readiness.degraded();
    let (code, head) = if failing.is_empty() {
        (StatusCode::OK, "ok\n".to_owned())
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("not ready: {}\n", failing.join(", ")),
        )
    };
    if degraded.is_empty() {
        (code, head)
    } else {
        (code, format!("{head}degraded: {}\n", degraded.join(", ")))
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

/// Bind the ops listener, so a bind failure surfaces at startup. `0.0.0.0`
/// listens on both families (see [`listen::bind`]).
pub fn bind(addr: SocketAddr) -> anyhow::Result<TcpListener> {
    listen::bind(addr).map_err(|e| anyhow::anyhow!("binding ops listener {addr}: {e}"))
}

/// Open ops connections; more are closed on accept. Probes and a few scrapers
/// need a handful, and the cap keeps idle sockets from exhausting descriptors.
const MAX_CONNECTIONS: usize = 64;
/// Longest wait for a request head, also the keep-alive idle bound. Tests use a
/// short real-time bound: a paused clock jumps ahead while the server is still
/// accepting, so the test deadline fired before the server timer existed.
const HEADER_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_secs(1)
} else {
    Duration::from_secs(10)
};
/// How long open connections may finish after `stop` before they are dropped.
pub const STOP_GRACE: Duration = Duration::from_secs(2);

/// Serve on `listener` until `stop` fires. Open connections then get
/// [`STOP_GRACE`] to finish their request, and the rest are dropped.
pub async fn serve(
    listener: TcpListener,
    state: OpsState,
    stop: CancellationToken,
) -> anyhow::Result<()> {
    tracing::info!(addr = ?listener.local_addr().ok(), "ops server listening");
    let app = router(state);
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut conns = JoinSet::new();
    loop {
        tokio::select! {
            () = stop.cancelled() => break,
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
            accepted = listener.accept() => {
                let tcp = match accepted {
                    Ok((tcp, _)) => tcp,
                    Err(e) => {
                        debug!(error = %e, "ops accept");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                    debug!("ops connection limit reached; closing");
                    continue;
                };
                let svc = TowerToHyperService::new(app.clone());
                let stop = stop.clone();
                conns.spawn(async move {
                    let conn = http1::Builder::new()
                        .timer(TokioTimer::new())
                        .header_read_timeout(HEADER_TIMEOUT)
                        .serve_connection(TokioIo::new(tcp), svc);
                    tokio::pin!(conn);
                    tokio::select! {
                        r = conn.as_mut() => if let Err(e) = r { debug!(error = %e, "ops connection") },
                        () = stop.cancelled() => {
                            conn.as_mut().graceful_shutdown();
                            let _ = tokio::time::timeout(STOP_GRACE, conn).await;
                        }
                    }
                    drop(permit);
                });
            }
        }
    }
    drop(listener);
    while conns.join_next().await.is_some() {}
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
        // A namespace a watch gave up on is listed, but keeps the replica ready.
        readiness.set_degraded("pod_store/team-b", true);
        let (code, body) = get_path(&app, "/readyz").await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body, "ok\ndegraded: pod_store/team-b\n");
        readiness.set_degraded("pod_store/team-b", false);
        assert_eq!(get_path(&app, "/readyz").await.1, "ok\n");
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

    fn state() -> OpsState {
        OpsState {
            metrics: Arc::new(Metrics::new()),
            readiness: Arc::new(Readiness::default()),
        }
    }

    fn start() -> (
        SocketAddr,
        CancellationToken,
        tokio::task::JoinHandle<anyhow::Result<()>>,
    ) {
        let l = bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = l.local_addr().unwrap();
        let stop = CancellationToken::new();
        let task = tokio::spawn(serve(l, state(), stop.clone()));
        (addr, stop, task)
    }

    /// Read until the server closes; true when it did (EOF or reset).
    async fn closed(s: &mut tokio::net::TcpStream) -> bool {
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 1024];
        loop {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => return true,
                Ok(_) => {}
            }
        }
    }

    #[tokio::test]
    async fn a_stalled_request_cannot_hold_the_stop() {
        use tokio::io::AsyncWriteExt;
        let (addr, stop, task) = start();
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /metrics HTTP/1.1\r\n").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop.cancel();
        tokio::time::timeout(STOP_GRACE * 2, task)
            .await
            .expect("ops server outlived its grace")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn a_request_head_that_never_ends_is_cut() {
        use tokio::io::AsyncWriteExt;
        let (addr, _stop, _task) = start();
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /metrics HTTP/1.1\r\n").await.unwrap();
        tokio::time::timeout(HEADER_TIMEOUT * 2, closed(&mut s))
            .await
            .expect("closed by the header read timeout");
    }

    #[tokio::test]
    async fn connections_past_the_cap_are_closed() {
        let (addr, _stop, _task) = start();
        let mut held = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            held.push(tokio::net::TcpStream::connect(addr).await.unwrap());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut extra = tokio::net::TcpStream::connect(addr).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), closed(&mut extra))
            .await
            .expect("closed at accept");
        drop(held);
    }
}
