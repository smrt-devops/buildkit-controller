//! The edge handshake cases through a real tunnel: `GET /v1/tunnel` on a
//! loopback API listener, then mTLS to the edge inside the WebSocket.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use buildkit_controller::edge::{
    Accepted, BoxIo, ConnectionHandler, EdgeMetrics, ListenerOptions, Serve, TlsBuilder, Transport,
    serve_with_tunnels, tunnel_channel,
};
use buildkit_controller::http::{Limits, Tunnels, WsBytes, serve, tunnel};
use buildkit_controller::net::ProxyProtocol;
use buildkit_controller::pki::{CaBundle, EdgeNames, LeafKey};
use buildkit_controller::runtime::metrics::Metrics;
use buildkit_controller::runtime::readiness::Readiness;
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::{SinkExt as _, StreamExt as _};
use hyper::client::conn::http2::SendRequest;
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::sign::CertifiedKey;
use rustls::{AlertDescription, ClientConfig, RootCertStore};
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_util::sync::CancellationToken;

const HOST: &str = "edge.example.test";
const ID: &str =
    "buildkit://team-a/pool-1/0f8fad5b-d9cb-469f-a165-70867728950e/abcdefghijklmnopqrstuvwxyz";

type Held = (Accepted, oneshot::Sender<()>);

/// Hands each connection to the test and holds it until the test lets go.
struct TestHandler {
    tx: mpsc::UnboundedSender<Held>,
}

impl ConnectionHandler for TestHandler {
    fn handle(&self, conn: Accepted) -> BoxFuture<'static, ()> {
        let (done, wait) = oneshot::channel();
        let _ = self.tx.send((conn, done));
        Box::pin(async move {
            let _ = wait.await;
        })
    }
}

/// An edge and an API listener on loopback, joined by a tunnel door.
struct Rig {
    api: SocketAddr,
    bundle: CaBundle,
    metrics: EdgeMetrics,
    conns: mpsc::UnboundedReceiver<Held>,
    edge_stop: CancellationToken,
    edge: JoinHandle<()>,
    api_stop: CancellationToken,
    api_task: JoinHandle<()>,
}

struct Knobs {
    edge: ListenerOptions,
    api: Limits,
    ping_every: Duration,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            edge: ListenerOptions {
                handshake_timeout: Duration::from_secs(2),
                max_connections: 16,
                ..ListenerOptions::default()
            },
            api: Limits::default(),
            ping_every: buildkit_controller::http::PING_EVERY,
        }
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn serving(bundle: &CaBundle) -> Arc<CertifiedKey> {
    let names = EdgeNames::new(HOST, &[], "bkc", &"ops".parse().unwrap()).unwrap();
    let c = bundle.issue_edge_serving(&names, now()).unwrap();
    let signer = provider()
        .key_provider
        .load_private_key(c.private_key_der().unwrap())
        .unwrap();
    Arc::new(CertifiedKey::new(vec![c.cert_der().clone()], signer))
}

async fn start(k: Knobs) -> Rig {
    let bundle = CaBundle::generate(now()).unwrap();
    let key = serving(&bundle);
    let (tx, conns) = mpsc::unbounded_channel();
    let handler = Arc::new(TestHandler { tx });
    let metrics = EdgeMetrics::register(&Metrics::new());
    let tls = TlsBuilder::new(handler.clone(), metrics.clone())
        .build(Arc::clone(&key), bundle.client_ca().cert_der().clone())
        .unwrap();
    let (tls_tx, tls_rx) = watch::channel(Some(Arc::new(tls)));
    std::mem::forget(tls_tx);
    let readiness = Readiness::with_spec_checks();
    let s = Serve {
        tls: tls_rx,
        handler,
        metrics: metrics.clone(),
        options: k.edge,
        ready: readiness.register("edge_listener"),
    };
    let (door, tunnels) = tunnel_channel();
    let edge_stop = CancellationToken::new();
    let edge_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let edge = tokio::spawn(serve_with_tunnels(
        edge_listener,
        s,
        ProxyProtocol::off(),
        tunnels,
        edge_stop.clone(),
    ));

    let app = tunnel::router(Tunnels::new(door).with_ping_every(k.ping_every));
    let (certs_tx, certs) = watch::channel(Some(key));
    std::mem::forget(certs_tx);
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = api_listener.local_addr().unwrap();
    let api_stop = CancellationToken::new();
    let api_task = tokio::spawn(serve(api_listener, app, certs, k.api, api_stop.clone()));
    Rig {
        api,
        bundle,
        metrics,
        conns,
        edge_stop,
        edge,
        api_stop,
        api_task,
    }
}

