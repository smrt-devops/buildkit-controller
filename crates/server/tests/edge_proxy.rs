//! The proxy end to end over real TLS: listener, admission,
//! upstream mTLS to a fake worker, revocation from Pod events.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use buildkit_controller::edge::{
    Direction, EdgeMetrics, EdgeTls, ListenerOptions, PodView, ProxyHandler, ProxyOptions,
    Revocation, Serve, TlsBuilder, client_config, serve,
};
use buildkit_controller::pki::{AllocationId, CaBundle, EdgeNames, LeafKey, WorkerIdentity};
use buildkit_controller::runtime::metrics::Metrics;
use buildkit_controller::runtime::readiness::Readiness;
use buildkit_controller::runtime::stores::{PodEvent, PodSubscription, StoreFreshness};
use buildkit_controller_api::v1alpha1::BuildKitPool;
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::reflector::store::Writer;
use kube::runtime::{reflector, watcher};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::WebPkiClientVerifier;
use rustls::sign::CertifiedKey;
use rustls::{AlertDescription, ClientConfig, RootCertStore, ServerConfig};
use serde_json::json;
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tokio_util::sync::CancellationToken;

const HOST: &str = "edge.example.test";
const UID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
const ALLOC: &str = "abcdefghijklmnopqrstuvwxyz";
const POOL_UID: &str = "7d9f3c1e-0000-4000-8000-000000000001";
/// Nonce of the pod the worker stand-in serves as, `pool-1-0`.
const NONCE: &str = "0123456789abcdef";

fn id() -> AllocationId {
    format!("buildkit://team-a/pool-1/{UID}/{ALLOC}")
        .parse()
        .unwrap()
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

fn pod(alloc: &str) -> Pod {
    pod_as(alloc, "pool-1-0", NONCE)
}

/// A worker pod named `name` whose `P worker-nonce` is `nonce`.
fn pod_as(alloc: &str, name: &str, nonce: &str) -> Pod {
    serde_json::from_value(json!({
        "metadata": {
            "name": name, "namespace": "team-a", "uid": UID,
            "labels": {
                "app.kubernetes.io/managed-by": "buildkit-controller",
                "buildkit.smrt-devops.net/pool": "pool-1",
            },
            "annotations": {
                "buildkit.smrt-devops.net/alloc": alloc,
                "buildkit.smrt-devops.net/expires-at": rfc3339(now() + time::Duration::hours(1)),
                "buildkit.smrt-devops.net/worker-nonce": nonce,
            },
            "ownerReferences": [{
                "apiVersion": "buildkit.smrt-devops.net/v1alpha1", "kind": "BuildKitPool",
                "name": "pool-1", "uid": POOL_UID, "controller": true,
            }],
        },
        "status": { "phase": "Running", "podIP": "127.0.0.1" },
    }))
    .unwrap()
}

fn certified(cert: CertificateDer<'static>, key: PrivateKeyDer<'static>) -> Arc<CertifiedKey> {
    Arc::new(CertifiedKey::new(
        vec![cert],
        provider().key_provider.load_private_key(key).unwrap(),
    ))
}

/// A buildkitd stand-in with an internal-CA serving cert, mandatory internal-CA
/// client auth, echoes bytes; reports each negotiated ALPN.
/// The identity of `pool-1-0` with [`NONCE`].
fn served() -> WorkerIdentity {
    WorkerIdentity {
        namespace: "team-a".parse().unwrap(),
        pool: "pool-1".parse().unwrap(),
        pod: "pool-1-0".into(),
        nonce: NONCE.into(),
    }
}

async fn worker(
    bundle: &CaBundle,
    serves: &WorkerIdentity,
) -> (u16, mpsc::UnboundedReceiver<Option<Vec<u8>>>) {
    let serving = bundle.issue_worker_serving(serves, now()).unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(bundle.internal_ca().cert_der().clone()).unwrap();
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider())
        .build()
        .unwrap();
    let mut cfg = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![serving.cert_der().clone()],
            serving.private_key_der().unwrap(),
        )
        .unwrap();
    cfg.alpn_protocols = vec![b"h2".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(cfg));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            let (acceptor, tx) = (acceptor.clone(), tx.clone());
            tokio::spawn(async move {
                let Ok(s) = acceptor.accept(tcp).await else {
                    return;
                };
                let _ = tx.send(s.get_ref().1.alpn_protocol().map(<[u8]>::to_vec));
                let (mut rd, mut wr) = tokio::io::split(s);
                let _ = tokio::io::copy(&mut rd, &mut wr).await;
                let _ = wr.shutdown().await;
            });
        }
    });
    (port, rx)
}

