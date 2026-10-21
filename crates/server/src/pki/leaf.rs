//! Leaf certificate profiles.

use std::fmt;
use std::net::IpAddr;

use rcgen::{
    CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256, PublicKeyData, SanType, SerialNumber, SubjectPublicKeyInfo,
};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{
    CertificateDer, DnsName, PrivateKeyDer, PrivatePkcs8KeyDer, SubjectPublicKeyInfoDer,
};
use time::{Duration, OffsetDateTime};

use super::ca::{Ca, CaBundle, CertKind};
use super::{
    AllocationId, BACKDATE, Namespace, WorkerIdentity, checked_add, checked_sub, random_serial,
    renew_at, whole_seconds,
};

const SERVING_VALIDITY: Duration = Duration::days(30);
const EDGE_CLIENT_VALIDITY: Duration = Duration::hours(24);

/// Validity of the renewable leaves. [`Default`] is the
/// production profile; anything else comes only from the test-only
/// `pki.testLeafLifetimes` config knob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeafValidity {
    /// Worker serving cert (30 days).
    pub worker_serving: Duration,
    /// Self-issued edge/API serving cert (30 days).
    pub edge_serving: Duration,
    /// Edge-to-worker client cert (24 hours).
    pub edge_client: Duration,
}

impl Default for LeafValidity {
    fn default() -> Self {
        Self {
            worker_serving: SERVING_VALIDITY,
            edge_serving: SERVING_VALIDITY,
            edge_client: EDGE_CLIENT_VALIDITY,
        }
    }
}

/// Why a certificate could not be issued.
#[derive(Debug, thiserror::Error)]
pub enum IssueError {
    #[error("certificate generation failed: {0}")]
    Rcgen(#[from] rcgen::Error),
    #[error("OS random number generator failed")]
    Rng,
    #[error("public key rejected: {0}")]
    PublicKey(&'static str),
    #[error("expires-at must be a whole second after now")]
    Expiry,
    #[error("invalid name: {0}")]
    Name(String),
    #[error("time arithmetic out of range")]
    TimeOverflow,
    #[error("allocation would outlive the client CA (see ca_expiry_warning)")]
    OutlivesCa,
    #[error("the issuing CA has expired")]
    CaExpired,
}

impl From<ring::error::Unspecified> for IssueError {
    fn from(_: ring::error::Unspecified) -> Self {
        Self::Rng
    }
}

/// Which profile a leaf was issued under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Allocation certificate: client CA, clientAuth, never renewed.
    Allocation,
    /// Worker serving certificate: internal CA, serverAuth.
    WorkerServing,
    /// Edge and API serving certificate: client CA, serverAuth (self-issued default).
    EdgeServing,
    /// Edge client certificate: internal CA, clientAuth.
    EdgeClient,
}

/// A caller-supplied public key, ECDSA P-256 `SubjectPublicKeyInfo` only.
pub struct ClientPublicKey(SubjectPublicKeyInfo);

impl fmt::Debug for ClientPublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClientPublicKey(P-256)")
    }
}

impl ClientPublicKey {
    /// From a single `-----BEGIN PUBLIC KEY-----` PEM block (the API's `publicKey`).
    pub fn from_pem(pem: &str) -> Result<Self, IssueError> {
        let mut it = SubjectPublicKeyInfoDer::pem_slice_iter(pem.as_bytes());
        match (it.next(), it.next()) {
            (Some(Ok(der)), None) => Self::from_der(&der),
            _ => Err(IssueError::PublicKey("not a single PEM PUBLIC KEY block")),
        }
    }

    /// From `SubjectPublicKeyInfo` DER.
    pub fn from_der(der: &[u8]) -> Result<Self, IssueError> {
        let spki = SubjectPublicKeyInfo::from_der(der)
            .map_err(|_| IssueError::PublicKey("not a supported SubjectPublicKeyInfo"))?;
        if spki.algorithm() != &PKCS_ECDSA_P256_SHA256 {
            return Err(IssueError::PublicKey("not ECDSA P-256"));
        }
        // Uncompressed point layout is 0x04 || X(32) || Y(32).
        if spki.der_bytes().len() != 65 || spki.der_bytes().first() != Some(&4) {
            return Err(IssueError::PublicKey("not an uncompressed P-256 point"));
        }
        on_curve(spki.der_bytes())?;
        Ok(Self(spki))
    }
}

