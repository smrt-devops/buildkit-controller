use std::net::SocketAddr;

use axum::http::Request as HttpRequest;
use futures::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};
use tower::ServiceExt as _;

use super::*;
use crate::edge::tunnel_channel;

/// The server end as the edge sees it, and the client end as a WebSocket.
async fn pair(ping_every: Duration) -> (WsBytes<DuplexStream>, WebSocketStream<DuplexStream>) {
    let (s, c) = tokio::io::duplex(4 * MAX_MESSAGE);
    let server = WebSocketStream::from_raw_socket(s, Role::Server, Some(ws_config())).await;
    let client = WebSocketStream::from_raw_socket(c, Role::Client, None).await;
    (WsBytes::new(server, ping_every), client)
}

const HOUR: Duration = Duration::from_hours(1);

#[tokio::test]
async fn binary_payloads_join_into_one_stream_both_ways() {
    let (mut server, mut client) = pair(HOUR).await;
    for part in [&b"ab"[..], b"", b"cde"] {
        client
            .send(Message::Binary(Bytes::copy_from_slice(part)))
            .await
            .unwrap();
    }
    let mut got = [0u8; 5];
    server.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"abcde");
    // A read smaller than the frame leaves the rest for the next read.
    client
        .send(Message::Binary(Bytes::from_static(b"wxyz")))
        .await
        .unwrap();
    let mut two = [0u8; 2];
    server.read_exact(&mut two).await.unwrap();
    server.read_exact(&mut two).await.unwrap();
    assert_eq!(&two, b"yz");
    server.write_all(b"back").await.unwrap();
    server.flush().await.unwrap();
    assert_eq!(
        client.next().await.unwrap().unwrap(),
        Message::Binary(Bytes::from_static(b"back"))
    );
}

#[tokio::test]
async fn a_close_frame_ends_the_stream_and_is_answered() {
    let (mut server, mut client) = pair(HOUR).await;
    client
        .send(Message::Binary(Bytes::from_static(b"x")))
        .await
        .unwrap();
    client.close(None).await.unwrap();
    let mut all = Vec::new();
    server.read_to_end(&mut all).await.unwrap();
    assert_eq!(all, b"x");
    assert!(
        matches!(client.next().await, Some(Ok(Message::Close(_))) | None),
        "the server answers the close"
    );
}

#[tokio::test]
async fn eof_without_a_close_frame_ends_the_stream() {
    let (mut server, client) = pair(HOUR).await;
    drop(client);
    let mut all = Vec::new();
    server.read_to_end(&mut all).await.unwrap();
    assert!(all.is_empty());
}

#[tokio::test]
async fn shutdown_sends_a_close_frame() {
    let (mut server, mut client) = pair(HOUR).await;
    server.shutdown().await.unwrap();
    assert!(matches!(client.next().await, Some(Ok(Message::Close(_)))));
}

#[tokio::test]
async fn a_text_frame_fails_the_stream() {
    let (mut server, mut client) = pair(HOUR).await;
    client.send(Message::text("hello")).await.unwrap();
    let e = server.read(&mut [0u8; 8]).await.unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidData);
}

#[tokio::test]
async fn a_message_over_256_kib_fails_the_stream() {
    let (mut server, mut client) = pair(HOUR).await;
    client
        .send(Message::Binary(Bytes::from(vec![0u8; MAX_MESSAGE])))
        .await
        .unwrap();
    let mut buf = vec![0u8; MAX_MESSAGE];
    server
        .read_exact(&mut buf)
        .await
        .expect("256 KiB is allowed");
    client
        .send(Message::Binary(Bytes::from(vec![0u8; MAX_MESSAGE + 1])))
        .await
        .unwrap();
    assert!(server.read(&mut buf).await.is_err());
}

/// The cap counts the fragments of one message together, though each
/// fragment alone is small.
#[tokio::test]
async fn fragments_over_256_kib_in_all_fail_the_stream() {
    use tokio_tungstenite::tungstenite::protocol::frame::Frame;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
    let (mut server, mut client) = pair(HOUR).await;
    let part = MAX_MESSAGE / 4;
    for i in 0..5 {
        let op = if i == 0 {
            OpCode::Data(Data::Binary)
        } else {
            OpCode::Data(Data::Continue)
        };
        let frame = Frame::message(vec![1u8; part], op, i == 4);
        client.send(Message::Frame(frame)).await.unwrap();
    }
    let e = server.read(&mut [0u8; 8]).await.unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidData);
}

#[tokio::test]
async fn writes_go_out_in_frames_of_at_most_64_kib() {
    let (mut server, mut client) = pair(HOUR).await;
    let n = server.write(&vec![7u8; MAX_MESSAGE]).await.unwrap();
    assert_eq!(n, WRITE_CHUNK);
    server.flush().await.unwrap();
    let Some(Ok(Message::Binary(b))) = client.next().await else {
        panic!("a binary frame")
    };
    assert_eq!(b.len(), WRITE_CHUNK);
}

/// A client that answers pings (any client that reads) keeps an idle tunnel
/// open; one that answers none is closed after two unanswered pings.
#[tokio::test]
async fn pings_keep_an_answering_tunnel_and_close_a_silent_one() {
    let every = Duration::from_millis(50);
    let (mut server, mut client) = pair(every).await;
    let pongs = tokio::spawn(async move {
        let mut pings = 0;
        while let Some(Ok(m)) = client.next().await {
            if matches!(m, Message::Ping(_)) {
                pings += 1;
            }
        }
        pings
    });
    let idle = tokio::time::timeout(every * 10, server.read(&mut [0u8; 1])).await;
    assert!(idle.is_err(), "an answering tunnel stays open: {idle:?}");
    drop(server);
    assert!(
        pongs.await.unwrap() >= 5,
        "the server pinged every interval"
    );

    let (mut server, _silent) = pair(every).await;
    let start = tokio::time::Instant::now();
    let e = tokio::time::timeout(every * 10, server.read(&mut [0u8; 1]))
        .await
        .expect("a silent tunnel is closed")
        .unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::TimedOut);
    assert!(
        start.elapsed() >= every * 3,
        "two pings went unanswered first"
    );
}