impl Rig {
    fn ca(&self) -> CertificateDer<'static> {
        self.bundle.client_ca().cert_der().clone()
    }

    fn tunnel_metrics(&self) -> EdgeMetrics {
        self.metrics.for_transport(Transport::Tunnel)
    }
}

fn leaf(
    bundle: &CaBundle,
    issued: OffsetDateTime,
    expires: OffsetDateTime,
) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let c = bundle
        .issue_allocation(&ID.parse().unwrap(), LeafKey::Generate, issued, expires)
        .unwrap();
    (c.cert_der().clone(), c.private_key_der().unwrap())
}

fn valid_leaf(bundle: &CaBundle) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let t = now();
    leaf(
        bundle,
        t,
        t + time::Duration::hours(1) - time::Duration::nanoseconds(i64::from(t.nanosecond())),
    )
}

fn roots(ca: &CertificateDer<'static>) -> RootCertStore {
    let mut store = RootCertStore::empty();
    store.add(ca.clone()).unwrap();
    store
}

/// The API leg: HTTPS with HTTP/1.1 only, so the upgrade is possible.
async fn api_tls(api: SocketAddr, ca: &CertificateDer<'static>) -> TlsStream<TcpStream> {
    api_tls_alpn(api, ca, b"http/1.1").await.unwrap()
}

/// The API leg with one ALPN protocol.
async fn api_tls_alpn(
    api: SocketAddr,
    ca: &CertificateDer<'static>,
    alpn: &[u8],
) -> std::io::Result<TlsStream<TcpStream>> {
    let mut cfg = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots(ca))
        .with_no_client_auth();
    cfg.alpn_protocols = vec![alpn.to_vec()];
    let tcp = TcpStream::connect(api).await?;
    TlsConnector::from(Arc::new(cfg))
        .connect(ServerName::try_from(HOST).unwrap(), tcp)
        .await
}

/// Sends `request` and returns the response head.
async fn exchange(s: &mut TlsStream<TcpStream>, request: &str) -> String {
    s.write_all(request.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = s.read(&mut b).await.unwrap();
        assert_eq!(n, 1, "the API closed mid-head: {head:?}");
        head.push(b[0]);
    }
    String::from_utf8(head).unwrap()
}

/// A new `Sec-WebSocket-Key`: 16 random bytes, base64.
fn fresh_key() -> String {
    use base64::Engine as _;
    let mut nonce = [0u8; 16];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut nonce).unwrap();
    base64::engine::general_purpose::STANDARD.encode(nonce)
}

/// The `Sec-WebSocket-Accept` RFC 6455 §4.2.2 requires for `key`.
fn accept_for(key: &str) -> String {
    use base64::Engine as _;
    let digest = ring::digest::digest(
        &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
        format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
    );
    base64::engine::general_purpose::STANDARD.encode(digest)
}

/// The two forms of the tunnel: an HTTP/1.1 upgrade answered `101`, and the
/// HTTP/2 extended `CONNECT` answered `200` that a gateway speaking HTTP/2 to
/// the API sends.
#[derive(Clone, Copy, Debug)]
enum Form {
    Upgrade,
    Connect,
}

const FORMS: [Form; 2] = [Form::Upgrade, Form::Connect];

type Ws = WebSocketStream<BoxIo>;

/// An open WebSocket on `/v1/tunnel` in `form`, with no bearer token.
async fn websocket(r: &Rig, form: Form) -> Ws {
    match form {
        Form::Upgrade => h1_websocket(r).await,
        Form::Connect => h2_websocket(&mut h2_conn(r).await).await,
    }
}

