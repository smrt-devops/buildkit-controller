//! Typed view of one worker pod, parsed once from the store
//! so the planner never touches raw `Pod` objects.

use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::jiff::Timestamp;

use crate::render::{
    ANN_ALLOC, ANN_EXPIRES_AT, ANN_INTERNAL_CA, ANN_WORKER_CERT_NOT_AFTER, LABEL_POOL,
    LABEL_TEMPLATE_HASH,
};

/// Name of `pod` if it is foreign to the pool (`Observed::foreign_pods`): in
/// the pool's namespace `pool_ns`, labelled `P pool = pool`, and without a
/// controller ownerReference to `pool_uid`. Pods in other namespaces never are.
#[must_use]
pub fn foreign_pod_name(pod: &Pod, pool_ns: &str, pool: &str, pool_uid: &str) -> Option<String> {
    let m = &pod.metadata;
    let labelled = m
        .labels
        .as_ref()
        .and_then(|l| l.get(LABEL_POOL))
        .is_some_and(|v| v == pool);
    let owned = m
        .owner_references
        .iter()
        .flatten()
        .any(|r| r.uid == pool_uid && r.controller == Some(true));
    (m.namespace.as_deref() == Some(pool_ns) && labelled && !owned)
        .then(|| m.name.clone())
        .flatten()
}

/// Grace period used when a pod does not state one.
pub const DEFAULT_GRACE_SECONDS: i64 = 5;

/// `P alloc`, the source of truth for a pod's state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Alloc {
    /// Warm and claimable.
    Free,
    /// Retired; every pass deletes it.
    Draining,
    /// Claimed, with the allocation id.
    Allocated(String),
    /// Missing or unrecognised annotation. The pod holds its slot and is never touched.
    Unknown,
}

/// Pod phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Pending (scheduling or pulling).
    Pending,
    /// Running.
    Running,
    /// Exited 0.
    Succeeded,
    /// Exited non-zero.
    Failed,
    /// Absent or unknown.
    Unknown,
}

impl Phase {
    /// `Failed` or `Succeeded`.
    #[must_use]
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Failed | Self::Succeeded)
    }
}

/// One pod of the pool, as the planner sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkerPod {
    /// `metadata.name`.
    pub name: String,
    /// `metadata.uid`; every write is preconditioned on it.
    pub uid: String,
    /// Slot parsed from `<pool>-<slot>`; `None` when the name is not canonical.
    pub slot: Option<u32>,
    /// `P alloc`.
    pub alloc: Alloc,
    /// `status.phase`.
    pub phase: Phase,
    /// `Ready` condition is `True`.
    pub ready: bool,
    /// `status.podIP` is set.
    pub has_ip: bool,
    /// `metadata.creationTimestamp`.
    pub created: Option<Timestamp>,
    /// `metadata.deletionTimestamp` (terminating).
    pub deletion: Option<Timestamp>,
    /// Grace period in seconds (`deletionGracePeriodSeconds`, else the spec's, else 5).
    pub grace_seconds: i64,
    /// `P template-hash`.
    pub template_hash: Option<String>,
    /// `P expires-at`.
    pub expires_at: Option<Timestamp>,
    /// `P expires-at` as written, which an expiry retire tests.
    pub expires_at_raw: Option<String>,
    /// `P worker-cert-not-after`.
    pub worker_cert_not_after: Option<Timestamp>,
    /// `P internal-ca` (16 hex chars).
    pub internal_ca: Option<String>,
    /// `status.reason` is set (`Evicted`, `Shutdown`, `NodeLost`): the node
    /// ended the pod, not buildkitd.
    pub evicted: bool,
}

impl WorkerPod {
    /// Parses a store pod. `None` unless its controller ownerReference uid is
    /// `pool_uid` and it has a uid.
    #[must_use]
    pub fn from_pod(pod: &Pod, pool: &str, pool_uid: &str) -> Option<Self> {
        let m = &pod.metadata;
        let owned = m
            .owner_references
            .iter()
            .flatten()
            .any(|r| r.uid == pool_uid && r.controller == Some(true));
        if !owned {
            return None;
        }
        let name = m.name.clone()?;
        let uid = m.uid.clone()?;
        let ann = |k: &str| m.annotations.as_ref().and_then(|a| a.get(k));
        let ts = |k: &str| ann(k).and_then(|v| v.parse::<Timestamp>().ok());
        let alloc = match ann(ANN_ALLOC).map(String::as_str) {
            Some("free") => Alloc::Free,
            Some("draining") => Alloc::Draining,
            Some(id) if is_alloc_id(id) => Alloc::Allocated(id.to_owned()),
            _ => Alloc::Unknown,
        };
        let status = pod.status.as_ref();
        let phase = match status.and_then(|s| s.phase.as_deref()) {
            Some("Pending") => Phase::Pending,
            Some("Running") => Phase::Running,
            Some("Succeeded") => Phase::Succeeded,
            Some("Failed") => Phase::Failed,
            _ => Phase::Unknown,
        };
        let ready = status
            .and_then(|s| s.conditions.as_ref())
            .into_iter()
            .flatten()
            .any(|c| c.type_ == "Ready" && c.status == "True");
        let grace_seconds = m
            .deletion_grace_period_seconds
            .or_else(|| {
                pod.spec
                    .as_ref()
                    .and_then(|s| s.termination_grace_period_seconds)
            })
            .unwrap_or(DEFAULT_GRACE_SECONDS);
        Some(Self {
            slot: parse_slot(&name, pool),
            name,
            uid,
            alloc,
            phase,
            ready,
            has_ip: status.and_then(|s| s.pod_ip.as_ref()).is_some(),
            created: m.creation_timestamp.as_ref().map(|t| t.0),
            deletion: m.deletion_timestamp.as_ref().map(|t| t.0),
            grace_seconds,
            template_hash: m
                .labels
                .as_ref()
                .and_then(|l| l.get(LABEL_TEMPLATE_HASH))
                .cloned(),
            expires_at: ts(ANN_EXPIRES_AT),
            expires_at_raw: ann(ANN_EXPIRES_AT).cloned(),
            worker_cert_not_after: ts(ANN_WORKER_CERT_NOT_AFTER),
            internal_ca: ann(ANN_INTERNAL_CA).cloned(),
            evicted: status.and_then(|s| s.reason.as_ref()).is_some(),
        })
    }

    /// Has a `deletionTimestamp`.
    #[must_use]
    pub fn terminating(&self) -> bool {
        self.deletion.is_some()
    }
}

/// Slot of `<pool>-<n>`: a decimal without leading zeros.
#[must_use]
pub fn parse_slot(name: &str, pool: &str) -> Option<u32> {
    let s = name.strip_prefix(pool)?.strip_prefix('-')?;
    let n: u32 = s.parse().ok()?;
    (s == n.to_string()).then_some(n)
}

/// Slot pod name `<pool>-<slot>`.
#[must_use]
pub fn slot_name(pool: &str, slot: u32) -> String {
    format!("{pool}-{slot}")
}

/// 26 chars of lowercase RFC 4648 base32.
fn is_alloc_id(s: &str) -> bool {
    s.len() == 26
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
}
