//! The cache registry's serving certificate, kept by the leader.
//!
//! The registry pod mounts the Secret `cache.tlsSecretName` (in the
//! controller namespace) and re-reads it every minute. The leader issues its
//! certificate from the internal CA (which workers trust) for the host of
//! `cache.host`, puts the client CA next to it as `ca.crt` (the anchor for the
//! workers' registry client certificates, which no buildkitd trusts), and
//! reissues it when either CA changes or two thirds of its life have passed.
//! The registry never sees a CA key.

use std::collections::BTreeMap;
use std::time::Duration;

use futures::future::BoxFuture;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, PostParams};
use time::OffsetDateTime;
use tracing::{info, warn};
use x509_parser::extensions::{GeneralName, ParsedExtension};
use x509_parser::pem::parse_x509_pem;

use crate::pki::{CaBundle, IssueError};
use crate::render::{CONTROLLER, LABEL_MANAGED_BY};

/// How often the keeper looks at the CA it holds.
pub const CHECK_EVERY: Duration = Duration::from_secs(30);

/// Where the certificate goes and what it names.
#[derive(Clone, Debug)]
pub struct Target {
    /// Controller namespace.
    pub namespace: String,
    /// `cache.tlsSecretName`.
    pub secret_name: String,
    /// The host of `cache.host`.
    pub server_name: String,
    /// Lifetime of a new certificate.
    pub validity: time::Duration,
}

/// What the Secret needs.
#[derive(Debug)]
pub enum Decision {
    /// It is current until the given time.
    Keep(OffsetDateTime),
    /// Write this Secret (create, or replace at its `resourceVersion`).
    Write(Box<Secret>, OffsetDateTime),
}

/// Compare the Secret `have` with what the CA `ca` should have issued.
///
/// # Errors
/// [`IssueError`] when a new certificate cannot be issued.
pub fn decide(
    have: Option<&Secret>,
    ca: &CaBundle,
    t: &Target,
    now: OffsetDateTime,
) -> Result<Decision, IssueError> {
    let ca_pem = ca.client_ca().cert_pem();
    if let Some(renew_at) = have
        .filter(|s| field(s, "ca.crt") == Some(ca_pem.as_bytes()) && field(s, "tls.key").is_some())
        .and_then(|s| current_until(field(s, "tls.crt")?, &t.server_name, ca, now))
        .filter(|at| *at > now)
    {
        return Ok(Decision::Keep(renew_at));
    }
    let cert = ca.issue_cache_serving(&t.server_name, now, t.validity)?;
    let key = cert.private_key_pem().ok_or(IssueError::Rng)?;
    let bytes = |s: &str| ByteString(s.as_bytes().to_vec());
    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(t.secret_name.clone()),
            namespace: Some(t.namespace.clone()),
            labels: Some(BTreeMap::from([(
                LABEL_MANAGED_BY.to_owned(),
                CONTROLLER.to_owned(),
            )])),
            resource_version: have.and_then(|s| s.metadata.resource_version.clone()),
            ..ObjectMeta::default()
        },
        type_: Some("kubernetes.io/tls".into()),
        data: Some(BTreeMap::from([
            ("tls.crt".to_owned(), bytes(cert.cert_pem())),
            ("tls.key".to_owned(), bytes(&key)),
            ("ca.crt".to_owned(), bytes(ca_pem)),
        ])),
        ..Secret::default()
    };
    let renew_at = cert.renew_at().unwrap_or(now);
    Ok(Decision::Write(Box::new(secret), renew_at))
}

fn field<'a>(s: &'a Secret, key: &str) -> Option<&'a [u8]> {
    s.data.as_ref()?.get(key).map(|v| v.0.as_slice())
}

/// When a serving certificate of the current internal CA that names exactly
/// `name` is due for renewal (two thirds of its life), or `None` when it does
/// not parse, does not chain to that CA, or names another host.
fn current_until(
    pem: &[u8],
    name: &str,
    ca: &CaBundle,
    now: OffsetDateTime,
) -> Option<OffsetDateTime> {
    let (_, pem) = parse_x509_pem(pem).ok()?;
    let anchor = webpki::anchor_from_trusted_cert(ca.internal_ca().cert_der()).ok()?;
    let der = rustls_pki_types::CertificateDer::from(pem.contents.as_slice());
    let at = rustls_pki_types::UnixTime::since_unix_epoch(Duration::from_secs(
        u64::try_from(now.unix_timestamp()).ok()?,
    ));
    webpki::EndEntityCert::try_from(&der)
        .ok()?
        .verify_for_usage(
            &[webpki::ring::ECDSA_P256_SHA256],
            &[anchor],
            &[],
            at,
            webpki::KeyUsage::server_auth(),
            None,
            None,
        )
        .ok()?;
    let cert = pem.parse_x509().ok()?;
    let names: Vec<&str> = cert
        .extensions()
        .iter()
        .filter_map(|e| match e.parsed_extension() {
            ParsedExtension::SubjectAlternativeName(san) => Some(&san.general_names),
            _ => None,
        })
        .flatten()
        .filter_map(|g| match g {
            GeneralName::DNSName(d) => Some(*d),
            _ => None,
        })
        .collect();
    if names != [name] {
        return None;
    }
    let v = cert.validity();
    let (from, to) = (v.not_before.to_datetime(), v.not_after.to_datetime());
    Some(from + (to - from) * 2 / 3)
}