struct Rig {
    edge: SocketAddr,
    bundle: CaBundle,
    proxy: ProxyHandler,
    metrics: EdgeMetrics,
    events: broadcast::Sender<PodEvent>,
    pods: Writer<Pod>,
    alpn: mpsc::UnboundedReceiver<Option<Vec<u8>>>,
    worker_port: u16,
    _stop: tokio_util::sync::DropGuard,
}

async fn rig(pods: Vec<Pod>) -> Rig {
    rig_serving(pods, &served()).await
}

/// A rig whose worker stand-in holds the serving cert of `serves`.
async fn rig_serving(pods: Vec<Pod>, serves: &WorkerIdentity) -> Rig {
    let bundle = CaBundle::generate(now()).unwrap();
    let (worker_port, alpn) = worker(&bundle, serves).await;
    let (reader, mut writer) = reflector::store::<Pod>();
    for p in pods {
        writer.apply_watcher_event(&watcher::Event::Apply(p));
    }
    let (pools, mut pw) = reflector::store::<BuildKitPool>();
    let mut pool = BuildKitPool::new("pool-1", serde_json::from_value(json!({})).unwrap());
    pool.metadata.namespace = Some("team-a".into());
    pool.metadata.uid = Some(POOL_UID.into());
    pw.apply_watcher_event(&watcher::Event::Apply(pool));

    let metrics = EdgeMetrics::register(&Metrics::new());
    let (tls_tx, tls_rx) = watch::channel::<Option<Arc<EdgeTls>>>(None);
    let proxy = ProxyHandler::new(
        PodView {
            pods: reader,
            pools,
            fresh: StoreFreshness::fixed(true),
        },
        tls_rx.clone(),
        metrics.clone(),
        ProxyOptions {
            upstream_connect_timeout: Duration::from_secs(2),
            upstream_port: worker_port,
        },
    );
    let builder = TlsBuilder::new(Arc::new(proxy.clone()), metrics.clone());
    let names = EdgeNames::new(HOST, &[], "bkc", &"ops".parse().unwrap()).unwrap();
    let srv = bundle.issue_edge_serving(&names, now()).unwrap();
    let mut g = builder
        .build(
            certified(srv.cert_der().clone(), srv.private_key_der().unwrap()),
            bundle.client_ca().cert_der().clone(),
        )
        .unwrap();
    let ec = bundle.issue_edge_client(now()).unwrap();
    g.upstream = Some(
        client_config(
            provider(),
            bundle.internal_ca().cert_der().clone(),
            vec![ec.cert_der().clone()],
            ec.private_key_der().unwrap(),
        )
        .unwrap(),
    );
    tls_tx.send_replace(Some(Arc::new(g)));

    let stop = CancellationToken::new();
    let (events, sub) = broadcast::channel(16);
    let p = proxy.clone();
    let s = stop.clone();
    tokio::spawn(async move { p.revoke_on(PodSubscription::new(sub), s).await });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let edge = listener.local_addr().unwrap();
    let readiness = Readiness::with_spec_checks();
    let serve_args = Serve {
        tls: tls_rx,
        handler: Arc::new(proxy.clone()),
        metrics: metrics.clone(),
        options: ListenerOptions {
            handshake_timeout: Duration::from_secs(2),
            max_connections: 16,
            ..ListenerOptions::default()
        },
        ready: readiness.register("edge_listener"),
    };
    tokio::spawn(serve(listener, serve_args, stop.clone()));
    std::mem::forget(tls_tx);
    Rig {
        edge,
        bundle,
        proxy,
        metrics,
        events,
        pods: writer,
        alpn,
        worker_port,
        _stop: stop.drop_guard(),
    }
}

