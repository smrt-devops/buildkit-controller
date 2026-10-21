//! Edge TLS front half over real TLS on 127.0.0.1.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use buildkit_controller::edge::{
    Accepted, ConnectionHandler, EdgeMetrics, EdgeTls, ListenerOptions, Refusal, Serve, TlsBuilder,
    serve,
};
use buildkit_controller::pki::{
    AllocationId, CLIENT_CA_KEY, CaBundle, EdgeNames, LeafKey, UserServingCert,
};
use buildkit_controller::runtime::metrics::Metrics;
use buildkit_controller::runtime::readiness::Readiness;
use futures::future::BoxFuture;
use rcgen::{
    CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, SanType, string::Ia5String,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::sign::CertifiedKey;
use rustls::{AlertDescription, ClientConfig, HandshakeKind, RootCertStore};
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_util::sync::CancellationToken;

const HOST: &str = "edge.example.test";
const ID: &str =
    "buildkit://team-a/pool-1/0f8fad5b-d9cb-469f-a165-70867728950e/abcdefghijklmnopqrstuvwxyz";

type Held = (Accepted, oneshot::Sender<()>);

/// Hands each connection to the test and holds it until the test lets go.
struct TestHandler {
    tx: mpsc::UnboundedSender<Held>,
    admits: AtomicUsize,
    refuse: Mutex<Option<Refusal>>,
}

impl ConnectionHandler for TestHandler {
    fn admit(&self, _id: &AllocationId) -> Result<(), Refusal> {
        self.admits.fetch_add(1, Ordering::SeqCst);
        self.refuse.lock().unwrap().map_or(Ok(()), Err)
    }

    fn handle(&self, conn: Accepted) -> BoxFuture<'static, ()> {
        let (done, wait) = oneshot::channel();
        let _ = self.tx.send((conn, done));
        Box::pin(async move {
            let _ = wait.await;
        })
    }
}

struct Edge {
    addr: SocketAddr,
    handler: Arc<TestHandler>,
    metrics: EdgeMetrics,
    builder: TlsBuilder,
    tls: watch::Sender<Option<Arc<EdgeTls>>>,
    conns: mpsc::UnboundedReceiver<Held>,
    readiness: Arc<Readiness>,
    shutdown: CancellationToken,
    task: JoinHandle<()>,
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn certified(cert: CertificateDer<'static>, key: PrivateKeyDer<'static>) -> Arc<CertifiedKey> {
    let signer = provider().key_provider.load_private_key(key).unwrap();
    Arc::new(CertifiedKey::new(vec![cert], signer))
}

fn self_issued(bundle: &CaBundle) -> Arc<CertifiedKey> {
    let names = EdgeNames::new(HOST, &[], "bkc", &"ops".parse().unwrap()).unwrap();
    let c = bundle.issue_edge_serving(&names, now()).unwrap();
    certified(c.cert_der().clone(), c.private_key_der().unwrap())
}

fn generation(e: &Edge, bundle: &CaBundle) -> Arc<EdgeTls> {
    let ca = bundle.client_ca().cert_der().clone();
    Arc::new(e.builder.build(self_issued(bundle), ca).unwrap())
}

async fn start(bundle: Option<&CaBundle>) -> Edge {
    let options = ListenerOptions {
        handshake_timeout: Duration::from_secs(2),
        max_connections: 16,
        ..ListenerOptions::default()
    };
    start_with(bundle, options).await
}

async fn start_with(bundle: Option<&CaBundle>, options: ListenerOptions) -> Edge {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, conns) = mpsc::unbounded_channel();
    let handler = Arc::new(TestHandler {
        tx,
        admits: AtomicUsize::new(0),
        refuse: Mutex::new(None),
    });
    let metrics = EdgeMetrics::register(&Metrics::new());
    let builder = TlsBuilder::new(handler.clone(), metrics.clone());
    let (tls, rx) = watch::channel(None);
    let readiness = Arc::new(Readiness::with_spec_checks());
    let shutdown = CancellationToken::new();
    let s = Serve {
        tls: rx,
        handler: handler.clone(),
        metrics: metrics.clone(),
        options,
        ready: readiness.register("edge_listener"),
    };
    let task = tokio::spawn(serve(listener, s, shutdown.clone()));
    let e = Edge {
        addr,
        handler,
        metrics,
        builder,
        tls,
        conns,
        readiness,
        shutdown,
        task,
    };
    if let Some(b) = bundle {
        e.tls.send_replace(Some(generation(&e, b)));
        e.readiness.wait_for(&["edge_listener"]).await;
    }
    e
}