/// The Secret's reads and writes, so one [`sync`] step can be tested.
pub trait SecretStore: Send + Sync {
    /// GET; `Ok(None)` on 404.
    fn get(&self, name: &str) -> BoxFuture<'_, Result<Option<Secret>, String>>;
    /// POST when `replace` is false, else PUT at the Secret's `resourceVersion`.
    fn put(&self, secret: Secret, replace: bool) -> BoxFuture<'_, Result<(), String>>;
}

impl SecretStore for Api<Secret> {
    fn get(&self, name: &str) -> BoxFuture<'_, Result<Option<Secret>, String>> {
        let name = name.to_owned();
        Box::pin(async move { self.get_opt(&name).await.map_err(|e| e.to_string()) })
    }

    fn put(&self, secret: Secret, replace: bool) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let pp = PostParams {
                field_manager: Some(CONTROLLER.into()),
                ..PostParams::default()
            };
            let name = secret.metadata.name.clone().unwrap_or_default();
            let r = if replace {
                self.replace(&name, &pp, &secret).await
            } else {
                self.create(&pp, &secret).await
            };
            r.map(drop).map_err(|e| e.to_string())
        })
    }
}

/// Keep the Secret current until the term ends: every [`CHECK_EVERY`] one
/// read, and a write whenever [`decide`] asks for one, so a Secret deleted or
/// edited by hand is restored within a check. `ca` returns the CA as the
/// leader last loaded it.
pub async fn keep(client: kube::Client, t: Target, ca: impl Fn() -> Option<CaBundle>) {
    let api: Api<Secret> = Api::namespaced(client, &t.namespace);
    loop {
        if let Some(bundle) = ca() {
            sync(&api, &t, &bundle, OffsetDateTime::now_utc()).await;
        }
        tokio::time::sleep(CHECK_EVERY).await;
    }
}

