//! Worker pod and sandbox rendering.
//!
//! Pure builders. They do no Kubernetes I/O, read no clock and use no randomness,
//! so the same inputs always produce byte-identical objects. A template that tries to set a
//! controller-owned field is a typed [`RenderError::TemplateConflict`] naming the
//! field, never a silent override.

mod config;
mod hash;
mod objects;
mod pod;
mod validate;

#[cfg(any(test, feature = "fixtures"))]
pub mod fixtures;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fmt;

use buildkit_controller_api::v1alpha1::{BuildKitPool, GROUP, VERSION};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;

pub use config::{buildkitd_config, is_scope_id, quantity_bytes, scope_id};
pub use hash::{config_hash, template_hash};
pub use objects::{ca_bundle_config_map, network_policy, service_account, worker_secret};
pub use pod::{pod_spec, worker_pod};
pub use validate::{
    Conflict, PROTECTED_CIDRS, refused_allow_cidr, refused_allow_cidrs, validate_template,
};

pub use crate::pki::worker_secret_name;
use crate::pki::{Ca, IssuedCert};

/// Label and annotation key prefix of the controller (`P`).
pub const KEY_PREFIX: &str = "buildkit.smrt-devops.net/";
/// Standard `managed-by` label key.
pub const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
/// Standard `name` label key.
pub const LABEL_NAME: &str = "app.kubernetes.io/name";
/// Value of [`LABEL_MANAGED_BY`], and of the controller's own pod `name` label.
pub const CONTROLLER: &str = "buildkit-controller";
/// Value of [`LABEL_NAME`] on worker pods.
pub const WORKER_NAME: &str = "buildkitd";
/// `P pool`.
pub const LABEL_POOL: &str = "buildkit.smrt-devops.net/pool";
/// `P state`.
pub const LABEL_STATE: &str = "buildkit.smrt-devops.net/state";
/// `P template-hash`.
pub const LABEL_TEMPLATE_HASH: &str = "buildkit.smrt-devops.net/template-hash";
/// `P alloc`: the source of truth for allocation state.
pub const ANN_ALLOC: &str = "buildkit.smrt-devops.net/alloc";
/// `P owner`.
pub const ANN_OWNER: &str = "buildkit.smrt-devops.net/owner";
/// `P allocated-at`.
pub const ANN_ALLOCATED_AT: &str = "buildkit.smrt-devops.net/allocated-at";
/// `P expires-at`.
pub const ANN_EXPIRES_AT: &str = "buildkit.smrt-devops.net/expires-at";
/// `P worker-cert-not-after`.
pub const ANN_WORKER_CERT_NOT_AFTER: &str = "buildkit.smrt-devops.net/worker-cert-not-after";
/// `P internal-ca`.
pub const ANN_INTERNAL_CA: &str = "buildkit.smrt-devops.net/internal-ca";
/// `P worker-nonce`: the nonce in the worker serving cert's SAN.
pub const ANN_WORKER_NONCE: &str = "buildkit.smrt-devops.net/worker-nonce";

/// `P scope`: the cache scope id of the allocation a pod serves.
pub const ANN_SCOPE: &str = "buildkit.smrt-devops.net/scope";
/// `P tier`: `trusted` or `untrusted`, whether that allocation may write the cache.
pub const ANN_TIER: &str = "buildkit.smrt-devops.net/tier";
/// Value of [`LABEL_NAME`] on the built-in cache registry pod; workers may reach it.
pub const REGISTRY_NAME: &str = "buildkit-registry";

/// Port buildkitd listens on (mTLS).
pub const BUILDKIT_PORT: i32 = 1234;
/// Name of the worker `ServiceAccount` and `NetworkPolicy` of a pool.
#[must_use]
pub fn worker_object_name(pool: &str) -> String {
    format!("{pool}-buildkit-worker")
}

/// Slot pod name `<pool>-<slot>`.
#[must_use]
pub fn worker_pod_name(pool: &str, slot: u32) -> String {
    format!("{pool}-{slot}")
}

/// Characters of the internal CA fingerprint kept in `P internal-ca`.
const INTERNAL_CA_LABEL_LEN: usize = 16;

/// One offending template field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplateConflictField {
    /// Field path, e.g. `spec.template.spec.volumes[0].name`.
    pub field: String,
    /// What is wrong with it.
    pub reason: String,
}

/// Why rendering failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RenderError {
    /// The template sets a controller-owned key, name, path or arch
    /// (`Ready=False/TemplateConflict`).
    #[error("template conflict: {}", .0.iter().map(|c| format!("{} ({})", c.field, c.reason)).collect::<Vec<_>>().join("; "))]
    TemplateConflict(Vec<TemplateConflictField>),
    /// The pool object lacks a field only the API server sets.
    #[error("pool is missing metadata.{0}")]
    MissingMetadata(&'static str),
    /// The pool name leaves no room for `-<slot>`.
    #[error("pool name `{0}` is longer than 52 characters")]
    NameTooLong(String),
    /// The issued certificate carries no private key.
    #[error("worker certificate has no private key")]
    NoPrivateKey,
    /// An object failed to serialise (hashing only).
    #[error("serialising: {0}")]
    Serialize(String),
}

impl RenderError {
    /// The offending field paths of a template conflict, for the condition message.
    #[must_use]
    pub fn conflict_fields(&self) -> Vec<&str> {
        match self {
            Self::TemplateConflict(c) => c.iter().map(|c| c.field.as_str()).collect(),
            _ => Vec::new(),
        }
    }
}