fn alloc_cert(
    bundle: &CaBundle,
    issued: OffsetDateTime,
    expires: OffsetDateTime,
) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let c = bundle
        .issue_allocation(&ID.parse().unwrap(), LeafKey::Generate, issued, expires)
        .unwrap();
    (c.cert_der().clone(), c.private_key_der().unwrap())
}

fn valid_cert(bundle: &CaBundle) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let t = now();
    alloc_cert(
        bundle,
        t,
        t + time::Duration::hours(1) - time::Duration::nanoseconds(i64::from(t.nanosecond())),
    )
}

/// A leaf signed by the bundle's client CA with arbitrary SANs and EKUs.
fn custom_cert(
    bundle: &CaBundle,
    sans: Vec<SanType>,
    ekus: Vec<ExtendedKeyUsagePurpose>,
) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let data = bundle.to_secret_data();
    let ca_key = KeyPair::from_pem(std::str::from_utf8(&data[CLIENT_CA_KEY]).unwrap()).unwrap();
    let issuer = Issuer::from_ca_cert_der(bundle.client_ca().cert_der(), ca_key).unwrap();
    let mut p = CertificateParams::default();
    p.subject_alt_names = sans;
    p.extended_key_usages = ekus;
    let key = KeyPair::generate().unwrap();
    let cert = p.signed_by(&key, &issuer).unwrap();
    (
        cert.der().clone(),
        PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
    )
}

fn uri(s: &str) -> SanType {
    SanType::URI(Ia5String::try_from(s).unwrap())
}

fn client(
    roots: &CertificateDer<'static>,
    cert: Option<(CertificateDer<'static>, PrivateKeyDer<'static>)>,
    alpn: &[&[u8]],
) -> ClientConfig {
    let mut store = RootCertStore::empty();
    store.add(roots.clone()).unwrap();
    let b = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(store);
    let mut c = match cert {
        Some((leaf, key)) => b.with_client_auth_cert(vec![leaf], key).unwrap(),
        None => b.with_no_client_auth(),
    };
    c.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    c
}

async fn dial(
    addr: SocketAddr,
    cfg: ClientConfig,
    sni: &str,
) -> std::io::Result<TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(addr).await?;
    TlsConnector::from(Arc::new(cfg))
        .connect(ServerName::try_from(sni.to_owned()).unwrap(), tcp)
        .await
}

fn alert_of(e: &std::io::Error) -> Option<AlertDescription> {
    match e.get_ref()?.downcast_ref::<rustls::Error>()? {
        rustls::Error::AlertReceived(a) => Some(*a),
        _ => None,
    }
}

/// Dials and returns the alert the server refused with (TLS 1.3 clients see
/// it on the first read after their side of the handshake finishes).
async fn refused(addr: SocketAddr, cfg: ClientConfig) -> AlertDescription {
    let mut s = match dial(addr, cfg, HOST).await {
        Ok(s) => s,
        Err(e) => return alert_of(&e).unwrap_or_else(|| panic!("no alert: {e}")),
    };
    let mut b = [0u8; 1];
    let e = s.read(&mut b).await.expect_err("server should refuse");
    alert_of(&e).unwrap_or_else(|| panic!("no alert: {e}"))
}

async fn count(e: &Edge, id: Option<&AllocationId>, result: &'static str, want: u64) {
    for _ in 0..100 {
        if e.metrics.outcome_count(id, result) == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "{result}: got {}, want {want}",
        e.metrics.outcome_count(id, result)
    );
}

#[tokio::test]
async fn valid_allocation_cert_is_delivered_with_its_id() {
    let b = CaBundle::generate(now()).unwrap();
    let mut e = start(Some(&b)).await;
    let ca = b.client_ca().cert_der().clone();
    let mut s = dial(e.addr, client(&ca, Some(valid_cert(&b)), &[b"h2"]), HOST)
        .await
        .unwrap();
    let (mut conn, _done) = e.conns.recv().await.unwrap();
    assert_eq!(conn.id, ID.parse().unwrap());
    assert_eq!(conn.peer, s.get_ref().0.local_addr().unwrap());
    assert_eq!(s.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    conn.stream.write_all(b"x").await.unwrap();
    let mut buf = [0u8; 1];
    s.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"x");
}

