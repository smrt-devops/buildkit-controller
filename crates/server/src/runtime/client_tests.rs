//! The read timeout turns a stalled API connection into a watcher error.
//!
//! These run on the real clock against a fake API server on loopback: the
//! paused clock auto-advances whenever the runtime waits on socket I/O, which
//! would fire the timeout before the server could answer.

use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::runtime::watcher::{self, Event};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::client_from;

const TIMEOUT: Duration = Duration::from_millis(300);

/// How the fake API server answers.
#[derive(Clone, Copy)]
enum Watch {
    /// Send the response headers, then nothing.
    Stall,
    /// Send a bookmark every 100 ms.
    Bookmarks,
}

#[derive(Clone, Copy)]
struct Server {
    /// Lists answer; otherwise they get no response at all.
    list: bool,
    watch: Watch,
}

const LIST: &str =
    r#"{"kind":"PodList","apiVersion":"v1","metadata":{"resourceVersion":"1"},"items":[]}"#;
const BOOKMARK: &str = r#"{"type":"BOOKMARK","object":{"kind":"Pod","apiVersion":"v1","metadata":{"resourceVersion":"1"}}}"#;

async fn read_head(sock: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        head.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&head).into_owned()
}

async fn serve(mut sock: TcpStream, server: Server) {
    // hyper may reuse a connection, so answer each request on it in turn.
    loop {
        let head = read_head(&mut sock).await;
        if head.is_empty() {
            return;
        }
        if !head.contains("watch=true") {
            if !server.list {
                std::future::pending::<()>().await;
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{LIST}",
                LIST.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            continue;
        }
        let headers = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n";
        sock.write_all(headers.as_bytes()).await.unwrap();
        if matches!(server.watch, Watch::Stall) {
            std::future::pending::<()>().await;
        }
        let line = format!("{BOOKMARK}\n");
        let chunk = format!("{:x}\r\n{line}\r\n", line.len());
        while sock.write_all(chunk.as_bytes()).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        return;
    }
}

/// A pod watcher against a fake API server that answers as `server` says.
async fn pod_watch(
    server: Server,
) -> impl futures::Stream<Item = watcher::Result<Event<Pod>>> + Send {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (sock, _) = listener.accept().await.unwrap();
            tokio::spawn(serve(sock, server));
        }
    });
    let config = kube::Config::new(format!("http://{addr}").parse().unwrap());
    let client = client_from(config, TIMEOUT).unwrap();
    watcher::watcher(
        Api::<Pod>::namespaced(client, "ns"),
        watcher::Config::default(),
    )
    .boxed()
}

/// The next item, or `None` if none arrives within `within`.
async fn next_within<S>(events: &mut S, within: Duration) -> Option<watcher::Result<Event<Pod>>>
where
    S: futures::Stream<Item = watcher::Result<Event<Pod>>> + Unpin,
{
    tokio::time::timeout(within, events.next())
        .await
        .ok()
        .flatten()
}

async fn listed<S>(events: &mut S)
where
    S: futures::Stream<Item = watcher::Result<Event<Pod>>> + Unpin,
{
    let within = Duration::from_secs(5);
    assert!(matches!(
        next_within(events, within).await,
        Some(Ok(Event::Init))
    ));
    assert!(matches!(
        next_within(events, within).await,
        Some(Ok(Event::InitDone))
    ));
}

#[tokio::test]
async fn a_watch_that_stalls_after_its_headers_fails_after_the_read_timeout() {
    let mut events = pod_watch(Server {
        list: true,
        watch: Watch::Stall,
    })
    .await;
    listed(&mut events).await;
    let started = tokio::time::Instant::now();
    let item = next_within(&mut events, Duration::from_secs(5)).await;
    assert!(
        matches!(item, Some(Err(watcher::Error::WatchFailed(_)))),
        "{item:?}"
    );
    assert!(started.elapsed() >= TIMEOUT, "not before the timeout");
}

#[tokio::test]
async fn a_list_that_gets_no_response_fails_after_the_read_timeout() {
    let mut events = pod_watch(Server {
        list: false,
        watch: Watch::Stall,
    })
    .await;
    let within = Duration::from_secs(5);
    assert!(matches!(
        next_within(&mut events, within).await,
        Some(Ok(Event::Init))
    ));
    let item = next_within(&mut events, within).await;
    assert!(
        matches!(item, Some(Err(watcher::Error::InitialListFailed(_)))),
        "{item:?}"
    );
}

#[tokio::test]
async fn bookmarks_keep_a_quiet_watch_alive() {
    let mut events = pod_watch(Server {
        list: true,
        watch: Watch::Bookmarks,
    })
    .await;
    listed(&mut events).await;
    // Five read timeouts without a change: bookmarks only, so no item at all.
    let item = next_within(&mut events, TIMEOUT * 5).await;
    assert!(item.is_none(), "{item:?}");
}
