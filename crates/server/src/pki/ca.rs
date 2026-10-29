//! The two CAs and the CA Secret layout.
//!
//! Both CAs are long-lived dedicated roots. There is no automatic rollover:
//! rotation is an explicit emergency step (delete the Secret). A missing or
//! unusable Secret is reported through [`LoadOutcome`] so the leader can
//! back it up ([`CaBundle::backup_name`]) and replace it.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256, PublicKeyData, SerialNumber,
};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use time::{Duration, OffsetDateTime};
use x509_parser::prelude::{FromDer, X509Certificate};

use super::leaf::{IssueError, Profile};
use super::{BACKDATE, checked_add, checked_sub, random_serial, sha256, whole_seconds};

pub const CLIENT_CA_CRT: &str = "client-ca.crt";
pub const CLIENT_CA_KEY: &str = "client-ca.key";
pub const INTERNAL_CA_CRT: &str = "internal-ca.crt";
pub const INTERNAL_CA_KEY: &str = "internal-ca.key";
/// Marker key the controller writes with the CAs it generated; a Secret
/// without it (exactly [`GENERATED_BY_VALUE`]) is foreign. The dedicated-root
/// checks, not the marker, are the defence against an imported CA.
pub const GENERATED_BY: &str = "generated-by";

/// The only accepted value of [`GENERATED_BY`].
pub const GENERATED_BY_VALUE: &str = "buildkit-controller";
/// CA lifetime is 20 years (7305 days incl. leap days). A `notAfter` from 2050 on
/// is encoded as `GeneralizedTime` (RFC 5280 §4.1.2.5).
pub const CA_VALIDITY: Duration = Duration::days(7305);
/// [`CaBundle::ca_expiry_warning`] reports once less than this remains.
pub const CA_EXPIRY_WARNING: Duration = Duration::days(365);

/// Which of the two CAs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaKind {
    /// Signs allocation certs and self-issued edge/API serving certs.
    Client,
    /// Signs worker serving certs and edge-to-worker client certs.
    Internal,
}

impl CaKind {
    const ALL: [Self; 2] = [Self::Client, Self::Internal];

    fn cn(self) -> &'static str {
        match self {
            Self::Client => "buildkit-controller client CA",
            Self::Internal => "buildkit-controller internal CA",
        }
    }

    fn keys(self) -> (&'static str, &'static str) {
        match self {
            Self::Client => (CLIENT_CA_CRT, CLIENT_CA_KEY),
            Self::Internal => (INTERNAL_CA_CRT, INTERNAL_CA_KEY),
        }
    }
}

/// What a certificate is, for `buildkit_cert_not_after_seconds{kind}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CertKind {
    ClientCa,
    InternalCa,
    Allocation,
    WorkerServing,
    CacheServing,
    EdgeServing,
    EdgeClient,
}

impl CertKind {
    /// The metric label value.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ClientCa => "client_ca",
            Self::InternalCa => "internal_ca",
            Self::Allocation => "allocation",
            Self::WorkerServing => "worker_serving",
            Self::CacheServing => "cache_serving",
            Self::EdgeServing => "edge_serving",
            Self::EdgeClient => "edge_client",
        }
    }
}

impl From<Profile> for CertKind {
    fn from(p: Profile) -> Self {
        match p {
            Profile::Allocation => Self::Allocation,
            Profile::WorkerServing => Self::WorkerServing,
            Profile::CacheServing => Self::CacheServing,
            Profile::EdgeServing => Self::EdgeServing,
            Profile::EdgeClient => Self::EdgeClient,
        }
    }
}