async fn h1_websocket(r: &Rig) -> Ws {
    let mut s = api_tls(r.api, &r.ca()).await;
    let key = fresh_key();
    let request = format!(
        "GET /v1/tunnel HTTP/1.1\r\nHost: edge.example.test\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n\r\n"
    );
    let head = exchange(&mut s, &request).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(
        head.contains(&format!("sec-websocket-accept: {}", accept_for(&key))),
        "{head}"
    );
    WebSocketStream::from_raw_socket(Box::new(s) as BoxIo, Role::Client, None).await
}

/// An HTTP/2 connection to the API (ALPN `h2`).
async fn h2_conn(r: &Rig) -> SendRequest<String> {
    let s = api_tls_alpn(r.api, &r.ca(), b"h2").await.unwrap();
    assert_eq!(s.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    let (send, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, String>(TokioIo::new(s))
        .await
        .unwrap();
    tokio::spawn(async move { drop(conn.await) });
    send
}

/// A `CONNECT /v1/tunnel` stream on `send` with `:protocol` `protocol`.
async fn h2_connect(
    send: &mut SendRequest<String>,
    protocol: &'static str,
) -> hyper::Response<hyper::body::Incoming> {
    let req = hyper::Request::builder()
        .method(hyper::Method::CONNECT)
        .uri(format!("https://{HOST}/v1/tunnel"))
        .header("sec-websocket-version", "13")
        .extension(hyper::ext::Protocol::from_static(protocol))
        .body(String::new())
        .unwrap();
    send.ready().await.unwrap();
    send.send_request(req).await.unwrap()
}

/// An open WebSocket on one new stream of `send`.
async fn h2_websocket(send: &mut SendRequest<String>) -> Ws {
    let resp = h2_connect(send, "websocket").await;
    assert_eq!(resp.status(), hyper::StatusCode::OK);
    let io = hyper::upgrade::on(resp).await.unwrap();
    WebSocketStream::from_raw_socket(Box::new(TokioIo::new(io)) as BoxIo, Role::Client, None).await
}

type Inner = tokio_rustls::client::TlsStream<WsBytes<BoxIo>>;

/// mTLS to the edge inside a new tunnel.
async fn dial(
    r: &Rig,
    form: Form,
    cert: (CertificateDer<'static>, PrivateKeyDer<'static>),
) -> std::io::Result<Inner> {
    tls_in(r, websocket(r, form).await, cert).await
}

/// mTLS to the edge inside `ws`.
async fn tls_in(
    r: &Rig,
    ws: Ws,
    cert: (CertificateDer<'static>, PrivateKeyDer<'static>),
) -> std::io::Result<Inner> {
    // The client answers pings as it reads; it sends none of its own.
    let bytes = WsBytes::new(ws, Duration::from_hours(1));
    let cfg = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots(&r.ca()))
        .with_client_auth_cert(vec![cert.0], cert.1)
        .unwrap();
    TlsConnector::from(Arc::new(cfg))
        .connect(ServerName::try_from(HOST).unwrap(), bytes)
        .await
}

fn alert_of(e: &std::io::Error) -> Option<AlertDescription> {
    match e.get_ref()?.downcast_ref::<rustls::Error>()? {
        rustls::Error::AlertReceived(a) => Some(*a),
        _ => None,
    }
}

async fn refused(
    r: &Rig,
    form: Form,
    cert: (CertificateDer<'static>, PrivateKeyDer<'static>),
) -> AlertDescription {
    let mut s = match dial(r, form, cert).await {
        Ok(s) => s,
        Err(e) => return alert_of(&e).unwrap_or_else(|| panic!("no alert: {e}")),
    };
    let e = s.read(&mut [0u8; 1]).await.expect_err("the edge refuses");
    alert_of(&e).unwrap_or_else(|| panic!("no alert: {e}"))
}

async fn count(m: &EdgeMetrics, result: &'static str, want: u64) {
    for _ in 0..200 {
        if m.outcome_count(None, result) == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "{result}: got {}, want {want}",
        m.outcome_count(None, result)
    );
}

/// The WebSocket ends (close, EOF or error) within `within`.
async fn ends_within<S>(ws: &mut WebSocketStream<S>, within: Duration)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    tokio::time::timeout(within, async {
        while let Some(Ok(m)) = ws.next().await {
            if m.is_close() {
                break;
            }
        }
    })
    .await
    .expect("the server ended the tunnel");
}