/// Controller-wide settings render reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderConfig {
    /// `POD_NAMESPACE`: where the controller pods (and the CA bundle) live.
    pub controller_namespace: String,
    /// `workerNetwork`.
    pub worker_network: WorkerNetwork,
    /// `cache.host`: where workers reach the built-in cache registry, in the
    /// controller namespace. `None` = no cache.
    pub cache_host: Option<String>,
}

/// The `workerNetwork` settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerNetwork {
    /// Ranges denied to the internet rule.
    pub denied_cidrs: Vec<String>,
    /// Extra denied ranges (the API server's public address, other clouds).
    pub extra_denied_cidrs: Vec<String>,
    /// The cluster's own pod, service and node ranges (both families), denied like
    /// `denied_cidrs`. The IPv6 internet rule is rendered only when this holds an IPv6
    /// range: without one the controller cannot tell cluster addresses (often global
    /// unicast on IPv6 clusters) from the internet, so IPv6 egress stays closed.
    pub cluster_cidrs: Vec<String>,
    /// Ranges a pool's `allowCIDRs` may open although they overlap the link-local,
    /// metadata, loopback, cluster or extra denied ranges. An entry must lie wholly inside one.
    pub permitted_cidrs: Vec<String>,
    /// Where workers may resolve names.
    pub dns: WorkerDns,
}

/// The `workerNetwork.dns` settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerDns {
    /// Namespace of the DNS pods.
    pub namespace: String,
    /// Labels of the DNS pods.
    pub pod_labels: BTreeMap<String, String>,
    /// DNS server CIDRs (`NodeLocal DNSCache`).
    pub cidrs: Vec<String>,
}

impl Default for WorkerNetwork {
    fn default() -> Self {
        Self {
            denied_cidrs: [
                "10.0.0.0/8",
                "172.16.0.0/12",
                "192.168.0.0/16",
                "100.64.0.0/10",
                "169.254.0.0/16",
                "127.0.0.0/8",
                "168.63.129.16/32",
                "fc00::/7",
                "fe80::/10",
                "64:ff9b::/96",
                "64:ff9b:1::/48",
            ]
            .map(String::from)
            .to_vec(),
            extra_denied_cidrs: Vec::new(),
            cluster_cidrs: Vec::new(),
            permitted_cidrs: Vec::new(),
            dns: WorkerDns {
                namespace: "kube-system".into(),
                pod_labels: BTreeMap::from([("k8s-app".into(), "kube-dns".into())]),
                cidrs: Vec::new(),
            },
        }
    }
}

/// Per-pod inputs of [`worker_pod`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PodInputs {
    /// Slot number, `0..max-1`.
    pub slot: u32,
    /// Full hex SHA-256 of the current internal CA DER (`Ca::fingerprint_hex`);
    /// the first 16 characters are rendered.
    pub internal_ca_fingerprint: String,
    /// The per-pod worker serving cert's notAfter, RFC 3339 UTC.
    pub worker_cert_not_after: String,
    /// Worker nonce ([`crate::pki::WorkerIdentity::nonce`]): names the per-pod
    /// Secret `<pod>-<nonce>` and is rendered as `P worker-nonce`.
    pub nonce: String,
}

/// Contents of the worker Secret that come from the PKI.
#[derive(Clone, PartialEq, Eq)]
pub struct WorkerTlsPem {
    /// Worker serving certificate.
    pub cert: String,
    /// Its PKCS#8 private key.
    pub key: String,
    /// Internal CA certificate only.
    pub ca: String,
}

impl fmt::Debug for WorkerTlsPem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerTlsPem").finish_non_exhaustive()
    }
}

impl WorkerTlsPem {
    /// Collect the PEMs from a freshly issued worker cert and the internal CA.
    pub fn from_pki(cert: &IssuedCert, internal_ca: &Ca) -> Result<Self, RenderError> {
        Ok(Self {
            cert: cert.cert_pem().to_owned(),
            key: cert.private_key_pem().ok_or(RenderError::NoPrivateKey)?,
            ca: internal_ca.cert_pem().to_owned(),
        })
    }
}

/// Name, namespace and uid of the pool, or the missing field.
fn pool_identity(pool: &BuildKitPool) -> Result<(&str, &str, &str), RenderError> {
    let m = &pool.metadata;
    let name = m
        .name
        .as_deref()
        .ok_or(RenderError::MissingMetadata("name"))?;
    if name.len() > buildkit_controller_api::v1alpha1::MAX_POOL_NAME_LEN {
        return Err(RenderError::NameTooLong(name.to_owned()));
    }
    let ns = m
        .namespace
        .as_deref()
        .ok_or(RenderError::MissingMetadata("namespace"))?;
    let uid = m
        .uid
        .as_deref()
        .ok_or(RenderError::MissingMetadata("uid"))?;
    Ok((name, ns, uid))
}

/// Controller ownerReference to the pool (`controller: true`, no `blockOwnerDeletion`).
fn owner_ref(name: &str, uid: &str) -> OwnerReference {
    OwnerReference {
        api_version: format!("{GROUP}/{VERSION}"),
        kind: "BuildKitPool".into(),
        name: name.to_owned(),
        uid: uid.to_owned(),
        controller: Some(true),
        block_owner_deletion: None,
    }
}
