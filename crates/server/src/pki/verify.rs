//! Edge-side checks of an allocation cert.

use rustls_pki_types::{CertificateDer, UnixTime};
use webpki::{EndEntityCert, KeyUsage, anchor_from_trusted_cert};
use x509_parser::extensions::{GeneralName, ParsedExtension};
use x509_parser::oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME;
use x509_parser::prelude::{FromDer, X509Certificate};

use super::AllocationId;

/// id-kp-clientAuth (1.3.6.1.5.5.7.3.2), DER contents.
const EKU_CLIENT_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x02];

/// Why an allocation cert was refused.
#[derive(Debug, thiserror::Error)]
pub enum AllocCertError {
    /// Check 1 (`tls_error`): chain, validity, signature or EKU.
    #[error("certificate chain rejected: {0}")]
    Chain(webpki::Error),
    /// Check 1 (`tls_error`): EKU extension absent or without clientAuth.
    #[error("certificate lacks EKU clientAuth")]
    ClientAuthEku,
    /// Check 2 (`bad_san`).
    #[error("SAN is not exactly one valid buildkit:// URI")]
    BadSan,
}

/// Verifies `cert` chains to `client_ca` (the only trust anchor) at `now`
/// with EKU clientAuth explicitly present, then returns the identity from its
/// single URI SAN. Never reads the CN.
pub fn verify_allocation_cert(
    cert: &CertificateDer<'_>,
    client_ca: &CertificateDer<'_>,
    now: UnixTime,
) -> Result<AllocationId, AllocCertError> {
    let anchors = [anchor_from_trusted_cert(client_ca).map_err(AllocCertError::Chain)?];
    let ee = EndEntityCert::try_from(cert).map_err(AllocCertError::Chain)?;
    ee.verify_for_usage(
        &[webpki::ring::ECDSA_P256_SHA256],
        &anchors,
        &[],
        now,
        KeyUsage::required(EKU_CLIENT_AUTH),
        None,
        None,
    )
    .map_err(AllocCertError::Chain)?;

    let (_, x509) = X509Certificate::from_der(cert).map_err(|_| AllocCertError::BadSan)?;
    match x509.extended_key_usage() {
        Ok(Some(eku)) if eku.value.client_auth => {}
        _ => return Err(AllocCertError::ClientAuthEku),
    }
    allocation_id_from_san(&x509)
}

/// The SAN extension must occur once and hold exactly one URI that parses.
fn allocation_id_from_san(x509: &X509Certificate<'_>) -> Result<AllocationId, AllocCertError> {
    let mut sans = x509
        .extensions()
        .iter()
        .filter(|e| e.oid == OID_X509_EXT_SUBJECT_ALT_NAME);
    let (Some(ext), None) = (sans.next(), sans.next()) else {
        return Err(AllocCertError::BadSan);
    };
    let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() else {
        return Err(AllocCertError::BadSan);
    };
    match san.general_names.as_slice() {
        [GeneralName::URI(uri)] => uri.parse().map_err(|_| AllocCertError::BadSan),
        _ => Err(AllocCertError::BadSan),
    }
}