#[tokio::test]
async fn a_valid_cert_is_delivered_through_the_tunnel() {
    for form in FORMS {
        let mut r = start(Knobs::default()).await;
        let mut s = dial(&r, form, valid_leaf(&r.bundle)).await.unwrap();
        let (mut conn, _done) = r.conns.recv().await.unwrap();
        assert_eq!(conn.id, ID.parse().unwrap(), "{form:?}");
        assert_eq!(conn.transport, Transport::Tunnel);
        assert_eq!(
            conn.peer.ip(),
            "127.0.0.1".parse::<std::net::IpAddr>().unwrap()
        );
        conn.stream.write_all(b"down").await.unwrap();
        conn.stream.flush().await.unwrap();
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"down");
        s.write_all(b"up").await.unwrap();
        s.flush().await.unwrap();
        let mut two = [0u8; 2];
        conn.stream.read_exact(&mut two).await.unwrap();
        assert_eq!(&two, b"up");
        assert_eq!(r.metrics.outcome_count(None, "tls_error"), 0);
    }
}

/// Several HTTP/2 tunnels share one API connection, each its own stream.
/// Tunnels the edge does not read fill only their own stream windows, so the
/// others on the connection keep moving, even when more of them stall than
/// 16 stream windows (the old connection window) hold; then they drain in full.
#[tokio::test]
async fn h2_tunnels_share_one_connection() {
    const BIG: usize = 1 << 20;
    const STALLED: usize = 20;
    // A pool namespace may hold a quarter of the edge's connections.
    let mut knobs = Knobs::default();
    knobs.edge.max_connections = 128;
    let mut r = start(knobs).await;
    let mut send = h2_conn(&r).await;
    // Each stalled tunnel gets 1 MiB, more than a stream's flow-control
    // window, while the edge side reads none of it.
    let mut stalled = Vec::new();
    for _ in 0..STALLED {
        let mut a = tls_in(&r, h2_websocket(&mut send).await, valid_leaf(&r.bundle))
            .await
            .unwrap();
        let (edge_side, done) = r.conns.recv().await.unwrap();
        let up = tokio::spawn(async move {
            a.write_all(&vec![0x5au8; BIG]).await.unwrap();
            a.flush().await.unwrap();
            a
        });
        stalled.push((edge_side, done, up));
    }
    let mut b = tls_in(&r, h2_websocket(&mut send).await, valid_leaf(&r.bundle))
        .await
        .unwrap();
    let (mut cb, _db) = r.conns.recv().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    for (_, _, up) in &stalled {
        assert!(!up.is_finished(), "held by flow control");
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        b.write_all(b"b").await.unwrap();
        b.flush().await.unwrap();
        cb.stream.read_exact(&mut [0u8; 1]).await.unwrap();
    })
    .await
    .expect("stalled tunnels do not stall the connection");
    for (mut edge_side, _done, up) in stalled {
        let mut got = vec![0u8; BIG];
        edge_side.stream.read_exact(&mut got).await.unwrap();
        assert!(got.iter().all(|&x| x == 0x5a));
        drop(up.await.unwrap());
    }
}

#[tokio::test]
async fn wrong_ca_and_expired_certs_get_the_tcp_alerts() {
    for form in FORMS {
        let r = start(Knobs::default()).await;
        let other = CaBundle::generate(now()).unwrap();
        assert_eq!(
            refused(&r, form, valid_leaf(&other)).await,
            AlertDescription::UnknownCA,
            "{form:?}"
        );
        count(&r.tunnel_metrics(), "tls_error", 1).await;
        let t = now().replace_nanosecond(0).unwrap();
        let expired = leaf(
            &r.bundle,
            t - time::Duration::hours(3),
            t - time::Duration::hours(2),
        );
        assert_eq!(
            refused(&r, form, expired).await,
            AlertDescription::CertificateExpired
        );
        count(&r.tunnel_metrics(), "tls_error", 2).await;
        assert_eq!(
            r.metrics.outcome_count(None, "tls_error"),
            0,
            "counted under transport=tunnel only"
        );
    }
}