/// Why the CA Secret is not usable. It is never used for signing or trust.
#[derive(Debug, thiserror::Error)]
pub enum CaError {
    #[error("CA secret key `{0}` is missing")]
    Missing(&'static str),
    #[error("CA secret key `{key}` is not exactly one PEM {what} section")]
    Pem {
        key: &'static str,
        what: &'static str,
    },
    #[error("CA secret key `{key}`: {reason}")]
    Cert { key: &'static str, reason: String },
    #[error("CA secret key `{0}` is not an ECDSA P-256 PKCS#8 key")]
    KeyType(&'static str),
    #[error("CA secret key `{0}` does not match its certificate")]
    KeyMismatch(&'static str),
    #[error("CA certificate `{key}` is not a usable CA: {reason}")]
    NotCa {
        key: &'static str,
        reason: &'static str,
    },
    #[error("CA certificate `{0}` has expired")]
    Validity(&'static str),
    #[error("client and internal CA must be distinct certificates with distinct keys")]
    NotDistinct,
    #[error("CA secret lacks `generated-by: buildkit-controller` (foreign)")]
    Marker,
}

impl CaError {
    /// `buildkit_ca_replacements_total{reason}` for an unusable Secret.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Marker => "foreign",
            Self::Validity(_) => "expired",
            _ => "invalid",
        }
    }
}

/// Result of reading the CA Secret. On `Missing` the leader writes
/// [`CaBundle::generate`]'s output. On `Invalid` it first copies the Secret to
/// [`CaBundle::backup_name`], then replaces it the same way, and surfaces a
/// condition, an event and a metric. Either way this is a hard rotation.
#[derive(Debug)]
pub enum LoadOutcome {
    Valid(Box<CaBundle>),
    /// The Secret does not exist.
    Missing,
    /// The Secret exists but is unparseable, mismatched, foreign or expired.
    Invalid(CaError),
}

/// One CA with its certificate and signing key.
pub struct Ca {
    cert_der: CertificateDer<'static>,
    cert_pem: String,
    not_after: OffsetDateTime,
    issuer: Issuer<'static, KeyPair>,
}

impl fmt::Debug for Ca {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ca")
            .field("not_after", &self.not_after)
            .finish_non_exhaustive()
    }
}

impl Ca {
    #[must_use]
    pub fn cert_der(&self) -> &CertificateDer<'static> {
        &self.cert_der
    }

    /// PEM re-encoded from the validated DER, never the Secret's raw bytes.
    #[must_use]
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    #[must_use]
    pub fn not_after(&self) -> OffsetDateTime {
        self.not_after
    }

    /// SHA-256 of the certificate DER (the internal CA fingerprint, `P internal-ca`).
    #[must_use]
    pub fn fingerprint_sha256(&self) -> [u8; 32] {
        sha256(&self.cert_der)
    }

    /// Lowercase hex of [`Self::fingerprint_sha256`].
    #[must_use]
    pub fn fingerprint_hex(&self) -> String {
        hex(&self.fingerprint_sha256())
    }

    pub(super) fn issuer(&self) -> &Issuer<'static, KeyPair> {
        &self.issuer
    }

    fn generate(kind: CaKind, now: OffsetDateTime) -> Result<Self, IssueError> {
        let now = whole_seconds(now);
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let mut p = CertificateParams::default();
        p.distinguished_name = rcgen::DistinguishedName::new();
        p.distinguished_name.push(DnType::CommonName, kind.cn());
        p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        p.not_before = checked_sub(now, BACKDATE)?;
        p.not_after = checked_add(now, CA_VALIDITY)?;
        p.serial_number = Some(SerialNumber::from_slice(&random_serial()?));
        let cert = p.self_signed(&key)?;
        let der = cert.der().clone();
        Ok(Self {
            cert_pem: pem_encode(&der),
            not_after: p.not_after,
            issuer: Issuer::from_ca_cert_der(&der, key)?,
            cert_der: der,
        })
    }

    fn load(
        kind: CaKind,
        crt_pem: &[u8],
        key_pem: &[u8],
        now: OffsetDateTime,
    ) -> Result<Self, CaError> {
        let (crt_name, key_name) = kind.keys();
        let (cert_der, not_after, spki) = parse_root(kind, crt_name, crt_pem)?;
        // Only expiry makes a CA unusable. A `notBefore` ahead of `now` is
        // another replica's clock skew and is tolerated.
        if not_after <= now {
            return Err(CaError::Validity(crt_name));
        }
        let pkcs8 =
            PrivatePkcs8KeyDer::from_pem_slice(single_section(key_name, key_pem, "PRIVATE KEY")?)
                .map_err(|_| CaError::Pem {
                key: key_name,
                what: "PRIVATE KEY",
            })?;
        let key = KeyPair::from_pkcs8_der_and_sign_algo(&pkcs8, &PKCS_ECDSA_P256_SHA256)
            .map_err(|_| CaError::KeyType(key_name))?;
        if spki != key.subject_public_key_info() {
            return Err(CaError::KeyMismatch(key_name));
        }
        let issuer = Issuer::from_ca_cert_der(&cert_der, key).map_err(|e| CaError::Cert {
            key: crt_name,
            reason: e.to_string(),
        })?;
        Ok(Self {
            cert_pem: pem_encode(&cert_der),
            cert_der,
            not_after,
            issuer,
        })
    }
}