/// One read and, when needed, one write. True when it wrote.
pub async fn sync(api: &dyn SecretStore, t: &Target, ca: &CaBundle, now: OffsetDateTime) -> bool {
    let have = match api.get(&t.secret_name).await {
        Ok(s) => s,
        Err(error) => {
            warn!(secret = %t.secret_name, %error, "reading the cache registry certificate");
            return false;
        }
    };
    let secret = match decide(have.as_ref(), ca, t, now) {
        Ok(Decision::Keep(_)) => return false,
        Ok(Decision::Write(s, _)) => *s,
        Err(error) => {
            warn!(%error, "issuing the cache registry certificate");
            return false;
        }
    };
    match api.put(secret, have.is_some()).await {
        Ok(()) => {
            info!(secret = %t.secret_name, name = %t.server_name, "issued the cache registry certificate");
            true
        }
        Err(error) => {
            warn!(secret = %t.secret_name, %error, "writing the cache registry certificate");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn target() -> Target {
        Target {
            namespace: "bk".into(),
            secret_name: "bk-registry-tls".into(),
            server_name: "bk-registry.bk.svc".into(),
            validity: time::Duration::days(30),
        }
    }

    fn write(d: Decision) -> (Secret, OffsetDateTime) {
        match d {
            Decision::Write(s, at) => (*s, at),
            Decision::Keep(_) => panic!("expected a write"),
        }
    }

    #[test]
    fn issues_then_keeps_until_two_thirds_of_its_life() {
        let now = OffsetDateTime::now_utc();
        let ca = CaBundle::generate(now).unwrap();
        let t = target();
        let (mut s, at) = write(decide(None, &ca, &t, now).unwrap());
        assert_eq!(s.type_.as_deref(), Some("kubernetes.io/tls"));
        assert_eq!(
            field(&s, "ca.crt"),
            Some(ca.client_ca().cert_pem().as_bytes())
        );
        assert!(at > now + time::Duration::days(19) && at < now + time::Duration::days(21));
        s.metadata.resource_version = Some("7".into());
        match decide(Some(&s), &ca, &t, now + time::Duration::days(1)).unwrap() {
            Decision::Keep(k) => assert!((k - at).abs() < time::Duration::seconds(2)),
            Decision::Write(..) => panic!("a current Secret is kept"),
        }
        // Renewal keeps the resourceVersion, so the replace is preconditioned.
        let (renewed, _) =
            write(decide(Some(&s), &ca, &t, at + time::Duration::seconds(1)).unwrap());
        assert_eq!(renewed.metadata.resource_version.as_deref(), Some("7"));
    }

    #[test]
    fn a_new_internal_ca_or_another_name_reissues() {
        let now = OffsetDateTime::now_utc();
        let ca = CaBundle::generate(now).unwrap();
        let (s, _) = write(decide(None, &ca, &target(), now).unwrap());
        let other = CaBundle::generate(now).unwrap();
        let (s2, _) = write(decide(Some(&s), &other, &target(), now).unwrap());
        assert_eq!(
            field(&s2, "ca.crt"),
            Some(other.client_ca().cert_pem().as_bytes())
        );
        let moved = Target {
            server_name: "elsewhere.bk.svc".into(),
            ..target()
        };
        write(decide(Some(&s), &ca, &moved, now).unwrap());
    }

    /// One Secret in memory, with the writes it saw.
    #[derive(Default)]
    struct Fake {
        secret: std::sync::Mutex<Option<Secret>>,
        writes: std::sync::Mutex<Vec<bool>>,
    }

    impl SecretStore for Fake {
        fn get(&self, _: &str) -> BoxFuture<'_, Result<Option<Secret>, String>> {
            let s = self.secret.lock().unwrap().clone();
            Box::pin(async move { Ok(s) })
        }
        fn put(&self, secret: Secret, replace: bool) -> BoxFuture<'_, Result<(), String>> {
            self.writes.lock().unwrap().push(replace);
            *self.secret.lock().unwrap() = Some(secret);
            Box::pin(async { Ok(()) })
        }
    }

    /// Every check reads the Secret: one deleted or edited by hand is written
    /// again at the next check, a current one is left alone.
    #[tokio::test]
    async fn a_deleted_or_edited_secret_is_restored_at_the_next_check() {
        let now = OffsetDateTime::now_utc();
        let ca = CaBundle::generate(now).unwrap();
        let (t, fake) = (target(), Fake::default());
        assert!(sync(&fake, &t, &ca, now).await, "created");
        assert!(!sync(&fake, &t, &ca, now).await, "current: no write");
        // Deleted.
        *fake.secret.lock().unwrap() = None;
        assert!(sync(&fake, &t, &ca, now).await, "recreated");
        // Edited: the key removed and ca.crt changed.
        {
            let mut s = fake.secret.lock().unwrap();
            let s = s.as_mut().unwrap();
            s.metadata.resource_version = Some("9".into());
            let data = s.data.as_mut().unwrap();
            data.remove("tls.key");
            data.insert("ca.crt".into(), ByteString(b"x".to_vec()));
        }
        assert!(sync(&fake, &t, &ca, now).await, "replaced");
        let s = fake.secret.lock().unwrap().clone().unwrap();
        assert_eq!(s.metadata.resource_version.as_deref(), Some("9"));
        assert!(field(&s, "tls.key").is_some());
        assert_eq!(
            field(&s, "ca.crt"),
            Some(ca.client_ca().cert_pem().as_bytes())
        );
        assert_eq!(*fake.writes.lock().unwrap(), [false, false, true]);
    }

    /// The serving cert must come from the internal CA workers trust: a Secret
    /// whose `ca.crt` is current but whose serving cert another CA issued is
    /// reissued.
    #[test]
    fn a_serving_cert_of_another_internal_ca_is_reissued() {
        let now = OffsetDateTime::now_utc();
        let (old, new) = (
            CaBundle::generate(now).unwrap(),
            CaBundle::generate(now).unwrap(),
        );
        let (mut s, _) = write(decide(None, &old, &target(), now).unwrap());
        s.data.as_mut().unwrap().insert(
            "ca.crt".into(),
            ByteString(new.client_ca().cert_pem().as_bytes().to_vec()),
        );
        let (fresh, _) = write(decide(Some(&s), &new, &target(), now).unwrap());
        let crt = field(&fresh, "tls.crt").unwrap();
        assert!(current_until(crt, "bk-registry.bk.svc", &new, now).is_some());
        assert!(current_until(crt, "bk-registry.bk.svc", &old, now).is_none());
    }
}
