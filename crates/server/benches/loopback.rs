//! Loopback benchmark for the edge (buildkit-controller-bln): throughput and
//! connect latency through the real listener and proxy against a TLS stand-in
//! for buildkitd, versus a direct connection to the same stand-in. All on
//! 127.0.0.1 in one process, with no kind and no Docker. The method and the
//! recorded runs are in `docs/performance.md`. Run it with
//! `cargo xtask perf --loopback`.
//!
//! The upstream protocol: the client sends 8 bytes, big-endian, naming how
//! many bytes the upstream sends back. The upstream then sends them while it
//! reads and discards the client's bytes to EOF, and closes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use buildkit_controller::edge::{
    EdgeMetrics, EdgeTls, ListenerOptions, PodView, ProxyHandler, ProxyOptions, Serve, TlsBuilder,
    client_config, serve,
};
use buildkit_controller::pki::{AllocationId, CaBundle, EdgeNames, LeafKey, WorkerIdentity};
use buildkit_controller::runtime::metrics::Metrics;
use buildkit_controller::runtime::readiness::Readiness;
use buildkit_controller::runtime::stores::StoreFreshness;
use buildkit_controller_api::v1alpha1::BuildKitPool;
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::{reflector, watcher};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::WebPkiClientVerifier;
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use serde_json::json;
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tokio_util::sync::CancellationToken;

const HOST: &str = "edge.example.test";
const UID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
const ALLOC: &str = "abcdefghijklmnopqrstuvwxyz";
const POOL_UID: &str = "7d9f3c1e-0000-4000-8000-000000000001";
/// Nonce of the one worker pod, `pool-1-0`.
const NONCE: &str = "0123456789abcdef";
/// Its serving cert name, `<nonce>.<pod>.<pool>.<ns>.worker.buildkit.internal`.
const WORKER_NAME: &str = "0123456789abcdef.pool-1-0.pool-1.team-a.worker.buildkit.internal";
/// Connects per path and concurrency level, after a warm-up of the same
/// size divided by ten.
const CONNECTS: usize = 2000;
/// Total bytes per throughput run, split across the streams.
const BULK: u64 = 512 * 1024 * 1024;
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

fn id() -> AllocationId {
    format!("buildkit://team-a/pool-1/{UID}/{ALLOC}")
        .parse()
        .unwrap()
}

