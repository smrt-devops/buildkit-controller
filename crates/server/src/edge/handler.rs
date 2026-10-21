//! The seam between the TLS front half and the proxy. The listener owns TLS; a [`ConnectionHandler`] owns everything
//! after it.

use std::net::SocketAddr;

use futures::future::BoxFuture;
use rustls::{CertificateError, Error};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;

use crate::pki::AllocationId;

/// A client connection that completed the mTLS handshake with a valid
/// allocation cert.
#[derive(Debug)]
pub struct Accepted {
    /// The client leg; `stream.get_ref().1.peer_certificates()` holds the
    /// verified leaf (its `notAfter` drives the per-connection timer).
    pub stream: TlsStream<TcpStream>,
    /// Identity from the leaf's single URI SAN.
    pub id: AllocationId,
    /// Remote address (`peer` log field).
    pub peer: SocketAddr,
}

/// Why [`ConnectionHandler::admit`] refused an allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Check 3 fails when no cached pod matches.
    PodNotFound,
    /// Check 4 fails when the pod's alloc label differs.
    AllocMismatch,
    /// Check 5 fails when the allocation expired or the pod is deleting.
    Expired,
    /// Check 6 fails when the pod is not Running, has no IP, or another
    /// Running pod reports the same IP.
    PodNotReady,
    /// Check 0 fails when the Pool or Pod store is not fresh (a watch error
    /// since the last completed list).
    StoreStale,
}

impl Refusal {
    /// `buildkit_edge_connections_total{result}`.
    #[must_use]
    pub fn result_label(self) -> &'static str {
        match self {
            Self::PodNotFound => "pod_not_found",
            Self::AllocMismatch => "alloc_mismatch",
            Self::Expired => "expired",
            Self::PodNotReady => "pod_not_ready",
            Self::StoreStale => "store_stale",
        }
    }

    /// The rustls error whose alert the spec requires: `certificate_revoked`
    /// for checks 3–5, `access_denied` for checks 0 and 6.
    #[must_use]
    pub fn tls_error(self) -> Error {
        Error::InvalidCertificate(match self {
            Self::PodNotFound | Self::AllocMismatch | Self::Expired => CertificateError::Revoked,
            Self::PodNotReady | Self::StoreStale => {
                CertificateError::ApplicationVerificationFailure
            }
        })
    }
}

/// Implemented by the proxy. One instance serves every connection.
pub trait ConnectionHandler: Send + Sync + 'static {
    /// The pod, allocation and revocation checks, run synchronously inside the handshake after
    /// checks 1–2 pass; a refusal aborts the handshake with its alert and is
    /// counted by the listener. The default admits every verified cert.
    fn admit(&self, id: &AllocationId) -> Result<(), Refusal> {
        let _ = id;
        Ok(())
    }

    /// Owns the connection until it ends. Holding the returned future keeps
    /// the connection counted against `edge.maxConnections` and the drain.
    fn handle(&self, conn: Accepted) -> BoxFuture<'static, ()>;
}

/// Placeholder until the proxy lands. It sends `close_notify` and closes.
#[derive(Debug, Default)]
pub struct CloseHandler;

impl ConnectionHandler for CloseHandler {
    fn handle(&self, mut conn: Accepted) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            let _ = conn.stream.shutdown().await;
        })
    }
}