#[tokio::test]
async fn bad_client_certs_are_refused_with_the_spec_alert() {
    let b = CaBundle::generate(now()).unwrap();
    let other = CaBundle::generate(now()).unwrap();
    let e = start(Some(&b)).await;
    let ca = b.client_ca().cert_der().clone();
    let t = now().replace_nanosecond(0).unwrap();
    let serving = b
        .issue_edge_serving(
            &EdgeNames::new(HOST, &[], "bkc", &"ops".parse().unwrap()).unwrap(),
            t,
        )
        .unwrap();
    let cases: Vec<(
        &str,
        (CertificateDer<'static>, PrivateKeyDer<'static>),
        AlertDescription,
        &str,
    )> = vec![
        (
            "expired",
            alloc_cert(
                &b,
                t - time::Duration::hours(3),
                t - time::Duration::hours(2),
            ),
            AlertDescription::CertificateExpired,
            "tls_error",
        ),
        (
            "other CA",
            valid_cert(&other),
            AlertDescription::UnknownCA,
            "tls_error",
        ),
        (
            "no EKU",
            custom_cert(&b, vec![uri(ID)], vec![]),
            AlertDescription::BadCertificate,
            "tls_error",
        ),
        (
            "serving cert",
            (
                serving.cert_der().clone(),
                serving.private_key_der().unwrap(),
            ),
            AlertDescription::BadCertificate,
            "tls_error",
        ),
        (
            "malformed SAN",
            custom_cert(
                &b,
                vec![uri("buildkit://team-a/pool-1/x/y")],
                vec![ExtendedKeyUsagePurpose::ClientAuth],
            ),
            AlertDescription::BadCertificate,
            "bad_san",
        ),
        (
            "two URI SANs",
            custom_cert(
                &b,
                vec![uri(ID), uri(ID)],
                vec![ExtendedKeyUsagePurpose::ClientAuth],
            ),
            AlertDescription::BadCertificate,
            "bad_san",
        ),
        (
            "DNS-only SAN",
            custom_cert(
                &b,
                vec![SanType::DnsName(Ia5String::try_from("a.example").unwrap())],
                vec![ExtendedKeyUsagePurpose::ClientAuth],
            ),
            AlertDescription::BadCertificate,
            "bad_san",
        ),
    ];
    let (mut tls_errors, mut bad_san) = (0, 0);
    for (name, cert, alert, result) in cases {
        assert_eq!(
            refused(e.addr, client(&ca, Some(cert), &[])).await,
            alert,
            "{name}"
        );
        if result == "bad_san" {
            bad_san += 1;
        } else {
            tls_errors += 1;
        }
        count(&e, None, "tls_error", tls_errors).await;
        count(&e, None, "bad_san", bad_san).await;
    }
    assert_eq!(
        refused(e.addr, client(&ca, None, &[])).await,
        AlertDescription::CertificateRequired
    );
    count(&e, None, "tls_error", tls_errors + 1).await;
    assert_eq!(e.handler.admits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn handler_refusals_map_to_alert_and_label() {
    let b = CaBundle::generate(now()).unwrap();
    let e = start(Some(&b)).await;
    let ca = b.client_ca().cert_der().clone();
    let id: AllocationId = ID.parse().unwrap();
    for (r, alert) in [
        (Refusal::PodNotFound, AlertDescription::CertificateRevoked),
        (Refusal::PodNotReady, AlertDescription::AccessDenied),
    ] {
        *e.handler.refuse.lock().unwrap() = Some(r);
        assert_eq!(
            refused(e.addr, client(&ca, Some(valid_cert(&b)), &[])).await,
            alert
        );
        count(&e, Some(&id), r.result_label(), 1).await;
    }
}

#[tokio::test]
async fn alpn_none_and_h2_work_other_protocols_are_refused() {
    let b = CaBundle::generate(now()).unwrap();
    let mut e = start(Some(&b)).await;
    let ca = b.client_ca().cert_der().clone();
    let s = dial(e.addr, client(&ca, Some(valid_cert(&b)), &[]), HOST)
        .await
        .unwrap();
    assert_eq!(s.get_ref().1.alpn_protocol(), None);
    assert!(e.conns.recv().await.is_some());
    let err = dial(
        e.addr,
        client(&ca, Some(valid_cert(&b)), &[b"http/1.1"]),
        HOST,
    )
    .await
    .unwrap_err();
    assert_eq!(
        alert_of(&err),
        Some(AlertDescription::NoApplicationProtocol)
    );
}

#[tokio::test]
async fn resumption_is_never_granted() {
    let b = CaBundle::generate(now()).unwrap();
    let mut e = start(Some(&b)).await;
    let ca = b.client_ca().cert_der().clone();
    for versions in [
        &[&rustls::version::TLS13][..],
        &[&rustls::version::TLS12][..],
    ] {
        let mut store = RootCertStore::empty();
        store.add(ca.clone()).unwrap();
        let (crt, key) = valid_cert(&b);
        let cfg = Arc::new(
            ClientConfig::builder_with_provider(provider())
                .with_protocol_versions(versions)
                .unwrap()
                .with_root_certificates(store)
                .with_client_auth_cert(vec![crt], key)
                .unwrap(),
        );
        let before = e.handler.admits.load(Ordering::SeqCst);
        for _ in 0..2 {
            let mut s = dial(e.addr, (*cfg).clone(), HOST).await.unwrap();
            let (mut conn, _done) = e.conns.recv().await.unwrap();
            assert_eq!(s.get_ref().1.handshake_kind(), Some(HandshakeKind::Full));
            conn.stream.write_all(b"x").await.unwrap();
            s.read_exact(&mut [0u8; 1]).await.unwrap(); // pulls any ticket
        }
        assert_eq!(e.handler.admits.load(Ordering::SeqCst), before + 2);
    }
}

#[tokio::test]
async fn sni_under_the_allocation_domain_is_served_by_a_wildcard_cert() {
    let ca_key = KeyPair::generate().unwrap();
    let mut cap = CertificateParams::default();
    cap.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = cap.self_signed(&ca_key).unwrap();
    let issuer = Issuer::from_ca_cert_der(ca.der(), ca_key).unwrap();
    let mut p =
        CertificateParams::new(vec![HOST.to_owned(), "*.alloc.example.test".to_owned()]).unwrap();
    p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let key = KeyPair::generate().unwrap();
    let leaf = p.signed_by(&key, &issuer).unwrap();
    let user = UserServingCert::from_tls_secret(
        leaf.pem().as_bytes(),
        key.serialize_pem().as_bytes(),
        HOST,
        now(),
    )
    .unwrap();

    let b = CaBundle::generate(now()).unwrap();
    let mut e = start(None).await;
    let gen1 = e
        .builder
        .build(user.certified_key(), b.client_ca().cert_der().clone())
        .unwrap();
    e.tls.send_replace(Some(Arc::new(gen1)));
    let sni = "abcdefghijklmnopqrstuvwxyz.alloc.example.test";
    let s = dial(e.addr, client(ca.der(), Some(valid_cert(&b)), &[]), sni)
        .await
        .unwrap();
    assert_eq!(s.get_ref().1.peer_certificates().unwrap()[0], *leaf.der());
    assert!(e.conns.recv().await.is_some());
}

#[tokio::test]
async fn hot_reload_serves_new_cert_and_keeps_open_connections() {
    let a = CaBundle::generate(now()).unwrap();
    let b = CaBundle::generate(now()).unwrap();
    let mut e = start(Some(&a)).await;
    let ca_a = a.client_ca().cert_der().clone();
    let mut old = dial(e.addr, client(&ca_a, Some(valid_cert(&a)), &[]), HOST)
        .await
        .unwrap();
    let (mut old_conn, _d1) = e.conns.recv().await.unwrap();

    e.tls.send_replace(Some(generation(&e, &b)));
    let ca_b = b.client_ca().cert_der().clone();
    let new = dial(e.addr, client(&ca_b, Some(valid_cert(&b)), &[]), HOST)
        .await
        .unwrap();
    let _ = e.conns.recv().await.unwrap();
    assert_ne!(
        new.get_ref().1.peer_certificates().unwrap()[0],
        old.get_ref().1.peer_certificates().unwrap()[0]
    );
    // The old trust is gone for new handshakes.
    assert_eq!(
        refused(e.addr, client(&ca_b, Some(valid_cert(&a)), &[])).await,
        AlertDescription::UnknownCA
    );
    // The pre-reload connection is still open both ways.
    old_conn.stream.write_all(b"y").await.unwrap();
    let mut buf = [0u8; 1];
    old.read_exact(&mut buf).await.unwrap();
    old.write_all(b"z").await.unwrap();
    old_conn.stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"z");
}

#[tokio::test]
async fn shutdown_stops_accepting_and_drains_open_connections() {
    let b = CaBundle::generate(now()).unwrap();
    let mut e = start(Some(&b)).await;
    let ca = b.client_ca().cert_der().clone();
    let _s = dial(e.addr, client(&ca, Some(valid_cert(&b)), &[]), HOST)
        .await
        .unwrap();
    let (_conn, done) = e.conns.recv().await.unwrap();
    e.shutdown.cancel();
    let mut closed = false;
    for _ in 0..100 {
        if TcpStream::connect(e.addr).await.is_err() {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(closed, "listener still accepting after shutdown");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !e.task.is_finished(),
        "drain must wait for the open connection"
    );
    drop(done);
    tokio::time::timeout(Duration::from_secs(2), e.task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn readiness_flips_only_once_a_cert_is_loaded() {
    let b = CaBundle::generate(now()).unwrap();
    let e = start(None).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(e.readiness.failing().iter().any(|c| c == "edge_listener"));
    // With no cert yet, a dial gets no TLS service.
    let ca = b.client_ca().cert_der().clone();
    let pending = tokio::time::timeout(
        Duration::from_millis(200),
        dial(e.addr, client(&ca, Some(valid_cert(&b)), &[]), HOST),
    )
    .await;
    assert!(pending.is_err(), "no handshake before a cert is loaded");
    e.tls.send_replace(Some(generation(&e, &b)));
    tokio::time::timeout(
        Duration::from_secs(2),
        e.readiness.wait_for(&["edge_listener"]),
    )
    .await
    .unwrap();
    assert!(!e.readiness.failing().iter().any(|c| c == "edge_listener"));
}

/// Opens `n` TCP connections that never send a `ClientHello`, and gives the
/// listener time to accept them.
async fn idle_connects(addr: SocketAddr, n: usize) -> Vec<TcpStream> {
    let mut held = Vec::new();
    for _ in 0..n {
        held.push(TcpStream::connect(addr).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    held
}

/// The server closes `s` without a handshake (EOF or reset within 1 s).
async fn closed_at_once(mut s: TcpStream) -> bool {
    let mut b = [0u8; 1];
    matches!(
        tokio::time::timeout(Duration::from_secs(1), s.read(&mut b)).await,
        Ok(Ok(0) | Err(_))
    )
}

/// Idle connects hold only handshake slots, never established ones, so they
/// cannot lock out a client with a valid certificate.
#[tokio::test]
async fn idle_connects_cannot_take_established_slots() {
    let b = CaBundle::generate(now()).unwrap();
    let mut e = start_with(
        Some(&b),
        ListenerOptions {
            handshake_timeout: Duration::from_secs(5),
            max_connections: 2,
            max_handshakes: 64,
            max_handshakes_per_ip: 16,
        },
    )
    .await;
    let _idle = idle_connects(e.addr, 4).await;
    let ca = b.client_ca().cert_der().clone();
    let dialled = tokio::time::timeout(
        Duration::from_millis(1500),
        dial(e.addr, client(&ca, Some(valid_cert(&b)), &[b"h2"]), HOST),
    )
    .await;
    let _s = dialled.expect("valid client locked out").unwrap();
    let (conn, _done) = e.conns.recv().await.unwrap();
    assert_eq!(conn.id, ID.parse().unwrap());
}

/// Connections over the per-address or the global handshake cap are closed
/// on accept; a freed slot admits the next client.
#[tokio::test]
async fn handshake_caps_close_extra_connects() {
    for (global, per_ip) in [(64, 2), (2, 16)] {
        let b = CaBundle::generate(now()).unwrap();
        let mut e = start_with(
            Some(&b),
            ListenerOptions {
                handshake_timeout: Duration::from_secs(5),
                max_connections: 16,
                max_handshakes: global,
                max_handshakes_per_ip: per_ip,
            },
        )
        .await;
        let mut idle = idle_connects(e.addr, 2).await;
        let extra = TcpStream::connect(e.addr).await.unwrap();
        assert!(
            closed_at_once(extra).await,
            "cap ({global}, {per_ip}) not enforced"
        );
        idle.clear();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let ca = b.client_ca().cert_der().clone();
        let _s = dial(e.addr, client(&ca, Some(valid_cert(&b)), &[b"h2"]), HOST)
            .await
            .unwrap();
        e.conns.recv().await.unwrap();
    }
}