#[test]
fn the_accept_key_follows_rfc_6455() {
    let mut h = HeaderMap::new();
    h.insert(
        header::CONNECTION,
        HeaderValue::from_static("keep-alive, Upgrade"),
    );
    h.insert(header::UPGRADE, HeaderValue::from_static("WebSocket"));
    h.insert(
        header::SEC_WEBSOCKET_VERSION,
        HeaderValue::from_static("13"),
    );
    let key = fresh_key();
    h.insert(
        header::SEC_WEBSOCKET_KEY,
        HeaderValue::from_str(&key).unwrap(),
    );
    assert_eq!(
        accept_key(&Method::GET, Version::HTTP_11, &h).unwrap(),
        accept_for(&key).as_str()
    );
    assert!(accept_key(&Method::POST, Version::HTTP_11, &h).is_none());
    assert!(accept_key(&Method::GET, Version::HTTP_2, &h).is_none());
    assert!(accept_key(&Method::GET, Version::HTTP_10, &h).is_none());
    let without = |name: HeaderName| {
        let mut h = h.clone();
        h.remove(name);
        accept_key(&Method::GET, Version::HTTP_11, &h)
    };
    for name in [
        header::CONNECTION,
        header::UPGRADE,
        header::SEC_WEBSOCKET_VERSION,
        header::SEC_WEBSOCKET_KEY,
    ] {
        assert!(without(name.clone()).is_none(), "{name}");
    }
    let with = |name: HeaderName, v: &str| {
        let mut h = h.clone();
        h.insert(name, HeaderValue::from_str(v).unwrap());
        accept_key(&Method::GET, Version::HTTP_11, &h)
    };
    assert!(with(header::SEC_WEBSOCKET_VERSION, "8").is_none());
    assert!(
        with(header::SEC_WEBSOCKET_KEY, &BASE64.encode([0u8; 15])).is_none(),
        "not 16 bytes"
    );
    assert!(with(header::UPGRADE, "h2c").is_none());
    let mut two = h.clone();
    two.append(
        header::SEC_WEBSOCKET_KEY,
        HeaderValue::from_str(&fresh_key()).unwrap(),
    );
    assert!(accept_key(&Method::GET, Version::HTTP_11, &two).is_none());
}

/// A new `Sec-WebSocket-Key`: 16 random bytes, base64.
fn fresh_key() -> String {
    let mut nonce = [0u8; 16];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut nonce).unwrap();
    BASE64.encode(nonce)
}

/// The `Sec-WebSocket-Accept` RFC 6455 §4.2.2 requires for `key`.
fn accept_for(key: &str) -> String {
    let digest = ring::digest::digest(
        &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
        format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
    );
    BASE64.encode(digest)
}

fn upgrade_request(method: Method, key: &str) -> HttpRequest<Body> {
    let mut r = HttpRequest::builder()
        .method(method)
        .uri(TUNNEL_PATH)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_VERSION, "13")
        .header(header::SEC_WEBSOCKET_KEY, key)
        .body(Body::empty())
        .unwrap();
    let peer: SocketAddr = ([192, 0, 2, 7], 1000).into();
    r.extensions_mut().insert(Peer(peer));
    r
}

/// Any request but a WebSocket upgrade gets 426, and a valid one gets 503
/// while no edge takes tunnels. No bearer token is asked for.
#[tokio::test]
async fn the_route_answers_426_or_503_without_an_edge() {
    let app: Router = router(Tunnels::new(tunnel_channel().0));
    let plain = HttpRequest::builder()
        .uri(TUNNEL_PATH)
        .body(Body::empty())
        .unwrap();
    let r = app.clone().oneshot(plain).await.unwrap();
    assert_eq!(r.status(), StatusCode::UPGRADE_REQUIRED);
    assert_eq!(r.headers()[header::UPGRADE], "websocket");
    let r = app
        .clone()
        .oneshot(upgrade_request(Method::POST, &fresh_key()))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UPGRADE_REQUIRED);
    let r = app
        .oneshot(upgrade_request(Method::GET, &fresh_key()))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(r.headers().get(header::WWW_AUTHENTICATE).is_none());
}

/// The `101` goes out only once the edge holds a handshake slot for the
/// tunnel; a refused hand-over is a 503 and never upgrades.
#[tokio::test]
async fn the_101_waits_for_the_edge_to_take_the_tunnel() {
    let (door, mut queue) = crate::edge::tunnel_channel();
    let app: Router = router(Tunnels::new(door));
    let key = fresh_key();
    let mut answer = tokio::spawn(app.clone().oneshot(upgrade_request(Method::GET, &key)));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!answer.is_finished(), "no answer before the edge took it");
    let taken = queue.take_for_test().await;
    assert_eq!(taken, ([192, 0, 2, 7], 1000).into());
    let r = (&mut answer).await.unwrap().unwrap();
    assert_eq!(r.status(), StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(
        r.headers()[header::SEC_WEBSOCKET_ACCEPT],
        accept_for(&key).as_str()
    );
    drop(queue);
    let r = app
        .oneshot(upgrade_request(Method::GET, &fresh_key()))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
}
