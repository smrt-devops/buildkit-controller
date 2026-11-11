//! The edge handshake cases through a real tunnel: `GET /v1/tunnel` on a
//! loopback API listener, then mTLS to the edge inside the WebSocket.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use buildkit_controller::edge::{
    Accepted, ConnectionHandler, EdgeMetrics, ListenerOptions, Serve, TlsBuilder, Transport,
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
    let mut cfg = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots(ca))
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    let tcp = TcpStream::connect(api).await.unwrap();
    TlsConnector::from(Arc::new(cfg))
        .connect(ServerName::try_from(HOST).unwrap(), tcp)
        .await
        .unwrap()
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

/// An open WebSocket on `/v1/tunnel`, with no bearer token.
async fn websocket(r: &Rig) -> WebSocketStream<TlsStream<TcpStream>> {
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
    WebSocketStream::from_raw_socket(s, Role::Client, None).await
}

type Inner = tokio_rustls::client::TlsStream<WsBytes<TlsStream<TcpStream>>>;

/// mTLS to the edge inside a new tunnel.
async fn dial(
    r: &Rig,
    cert: (CertificateDer<'static>, PrivateKeyDer<'static>),
) -> std::io::Result<Inner> {
    let ws = websocket(r).await;
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
    cert: (CertificateDer<'static>, PrivateKeyDer<'static>),
) -> AlertDescription {
    let mut s = match dial(r, cert).await {
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
    let mut r = start(Knobs::default()).await;
    let mut s = dial(&r, valid_leaf(&r.bundle)).await.unwrap();
    let (mut conn, _done) = r.conns.recv().await.unwrap();
    assert_eq!(conn.id, ID.parse().unwrap());
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

#[tokio::test]
async fn wrong_ca_and_expired_certs_get_the_tcp_alerts() {
    let r = start(Knobs::default()).await;
    let other = CaBundle::generate(now()).unwrap();
    assert_eq!(
        refused(&r, valid_leaf(&other)).await,
        AlertDescription::UnknownCA
    );
    count(&r.tunnel_metrics(), "tls_error", 1).await;
    let t = now().replace_nanosecond(0).unwrap();
    let expired = leaf(
        &r.bundle,
        t - time::Duration::hours(3),
        t - time::Duration::hours(2),
    );
    assert_eq!(
        refused(&r, expired).await,
        AlertDescription::CertificateExpired
    );
    count(&r.tunnel_metrics(), "tls_error", 2).await;
    assert_eq!(
        r.metrics.outcome_count(None, "tls_error"),
        0,
        "counted under transport=tunnel only"
    );
}

/// A tunnel gives its API slots up at the `101` and holds an edge handshake
/// slot until its handshake times out; then the edge budget is free again
/// and the API connection count is where it was.
#[tokio::test]
async fn a_stalled_handshake_frees_its_slot_and_the_api_count_returns() {
    let mut r = start(Knobs {
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
    })
    .await;
    let mut stalled = websocket(&r).await;
    // The only API slot is free again although the tunnel is open.
    let mut plain = api_tls(r.api, &r.ca()).await;
    let head = exchange(&mut plain, "GET /v1/tunnel HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(head.starts_with("HTTP/1.1 426"), "{head}");
    drop(plain);
    ends_within(&mut stalled, Duration::from_secs(3)).await;
    count(&r.tunnel_metrics(), "tls_error", 1).await;
    // The edge's only handshake slot is free: this tunnel evicts nothing.
    let _held = dial(&r, valid_leaf(&r.bundle)).await.unwrap();
    r.conns.recv().await.unwrap();
    assert_eq!(r.tunnel_metrics().outcome_count(None, "handshake_limit"), 0);
}

/// With pings answered, a tunnel with no traffic stays open past several
/// ping intervals and still carries data.
#[tokio::test]
async fn pings_keep_an_idle_tunnel_open() {
    let every = Duration::from_millis(100);
    let mut r = start(Knobs {
        ping_every: every,
        ..Knobs::default()
    })
    .await;
    let s = dial(&r, valid_leaf(&r.bundle)).await.unwrap();
    let (mut conn, _done) = r.conns.recv().await.unwrap();
    let (mut rd, mut wr) = tokio::io::split(s);
    let reader = tokio::spawn(async move {
        let mut b = [0u8; 4];
        rd.read_exact(&mut b).await.map(|_| b)
    });
    tokio::time::sleep(every * 8).await;
    assert!(!reader.is_finished(), "the idle tunnel stayed open");
    conn.stream.write_all(b"late").await.unwrap();
    conn.stream.flush().await.unwrap();
    assert_eq!(&reader.await.unwrap().unwrap(), b"late");
    wr.write_all(b"ok").await.unwrap();
    wr.flush().await.unwrap();
    let mut two = [0u8; 2];
    conn.stream.read_exact(&mut two).await.unwrap();
}

#[tokio::test]
async fn a_text_frame_or_an_oversize_frame_closes_the_tunnel() {
    let r = start(Knobs::default()).await;
    let mut ws = websocket(&r).await;
    ws.send(Message::text("hello")).await.unwrap();
    ends_within(&mut ws, Duration::from_secs(2)).await;
    let mut ws = websocket(&r).await;
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

/// The API listener stops without cutting a tunnel; the edge drains it like
/// any edge connection and only then returns.
#[tokio::test]
async fn tunnels_drain_with_the_edge_not_the_api() {
    let mut r = start(Knobs::default()).await;
    let mut s = dial(&r, valid_leaf(&r.bundle)).await.unwrap();
    let (mut conn, done) = r.conns.recv().await.unwrap();
    r.api_stop.cancel();
    tokio::time::timeout(Duration::from_secs(2), r.api_task)
        .await
        .expect("the API stops at once")
        .unwrap();
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