/// Rejects points that are not on P-256 (including the all-zero encoding).
/// ring validates an ECDH peer key fully (SP 800-56A partial public-key
/// validation, namely coordinates in range, point on the curve, not infinity) before
/// agreeing, so an ephemeral agreement against `point` is exactly that check.
fn on_curve(point: &[u8]) -> Result<(), IssueError> {
    use ring::agreement::{ECDH_P256, EphemeralPrivateKey, UnparsedPublicKey, agree_ephemeral};
    let ours = EphemeralPrivateKey::generate(&ECDH_P256, &ring::rand::SystemRandom::new())?;
    agree_ephemeral(ours, &UnparsedPublicKey::new(&ECDH_P256, point), |_| ())
        .map_err(|_| IssueError::PublicKey("not a point on P-256"))
}

/// Where the allocation cert's key comes from.
#[derive(Debug)]
pub enum LeafKey {
    /// Generate a P-256 key; it is returned once in [`IssuedCert`].
    Generate,
    /// Certify the caller's public key; no private key is returned.
    Client(ClientPublicKey),
}

/// The edge/API serving cert names, validated.
#[derive(Debug, Clone)]
pub struct EdgeNames {
    server_name: String,
    sans: Vec<SanName>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SanName {
    Dns(String),
    Ip(IpAddr),
}

impl EdgeNames {
    /// `server_name` (`edge.serverName`) must be a DNS name; each `extra_names`
    /// entry is an IP if it parses as one, else a DNS name; `service_name` is
    /// a DNS label; SANs are deduplicated in order.
    pub fn new(
        server_name: &str,
        extra_names: &[String],
        service_name: &str,
        namespace: &Namespace,
    ) -> Result<Self, IssueError> {
        let dns = |s: &str| -> Result<SanName, IssueError> {
            if s.parse::<IpAddr>().is_ok() || DnsName::try_from(s).is_err() {
                return Err(IssueError::Name(s.to_owned()));
            }
            Ok(SanName::Dns(s.to_owned()))
        };
        service_name
            .parse::<Namespace>()
            .map_err(|_| IssueError::Name(service_name.to_owned()))?;
        let mut sans = vec![dns(server_name)?];
        for n in extra_names {
            sans.push(match n.parse::<IpAddr>() {
                Ok(ip) => SanName::Ip(ip),
                Err(_) => dns(n)?,
            });
        }
        sans.push(dns(&format!("{service_name}.{namespace}.svc"))?);
        sans.push(dns(&format!(
            "{service_name}.{namespace}.svc.cluster.local"
        ))?);
        let mut uniq: Vec<SanName> = Vec::with_capacity(sans.len());
        for s in sans {
            if !uniq.contains(&s) {
                uniq.push(s);
            }
        }
        Ok(Self {
            server_name: server_name.to_owned(),
            sans: uniq,
        })
    }
}

/// An issued leaf certificate and, when generated here, its private key.
pub struct IssuedCert {
    profile: Profile,
    cert_der: CertificateDer<'static>,
    cert_pem: String,
    key: Option<KeyPair>,
    not_before: OffsetDateTime,
    not_after: OffsetDateTime,
}

impl fmt::Debug for IssuedCert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedCert")
            .field("profile", &self.profile)
            .field("not_before", &self.not_before)
            .field("not_after", &self.not_after)
            .finish_non_exhaustive()
    }
}

