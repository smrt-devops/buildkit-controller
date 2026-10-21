//! Profile, chain, verification and renewal tests across the pki module.

use proptest::prelude::*;
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
    PublicKeyData, SanType,
};
use rustls_pki_types::{CertificateDer, DnsName, ServerName, UnixTime};
use time::{Duration, OffsetDateTime};
use webpki::{EndEntityCert, KeyUsage, anchor_from_trusted_cert};
use x509_parser::extensions::{GeneralName, ParsedExtension};
use x509_parser::prelude::{FromDer, X509Certificate};

use super::alloc_id::tests::{arb_allocation_id, sample};
use super::*;

fn now() -> OffsetDateTime {
    whole_seconds(OffsetDateTime::now_utc())
}

fn unix(t: OffsetDateTime) -> UnixTime {
    UnixTime::since_unix_epoch(std::time::Duration::from_secs(
        u64::try_from(t.unix_timestamp()).unwrap(),
    ))
}

fn edge_names() -> EdgeNames {
    EdgeNames::new(
        "buildkit.example.com",
        &["10.0.0.7".to_owned(), "alt.example.com".to_owned()],
        "buildkit-controller",
        &"bk-system".parse().unwrap(),
    )
    .unwrap()
}

fn chain_ok(
    leaf: &CertificateDer<'_>,
    ca: &CertificateDer<'_>,
    usage: KeyUsage,
    at: OffsetDateTime,
) -> bool {
    let anchor = anchor_from_trusted_cert(ca).unwrap();
    EndEntityCert::try_from(leaf)
        .unwrap()
        .verify_for_usage(
            &[webpki::ring::ECDSA_P256_SHA256],
            &[anchor],
            &[],
            unix(at),
            usage,
            None,
            None,
        )
        .is_ok()
}

fn ski(x: &X509Certificate<'_>) -> Vec<u8> {
    x.iter_extensions()
        .find_map(|e| match e.parsed_extension() {
            ParsedExtension::SubjectKeyIdentifier(k) => Some(k.0.to_vec()),
            _ => None,
        })
        .unwrap()
}