fn stalling_knobs() -> Knobs {
    Knobs {
        edge: ListenerOptions {
            handshake_timeout: Duration::from_secs(1),
            max_connections: 16,
            max_handshakes: 1,
            max_handshakes_per_ip: 0,
        },
        api: Limits {
            max_connections: 1,
            max_connections_per_ip: 1,
            ..Limits::default()
        },
        ..Knobs::default()
    }
}

/// A tunnel gives its API slots up at the `101` and holds an edge handshake
/// slot until its handshake times out; then the edge budget is free again
/// and the API connection count is where it was.
#[tokio::test]
async fn a_stalled_handshake_frees_its_slot_and_the_api_count_returns() {
    let mut r = start(stalling_knobs()).await;
    let mut stalled = websocket(&r, Form::Upgrade).await;
    // The only API slot is free again although the tunnel is open.
    let mut plain = api_tls(r.api, &r.ca()).await;
    let head = exchange(&mut plain, "GET /v1/tunnel HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(head.starts_with("HTTP/1.1 426"), "{head}");
    drop(plain);
    ends_within(&mut stalled, Duration::from_secs(3)).await;
    count(&r.tunnel_metrics(), "tls_error", 1).await;
    // The edge's only handshake slot is free: this tunnel evicts nothing.
    let _held = dial(&r, Form::Upgrade, valid_leaf(&r.bundle))
        .await
        .unwrap();
    r.conns.recv().await.unwrap();
    assert_eq!(r.tunnel_metrics().outcome_count(None, "handshake_limit"), 0);
}

/// An HTTP/2 tunnel is one stream: it frees no API slot of its connection,
/// which stays one connection that serves other streams. Its handshake slot
/// is freed at the handshake timeout, and the API count stays where it was.
#[tokio::test]
async fn a_stalled_h2_handshake_frees_its_slot_and_keeps_the_connection() {
    let mut r = start(stalling_knobs()).await;
    let mut send = h2_conn(&r).await;
    let mut stalled = h2_websocket(&mut send).await;
    // The connection holds the only API slot; a second one is closed.
    assert!(
        api_tls_alpn(r.api, &r.ca(), b"h2").await.is_err(),
        "the tunnel's connection keeps its API slot"
    );
    // Another :protocol on the same connection is refused, not upgraded.
    let other = h2_connect(&mut send, "connect-udp").await;
    assert_eq!(other.status(), hyper::StatusCode::UPGRADE_REQUIRED);
    ends_within(&mut stalled, Duration::from_secs(3)).await;
    count(&r.tunnel_metrics(), "tls_error", 1).await;
    // The edge's only handshake slot is free, and the same connection
    // carries a new tunnel.
    let _held = tls_in(&r, h2_websocket(&mut send).await, valid_leaf(&r.bundle))
        .await
        .unwrap();
    r.conns.recv().await.unwrap();
    assert_eq!(r.tunnel_metrics().outcome_count(None, "handshake_limit"), 0);
}

/// With pings answered, a tunnel with no traffic stays open past several
/// ping intervals and still carries data.
#[tokio::test]
async fn pings_keep_an_idle_tunnel_open() {
    let every = Duration::from_millis(100);
    for form in FORMS {
        let mut r = start(Knobs {
            ping_every: every,
            ..Knobs::default()
        })
        .await;
        let s = dial(&r, form, valid_leaf(&r.bundle)).await.unwrap();
        let (mut conn, _done) = r.conns.recv().await.unwrap();
        let (mut rd, mut wr) = tokio::io::split(s);
        let reader = tokio::spawn(async move {
            let mut b = [0u8; 4];
            rd.read_exact(&mut b).await.map(|_| b)
        });
        tokio::time::sleep(every * 8).await;
        assert!(
            !reader.is_finished(),
            "{form:?}: the idle tunnel stayed open"
        );
        conn.stream.write_all(b"late").await.unwrap();
        conn.stream.flush().await.unwrap();
        assert_eq!(&reader.await.unwrap().unwrap(), b"late");
        wr.write_all(b"ok").await.unwrap();
        wr.flush().await.unwrap();
        let mut two = [0u8; 2];
        conn.stream.read_exact(&mut two).await.unwrap();
    }
}

/// An HTTP/2 tunnel with no traffic outlives the API connection's idle bound:
/// the connection stays open while a tunnel on it lives.
#[tokio::test]
async fn an_idle_h2_tunnel_outlives_the_api_idle_bound() {
    let idle = Duration::from_millis(100);
    let mut r = start(Knobs {
        api: Limits {
            idle_timeout: idle,
            ..Limits::default()
        },
        ..Knobs::default()
    })
    .await;
    let mut s = dial(&r, Form::Connect, valid_leaf(&r.bundle))
        .await
        .unwrap();
    let (mut conn, _done) = r.conns.recv().await.unwrap();
    tokio::time::sleep(idle * 5).await;
    conn.stream.write_all(b"late").await.unwrap();
    conn.stream.flush().await.unwrap();
    let mut b = [0u8; 4];
    s.read_exact(&mut b).await.unwrap();
    assert_eq!(&b, b"late");
}

#[tokio::test]
async fn a_text_frame_or_an_oversize_frame_closes_the_tunnel() {
    for form in FORMS {
        let r = start(Knobs::default()).await;
        let mut ws = websocket(&r, form).await;
        ws.send(Message::text("hello")).await.unwrap();
        ends_within(&mut ws, Duration::from_secs(2)).await;
        let mut ws = websocket(&r, form).await;
        // The server closes on the frame header, so the send itself may be cut.
        let _ = ws
            .send(Message::Binary(Bytes::from(vec![
                0x16u8;
                buildkit_controller::http::MAX_MESSAGE
                    + 1
            ])))
            .await;
        ends_within(&mut ws, Duration::from_secs(2)).await;
        count(&r.tunnel_metrics(), "tls_error", 2).await;
    }
}

/// A frame of exactly [`MAX_MESSAGE`] payload bytes is read by the server (the
/// bytes are not TLS, so the edge answers with an alert frame); one byte more
/// closes the tunnel without a word.
#[tokio::test]
async fn a_frame_of_exactly_the_cap_is_read_and_one_more_byte_is_not() {
    let cap = buildkit_controller::http::MAX_MESSAGE;
    for form in FORMS {
        let r = start(Knobs::default()).await;
        let mut ws = websocket(&r, form).await;
        ws.send(Message::Binary(Bytes::from(vec![0x16u8; cap])))
            .await
            .unwrap();
        let first = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("an answer");
        assert!(
            matches!(first, Some(Ok(Message::Binary(_)))),
            "{form:?}: {first:?}"
        );
        let mut ws = websocket(&r, form).await;
        let _ = ws
            .send(Message::Binary(Bytes::from(vec![0x16u8; cap + 1])))
            .await;
        let first = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("the tunnel ends");
        assert!(
            !matches!(first, Some(Ok(Message::Binary(_)))),
            "{form:?}: {first:?}"
        );
    }
}

/// The API listener stops without cutting a tunnel; the edge drains it like
/// any edge connection and only then returns.
#[tokio::test]
async fn tunnels_drain_with_the_edge_not_the_api() {
    let idle = Duration::from_millis(100);
    for form in FORMS {
        let mut r = start(Knobs {
            api: Limits {
                idle_timeout: idle,
                ..Limits::default()
            },
            ..Knobs::default()
        })
        .await;
        let mut s = dial(&r, form, valid_leaf(&r.bundle)).await.unwrap();
        let (mut conn, done) = r.conns.recv().await.unwrap();
        r.api_stop.cancel();
        tokio::time::timeout(Duration::from_secs(2), r.api_task)
            .await
            .unwrap_or_else(|_| panic!("{form:?}: the API stops at once"))
            .unwrap();
        // Past the API idle bound too: the tunnel still carries data.
        tokio::time::sleep(idle * 3).await;
        conn.stream.write_all(b"x").await.unwrap();
        conn.stream.flush().await.unwrap();
        s.read_exact(&mut [0u8; 1]).await.unwrap();
        r.edge_stop.cancel();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!r.edge.is_finished(), "the edge waits for the tunnel");
        drop(done);
        tokio::time::timeout(Duration::from_secs(2), r.edge)
            .await
            .unwrap()
            .unwrap();
    }
}