/// Both CAs, as stored in the CA Secret.
#[derive(Debug)]
pub struct CaBundle {
    client: Ca,
    internal: Ca,
}

impl CaBundle {
    /// Fresh CAs, for a missing or invalid Secret.
    pub fn generate(now: OffsetDateTime) -> Result<Self, IssueError> {
        Ok(Self {
            client: Ca::generate(CaKind::Client, now)?,
            internal: Ca::generate(CaKind::Internal, now)?,
        })
    }

    /// Classifies the CA Secret's `data` (`None`: Secret absent).
    #[must_use]
    pub fn load<V: AsRef<[u8]>>(
        data: Option<&BTreeMap<String, V>>,
        now: OffsetDateTime,
    ) -> LoadOutcome {
        match data {
            None => LoadOutcome::Missing,
            Some(d) => match Self::from_secret_data(d, now) {
                Ok(b) => LoadOutcome::Valid(Box::new(b)),
                Err(e) => LoadOutcome::Invalid(e),
            },
        }
    }

    /// Parses and validates the CA Secret's `data`, including the
    /// `generated-by` marker.
    pub fn from_secret_data<V: AsRef<[u8]>>(
        data: &BTreeMap<String, V>,
        now: OffsetDateTime,
    ) -> Result<Self, CaError> {
        let get = |k: &'static str| data.get(k).map(AsRef::as_ref).ok_or(CaError::Missing(k));
        if data.get(GENERATED_BY).map(AsRef::as_ref) != Some(GENERATED_BY_VALUE.as_bytes()) {
            return Err(CaError::Marker);
        }
        let load = |kind: CaKind| {
            let (crt, key) = kind.keys();
            Ca::load(kind, get(crt)?, get(key)?, now)
        };
        let b = Self {
            client: load(CaKind::Client)?,
            internal: load(CaKind::Internal)?,
        };
        if b.client.cert_der == b.internal.cert_der
            || b.client.issuer.key().public_key_raw() == b.internal.issuer.key().public_key_raw()
        {
            return Err(CaError::NotDistinct);
        }
        Ok(b)
    }

    /// The Secret's `data` map (contains private keys, so never log it).
    #[must_use]
    pub fn to_secret_data(&self) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        for kind in CaKind::ALL {
            let (crt, key) = kind.keys();
            let ca = self.ca(kind);
            out.insert(crt.to_owned(), ca.cert_pem.clone().into_bytes());
            out.insert(key.to_owned(), ca.issuer.key().serialize_pem().into_bytes());
        }
        out.insert(
            GENERATED_BY.to_owned(),
            GENERATED_BY_VALUE.as_bytes().to_vec(),
        );
        out
    }

    #[must_use]
    pub fn ca(&self, kind: CaKind) -> &Ca {
        match kind {
            CaKind::Client => &self.client,
            CaKind::Internal => &self.internal,
        }
    }

    /// Signs allocation certs and self-issued edge/API serving certs; the only
    /// content of the `<caSecretName>-bundle` `ConfigMap`.
    #[must_use]
    pub fn client_ca(&self) -> &Ca {
        &self.client
    }

    /// Signs worker serving certs and edge-to-worker client certs; buildkitd's
    /// only trust anchor.
    #[must_use]
    pub fn internal_ca(&self) -> &Ca {
        &self.internal
    }

    /// `notAfter` of both CA certificates.
    #[must_use]
    pub fn expiries(&self) -> [(CertKind, OffsetDateTime); 2] {
        [
            (CertKind::ClientCa, self.client.not_after),
            (CertKind::InternalCa, self.internal.not_after),
        ]
    }

    /// Time left on the earlier-expiring CA once it is under
    /// [`CA_EXPIRY_WARNING`] (drives `CAExpiringSoon`); `None` before that.
    #[must_use]
    pub fn ca_expiry_warning(&self, now: OffsetDateTime) -> Option<Duration> {
        let left = self.client.not_after.min(self.internal.not_after) - now;
        (left < CA_EXPIRY_WARNING).then_some(left)
    }

    /// Name for a copy of an invalid CA Secret `secret_name`, taken before the
    /// controller replaces it. The format is `<secret_name>-replaced-<yyyymmdd>t<hhmmss>z`
    /// (UTC), with the base truncated so the result stays a valid 253-character
    /// Secret name.
    #[must_use]
    pub fn backup_name(secret_name: &str, now: OffsetDateTime) -> String {
        let t = now.to_offset(time::UtcOffset::UTC);
        let suffix = format!(
            "-replaced-{:04}{:02}{:02}t{:02}{:02}{:02}z",
            t.year(),
            u8::from(t.month()),
            t.day(),
            t.hour(),
            t.minute(),
            t.second()
        );
        let max = 253usize.saturating_sub(suffix.len());
        let base = secret_name.get(..max).unwrap_or(secret_name);
        format!("{}{suffix}", base.trim_end_matches(['-', '.']))
    }
}