/// Asserts the common leaf rules plus the profile's CN, SANs, EKU and life.
fn assert_leaf(
    cert: &IssuedCert,
    ca: &Ca,
    cn: &str,
    sans: &[GeneralName<'_>],
    client: bool,
    life: Duration,
    t: OffsetDateTime,
) {
    let (_, x) = X509Certificate::from_der(cert.cert_der()).unwrap();
    let (_, cax) = X509Certificate::from_der(ca.cert_der()).unwrap();
    let bc = x.basic_constraints().unwrap().unwrap();
    assert!(bc.critical && !bc.value.ca);
    let ku = x.key_usage().unwrap().unwrap();
    assert!(ku.critical);
    assert_eq!(ku.value.flags, 1, "digitalSignature only");
    let eku = x.extended_key_usage().unwrap().unwrap().value;
    assert_eq!((eku.client_auth, eku.server_auth), (client, !client));
    assert!(eku.other.is_empty() && !eku.any && !eku.code_signing && !eku.email_protection);
    let aki = x
        .iter_extensions()
        .find_map(|e| match e.parsed_extension() {
            ParsedExtension::AuthorityKeyIdentifier(a) => {
                a.key_identifier.as_ref().map(|k| k.0.to_vec())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(aki, ski(&cax));
    assert_eq!(x.issuer(), cax.subject());
    assert_eq!(
        x.subject()
            .iter_common_name()
            .next()
            .unwrap()
            .as_str()
            .unwrap(),
        cn
    );
    assert_eq!(
        x.subject_alternative_name()
            .unwrap()
            .unwrap()
            .value
            .general_names,
        sans
    );
    assert_eq!(
        x.validity().not_before.timestamp(),
        (t - BACKDATE).unix_timestamp()
    );
    assert_eq!(
        x.validity().not_after.timestamp(),
        (t + life).unix_timestamp()
    );
    assert_eq!(x.raw_serial().len(), 16);
    assert_eq!(x.raw_serial()[0] & 0xc0, 0x40);
    assert_eq!(
        x.public_key()
            .algorithm
            .parameters
            .as_ref()
            .map(|p| p.as_oid().unwrap().to_id_string()),
        Some("1.2.840.10045.3.1.7".to_owned())
    );
    assert_eq!(
        cert.not_before().unix_timestamp(),
        (t - BACKDATE).unix_timestamp()
    );
}

#[test]
fn allocation_profile_and_chain() {
    let t = now();
    let b = CaBundle::generate(t).unwrap();
    let id = sample();
    let exp = t + Duration::hours(1);
    let c = b.issue_allocation(&id, LeafKey::Generate, t, exp).unwrap();
    let uri = id.to_uri();
    assert_leaf(
        &c,
        b.client_ca(),
        id.alloc_id.as_str(),
        &[GeneralName::URI(&uri)],
        true,
        Duration::hours(1),
        t,
    );
    assert_eq!(c.not_after(), exp);
    assert!(
        c.private_key_pem()
            .unwrap()
            .starts_with("-----BEGIN PRIVATE KEY-----")
    );
    assert!(chain_ok(
        c.cert_der(),
        b.client_ca().cert_der(),
        KeyUsage::client_auth(),
        t
    ));
    assert!(!chain_ok(
        c.cert_der(),
        b.client_ca().cert_der(),
        KeyUsage::server_auth(),
        t
    ));
    assert!(!chain_ok(
        c.cert_der(),
        b.internal_ca().cert_der(),
        KeyUsage::client_auth(),
        t
    ));
    assert_eq!(c.renew_at(), None);
    assert!(!c.needs_renewal(exp + Duration::days(1)));
    assert_eq!(
        verify_allocation_cert(c.cert_der(), b.client_ca().cert_der(), unix(t)).unwrap(),
        id
    );
}

#[test]
fn allocation_expiry_rules() {
    let t = now();
    let b = CaBundle::generate(t).unwrap();
    let id = sample();
    for exp in [
        t,
        t - Duration::seconds(1),
        t + Duration::milliseconds(1500),
    ] {
        assert!(matches!(
            b.issue_allocation(&id, LeafKey::Generate, t, exp),
            Err(IssueError::Expiry)
        ));
    }
}

#[test]
fn allocation_with_client_public_key() {
    let t = now();
    let b = CaBundle::generate(t).unwrap();
    let kp = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let pk = ClientPublicKey::from_pem(&kp.public_key_pem()).unwrap();
    let c = b
        .issue_allocation(&sample(), LeafKey::Client(pk), t, t + Duration::hours(1))
        .unwrap();
    assert!(c.private_key_pem().is_none() && c.private_key_der().is_none());
    let (_, x) = X509Certificate::from_der(c.cert_der()).unwrap();
    assert_eq!(x.public_key().raw, kp.subject_public_key_info().as_slice());
    assert!(ClientPublicKey::from_der(&kp.subject_public_key_info()).is_ok());

    let p384 = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let ed = KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
    for bad in [
        p384.public_key_pem(),
        ed.public_key_pem(),
        "junk".to_owned(),
        format!("{0}{0}", kp.public_key_pem()),
        kp.serialize_pem(),
    ] {
        assert!(
            matches!(
                ClientPublicKey::from_pem(&bad),
                Err(IssueError::PublicKey(_))
            ),
            "{bad}"
        );
    }
}

#[test]
fn worker_serving_profile_and_renewal() {
    let t = now();
    let b = CaBundle::generate(t).unwrap();
    let (ns, pool): (Namespace, PoolName) = ("ci".parse().unwrap(), "default".parse().unwrap());
    let id = WorkerIdentity {
        namespace: ns,
        pool,
        pod: "default-3".into(),
        nonce: "0123456789abcdef".into(),
    };
    let c = b.issue_worker_serving(&id, t).unwrap();
    let name = "0123456789abcdef.default-3.default.ci.worker.buildkit.internal";
    assert_leaf(
        &c,
        b.internal_ca(),
        "buildkitd",
        &[GeneralName::DNSName(name)],
        false,
        Duration::days(30),
        t,
    );
    assert!(chain_ok(
        c.cert_der(),
        b.internal_ca().cert_der(),
        KeyUsage::server_auth(),
        t
    ));
    assert!(!chain_ok(
        c.cert_der(),
        b.internal_ca().cert_der(),
        KeyUsage::client_auth(),
        t
    ));
    assert!(!chain_ok(
        c.cert_der(),
        b.client_ca().cert_der(),
        KeyUsage::server_auth(),
        t
    ));
    let ee = EndEntityCert::try_from(c.cert_der()).unwrap();
    let sn = |s: &str| ServerName::DnsName(DnsName::try_from(s.to_owned()).unwrap());
    assert!(ee.verify_is_valid_for_subject_name(&sn(name)).is_ok());
    assert!(
        ee.verify_is_valid_for_subject_name(&sn("other.ci.worker.buildkit.internal"))
            .is_err()
    );

    // Renew at 2/3 of (30 d + backdate); at that point at least 10 days remain.
    let at = c.renew_at().unwrap();
    assert_eq!(
        at - c.not_before(),
        (c.not_after() - c.not_before()) * 2 / 3
    );
    assert!(c.not_after() - at >= Duration::days(10));
    assert!(!c.needs_renewal(at - Duration::seconds(1)));
    assert!(c.needs_renewal(at));
    assert!(c.needs_renewal(c.not_after() + Duration::days(1)));
}

#[test]
fn edge_serving_profile() {
    let t = now();
    let b = CaBundle::generate(t).unwrap();
    let c = b.issue_edge_serving(&edge_names(), t).unwrap();
    let sans = [
        GeneralName::DNSName("buildkit.example.com"),
        GeneralName::IPAddress(&[10, 0, 0, 7]),
        GeneralName::DNSName("alt.example.com"),
        GeneralName::DNSName("buildkit-controller.bk-system.svc"),
        GeneralName::DNSName("buildkit-controller.bk-system.svc.cluster.local"),
    ];
    assert_leaf(
        &c,
        b.client_ca(),
        "buildkit.example.com",
        &sans,
        false,
        Duration::days(30),
        t,
    );
    assert!(chain_ok(
        c.cert_der(),
        b.client_ca().cert_der(),
        KeyUsage::server_auth(),
        t
    ));
    assert!(!chain_ok(
        c.cert_der(),
        b.client_ca().cert_der(),
        KeyUsage::client_auth(),
        t
    ));
    // notBefore = t - 300 s; 2/3 of (30 d + 300 s) = 20 d + 200 s.
    assert!(!c.needs_renewal(t + Duration::days(20) - Duration::seconds(101)));
    assert!(c.needs_renewal(t + Duration::days(20) - Duration::seconds(100)));

    let ns: Namespace = "bk-system".parse().unwrap();
    for (server, extra, svc) in [
        ("10.0.0.1", "", "svc"),
        ("bad name", "", "svc"),
        ("ok.example", "bad name", "svc"),
        ("ok.example", "", "Svc"),
    ] {
        let extra: Vec<String> = [extra]
            .iter()
            .filter(|e| !e.is_empty())
            .map(|e| (*e).to_owned())
            .collect();
        assert!(matches!(
            EdgeNames::new(server, &extra, svc, &ns),
            Err(IssueError::Name(_))
        ));
    }
}

#[test]
fn edge_client_profile_and_renewal() {
    let t = now();
    let b = CaBundle::generate(t).unwrap();
    let c = b.issue_edge_client(t).unwrap();
    assert_leaf(
        &c,
        b.internal_ca(),
        "buildkit-edge",
        &[GeneralName::DNSName("buildkit-edge")],
        true,
        Duration::hours(24),
        t,
    );
    assert!(chain_ok(
        c.cert_der(),
        b.internal_ca().cert_der(),
        KeyUsage::client_auth(),
        t
    ));
    assert!(!chain_ok(
        c.cert_der(),
        b.internal_ca().cert_der(),
        KeyUsage::server_auth(),
        t
    ));
    // 2/3 of (24 h + backdate) after notBefore = now - backdate.
    let at = t - BACKDATE + Duration::seconds((86_400 + BACKDATE.whole_seconds()) * 2 / 3);
    assert_eq!(c.renew_at(), Some(at));
    assert!(!c.needs_renewal(at - Duration::seconds(1)));
    assert!(c.needs_renewal(at));
}

/// A client-CA leaf with arbitrary EKUs and SANs, for negative tests.
fn custom_leaf(
    b: &CaBundle,
    eku: Vec<ExtendedKeyUsagePurpose>,
    sans: Vec<SanType>,
    t: OffsetDateTime,
) -> CertificateDer<'static> {
    let kp = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut p = CertificateParams::default();
    p.extended_key_usages = eku;
    p.subject_alt_names = sans;
    p.not_before = t - Duration::seconds(60);
    p.not_after = t + Duration::hours(1);
    p.signed_by(&kp, b.client_ca().issuer())
        .unwrap()
        .der()
        .clone()
}

#[test]
fn verify_allocation_rejections() {
    let t = now();
    let b = CaBundle::generate(t).unwrap();
    let ca = b.client_ca().cert_der();
    let uri = || SanType::URI(sample().to_uri().try_into().unwrap());
    let client = || vec![ExtendedKeyUsagePurpose::ClientAuth];

    // A cert with no EKU at all is refused, though webpki's client_auth() would accept it.
    let no_eku = custom_leaf(&b, vec![], vec![uri()], t);
    assert!(chain_ok(&no_eku, ca, KeyUsage::client_auth(), t));
    assert!(verify_allocation_cert(&no_eku, ca, unix(t)).is_err());

    let server = custom_leaf(
        &b,
        vec![ExtendedKeyUsagePurpose::ServerAuth],
        vec![uri()],
        t,
    );
    assert!(matches!(
        verify_allocation_cert(&server, ca, unix(t)),
        Err(AllocCertError::Chain(_))
    ));

    for sans in [
        vec![SanType::DnsName("ci".try_into().unwrap())],
        vec![uri(), uri()],
        vec![uri(), SanType::DnsName("x.example".try_into().unwrap())],
        vec![SanType::URI(
            "buildkit://ci/default/x/y".try_into().unwrap(),
        )],
        vec![],
    ] {
        let c = custom_leaf(&b, client(), sans, t);
        assert!(matches!(
            verify_allocation_cert(&c, ca, unix(t)),
            Err(AllocCertError::BadSan)
        ));
    }

    // Wrong CA, expired, not yet valid.
    let c = b
        .issue_allocation(&sample(), LeafKey::Generate, t, t + Duration::hours(1))
        .unwrap();
    let wrong = |ca: &CertificateDer<'_>, at| {
        matches!(
            verify_allocation_cert(c.cert_der(), ca, unix(at)),
            Err(AllocCertError::Chain(_))
        )
    };
    assert!(wrong(b.internal_ca().cert_der(), t));
    assert!(wrong(
        CaBundle::generate(t).unwrap().client_ca().cert_der(),
        t
    ));
    assert!(wrong(ca, t + Duration::hours(1) + Duration::seconds(1)));
    assert!(wrong(ca, t - BACKDATE - Duration::seconds(1)));
}

#[test]
fn user_serving_cert() {
    let t = now();
    let b = CaBundle::generate(t).unwrap();
    let c = b.issue_edge_serving(&edge_names(), t).unwrap();
    let chain = format!("{}{}", c.cert_pem(), b.client_ca().cert_pem());
    let key = c.private_key_pem().unwrap();
    let ok = UserServingCert::from_tls_secret(
        chain.as_bytes(),
        key.as_bytes(),
        "buildkit.example.com",
        t,
    )
    .unwrap();
    assert_eq!(ok.certified_key().cert.len(), 2);
    assert_eq!(ok.not_after(), c.not_after());

    let load = |crt: &str, key: &str, name: &str, at| {
        UserServingCert::from_tls_secret(crt.as_bytes(), key.as_bytes(), name, at).unwrap_err()
    };
    assert!(matches!(
        load(&chain, &key, "other.example.com", t),
        ServingCertError::Name
    ));
    assert!(matches!(
        load(&chain, &key, "buildkit.example.com", t + Duration::days(31)),
        ServingCertError::Validity
    ));
    assert!(matches!(
        load("", &key, "buildkit.example.com", t),
        ServingCertError::NoCertificate
    ));
    assert!(matches!(
        load(&chain, "junk", "buildkit.example.com", t),
        ServingCertError::Key
    ));
    let other = b
        .issue_edge_serving(&edge_names(), t)
        .unwrap()
        .private_key_pem()
        .unwrap();
    assert!(matches!(
        load(&chain, &other, "buildkit.example.com", t),
        ServingCertError::KeyMismatch
    ));
    // A CA cert, and a leaf whose KeyUsage lacks digitalSignature, are not
    // serving certs even with serverAuth and a matching SAN.
    for (ca, ku) in [
        (
            IsCa::Ca(BasicConstraints::Constrained(0)),
            vec![KeyUsagePurpose::KeyCertSign],
        ),
        (IsCa::ExplicitNoCa, vec![KeyUsagePurpose::KeyEncipherment]),
    ] {
        let kp = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut p = CertificateParams::new(vec!["buildkit.example.com".to_owned()]).unwrap();
        p.is_ca = ca;
        p.key_usages = ku;
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        p.not_before = t - Duration::seconds(60);
        p.not_after = t + Duration::hours(1);
        let crt = p.signed_by(&kp, b.client_ca().issuer()).unwrap().pem();
        assert!(matches!(
            load(&crt, &kp.serialize_pem(), "buildkit.example.com", t),
            ServingCertError::NotLeaf
        ));
    }

    let cl = b.issue_edge_client(t).unwrap();
    assert!(matches!(
        load(
            cl.cert_pem(),
            &cl.private_key_pem().unwrap(),
            "buildkit-edge",
            t
        ),
        ServingCertError::Eku
    ));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    #[test]
    fn san_round_trip(id in arb_allocation_id()) {
        let t = now();
        let b = CaBundle::generate(t).unwrap();
        let c = b.issue_allocation(&id, LeafKey::Generate, t, t + Duration::minutes(5)).unwrap();
        prop_assert_eq!(verify_allocation_cert(c.cert_der(), b.client_ca().cert_der(), unix(t)).unwrap(), id);
    }
}

/// SPKI DER for a raw 65-byte P-256 point.
fn p256_spki(point: &[u8]) -> Vec<u8> {
    const PREFIX: [u8; 26] = [
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
        0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
    ];
    [&PREFIX[..], point].concat()
}

#[test]
fn client_public_key_must_be_on_curve() {
    let kp = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let good = kp.public_key_raw().to_vec();
    assert_eq!(p256_spki(&good), kp.subject_public_key_info());
    assert!(ClientPublicKey::from_der(&p256_spki(&good)).is_ok());

    let mut zero = vec![0u8; 65];
    zero[0] = 4;
    let mut off = good.clone();
    off[64] ^= 1; // y flipped, so the point is off the curve
    let mut big = vec![0xffu8; 65]; // coordinates >= p
    big[0] = 4;
    for bad in [zero, off, big] {
        assert!(matches!(
            ClientPublicKey::from_der(&p256_spki(&bad)),
            Err(IssueError::PublicKey("not a point on P-256"))
        ));
    }
}

#[test]
fn leaf_time_overflow_and_ca_lifetime() {
    let t = now();
    let b = CaBundle::generate(t).unwrap();
    let max = time::PrimitiveDateTime::MAX.assume_utc();
    let min = time::PrimitiveDateTime::MIN.assume_utc();
    assert!(matches!(
        b.issue_edge_client(max),
        Err(IssueError::TimeOverflow)
    ));
    assert!(matches!(
        b.issue_worker_serving(&worker_id(), min),
        Err(IssueError::TimeOverflow)
    ));
    assert!(matches!(
        b.issue_edge_serving(&edge_names(), max),
        Err(IssueError::TimeOverflow)
    ));
    assert!(matches!(
        b.issue_allocation(&sample(), LeafKey::Generate, min, t),
        Err(IssueError::TimeOverflow)
    ));
    // Renewable leaves are capped at their CA's notAfter; allocation certs
    // (which must end exactly at expires-at) are refused instead.
    let ca_end = b.internal_ca().not_after();
    let late = ca_end - Duration::days(10);
    let worker = b.issue_worker_serving(&worker_id(), late).unwrap();
    assert_eq!(worker.not_after(), ca_end);
    assert!(!worker.needs_renewal(late));
    let (_, x) = X509Certificate::from_der(worker.cert_der()).unwrap();
    assert_eq!(x.validity().not_after.to_datetime(), ca_end);
    let serving = b.issue_edge_serving(&edge_names(), late).unwrap();
    assert_eq!(serving.not_after(), b.client_ca().not_after());
    let edge_client = b.issue_edge_client(ca_end - Duration::hours(1)).unwrap();
    assert_eq!(edge_client.not_after(), ca_end);
    assert!(matches!(
        b.issue_allocation(
            &sample(),
            LeafKey::Generate,
            late,
            late + Duration::days(11)
        ),
        Err(IssueError::OutlivesCa)
    ));
    assert!(matches!(
        b.issue_edge_client(ca_end),
        Err(IssueError::CaExpired)
    ));
    assert_eq!(
        b.issue_edge_client(t).unwrap().expiry().0.label(),
        "edge_client"
    );
}

#[test]
fn renewable_leaves_honour_a_custom_validity() {
    let bundle = CaBundle::generate(now()).unwrap();
    let b = &bundle;
    let v = Duration::minutes(4);
    let worker = b.issue_worker_serving_for(&worker_id(), now(), v).unwrap();
    let serving = b.issue_edge_serving_for(&edge_names(), now(), v).unwrap();
    let client = b.issue_edge_client_for(now(), v).unwrap();
    for leaf in [&worker, &serving, &client] {
        assert_eq!(leaf.not_after(), now() + v);
        // 2/3 of (4m + 5m backdate) after notBefore = now + 1m.
        assert_eq!(leaf.renew_at(), Some(now() + Duration::minutes(1)));
    }
    let d = LeafValidity::default();
    assert_eq!(
        b.issue_edge_client(now()).unwrap().not_after(),
        now() + d.edge_client
    );
    assert_eq!(
        b.issue_worker_serving(&worker_id(), now())
            .unwrap()
            .not_after(),
        now() + d.worker_serving
    );
}

fn worker_id() -> WorkerIdentity {
    WorkerIdentity::generate("ci".parse().unwrap(), "p".parse().unwrap(), "p-0".into()).unwrap()
}

#[test]
fn worker_identity_names_the_pod_incarnation() {
    let a = worker_id();
    let b = WorkerIdentity::generate(a.namespace.clone(), a.pool.clone(), a.pod.clone()).unwrap();
    assert_eq!(a.nonce.len(), crate::pki::NONCE_LEN);
    assert!(
        a.nonce
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    // Same slot pod name, fresh nonce: a different SAN and Secret.
    assert_ne!(a.dns_name(), b.dns_name());
    assert_ne!(a.secret_name(), b.secret_name());
    assert_eq!(a.secret_name(), format!("p-0-{}", a.nonce));
    assert_eq!(
        a.dns_name(),
        format!("{}.p-0.p.ci.worker.buildkit.internal", a.nonce)
    );
}
