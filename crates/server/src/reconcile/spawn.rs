//! Creating one worker, shared by the reconciler and the
//! API's cold path: issue the pod's serving cert into its own Secret, create
//! the pod, then hand the Secret to the pod.
//!
//! The Secret is created owned by the pool, so it never outlives the pool.
//! Once the pod exists its ownerReferences are replaced by the pod alone, and
//! the garbage collector removes it with the pod. A pod create that certainly
//! failed deletes the Secret by name. The API's cold path runs [`spawn`] in
//! its own task, so a cancelled request does not stop it between the steps.
//! A crash between the two steps, or a create whose outcome is unknown, still
//! leaves the Secret owned by the pool until the pool is deleted; the
//! controller never lists Secrets in pool namespaces.

use std::future::Future;

use buildkit_controller_api::v1alpha1::BuildKitPool;
use futures::future::BoxFuture;
use k8s_openapi::api::core::v1::{Pod, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams};
use kube::{Client, ResourceExt};
use serde_json::json;
use time::{Duration, OffsetDateTime};
use tracing::warn;

use crate::pki::{CaBundle, Namespace, PoolName, WorkerIdentity};
use crate::render::{self, CONTROLLER, Claim, PodInputs, WorkerTlsPem, worker_pod_name};

/// The writes [`spawn`] needs beyond the pod create: create, re-own and
/// delete one Secret by name. No reads, so RBAC grants no get/list/watch.
pub trait WorkerSecrets: Send + Sync {
    /// POST the per-pod Secret.
    fn create_worker_secret(&self, secret: &Secret) -> BoxFuture<'static, Result<(), String>>;
    /// Merge-patch `metadata.ownerReferences` to `[owner]` (no read first).
    fn reown_worker_secret(
        &self,
        ns: &str,
        name: &str,
        owner: OwnerReference,
    ) -> BoxFuture<'static, Result<(), String>>;
    /// DELETE the Secret by name; a 404 counts as done.
    fn delete_worker_secret(&self, ns: &str, name: &str) -> BoxFuture<'static, Result<(), String>>;
}

/// What a pod create did, as far as the Secret is concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PodCreated {
    /// The pod exists with this name and uid.
    Yes { name: String, uid: String },
    /// The API server refused it (409, quota, admission): delete the Secret.
    No,
    /// Timeout or 5xx: the pod may exist and mount the Secret, so keep it.
    Unknown,
}

/// Maps a caller's own pod-create result to [`PodCreated`].
pub trait PodOutcome {
    /// See [`PodCreated`].
    fn pod_created(&self) -> PodCreated;
}

/// A rendered worker: the pod, its Secret and the cert's notAfter.
#[derive(Debug)]
pub struct Worker {
    /// The pod, named `<pool>-<slot>`, mounting `secret`.
    pub pod: Pod,
    /// `<pod>-<nonce>`, owned by the pool.
    pub secret: Secret,
    /// The serving cert's notAfter.
    pub not_after: OffsetDateTime,
}

/// Picks a nonce, issues the serving cert for `<nonce>.<pod>.<pool>.<ns>` valid
/// for `validity`, and renders the pod and its Secret.
pub fn prepare(
    pool: &BuildKitPool,
    ca: &CaBundle,
    slot: u32,
    claim: Option<Claim>,
    now: OffsetDateTime,
    validity: Duration,
) -> Result<Worker, String> {
    let ns =
        Namespace::try_from(pool.namespace().unwrap_or_default()).map_err(|e| e.to_string())?;
    let name = PoolName::try_from(pool.name_any()).map_err(|e| e.to_string())?;
    let pod = worker_pod_name(name.as_str(), slot);
    let id = WorkerIdentity::generate(ns, name, pod).map_err(|_| "no randomness".to_owned())?;
    let cert = ca
        .issue_worker_serving_for(&id, now, validity)
        .map_err(|e| e.to_string())?;
    let pem = WorkerTlsPem::from_pki(&cert, ca.internal_ca()).map_err(|e| e.to_string())?;
    let inputs = PodInputs {
        slot,
        internal_ca_fingerprint: ca.internal_ca().fingerprint_hex(),
        worker_cert_not_after: cert
            .not_after()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| e.to_string())?,
        nonce: id.nonce,
        claim,
    };
    Ok(Worker {
        pod: render::worker_pod(pool, &inputs).map_err(|e| e.to_string())?,
        secret: render::worker_secret(pool, &inputs, &pem).map_err(|e| e.to_string())?,
        not_after: cert.not_after(),
    })
}

