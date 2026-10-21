//! The upstream leg: mTLS to `<podIP>:1234` with the
//! internal-CA edge client cert.

use std::net::SocketAddr;
use std::sync::Arc;

use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, Error, RootCertStore};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tracing::debug;

use super::listener::tune_socket;
use crate::pki::WorkerIdentity;

/// Client config for the worker leg. The only root is the internal CA, the
/// client cert is the edge client cert, and ALPN is always `h2`.
pub fn client_config(
    provider: Arc<CryptoProvider>,
    internal_ca: CertificateDer<'static>,
    client_chain: Vec<CertificateDer<'static>>,
    client_key: PrivateKeyDer<'static>,
) -> Result<Arc<ClientConfig>, Error> {
    let mut roots = RootCertStore::empty();
    roots.add(internal_ca)?;
    let mut cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_client_auth_cert(client_chain, client_key)?;
    cfg.alpn_protocols = vec![b"h2".to_vec()];
    Ok(Arc::new(cfg))
}

/// The name the worker serving cert must carry: the exact pod incarnation,
/// `<nonce>.<pod>.<pool>.<namespace>.worker.buildkit.internal`.
pub fn worker_name(worker: &WorkerIdentity) -> Result<ServerName<'static>, Error> {
    ServerName::try_from(worker.dns_name())
        .map_err(|_| Error::General("worker name is not a DNS name".into()))
}

/// TCP connect then TLS; the caller bounds it by `edge.upstreamConnectTimeout`.
pub async fn dial(
    addr: SocketAddr,
    cfg: Arc<ClientConfig>,
    name: ServerName<'static>,
) -> std::io::Result<TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(addr).await?;
    if let Err(e) = tune_socket(&tcp) {
        debug!(error = %e, "upstream socket options");
    }
    TlsConnector::from(cfg).connect(name, tcp).await
}