fn client(rig: &Rig, alloc: &AllocationId, roots: &CertificateDer<'static>) -> ClientConfig {
    let t = now();
    let exp = t + time::Duration::hours(1) - time::Duration::nanoseconds(i64::from(t.nanosecond()));
    let c = rig
        .bundle
        .issue_allocation(alloc, LeafKey::Generate, t, exp)
        .unwrap();
    let mut store = RootCertStore::empty();
    store.add(roots.clone()).unwrap();
    let mut cfg = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(store)
        .with_client_auth_cert(vec![c.cert_der().clone()], c.private_key_der().unwrap())
        .unwrap();
    cfg.alpn_protocols = vec![b"h2".to_vec()];
    cfg
}

async fn connect(
    addr: SocketAddr,
    cfg: ClientConfig,
    sni: &str,
) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(addr).await?;
    let mut s = TlsConnector::from(Arc::new(cfg))
        .connect(ServerName::try_from(sni.to_owned()).unwrap(), tcp)
        .await?;
    // TLS 1.3 client auth failures surface on the first read.
    s.write_all(b"ping").await?;
    let mut buf = [0u8; 4];
    s.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"ping");
    Ok(s)
}

fn alert_of(e: &std::io::Error) -> Option<AlertDescription> {
    match e.get_ref()?.downcast_ref::<rustls::Error>()? {
        rustls::Error::AlertReceived(a) => Some(*a),
        _ => None,
    }
}

/// Reads until the edge closes; panics if it stays open past 1 s.
async fn closed_within_a_second(s: &mut tokio_rustls::client::TlsStream<TcpStream>) {
    let mut buf = [0u8; 64];
    let r = tokio::time::timeout(Duration::from_secs(1), s.read(&mut buf)).await;
    assert!(
        matches!(r, Ok(Ok(0) | Err(_))),
        "connection still open: {r:?}"
    );
}

async fn live(rig: &mut Rig) -> tokio_rustls::client::TlsStream<TcpStream> {
    let ca = rig.bundle.client_ca().cert_der().clone();
    let s = connect(rig.edge, client(rig, &id(), &ca), HOST)
        .await
        .unwrap();
    assert_eq!(
        rig.alpn.recv().await.unwrap().as_deref(),
        Some(&b"h2"[..]),
        "upstream ALPN h2"
    );
    assert_eq!(rig.metrics.outcome_count(Some(&id()), "ok"), 1);
    s
}

#[tokio::test]
async fn proxies_and_pod_delete_cuts_the_session() {
    let mut rig = rig(vec![pod(ALLOC)]).await;
    let mut s = live(&mut rig).await;
    assert_eq!(rig.proxy.live_connections(), 1);
    rig.pods
        .apply_watcher_event(&watcher::Event::Delete(pod(ALLOC)));
    rig.events
        .send(PodEvent::Delete(Arc::new(pod(ALLOC))))
        .unwrap();
    closed_within_a_second(&mut s).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(rig.metrics.revocation_count(Revocation::PodGone), 1);
    assert_eq!(rig.proxy.live_connections(), 0);
}

#[tokio::test]
async fn alloc_change_and_draining_cut_the_session() {
    for (alloc, reason) in [
        ("draining", Revocation::Released),
        ("free", Revocation::Released),
    ] {
        let mut rig = rig(vec![pod(ALLOC)]).await;
        let mut s = live(&mut rig).await;
        rig.events
            .send(PodEvent::Apply(Arc::new(pod(alloc))))
            .unwrap();
        closed_within_a_second(&mut s).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(rig.metrics.revocation_count(reason), 1, "{alloc}");
    }
}