/// Creates the Secret, runs `create` on the pod, then re-owns or deletes the
/// Secret. `Err` means the Secret create failed and `create` never ran.
pub async fn spawn<S, F, Fut>(secrets: &S, worker: Worker, create: F) -> Result<Fut::Output, String>
where
    S: WorkerSecrets + ?Sized,
    F: FnOnce(Pod) -> Fut,
    Fut: Future,
    Fut::Output: PodOutcome,
{
    let Worker { pod, secret, .. } = worker;
    let ns = secret.namespace().unwrap_or_default();
    let name = secret.name_any();
    secrets
        .create_worker_secret(&secret)
        .await
        .map_err(|e| format!("create Secret {name}: {e}"))?;
    let out = create(pod).await;
    match out.pod_created() {
        PodCreated::Yes { name: pod, uid } => {
            if let Err(error) = secrets
                .reown_worker_secret(&ns, &name, pod_owner(&pod, &uid))
                .await
            {
                warn!(namespace = %ns, secret = %name, %error, "re-owning the worker Secret; it stays with the pool");
            }
        }
        PodCreated::No => {
            if let Err(error) = secrets.delete_worker_secret(&ns, &name).await {
                warn!(namespace = %ns, secret = %name, %error, "deleting the worker Secret; it stays with the pool");
            }
        }
        PodCreated::Unknown => {}
    }
    Ok(out)
}

/// ownerReference to the pod (`controller: true`).
fn pod_owner(name: &str, uid: &str) -> OwnerReference {
    OwnerReference {
        api_version: "v1".into(),
        kind: "Pod".into(),
        name: name.to_owned(),
        uid: uid.to_owned(),
        controller: Some(true),
        block_owner_deletion: None,
    }
}

/// `f` cut off after `timeout`.
#[must_use]
pub fn bounded(
    timeout: std::time::Duration,
    f: BoxFuture<'static, Result<(), String>>,
) -> BoxFuture<'static, Result<(), String>> {
    Box::pin(async move {
        tokio::time::timeout(timeout, f)
            .await
            .unwrap_or_else(|_| Err(format!("no answer within {timeout:?}")))
    })
}

fn secrets(client: &Client, ns: &str) -> Api<Secret> {
    Api::namespaced(client.clone(), ns)
}

impl WorkerSecrets for Client {
    fn create_worker_secret(&self, secret: &Secret) -> BoxFuture<'static, Result<(), String>> {
        let api = secrets(self, &secret.namespace().unwrap_or_default());
        let secret = secret.clone();
        Box::pin(async move {
            let pp = PostParams {
                field_manager: Some(CONTROLLER.into()),
                ..PostParams::default()
            };
            api.create(&pp, &secret)
                .await
                .map(drop)
                .map_err(|e| e.to_string())
        })
    }

    fn reown_worker_secret(
        &self,
        ns: &str,
        name: &str,
        owner: OwnerReference,
    ) -> BoxFuture<'static, Result<(), String>> {
        let (api, name) = (secrets(self, ns), name.to_owned());
        Box::pin(async move {
            // A JSON merge patch replaces the whole list, so no read is needed.
            let patch = json!({"metadata": {"ownerReferences": [owner]}});
            api.patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
                .await
                .map(drop)
                .map_err(|e| e.to_string())
        })
    }

    fn delete_worker_secret(&self, ns: &str, name: &str) -> BoxFuture<'static, Result<(), String>> {
        let (api, name) = (secrets(self, ns), name.to_owned());
        Box::pin(async move {
            match api.delete(&name, &DeleteParams::default()).await {
                Ok(_) => Ok(()),
                Err(kube::Error::Api(s)) if s.code == 404 => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        })
    }
}
