//! The pod, allocation and revocation checks, against the shared
//! Pod and Pool stores.

use std::net::IpAddr;

use k8s_openapi::api::core::v1::Pod;
use kube::runtime::reflector::{ObjectRef, Store};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use buildkit_controller_api::v1alpha1::BuildKitPool;

use super::handler::Refusal;
use super::metrics::Revocation;
use crate::pki::{AllocId, AllocationId, WorkerIdentity, is_worker_nonce};
use crate::render::{
    ANN_ALLOC, ANN_EXPIRES_AT, ANN_OWNER, ANN_WORKER_NONCE, CONTROLLER, LABEL_MANAGED_BY,
    LABEL_POOL,
};
use crate::runtime::stores::StoreFreshness;

/// Read-only views of the stores the edge decides from.
#[derive(Clone)]
pub struct PodView {
    pub pods: Store<Pod>,
    pub pools: Store<BuildKitPool>,
    /// Both stores listed and have not been without a working watch for
    /// longer than [`STALE_AFTER`](crate::runtime::stores::STALE_AFTER).
    pub fresh: StoreFreshness,
}

/// Where to dial and which worker identity the upstream cert must carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// The pod's IP.
    pub ip: IpAddr,
    /// The pod's name and `P worker-nonce`.
    pub worker: WorkerIdentity,
    /// Who holds the allocation (`P owner`); the namespace when unset.
    pub owner: String,
}

impl PodView {
    /// Checks 0 and 3–6 for `id` at `now`; on success, the pod's IP and worker
    /// identity. The proxy runs this inside the handshake and again just
    /// before it dials.
    pub fn resolve(&self, id: &AllocationId, now: OffsetDateTime) -> Result<Target, Refusal> {
        // Check 0. A store that lost its watch may miss a release or a delete.
        // Within STALE_AFTER that only admits the client that already held the
        // single-use worker, and a reused IP still fails the upstream
        // handshake on the worker's serving-cert name. Past the bound the
        // cache is too old to vouch for an allocation, so refuse.
        if !self.fresh.is_fresh() {
            return Err(Refusal::StoreStale);
        }
        let uid = id.pod_uid.as_str();
        let pod = self
            .pods
            .find(|p| p.metadata.uid.as_deref() == Some(uid))
            .ok_or(Refusal::PodNotFound)?;
        if !self.belongs(&pod, id) {
            return Err(Refusal::PodNotFound);
        }
        if annotation(&pod, ANN_ALLOC) != Some(id.alloc_id.as_str()) {
            return Err(Refusal::AllocMismatch);
        }
        let live = annotation(&pod, ANN_EXPIRES_AT)
            .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
            .is_some_and(|t| t > now);
        if !live || pod.metadata.deletion_timestamp.is_some() {
            return Err(Refusal::Expired);
        }
        if phase(&pod) != Some("Running") {
            return Err(Refusal::PodNotReady);
        }
        let ip = pod_ip(&pod).ok_or(Refusal::PodNotReady)?;
        // The IP must still be this pod's alone: another Running pod with it
        // means the IP was reused. A terminating pod may still report an IP the
        // CNI has already handed on, so it does not count.
        let shared = self.pods.find(|p| {
            p.metadata.uid.as_deref() != Some(uid)
                && p.metadata.deletion_timestamp.is_none()
                && phase(p) == Some("Running")
                && pod_ip(p) == Some(ip)
        });
        if shared.is_some() {
            return Err(Refusal::PodNotReady);
        }
        // A worker without a nonce has no serving cert the edge would accept.
        let nonce = annotation(&pod, ANN_WORKER_NONCE)
            .filter(|n| is_worker_nonce(n))
            .ok_or(Refusal::PodNotReady)?;
        Ok(Target {
            ip,
            worker: WorkerIdentity {
                namespace: id.namespace.clone(),
                pool: id.pool.clone(),
                pod: pod.metadata.name.clone().ok_or(Refusal::PodNotFound)?,
                nonce: nonce.to_owned(),
            },
            owner: annotation(&pod, ANN_OWNER)
                .map_or_else(|| id.namespace.to_string(), str::to_owned),
        })
    }

    /// Check 3 beyond the uid, namely namespace, pool and managed-by labels, and a
    /// controller ownerReference to the cached pool's uid.
    fn belongs(&self, pod: &Pod, id: &AllocationId) -> bool {
        let ns = id.namespace.as_str();
        let pool = id.pool.as_str();
        let Some(pool_uid) = self
            .pools
            .get(&ObjectRef::new(pool).within(ns))
            .and_then(|p| p.metadata.uid.clone())
        else {
            return false;
        };
        pod.metadata.namespace.as_deref() == Some(ns)
            && label(pod, LABEL_POOL) == Some(pool)
            && label(pod, LABEL_MANAGED_BY) == Some(CONTROLLER)
            && pod
                .metadata
                .owner_references
                .iter()
                .flatten()
                .any(|o| o.controller == Some(true) && o.uid == pool_uid)
    }
}

/// Returns why a live connection to `alloc` on this pod must close.
/// `None` (the pod is gone from the store) means deleted.
#[must_use]
pub fn revocation(pod: Option<&Pod>, alloc: &AllocId) -> Option<Revocation> {
    let Some(pod) = pod else {
        return Some(Revocation::PodGone);
    };
    if pod.metadata.deletion_timestamp.is_some() {
        Some(Revocation::PodGone)
    } else if annotation(pod, ANN_ALLOC) != Some(alloc.as_str()) {
        Some(Revocation::Released)
    } else {
        None
    }
}

fn annotation<'a>(pod: &'a Pod, key: &str) -> Option<&'a str> {
    pod.metadata
        .annotations
        .as_ref()?
        .get(key)
        .map(String::as_str)
}

fn label<'a>(pod: &'a Pod, key: &str) -> Option<&'a str> {
    pod.metadata.labels.as_ref()?.get(key).map(String::as_str)
}

fn phase(pod: &Pod) -> Option<&str> {
    pod.status.as_ref()?.phase.as_deref()
}

fn pod_ip(pod: &Pod) -> Option<IpAddr> {
    pod.status.as_ref()?.pod_ip.as_deref()?.parse().ok()
}