impl IssuedCert {
    #[must_use]
    pub fn profile(&self) -> Profile {
        self.profile
    }
    #[must_use]
    pub fn cert_der(&self) -> &CertificateDer<'static> {
        &self.cert_der
    }
    #[must_use]
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }
    /// PKCS#8 PEM; `None` when the caller supplied the public key.
    #[must_use]
    pub fn private_key_pem(&self) -> Option<String> {
        self.key.as_ref().map(KeyPair::serialize_pem)
    }
    /// PKCS#8 DER for rustls; `None` when the caller supplied the public key.
    #[must_use]
    pub fn private_key_der(&self) -> Option<PrivateKeyDer<'static>> {
        self.key
            .as_ref()
            .map(|k| PrivatePkcs8KeyDer::from(k.serialize_der()).into())
    }
    #[must_use]
    pub fn not_before(&self) -> OffsetDateTime {
        self.not_before
    }
    #[must_use]
    pub fn not_after(&self) -> OffsetDateTime {
        self.not_after
    }

    /// When to reissue (two thirds of the lifetime); `None` for allocation
    /// certs, which are never renewed.
    #[must_use]
    pub fn renew_at(&self) -> Option<OffsetDateTime> {
        match self.profile {
            Profile::Allocation => None,
            _ => Some(renew_at(self.not_before, self.not_after)),
        }
    }

    /// For `buildkit_cert_not_after_seconds{kind}`.
    #[must_use]
    pub fn expiry(&self) -> (CertKind, OffsetDateTime) {
        (self.profile.into(), self.not_after)
    }

    /// True once two thirds of the lifetime has elapsed.
    #[must_use]
    pub fn needs_renewal(&self, now: OffsetDateTime) -> bool {
        self.renew_at().is_some_and(|at| now >= at)
    }
}

struct Spec<'a> {
    profile: Profile,
    cn: &'a str,
    sans: Vec<SanType>,
    eku: ExtendedKeyUsagePurpose,
    not_before: OffsetDateTime,
    not_after: OffsetDateTime,
}

fn issue(ca: &Ca, spec: Spec<'_>, key: LeafKey) -> Result<IssuedCert, IssueError> {
    // Renewable leaves end no later than their CA (and are renewed up to its
    // end); an allocation cert must end exactly at `expires-at`.
    let not_after = match spec.profile {
        Profile::Allocation if spec.not_after > ca.not_after() => {
            return Err(IssueError::OutlivesCa);
        }
        Profile::Allocation => spec.not_after,
        _ => spec.not_after.min(ca.not_after()),
    };
    if not_after <= spec.not_before.saturating_add(BACKDATE) {
        return Err(IssueError::CaExpired);
    }
    let mut p = CertificateParams::default();
    p.distinguished_name = rcgen::DistinguishedName::new();
    p.distinguished_name.push(DnType::CommonName, spec.cn);
    p.subject_alt_names = spec.sans;
    p.is_ca = IsCa::ExplicitNoCa;
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    p.extended_key_usages = vec![spec.eku];
    p.use_authority_key_identifier_extension = true;
    p.not_before = spec.not_before;
    p.not_after = not_after;
    p.serial_number = Some(SerialNumber::from_slice(&random_serial()?));
    let (cert, key) = match key {
        LeafKey::Generate => {
            let kp = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
            (p.signed_by(&kp, ca.issuer())?, Some(kp))
        }
        LeafKey::Client(pk) => (p.signed_by(&pk.0, ca.issuer())?, None),
    };
    Ok(IssuedCert {
        profile: spec.profile,
        cert_pem: cert.pem(),
        cert_der: cert.der().clone(),
        key,
        not_before: spec.not_before,
        not_after,
    })
}

fn dns(s: String) -> Result<SanType, IssueError> {
    Ok(SanType::DnsName(s.try_into()?))
}

