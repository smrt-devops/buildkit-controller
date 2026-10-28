//! The edge's rustls server config and client-cert verifier.

use std::fmt;
use std::sync::Arc;

use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{
    CryptoProvider, WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature,
};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::{ClientHello, NoServerSessionStorage, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, Error, ServerConfig,
    SignatureScheme,
};
use rustls_pki_types::{CertificateDer, UnixTime};
use tracing::debug;
use x509_parser::prelude::{FromDer, X509Certificate};

use super::handler::ConnectionHandler;
use super::metrics::EdgeMetrics;
use crate::pki::{AllocCertError, AllocationId, verify_allocation_cert};

/// One generation of edge TLS, the one new handshakes use. Swapped whole on hot
/// reload; established connections keep the generation they started on.
#[derive(Debug)]
pub struct EdgeTls {
    /// Server config for the client leg.
    pub server: Arc<ServerConfig>,
    /// The client CA the verifier trusts (the only anchor).
    pub client_ca: CertificateDer<'static>,
    /// Client config for the worker leg; `None` until the
    /// edge client cert is issued.
    pub upstream: Option<Arc<ClientConfig>>,
}

impl EdgeTls {
    /// Re-derives the identity of a verified peer leaf (same rules as the
    /// handshake, evaluated now).
    pub fn identify(&self, leaf: &CertificateDer<'_>) -> Result<AllocationId, AllocCertError> {
        verify_allocation_cert(leaf, &self.client_ca, UnixTime::now())
    }
}

/// Builds [`EdgeTls`] generations around one handler and metrics set.
#[derive(Clone)]
pub struct TlsBuilder {
    provider: Arc<CryptoProvider>,
    handler: Arc<dyn ConnectionHandler>,
    metrics: EdgeMetrics,
}

impl fmt::Debug for TlsBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsBuilder").finish_non_exhaustive()
    }
}

impl TlsBuilder {
    #[must_use]
    pub fn new(handler: Arc<dyn ConnectionHandler>, metrics: EdgeMetrics) -> Self {
        Self {
            provider: Arc::new(rustls::crypto::ring::default_provider()),
            handler,
            metrics,
        }
    }

    /// The crypto provider (to load serving keys with).
    #[must_use]
    pub fn provider(&self) -> &Arc<CryptoProvider> {
        &self.provider
    }

    /// TLS 1.2 + 1.3, default suites, one serving cert for every SNI,
    /// mandatory client auth against `client_ca` only, ALPN `h2`, and no
    /// session resumption of any kind (rustls skips the verifier on resume).
    pub fn build(
        &self,
        serving: Arc<CertifiedKey>,
        client_ca: CertificateDer<'static>,
    ) -> Result<EdgeTls, Error> {
        let (_, ca) = X509Certificate::from_der(&client_ca)
            .map_err(|_| Error::General("client CA is not a parseable certificate".into()))?;
        let verifier = AllocationVerifier {
            client_ca: client_ca.clone(),
            hints: vec![DistinguishedName::from(ca.subject().as_raw().to_vec())],
            algs: self.provider.signature_verification_algorithms,
            handler: Arc::clone(&self.handler),
            metrics: self.metrics.clone(),
        };
        let mut server = ServerConfig::builder_with_provider(Arc::clone(&self.provider))
            .with_safe_default_protocol_versions()?
            .with_client_cert_verifier(Arc::new(verifier))
            .with_cert_resolver(Arc::new(OneCert(serving)));
        server.session_storage = Arc::new(NoServerSessionStorage {});
        server.send_tls13_tickets = 0;
        server.alpn_protocols = vec![b"h2".to_vec()];
        Ok(EdgeTls {
            server: Arc::new(server),
            client_ca,
            upstream: None,
        })
    }
}

/// The same cert whatever the SNI. SNI is not used for routing.
#[derive(Debug)]
struct OneCert(Arc<CertifiedKey>);

impl ResolvesServerCert for OneCert {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(&self.0))
    }
}

/// The edge client verifier. It runs checks 1 to 2 through the pki helper, then the handler's checks 3 to 6.
/// Every failure the verifier returns is counted here, with its `result` label.
struct AllocationVerifier {
    client_ca: CertificateDer<'static>,
    hints: Vec<DistinguishedName>,
    algs: WebPkiSupportedAlgorithms,
    handler: Arc<dyn ConnectionHandler>,
    metrics: EdgeMetrics,
}

impl fmt::Debug for AllocationVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AllocationVerifier").finish_non_exhaustive()
    }
}

/// Maps a pki verdict to the spec's `(result, alert)`. webpki's errors become
/// `unknown_ca` or `certificate_expired`, and everything else `bad_certificate`.
fn classify(e: &AllocCertError) -> (&'static str, CertificateError) {
    use webpki::Error as W;
    match e {
        // Every install's client CA has the same subject DN, so a leaf from another
        // install or from before a rotation fails on the signature, not the issuer
        // name. Either way the alert is unknown_ca.
        AllocCertError::Chain(W::UnknownIssuer | W::InvalidSignatureForPublicKey) => {
            ("tls_error", CertificateError::UnknownIssuer)
        }
        AllocCertError::Chain(
            W::CertExpired { .. } | W::CertNotValidYet { .. } | W::InvalidCertValidity,
        ) => ("tls_error", CertificateError::Expired),
        AllocCertError::Chain(_) | AllocCertError::ClientAuthEku => {
            ("tls_error", CertificateError::BadEncoding)
        }
        AllocCertError::BadSan => ("bad_san", CertificateError::BadEncoding),
    }
}

impl ClientCertVerifier for AllocationVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.hints
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        let id = verify_allocation_cert(end_entity, &self.client_ca, now).map_err(|e| {
            let (result, err) = classify(&e);
            debug!(error = %e, result, "client cert refused");
            self.metrics.outcome(None, result);
            Error::InvalidCertificate(err)
        })?;
        self.handler.admit(&id).map_err(|r| {
            debug!(alloc_id = %id.alloc_id, result = r.result_label(), "allocation refused");
            self.metrics.outcome(Some(&id), r.result_label());
            r.tls_error()
        })?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(message, cert, dss, &self.algs)
            .inspect_err(|_| self.metrics.outcome(None, "tls_error"))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
            .inspect_err(|_| self.metrics.outcome(None, "tls_error"))
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// True when the verifier already counted this handshake failure.
pub(crate) fn counted_by_verifier(e: &std::io::Error) -> bool {
    matches!(
        e.get_ref().and_then(|i| i.downcast_ref::<Error>()),
        Some(Error::InvalidCertificate(_))
    )
}