#[tokio::test]
async fn admit_refuses_inside_the_handshake() {
    let rig = rig(vec![pod("free")]).await;
    let ca = rig.bundle.client_ca().cert_der().clone();
    let e = connect(rig.edge, client(&rig, &id(), &ca), HOST)
        .await
        .unwrap_err();
    assert_eq!(
        alert_of(&e),
        Some(AlertDescription::CertificateRevoked),
        "{e}"
    );
    assert_eq!(rig.metrics.outcome_count(Some(&id()), "alloc_mismatch"), 1);

    let other: AllocationId = format!("buildkit://team-a/pool-1/{UID}/bbbbbbbbbbbbbbbbbbbbbbbbbb")
        .parse()
        .unwrap();
    let e = connect(rig.edge, client(&rig, &other, &ca), HOST)
        .await
        .unwrap_err();
    assert_eq!(
        alert_of(&e),
        Some(AlertDescription::CertificateRevoked),
        "{e}"
    );
}

#[tokio::test]
async fn allocation_cert_straight_at_the_worker_is_refused() {
    let rig = rig(vec![pod(ALLOC)]).await;
    let worker: SocketAddr = ([127, 0, 0, 1], rig.worker_port).into();
    let roots = rig.bundle.internal_ca().cert_der().clone();
    let e = connect(worker, client(&rig, &id(), &roots), &served().dns_name())
        .await
        .unwrap_err();
    assert!(
        matches!(
            alert_of(&e),
            Some(
                AlertDescription::UnknownCA
                    | AlertDescription::BadCertificate
                    | AlertDescription::DecryptError
            )
        ),
        "{e}"
    );
}

#[tokio::test]
async fn bytes_are_counted() {
    let mut rig = rig(vec![pod(ALLOC)]).await;
    let mut s = live(&mut rig).await;
    let data = vec![5u8; 300_000];
    s.write_all(&data).await.unwrap();
    let mut back = vec![0u8; data.len()];
    s.read_exact(&mut back).await.unwrap();
    s.shutdown().await.unwrap();
    let mut rest = Vec::new();
    let _ = s.read_to_end(&mut rest).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The bytes are the TLS records' plaintext, a ping and data each way.
    let (up, down) = (
        rig.metrics.bytes(Direction::ClientToWorker).get(),
        rig.metrics.bytes(Direction::WorkerToClient).get(),
    );
    assert_eq!((up, down), (300_004, 300_004));
}

/// Connects through the edge to a worker whose cert names another pod
/// incarnation than the store's pod, and expects the edge to refuse the dial.
async fn refused_upstream(store_pod: Pod) {
    let rig = rig_serving(vec![store_pod], &served()).await;
    let ca = rig.bundle.client_ca().cert_der().clone();
    assert!(
        connect(rig.edge, client(&rig, &id(), &ca), HOST)
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(rig.metrics.outcome_count(Some(&id()), "upstream_error"), 1);
    assert_eq!(rig.metrics.outcome_count(Some(&id()), "ok"), 0);
}

#[tokio::test]
async fn pod_a_cert_is_refused_when_dialling_pod_b() {
    // The store resolves the allocation to pool-1-1, but the IP answers with
    // pool-1-0's cert (a stale store, or a reused IP).
    refused_upstream(pod_as(ALLOC, "pool-1-1", "fedcba9876543210")).await;
}

#[tokio::test]
async fn same_slot_successor_is_refused_with_its_predecessors_cert() {
    // Same slot pod name, new incarnation: the predecessor's cert names the old
    // nonce, so it no longer passes.
    refused_upstream(pod_as(ALLOC, "pool-1-0", "fedcba9876543210")).await;
}
