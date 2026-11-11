//! The seam between the TLS front half and the proxy. The listener owns TLS; a [`ConnectionHandler`] owns everything
//! after it.

use std::net::SocketAddr;

use futures::future::BoxFuture;
use rustls::{CertificateError, Error};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::server::TlsStream;

use super::metrics::Transport;
use crate::pki::AllocationId;

/// A client byte stream the edge runs its handshake on: an accepted TCP
/// connection, or the joined binary frames of a tunnel.
pub trait ClientIo: AsyncRead + AsyncWrite + Unpin + Send + 'static {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> ClientIo for T {}

/// A boxed [`ClientIo`], so one handler serves every transport.
pub type BoxIo = Box<dyn ClientIo>;

/// A client connection that completed the mTLS handshake with a valid
/// allocation cert.
pub struct Accepted {
    /// The client leg; `stream.get_ref().1.peer_certificates()` holds the
    /// verified leaf (its `notAfter` drives the per-connection timer).
    pub stream: TlsStream<BoxIo>,
    /// Identity from the leaf's single URI SAN.
    pub id: AllocationId,
    /// Remote address (`peer` log field). For a tunnel, the client address
    /// of the API connection that carries it.
    pub peer: SocketAddr,
    /// How the connection reached the edge.
    pub transport: Transport,
}

impl std::fmt::Debug for Accepted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Accepted")
            .field("id", &self.id)
            .field("peer", &self.peer)
            .field("transport", &self.transport)
            .finish_non_exhaustive()
    }
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
    /// Check 0 fails when the Pool or Pod store has not listed yet, or has
    /// gone longer than [`STALE_AFTER`](crate::runtime::stores::STALE_AFTER)
    /// since a watch error without completing a relist.
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
    /// checks 1–2 pass and before the client's signature is checked (see
    /// [`crate::edge`]); a refusal aborts the handshake with its alert and is
    /// counted by the verifier. The default admits every verified cert.
    fn admit(&self, id: &AllocationId) -> Result<(), Refusal> {
        let _ = id;
        Ok(())
    }

    /// Owns the connection until it ends. Holding the returned future keeps
    /// the connection counted against `edge.maxConnections` and the drain.
    fn handle(&self, conn: Accepted) -> BoxFuture<'static, ()>;
}
