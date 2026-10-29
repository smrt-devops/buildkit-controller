//! Who is calling and what they may do.
//!
//! The TLS layer has already chained the client cert to the client CA.
//! [`peer_identity`] then reads the worker it names, and an [`Authorizer`]
//! maps that worker to a [`Grant`]. The grant is asked for again on every
//! request, so an allocation that ends takes its access with it.

use std::collections::HashMap;
use std::path::Path;

use rustls_pki_types::CertificateDer;
use x509_parser::extensions::{GeneralName, ParsedExtension};
use x509_parser::oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME;
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::pki::{WorkerIdentity, is_worker_nonce};

/// What a worker may do: pull from `repository`, and push there too when
/// `push` is set. Nothing else, not even a repository nested below it, so one
/// allocation's cache is one repository.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct Grant {
    /// `<pool-namespace>/<pool>/<scope>`.
    pub repository: String,
    /// Trusted allocations only.
    pub push: bool,
}

/// Pull or push.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Pull,
    Push,
}

impl Action {
    /// Metric and log label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Pull => "pull",
            Self::Push => "push",
        }
    }
}

impl Grant {
    /// Whether this grant allows `action` on `repository`.
    #[must_use]
    pub fn allows(&self, repository: &str, action: Action) -> bool {
        repository == self.repository && (action == Action::Pull || self.push)
    }
}

/// Maps a verified worker to its grant. `None` denies everything.
pub trait Authorizer: Send + Sync {
    fn grant(&self, peer: &WorkerIdentity) -> Option<Grant>;
}

/// Grants by pod, from a file or built in tests. A pod not listed gets nothing.
#[derive(Debug, Default)]
pub struct StaticGrants(HashMap<(String, String), Grant>);

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    namespace: String,
    pod: String,
    repository: String,
    #[serde(default)]
    push: bool,
}

impl StaticGrants {
    /// Adds the grant of pod `namespace/pod`.
    #[must_use]
    pub fn with(mut self, namespace: &str, pod: &str, grant: Grant) -> Self {
        self.0.insert((namespace.to_owned(), pod.to_owned()), grant);
        self
    }

    /// Reads a YAML list of `{namespace, pod, repository, push}`.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let entries: Vec<Entry> = serde_saphyr::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
        Ok(entries.into_iter().fold(Self::default(), |g, e| {
            g.with(
                &e.namespace,
                &e.pod,
                Grant {
                    repository: e.repository,
                    push: e.push,
                },
            )
        }))
    }
}

impl Authorizer for StaticGrants {
    fn grant(&self, peer: &WorkerIdentity) -> Option<Grant> {
        self.0
            .get(&(peer.namespace.to_string(), peer.pod.clone()))
            .cloned()
    }
}

/// Why a verified client cert does not name a worker.
#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    #[error("certificate does not parse")]
    Parse,
    #[error("certificate lacks EKU clientAuth")]
    ClientAuthEku,
    #[error("SAN is not exactly one worker DNS name")]
    BadSan,
}

const WORKER_SUFFIX: &str = ".worker.buildkit.internal";

/// The worker a client cert names: EKU clientAuth must be present, and the
/// SAN must be exactly one `<nonce>.<pod>.<pool>.<namespace>.worker.buildkit.internal`.
/// The caller has verified the chain.
pub fn peer_identity(cert: &CertificateDer<'_>) -> Result<WorkerIdentity, PeerError> {
    let (_, x509) = X509Certificate::from_der(cert).map_err(|_| PeerError::Parse)?;
    match x509.extended_key_usage() {
        Ok(Some(eku)) if eku.value.client_auth => {}
        _ => return Err(PeerError::ClientAuthEku),
    }
    let mut sans = x509
        .extensions()
        .iter()
        .filter(|e| e.oid == OID_X509_EXT_SUBJECT_ALT_NAME);
    let (Some(ext), None) = (sans.next(), sans.next()) else {
        return Err(PeerError::BadSan);
    };
    let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() else {
        return Err(PeerError::BadSan);
    };
    let [GeneralName::DNSName(name)] = san.general_names.as_slice() else {
        return Err(PeerError::BadSan);
    };
    parse_worker_name(name).ok_or(PeerError::BadSan)
}

fn parse_worker_name(name: &str) -> Option<WorkerIdentity> {
    let head = name.strip_suffix(WORKER_SUFFIX)?;
    let mut parts = head.split('.');
    let (Some(nonce), Some(pod), Some(pool), Some(ns), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return None;
    };
    let label = pod.len() <= 63
        && pod
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !pod.starts_with('-')
        && !pod.ends_with('-')
        && !pod.is_empty();
    if !is_worker_nonce(nonce) || !label {
        return None;
    }
    Some(WorkerIdentity {
        namespace: ns.parse().ok()?,
        pool: pool.parse().ok()?,
        pod: pod.to_owned(),
        nonce: nonce.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn grants_cover_exactly_the_repository() {
        let g = Grant {
            repository: "ns/pool/app".into(),
            push: false,
        };
        assert!(g.allows("ns/pool/app", Action::Pull));
        assert!(!g.allows("ns/pool/app/sub", Action::Pull));
        assert!(!g.allows("ns/pool/apple", Action::Pull));
        assert!(!g.allows("ns/pool", Action::Pull));
        assert!(!g.allows("ns/pool/app", Action::Push));
        let w = Grant { push: true, ..g };
        assert!(w.allows("ns/pool/app", Action::Push));
        assert!(!w.allows("ns/other/app", Action::Push));
    }

    #[test]
    fn worker_names_parse_strictly() {
        let id = parse_worker_name("0123456789abcdef.p-0.p.ns.worker.buildkit.internal").unwrap();
        assert_eq!(id.pod, "p-0");
        assert_eq!(id.pool.as_str(), "p");
        assert_eq!(id.namespace.as_str(), "ns");
        for bad in [
            "0123456789abcdef.p-0.p.ns.worker.buildkit.internal.evil",
            "0123456789ABCDEF.p-0.p.ns.worker.buildkit.internal",
            "0123456789abcdef.x.p-0.p.ns.worker.buildkit.internal",
            "0123456789abcdef.p-0.p.worker.buildkit.internal",
            "0123456789abcdef.-p.p.ns.worker.buildkit.internal",
            "0123456789abcdef.p-0.p.NS.worker.buildkit.internal",
        ] {
            assert!(parse_worker_name(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn static_grants_load_from_yaml() {
        let dir = std::env::temp_dir().join(format!("bk-grants-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("grants.yaml");
        std::fs::write(
            &f,
            "- {namespace: ns, pod: a, repository: ns/p/a, push: true}\n- {namespace: ns, pod: b, repository: ns/p/a}\n",
        )
        .unwrap();
        let g = StaticGrants::load(&f).unwrap();
        let id = |pod: &str| {
            parse_worker_name(&format!("0123456789abcdef.{pod}.p.ns{WORKER_SUFFIX}")).unwrap()
        };
        assert!(g.grant(&id("a")).unwrap().push);
        assert!(!g.grant(&id("b")).unwrap().push);
        assert!(g.grant(&id("c")).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