impl CaBundle {
    /// Allocation certificate. `expires_at` is the allocation's `expires-at` (whole
    /// seconds, after `now`) and becomes `notAfter` exactly.
    pub fn issue_allocation(
        &self,
        id: &AllocationId,
        key: LeafKey,
        now: OffsetDateTime,
        expires_at: OffsetDateTime,
    ) -> Result<IssuedCert, IssueError> {
        let now = whole_seconds(now);
        if expires_at.nanosecond() != 0 || expires_at <= now {
            return Err(IssueError::Expiry);
        }
        let spec = Spec {
            profile: Profile::Allocation,
            cn: id.alloc_id.as_str(),
            sans: vec![SanType::URI(id.to_uri().try_into()?)],
            eku: ExtendedKeyUsagePurpose::ClientAuth,
            not_before: checked_sub(now, BACKDATE)?,
            not_after: expires_at,
        };
        issue(self.client_ca(), spec, key)
    }

    /// Worker serving certificate with DNS [`WorkerIdentity::dns_name`], 30 days.
    pub fn issue_worker_serving(
        &self,
        id: &WorkerIdentity,
        now: OffsetDateTime,
    ) -> Result<IssuedCert, IssueError> {
        self.issue_worker_serving_for(id, now, SERVING_VALIDITY)
    }

    /// [`Self::issue_worker_serving`] valid for `validity` ([`LeafValidity`]).
    pub fn issue_worker_serving_for(
        &self,
        id: &WorkerIdentity,
        now: OffsetDateTime,
        validity: Duration,
    ) -> Result<IssuedCert, IssueError> {
        let now = whole_seconds(now);
        let spec = Spec {
            profile: Profile::WorkerServing,
            cn: "buildkitd",
            sans: vec![dns(id.dns_name())?],
            eku: ExtendedKeyUsagePurpose::ServerAuth,
            not_before: checked_sub(now, BACKDATE)?,
            not_after: checked_add(now, validity)?,
        };
        issue(self.internal_ca(), spec, LeafKey::Generate)
    }

    /// Edge and API serving certificate, self-issued default, 30 days, in memory only.
    pub fn issue_edge_serving(
        &self,
        names: &EdgeNames,
        now: OffsetDateTime,
    ) -> Result<IssuedCert, IssueError> {
        self.issue_edge_serving_for(names, now, SERVING_VALIDITY)
    }

    /// [`Self::issue_edge_serving`] valid for `validity` ([`LeafValidity`]).
    pub fn issue_edge_serving_for(
        &self,
        names: &EdgeNames,
        now: OffsetDateTime,
        validity: Duration,
    ) -> Result<IssuedCert, IssueError> {
        let now = whole_seconds(now);
        let sans = names
            .sans
            .iter()
            .map(|s| match s {
                SanName::Dns(d) => dns(d.clone()),
                SanName::Ip(ip) => Ok(SanType::IpAddress(*ip)),
            })
            .collect::<Result<_, _>>()?;
        let spec = Spec {
            profile: Profile::EdgeServing,
            cn: &names.server_name,
            sans,
            eku: ExtendedKeyUsagePurpose::ServerAuth,
            not_before: checked_sub(now, BACKDATE)?,
            not_after: checked_add(now, validity)?,
        };
        issue(self.client_ca(), spec, LeafKey::Generate)
    }

    /// Edge client certificate with CN and DNS `buildkit-edge`, 24 hours, in memory only.
    pub fn issue_edge_client(&self, now: OffsetDateTime) -> Result<IssuedCert, IssueError> {
        self.issue_edge_client_for(now, EDGE_CLIENT_VALIDITY)
    }

    /// [`Self::issue_edge_client`] valid for `validity` ([`LeafValidity`]).
    pub fn issue_edge_client_for(
        &self,
        now: OffsetDateTime,
        validity: Duration,
    ) -> Result<IssuedCert, IssueError> {
        let now = whole_seconds(now);
        let spec = Spec {
            profile: Profile::EdgeClient,
            cn: "buildkit-edge",
            sans: vec![dns("buildkit-edge".to_owned())?],
            eku: ExtendedKeyUsagePurpose::ClientAuth,
            not_before: checked_sub(now, BACKDATE)?,
            not_after: checked_add(now, validity)?,
        };
        issue(self.internal_ca(), spec, LeafKey::Generate)
    }
}