pub(super) fn hex(d: &[u8]) -> String {
    d.iter()
        .fold(String::with_capacity(d.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// PEM with LF line endings, as rcgen writes it.
pub(super) fn pem_encode(der: &[u8]) -> String {
    pem::encode_config(
        &pem::Pem::new("CERTIFICATE", der.to_vec()),
        pem::EncodeConfig::new().set_line_ending(pem::LineEnding::LF),
    )
}

/// `input` must hold exactly one PEM section (of any label); anything else,
/// such as a private key next to a certificate, is refused.
fn single_section<'a>(
    key: &'static str,
    input: &'a [u8],
    what: &'static str,
) -> Result<&'a [u8], CaError> {
    let begins = input.windows(11).filter(|w| w == b"-----BEGIN ").count();
    if begins == 1 {
        Ok(input)
    } else {
        Err(CaError::Pem { key, what })
    }
}

/// Parses one self-signed root CA cert of `kind`. Returns the DER, `notAfter` and `SubjectPublicKeyInfo` DER.
fn parse_root(
    kind: CaKind,
    key: &'static str,
    pem: &[u8],
) -> Result<(CertificateDer<'static>, OffsetDateTime, Vec<u8>), CaError> {
    let err = CaError::Pem {
        key,
        what: "CERTIFICATE",
    };
    let der = CertificateDer::from_pem_slice(single_section(key, pem, "CERTIFICATE")?)
        .map_err(|_| err)?;
    let (rest, x509) = X509Certificate::from_der(&der).map_err(|e| CaError::Cert {
        key,
        reason: e.to_string(),
    })?;
    if !rest.is_empty() {
        return Err(CaError::Cert {
            key,
            reason: "trailing data".into(),
        });
    }
    let not_ca = |reason| CaError::NotCa { key, reason };
    match x509.basic_constraints() {
        Ok(Some(bc)) if bc.critical && bc.value.ca && bc.value.path_len_constraint == Some(0) => {}
        _ => {
            return Err(not_ca(
                "BasicConstraints is not critical CA:TRUE, pathlen:0",
            ));
        }
    }
    let subject = x509.subject();
    let cn_ok = subject.iter_attributes().count() == 1
        && subject
            .iter_common_name()
            .next()
            .and_then(|a| a.as_str().ok())
            == Some(kind.cn());
    if !cn_ok {
        return Err(not_ca("subject is not exactly the expected CN"));
    }
    if !self_signed(&x509) {
        return Err(not_ca("not a self-signed root"));
    }
    match x509.key_usage() {
        Ok(Some(ku)) if ku.value.key_cert_sign() => {}
        _ => return Err(not_ca("KeyUsage keyCertSign missing")),
    }
    if !has_ski(&x509) {
        return Err(not_ca("SubjectKeyIdentifier missing"));
    }
    let spki = x509.public_key().raw.to_vec();
    let not_after = x509.validity().not_after.to_datetime();
    Ok((der, not_after, spki))
}

/// Issuer equals subject and the ecdsa-with-SHA256 signature verifies under
/// the certificate's own key.
fn self_signed(x509: &X509Certificate<'_>) -> bool {
    use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
    use x509_parser::oid_registry::OID_SIG_ECDSA_WITH_SHA256;
    x509.issuer().as_raw() == x509.subject().as_raw()
        && x509.signature_algorithm.algorithm == OID_SIG_ECDSA_WITH_SHA256
        && x509.tbs_certificate.signature.algorithm == OID_SIG_ECDSA_WITH_SHA256
        && UnparsedPublicKey::new(
            &ECDSA_P256_SHA256_ASN1,
            &x509.public_key().subject_public_key.data,
        )
        .verify(x509.tbs_certificate.as_ref(), &x509.signature_value.data)
        .is_ok()
}

fn has_ski(x509: &X509Certificate<'_>) -> bool {
    x509.iter_extensions().any(|e| {
        matches!(
            e.parsed_extension(),
            x509_parser::extensions::ParsedExtension::SubjectKeyIdentifier(_)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::KeyIdMethod;
    use time::PrimitiveDateTime;

    fn now() -> OffsetDateTime {
        whole_seconds(OffsetDateTime::now_utc())
    }

    fn data() -> BTreeMap<String, Vec<u8>> {
        CaBundle::generate(now()).unwrap().to_secret_data()
    }

    fn err(d: &BTreeMap<String, Vec<u8>>) -> CaError {
        CaBundle::from_secret_data(d, now()).unwrap_err()
    }

    #[test]
    fn round_trip_and_layout() {
        let d = data();
        let b = CaBundle::from_secret_data(&d, now()).unwrap();
        assert_eq!(b.to_secret_data(), d);
        assert_eq!(
            d.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                CLIENT_CA_CRT,
                CLIENT_CA_KEY,
                GENERATED_BY,
                INTERNAL_CA_CRT,
                INTERNAL_CA_KEY
            ]
        );
        assert!(
            String::from_utf8_lossy(&d[CLIENT_CA_KEY]).starts_with("-----BEGIN PRIVATE KEY-----")
        );
        assert_eq!(d[GENERATED_BY], b"buildkit-controller");
        let kinds = b.expiries().map(|(k, _)| k.label());
        assert_eq!(kinds, ["client_ca", "internal_ca"]);
    }

    #[test]
    fn ca_profile() {
        let t = OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap(); // 2026-01-01
        let b = CaBundle::generate(t).unwrap();
        for kind in CaKind::ALL {
            let ca = b.ca(kind);
            let (_, x) = X509Certificate::from_der(ca.cert_der()).unwrap();
            let bc = x.basic_constraints().unwrap().unwrap();
            assert!(bc.critical && bc.value.ca);
            assert_eq!(bc.value.path_len_constraint, Some(0));
            let ku = x.key_usage().unwrap().unwrap();
            assert!(ku.critical && ku.value.key_cert_sign() && ku.value.crl_sign());
            assert!(!ku.value.digital_signature());
            assert!(x.extended_key_usage().unwrap().is_none());
            let ski = x
                .iter_extensions()
                .find_map(|e| match e.parsed_extension() {
                    x509_parser::extensions::ParsedExtension::SubjectKeyIdentifier(k) => {
                        Some(k.0.to_vec())
                    }
                    _ => None,
                });
            // rcgen's default is SHA-256 of the SPKI truncated to 160 bits (RFC 7093).
            assert_eq!(ski, Some(sha256(x.public_key().raw)[..20].to_vec()));
            let cn = x.subject().iter_common_name().next().unwrap();
            assert_eq!(cn.as_str().unwrap(), kind.cn());
            let v = x.validity();
            assert_eq!(v.not_before.timestamp(), (t - BACKDATE).unix_timestamp());
            assert_eq!(v.not_after.timestamp(), (t + CA_VALIDITY).unix_timestamp());
            assert_eq!(ca.not_after(), t + CA_VALIDITY);
            assert_eq!(x.raw_serial().len(), 16);
        }
    }

    #[test]
    fn ca_lifetime_is_20_years_past_2050_too() {
        let at = |y: i32| {
            time::Date::from_calendar_date(y, time::Month::January, 1)
                .unwrap()
                .midnight()
                .assume_utc()
        };
        for y in [2026, 2035, 2049] {
            let b = CaBundle::generate(at(y)).unwrap();
            assert_eq!(b.client_ca().not_after(), at(y) + CA_VALIDITY);
            let (_, x) = X509Certificate::from_der(b.client_ca().cert_der()).unwrap();
            assert_eq!(x.validity().not_after.to_datetime(), at(y) + CA_VALIDITY);
            // GeneralizedTime past 2050 round-trips through the Secret.
            let d = b.to_secret_data();
            assert!(CaBundle::from_secret_data(&d, at(y) + Duration::days(1)).is_ok());
        }
    }

    #[test]
    fn expiry_warning_within_a_year() {
        let t = now();
        let b = CaBundle::generate(t).unwrap();
        let end = b.client_ca().not_after().min(b.internal_ca().not_after());
        assert_eq!(b.ca_expiry_warning(t), None);
        let edge = end - CA_EXPIRY_WARNING;
        assert_eq!(b.ca_expiry_warning(edge), None);
        assert_eq!(
            b.ca_expiry_warning(edge + Duration::seconds(1)),
            Some(CA_EXPIRY_WARNING - Duration::seconds(1))
        );
        assert_eq!(b.ca_expiry_warning(end), Some(Duration::ZERO));
    }

    #[test]
    fn load_outcome() {
        let none: Option<&BTreeMap<String, Vec<u8>>> = None;
        assert!(matches!(CaBundle::load(none, now()), LoadOutcome::Missing));
        let d = data();
        assert!(matches!(
            CaBundle::load(Some(&d), now()),
            LoadOutcome::Valid(_)
        ));
        let empty = BTreeMap::<String, Vec<u8>>::new();
        assert!(matches!(
            CaBundle::load(Some(&empty), now()),
            LoadOutcome::Invalid(CaError::Marker)
        ));
        // Expired CAs are unusable, hence Invalid (replaced after backup).
        assert!(matches!(
            CaBundle::load(Some(&d), now() + CA_VALIDITY + Duration::days(1)),
            LoadOutcome::Invalid(CaError::Validity(_))
        ));
    }

    #[test]
    fn missing_keys() {
        for k in [
            CLIENT_CA_CRT,
            CLIENT_CA_KEY,
            INTERNAL_CA_CRT,
            INTERNAL_CA_KEY,
        ] {
            let mut d = data();
            d.remove(k);
            assert!(matches!(err(&d), CaError::Missing(m) if m == k));
            assert_eq!(err(&d).reason(), "invalid");
        }
        // Without the marker the Secret is foreign.
        let mut d = data();
        d.remove(GENERATED_BY);
        assert!(matches!(err(&d), CaError::Marker));
        assert_eq!(err(&d).reason(), "foreign");
    }

    #[test]
    fn extra_pem_sections_are_refused_and_never_republished() {
        // A private key smuggled next to a CA cert must not reach the
        // `<caSecretName>-bundle` ConfigMap, so the controller refuses the field outright.
        for kind in CaKind::ALL {
            let (crt, key) = kind.keys();
            for key_first in [false, true] {
                let mut d = data();
                let (c, k) = (d[crt].clone(), d[key].clone());
                let both = if key_first { [k, c] } else { [c, k] }.concat();
                d.insert(crt.into(), both);
                assert!(matches!(err(&d), CaError::Pem { key: x, .. } if x == crt));
            }
        }
        // The controller drops arbitrary text around a single cert, and the published PEM
        // is re-encoded from the validated DER.
        let mut d = data();
        let noisy = [
            b"note: hello\n".to_vec(),
            d[CLIENT_CA_CRT].clone(),
            b"trailer\n".to_vec(),
        ]
        .concat();
        d.insert(CLIENT_CA_CRT.into(), noisy);
        let b = CaBundle::from_secret_data(&d, now()).unwrap();
        let pem = b.client_ca().cert_pem();
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----\n"));
        assert!(!pem.contains("hello") && !pem.contains("trailer"));
        assert!(
            !b.to_secret_data()[CLIENT_CA_CRT]
                .windows(7)
                .any(|w| w == b"PRIVATE")
        );
    }

    #[test]
    fn broken_pem_and_mismatch() {
        let mut d = data();
        d.insert(CLIENT_CA_CRT.into(), b"garbage".to_vec());
        assert!(matches!(
            err(&d),
            CaError::Pem {
                key: CLIENT_CA_CRT,
                ..
            }
        ));

        let mut d = data();
        let two = [d[CLIENT_CA_CRT].clone(), d[CLIENT_CA_CRT].clone()].concat();
        d.insert(CLIENT_CA_CRT.into(), two);
        assert!(matches!(
            err(&d),
            CaError::Pem {
                key: CLIENT_CA_CRT,
                ..
            }
        ));

        let mut d = data();
        let other = data();
        d.insert(CLIENT_CA_KEY.into(), other[CLIENT_CA_KEY].clone());
        assert!(matches!(err(&d), CaError::KeyMismatch(CLIENT_CA_KEY)));

        let mut d = data();
        let p384 = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
        d.insert(CLIENT_CA_KEY.into(), p384.serialize_pem().into_bytes());
        assert!(matches!(err(&d), CaError::KeyType(CLIENT_CA_KEY)));
    }

    fn ca_params(cn: &str, path_len: Option<u8>) -> CertificateParams {
        let t = now();
        let mut p = CertificateParams::default();
        p.distinguished_name = rcgen::DistinguishedName::new();
        p.distinguished_name.push(DnType::CommonName, cn);
        p.is_ca = IsCa::Ca(path_len.map_or(
            BasicConstraints::Unconstrained,
            BasicConstraints::Constrained,
        ));
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        p.key_identifier_method = KeyIdMethod::Sha256;
        p.not_before = t - BACKDATE;
        p.not_after = t + Duration::days(1);
        p
    }

    #[test]
    fn distinctness_and_marker() {
        // Internal CA copied into the client slot fails the CN check.
        let mut d = data();
        d.insert(CLIENT_CA_CRT.into(), d[INTERNAL_CA_CRT].clone());
        d.insert(CLIENT_CA_KEY.into(), d[INTERNAL_CA_KEY].clone());
        assert!(matches!(
            err(&d),
            CaError::NotCa {
                key: CLIENT_CA_CRT,
                ..
            }
        ));

        // Two well-formed roots sharing one key (marker recomputed).
        let kp = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let client = ca_params(CaKind::Client.cn(), Some(0))
            .self_signed(&kp)
            .unwrap();
        let internal = ca_params(CaKind::Internal.cn(), Some(0))
            .self_signed(&kp)
            .unwrap();
        let key = kp.serialize_pem().into_bytes();
        let d = BTreeMap::from([
            (
                GENERATED_BY.to_owned(),
                GENERATED_BY_VALUE.as_bytes().to_vec(),
            ),
            (CLIENT_CA_CRT.to_owned(), client.pem().into_bytes()),
            (CLIENT_CA_KEY.to_owned(), key.clone()),
            (INTERNAL_CA_CRT.to_owned(), internal.pem().into_bytes()),
            (INTERNAL_CA_KEY.to_owned(), key),
        ]);
        assert!(matches!(err(&d), CaError::NotDistinct));

        // Another dedicated root of the right kind in either slot is usable, and only
        // the section 5 conditions trigger a replacement.
        for kind in CaKind::ALL {
            let (crt, key) = kind.keys();
            let mut d = data();
            let other = data();
            d.insert(crt.into(), other[crt].clone());
            d.insert(key.into(), other[key].clone());
            assert!(CaBundle::from_secret_data(&d, now()).is_ok());
        }
        // The marker value is exactly `buildkit-controller`.
        for bad in [
            &b"buildkit-controller\n"[..],
            b"Buildkit-controller",
            b"",
            b"buildkit-controller ca v3",
        ] {
            let mut d = data();
            d.insert(GENERATED_BY.into(), bad.to_vec());
            assert!(matches!(err(&d), CaError::Marker));
        }
        // The controller does not replace merely unusual Secrets, and it ignores extra keys.
        let mut d = data();
        d.insert("note".into(), b"hello".to_vec());
        d.insert("ca.crt".into(), b"anything".to_vec());
        assert!(CaBundle::from_secret_data(&d, now()).is_ok());
    }

    /// A CA cert in the client slot built with `p`, signed by `signer` (or
    /// self-signed), with the marker recomputed.
    fn foreign_client_ca(p: &CertificateParams, signer: Option<&Issuer<'_, KeyPair>>) -> CaError {
        let mut d = data();
        let kp = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let cert = match signer {
            Some(i) => p.signed_by(&kp, i).unwrap(),
            None => p.self_signed(&kp).unwrap(),
        };
        d.insert(CLIENT_CA_CRT.into(), cert.pem().into_bytes());
        d.insert(CLIENT_CA_KEY.into(), kp.serialize_pem().into_bytes());
        err(&d)
    }

    #[test]
    fn foreign_cas_rejected_despite_valid_marker() {
        let reason = |e: CaError| match e {
            CaError::NotCa {
                key: CLIENT_CA_CRT,
                reason,
            } => reason,
            other => panic!("unexpected {other:?}"),
        };
        let cn = CaKind::Client.cn();
        let forger = Issuer::new(
            ca_params(cn, Some(0)),
            KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap(),
        );
        let e = foreign_client_ca(&ca_params(cn, Some(0)), Some(&forger));
        assert_eq!(reason(e), "not a self-signed root");
        let other_root = CaBundle::generate(now()).unwrap();
        let e = foreign_client_ca(
            &ca_params(cn, Some(0)),
            Some(other_root.client_ca().issuer()),
        );
        assert_eq!(reason(e), "not a self-signed root");
        let e = foreign_client_ca(&ca_params("corp root", Some(0)), None);
        assert_eq!(reason(e), "subject is not exactly the expected CN");
        let mut p = ca_params(cn, Some(0));
        p.distinguished_name.push(DnType::OrganizationName, "corp");
        assert_eq!(
            reason(foreign_client_ca(&p, None)),
            "subject is not exactly the expected CN"
        );
        let e = foreign_client_ca(&ca_params(cn, None), None);
        assert_eq!(
            reason(e),
            "BasicConstraints is not critical CA:TRUE, pathlen:0"
        );
    }

    #[test]
    fn validity_window_and_non_ca() {
        let d = data();
        // A notBefore ahead of this replica's clock (skew) is tolerated.
        let skewed = now() - BACKDATE - Duration::hours(2);
        assert!(CaBundle::from_secret_data(&d, skewed).is_ok());
        // With notAfter at or before now, the CA is expired and unusable.
        let not_after = CaBundle::from_secret_data(&d, now())
            .unwrap()
            .client_ca()
            .not_after();
        assert!(CaBundle::from_secret_data(&d, not_after - Duration::seconds(1)).is_ok());
        assert!(matches!(
            CaBundle::from_secret_data(&d, not_after).unwrap_err(),
            CaError::Validity(_)
        ));

        // A leaf (CA:FALSE) in the client CA slot.
        let b = CaBundle::from_secret_data(&d, now()).unwrap();
        let leaf = b.issue_edge_client(now()).unwrap();
        let mut d2 = d.clone();
        d2.insert(CLIENT_CA_CRT.into(), leaf.cert_pem().as_bytes().to_vec());
        d2.insert(
            CLIENT_CA_KEY.into(),
            leaf.private_key_pem().unwrap().into_bytes(),
        );
        assert!(matches!(
            err(&d2),
            CaError::NotCa {
                key: CLIENT_CA_CRT,
                ..
            }
        ));
    }

    #[test]
    fn time_overflow_is_an_error_not_a_panic() {
        assert!(matches!(
            CaBundle::generate(PrimitiveDateTime::MAX.assume_utc()),
            Err(IssueError::TimeOverflow)
        ));
        assert!(matches!(
            CaBundle::generate(PrimitiveDateTime::MIN.assume_utc()),
            Err(IssueError::TimeOverflow)
        ));
    }

    #[test]
    fn backup_name_is_a_valid_secret_name() {
        let t = OffsetDateTime::from_unix_timestamp(1_792_481_696).unwrap(); // 2026-10-20 07:34:56Z
        assert_eq!(
            CaBundle::backup_name("buildkit-controller-ca", t),
            "buildkit-controller-ca-replaced-20261020t073456z"
        );
        let n = CaBundle::backup_name(&format!("{}-x", "a".repeat(250)), t);
        assert_eq!(n.len(), 253);
        assert!(n.ends_with("-replaced-20261020t073456z"));
        let n = CaBundle::backup_name(&format!("{}-{}", "b".repeat(226), "c".repeat(5)), t);
        assert!(!n.contains("--"), "{n}");
        assert!(
            n.bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.')
        );
    }
}
