//! User-supplied edge/API serving cert (`edge.tls.secretName`).

use std::fmt;
use std::sync::Arc;

use rustls::sign::CertifiedKey;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, DnsName, PrivateKeyDer, ServerName};
use time::OffsetDateTime;
use webpki::EndEntityCert;
use x509_parser::prelude::{FromDer, X509Certificate};

/// Why a `kubernetes.io/tls` Secret was refused (at startup the process exits 2; later
/// the controller keeps the previous cert and counts `buildkit_cert_load_errors_total`).
#[derive(Debug, thiserror::Error)]
pub enum ServingCertError {
    #[error("tls.crt holds no PEM certificate")]
    NoCertificate,
    #[error("tls.key is not a single supported PEM private key")]
    Key,
    #[error("tls.key does not match the leaf certificate")]
    KeyMismatch,
    #[error("leaf certificate does not parse: {0}")]
    Parse(String),
    #[error("Secret lacks tls.crt or tls.key")]
    Missing,
    #[error("leaf certificate is not valid now")]
    Validity,
    /// `notBefore` is more than [`NOT_BEFORE_SKEW`] ahead of the clock. The
    /// cert can still become valid, so callers re-check at the carried instant.
    #[error("leaf certificate is not valid before {0}")]
    NotYetValid(OffsetDateTime),
    #[error("leaf certificate is a CA or its KeyUsage lacks digitalSignature")]
    NotLeaf,
    #[error("leaf certificate has an EKU without serverAuth")]
    Eku,
    #[error("leaf certificate SANs do not cover the server name")]
    Name,
}

/// Clock skew tolerated on a leaf `notBefore`, matching the 5-minute leaf backdating.
pub const NOT_BEFORE_SKEW: time::Duration = time::Duration::minutes(5);

/// A validated user serving cert, ready for rustls.
pub struct UserServingCert {
    certified_key: Arc<CertifiedKey>,
    not_after: OffsetDateTime,
}

impl fmt::Debug for UserServingCert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserServingCert")
            .field("not_after", &self.not_after)
            .finish_non_exhaustive()
    }
}

impl UserServingCert {
    /// Validates `tls.crt` (full chain, leaf first) and `tls.key` at `now`:
    /// key matches the leaf, leaf valid now, not a CA, `KeyUsage` (if present)
    /// includes digitalSignature, EKU serverAuth or no EKU, and its
    /// SANs cover `server_name` (RFC 6125 matching, as clients apply it).
    pub fn from_tls_secret(
        tls_crt: &[u8],
        tls_key: &[u8],
        server_name: &str,
        now: OffsetDateTime,
    ) -> Result<Self, ServingCertError> {
        let chain = CertificateDer::pem_slice_iter(tls_crt)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ServingCertError::NoCertificate)?;
        let leaf = chain
            .first()
            .ok_or(ServingCertError::NoCertificate)?
            .clone();
        let key = PrivateKeyDer::from_pem_slice(tls_key).map_err(|_| ServingCertError::Key)?;
        let signer = rustls::crypto::ring::default_provider()
            .key_provider
            .load_private_key(key)
            .map_err(|_| ServingCertError::Key)?;
        let certified_key = CertifiedKey::new(chain, signer);
        certified_key
            .keys_match()
            .map_err(|_| ServingCertError::KeyMismatch)?;

        let (_, x509) =
            X509Certificate::from_der(&leaf).map_err(|e| ServingCertError::Parse(e.to_string()))?;
        let v = x509.validity();
        let t = now.unix_timestamp();
        if t > v.not_after.timestamp() || v.not_before.timestamp() > v.not_after.timestamp() {
            return Err(ServingCertError::Validity);
        }
        // A duplicated or malformed extension counts as failing (stricter).
        let is_ca = match x509.basic_constraints() {
            Ok(None) => false,
            Ok(Some(bc)) => bc.value.ca,
            Err(_) => true,
        };
        let ku_ok = match x509.key_usage() {
            Ok(None) => true,
            Ok(Some(ku)) => ku.value.digital_signature(),
            Err(_) => false,
        };
        if is_ca || !ku_ok {
            return Err(ServingCertError::NotLeaf);
        }
        match x509.extended_key_usage() {
            Ok(None) => {}
            Ok(Some(eku)) if eku.value.server_auth => {}
            _ => return Err(ServingCertError::Eku),
        }
        let name = DnsName::try_from(server_name)
            .map_err(|_| ServingCertError::Name)?
            .to_owned();
        EndEntityCert::try_from(&leaf)
            .map_err(|e| ServingCertError::Parse(e.to_string()))?
            .verify_is_valid_for_subject_name(&ServerName::DnsName(name))
            .map_err(|_| ServingCertError::Name)?;
        // Last, so only a cert that would otherwise pass can be told to wait.
        if t < v.not_before.timestamp() - NOT_BEFORE_SKEW.whole_seconds() {
            return Err(ServingCertError::NotYetValid(v.not_before.to_datetime()));
        }
        Ok(Self {
            certified_key: Arc::new(certified_key),
            not_after: v.not_after.to_datetime(),
        })
    }

    #[must_use]
    pub fn certified_key(&self) -> Arc<CertifiedKey> {
        Arc::clone(&self.certified_key)
    }

    #[must_use]
    pub fn not_after(&self) -> OffsetDateTime {
        self.not_after
    }
}

/// The CERTIFICATE blocks of a user `ca.crt`, each re-encoded from its parsed
/// DER. Any other block (a private key put there by mistake) and any
/// certificate that does not parse is dropped. `None` when nothing is left.
#[must_use]
pub fn sanitize_ca_pem(raw: &[u8]) -> Option<String> {
    let pem: String = CertificateDer::pem_slice_iter(raw)
        .filter_map(Result::ok)
        .filter(|der| X509Certificate::from_der(der).is_ok())
        .map(|der| super::ca::pem_encode(&der))
        .collect();
    (!pem.is_empty()).then_some(pem)
}