fn pod() -> Pod {
    let expires = (now() + time::Duration::hours(1))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    serde_json::from_value(json!({
        "metadata": {
            "name": "pool-1-0", "namespace": "team-a", "uid": UID,
            "labels": {
                "app.kubernetes.io/managed-by": "buildkit-controller",
                "buildkit.smrt-devops.net/pool": "pool-1",
            },
            "annotations": {
                "buildkit.smrt-devops.net/alloc": ALLOC,
                "buildkit.smrt-devops.net/expires-at": expires,
                "buildkit.smrt-devops.net/worker-nonce": NONCE,
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

/// The buildkitd stand-in: internal-CA serving cert, mandatory internal-CA
/// client auth, the protocol in the module docs.
async fn upstream(bundle: &CaBundle) -> u16 {
    let serving = bundle
        .issue_worker_serving(
            &WorkerIdentity {
                namespace: "team-a".parse().unwrap(),
                pool: "pool-1".parse().unwrap(),
                pod: "pool-1-0".into(),
                nonce: NONCE.into(),
            },
            now(),
        )
        .unwrap();
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
    tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(s) = acceptor.accept(tcp).await else {
                    return;
                };
                let (mut rd, mut wr) = tokio::io::split(s);
                let mut head = [0u8; 8];
                if rd.read_exact(&mut head).await.is_err() {
                    return;
                }
                let mut left = u64::from_be_bytes(head);
                let send = async {
                    let chunk = vec![0xA5u8; 64 * 1024];
                    while left > 0 {
                        let n = usize::try_from(left.min(chunk.len() as u64)).unwrap();
                        if wr.write_all(&chunk[..n]).await.is_err() {
                            return;
                        }
                        left -= n as u64;
                    }
                    let _ = wr.shutdown().await;
                };
                let sink = async {
                    let mut buf = vec![0u8; 256 * 1024];
                    while matches!(rd.read(&mut buf).await, Ok(n) if n > 0) {}
                };
                tokio::join!(send, sink);
            });
        }
    });
    port
}

fn certified(cert: CertificateDer<'static>, key: PrivateKeyDer<'static>) -> Arc<CertifiedKey> {
    Arc::new(CertifiedKey::new(
        vec![cert],
        provider().key_provider.load_private_key(key).unwrap(),
    ))
}

/// The real listener and proxy over a one-pod store. Returns the edge address.
async fn edge(bundle: &CaBundle, worker_port: u16, stop: &CancellationToken) -> SocketAddr {
    let (reader, mut writer) = reflector::store::<Pod>();
    writer.apply_watcher_event(&watcher::Event::Apply(pod()));
    let (pools, mut pw) = reflector::store::<BuildKitPool>();
    let mut pool = BuildKitPool::new("pool-1", serde_json::from_value(json!({})).unwrap());
    pool.metadata.namespace = Some("team-a".into());
    pool.metadata.uid = Some(POOL_UID.into());
    pw.apply_watcher_event(&watcher::Event::Apply(pool));
    // The stores must outlive the run.
    std::mem::forget((writer, pw));

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
            upstream_connect_timeout: Duration::from_secs(5),
            upstream_port: worker_port,
            max_per_namespace: 500,
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
    std::mem::forget(tls_tx);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let args = Serve {
        tls: tls_rx,
        handler: Arc::new(proxy),
        metrics,
        options: ListenerOptions {
            max_connections: 4096,
            ..ListenerOptions::default()
        },
        ready: Readiness::with_spec_checks().register("edge_listener"),
    };
    tokio::spawn(serve(listener, args, stop.clone()));
    addr
}

fn client_cfg(
    roots: &CertificateDer<'static>,
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Arc<ClientConfig> {
    let mut store = RootCertStore::empty();
    store.add(roots.clone()).unwrap();
    let mut cfg = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(store)
        .with_client_auth_cert(chain, key)
        .unwrap();
    cfg.alpn_protocols = vec![b"h2".to_vec()];
    Arc::new(cfg)
}

/// One path to the upstream: a TLS client config and the address and name to dial.
#[derive(Clone)]
struct Path {
    addr: SocketAddr,
    sni: &'static str,
    cfg: Arc<ClientConfig>,
}

impl Path {
    async fn open(&self, reply: u64) -> tokio_rustls::client::TlsStream<TcpStream> {
        let tcp = TcpStream::connect(self.addr).await.unwrap();
        tcp.set_nodelay(true).unwrap();
        let mut s = TlsConnector::from(Arc::clone(&self.cfg))
            .connect(ServerName::try_from(self.sni).unwrap(), tcp)
            .await
            .unwrap();
        s.write_all(&reply.to_be_bytes()).await.unwrap();
        s
    }

    /// Full mTLS connect up to the first response byte.
    async fn connect_once(&self) -> Duration {
        let t = Instant::now();
        let mut s = self.open(1).await;
        let mut b = [0u8; 1];
        s.read_exact(&mut b).await.unwrap();
        t.elapsed()
    }

    /// Moves `up` bytes to the upstream and `down` bytes back.
    async fn bulk(&self, up: u64, down: u64) {
        let s = self.open(down).await;
        let (mut rd, mut wr) = tokio::io::split(s);
        let send = async {
            let chunk = vec![0x5Au8; 64 * 1024];
            let mut left = up;
            while left > 0 {
                let n = usize::try_from(left.min(chunk.len() as u64)).unwrap();
                wr.write_all(&chunk[..n]).await.unwrap();
                left -= n as u64;
            }
            wr.shutdown().await.unwrap();
        };
        let recv = async {
            let mut buf = vec![0u8; 256 * 1024];
            let mut got = 0u64;
            while let Ok(n) = rd.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                got += n as u64;
            }
            assert_eq!(got, down, "short read");
        };
        tokio::join!(send, recv);
    }
}

/// `n` connects, `conc` at a time. Returns the sorted samples in ms.
async fn latencies(p: &Path, n: usize, conc: usize) -> Vec<f64> {
    let mut out = Vec::with_capacity(n);
    let mut left = n;
    while left > 0 {
        let batch = left.min(conc);
        let tasks: Vec<_> = (0..batch)
            .map(|_| {
                let p = p.clone();
                tokio::spawn(async move { p.connect_once().await })
            })
            .collect();
        for t in tasks {
            out.push(t.await.unwrap().as_secs_f64() * 1e3);
        }
        left -= batch;
    }
    out.sort_by(f64::total_cmp);
    out
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Aggregate GiB/s of `streams` concurrent transfers.
async fn throughput(p: &Path, streams: usize, up: bool) -> f64 {
    let per = BULK / streams as u64;
    let t = Instant::now();
    let tasks: Vec<_> = (0..streams)
        .map(|_| {
            let p = p.clone();
            tokio::spawn(async move {
                if up {
                    p.bulk(per, 0).await;
                } else {
                    p.bulk(0, per).await;
                }
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    (per * streams as u64) as f64 / GIB / t.elapsed().as_secs_f64()
}

#[tokio::main]
async fn main() {
    let bundle = CaBundle::generate(now()).unwrap();
    let port = upstream(&bundle).await;
    let stop = CancellationToken::new();
    let edge_addr = edge(&bundle, port, &stop).await;

    let t = now();
    let exp = t + time::Duration::hours(1) - time::Duration::nanoseconds(i64::from(t.nanosecond()));
    let leaf = bundle
        .issue_allocation(&id(), LeafKey::Generate, t, exp)
        .unwrap();
    let through = Path {
        addr: edge_addr,
        sni: HOST,
        cfg: client_cfg(
            bundle.client_ca().cert_der(),
            vec![leaf.cert_der().clone()],
            leaf.private_key_der().unwrap(),
        ),
    };
    let ec = bundle.issue_edge_client(t).unwrap();
    let direct = Path {
        addr: SocketAddr::from(([127, 0, 0, 1], port)),
        sni: WORKER_NAME,
        cfg: client_cfg(
            bundle.internal_ca().cert_der(),
            vec![ec.cert_der().clone()],
            ec.private_key_der().unwrap(),
        ),
    };

    println!(
        "loopback, {CONNECTS} connects per row, {} MiB per throughput run",
        BULK >> 20
    );
    println!("| Metric | direct | edge |\n|---|---|---|");
    for conc in [1, 8] {
        let mut rows = Vec::new();
        for p in [&direct, &through] {
            latencies(p, CONNECTS / 10, conc).await;
            rows.push(latencies(p, CONNECTS, conc).await);
        }
        for q in [50.0, 95.0, 99.0] {
            let (d, e) = (pct(&rows[0], q), pct(&rows[1], q));
            println!(
                "| connect p{q:.0}, {conc} at a time | {d:.2} ms | {e:.2} ms (overhead {:+.2} ms) |",
                e - d
            );
        }
    }
    for (streams, up) in [(1, true), (8, true), (1, false), (8, false)] {
        let mut cells = Vec::new();
        for p in [&direct, &through] {
            throughput(p, streams, up).await; // warm-up
            cells.push(throughput(p, streams, up).await);
        }
        let dir = if up { "upload" } else { "download" };
        println!(
            "| {dir}, {streams} stream(s) | {:.2} GiB/s | {:.2} GiB/s ({:.0}% of direct) |",
            cells[0],
            cells[1],
            cells[1] / cells[0] * 100.0
        );
    }
    stop.cancel();
}
