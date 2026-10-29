//! Creating one worker for the reconciler:
//! issue the pod's serving cert, create the pod, then create the pod's Secret
//! owned by the pod.
//!
//! The Secret is born owned by the pod, so the garbage collector removes it
//! with the pod and no other owner ever holds a worker's private key. Until
//! the Secret exists the kubelet cannot mount it, so the pod does not start.
//! A pod whose Secret never arrives (the create failed, the creator crashed)
//! stays unready and is retired at `workers.startTimeout`, taking nothing with
//! it. A pod create whose outcome is unknown is confirmed with a read of the
//! pod before any Secret is created.

use std::future::Future;

use buildkit_controller_api::v1alpha1::BuildKitPool;
use futures::future::BoxFuture;
use k8s_openapi::api::core::v1::{Pod, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, PostParams};
use kube::{Client, ResourceExt};
use time::{Duration, OffsetDateTime};

use crate::pki::{CaBundle, Namespace, PoolName, WorkerIdentity};
use crate::render::{self, ANN_WORKER_NONCE, CONTROLLER, PodInputs, WorkerTlsPem, worker_pod_name};

/// The calls [`spawn`] needs beyond the pod create: create the pod's Secret
/// and read the pod back after a create whose outcome is unknown. RBAC grants
/// no read of Secrets.
pub trait WorkerSecrets: Send + Sync {
    /// POST the per-pod Secret.
    fn create_worker_secret(&self, secret: &Secret) -> BoxFuture<'static, Result<(), String>>;
    /// GET a pod; `Ok(None)` on 404.
    fn get_worker_pod(
        &self,
        ns: &str,
        name: &str,
    ) -> BoxFuture<'static, Result<Option<Pod>, String>>;
}

/// What a pod create did, as far as the Secret is concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PodCreated {
    /// The pod exists with this name and uid.
    Yes { name: String, uid: String },
    /// The API server refused it (409, quota, admission), or the read back
    /// found no pod of this create.
    No,
    /// Timeout or 5xx: the pod may exist.
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
    /// `<pod>-<nonce>`; [`spawn`] makes the pod its owner.
    pub secret: Secret,
    /// The earlier notAfter of its serving and registry client certs.
    pub not_after: OffsetDateTime,
}

/// Picks a nonce, issues the serving cert (internal CA) and the registry client
/// cert (client CA, its own key) for `<nonce>.<pod>.<pool>.<ns>` valid for
/// `validity`, and renders the pod and its Secret, whose `buildkitd.toml`
/// names the cache registry at `registry_host`.
pub fn prepare(
    pool: &BuildKitPool,
    ca: &CaBundle,
    slot: u32,
    now: OffsetDateTime,
    validity: Duration,
    registry_host: Option<&str>,
) -> Result<Worker, String> {
    let ns =
        Namespace::try_from(pool.namespace().unwrap_or_default()).map_err(|e| e.to_string())?;
    let name = PoolName::try_from(pool.name_any()).map_err(|e| e.to_string())?;
    let pod = worker_pod_name(name.as_str(), slot);
    let id = WorkerIdentity::generate(ns, name, pod).map_err(|_| "no randomness".to_owned())?;
    let cert = ca
        .issue_worker_serving_for(&id, now, validity)
        .map_err(|e| e.to_string())?;
    let client = ca
        .issue_worker_client_for(&id, now, validity)
        .map_err(|e| e.to_string())?;
    let pem =
        WorkerTlsPem::from_pki(&cert, &client, ca.internal_ca()).map_err(|e| e.to_string())?;
    // Either CA can end first; the pod is good until the earlier leaf ends.
    let not_after = cert.not_after().min(client.not_after());
    let inputs = PodInputs {
        slot,
        internal_ca_fingerprint: ca.internal_ca().fingerprint_hex(),
        worker_cert_not_after: not_after
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| e.to_string())?,
        nonce: id.nonce,
    };
    Ok(Worker {
        pod: render::worker_pod(pool, &inputs).map_err(|e| e.to_string())?,
        secret: render::worker_secret(pool, &inputs, &pem, registry_host)
            .map_err(|e| e.to_string())?,
        not_after,
    })
}

/// What [`spawn`] did.
#[derive(Debug)]
pub struct Spawned<T> {
    /// The pod create's own result.
    pub out: T,
    /// The pod as resolved: an unknown create outcome is confirmed by a read
    /// of the pod (and its nonce), so `Unknown` here means the read failed too.
    pub pod: PodCreated,
    /// `Err` when the pod exists but its Secret was not created. That pod
    /// never starts and is retired at `workers.startTimeout`.
    pub secret: Result<(), String>,
}

/// Creates the pod with `create`, then, once the pod is known to exist, its
/// Secret owned by the pod. No Secret is created for a pod that was refused
/// or whose existence cannot be confirmed.
pub async fn spawn<S, F, Fut>(secrets: &S, worker: Worker, create: F) -> Spawned<Fut::Output>
where
    S: WorkerSecrets + ?Sized,
    F: FnOnce(Pod) -> Fut,
    Fut: Future,
    Fut::Output: PodOutcome,
{
    let Worker {
        pod, mut secret, ..
    } = worker;
    let ns = secret.namespace().unwrap_or_default();
    let name = pod.name_any();
    let nonce = pod.annotations().get(ANN_WORKER_NONCE).cloned();
    let out = create(pod).await;
    let pod = match out.pod_created() {
        PodCreated::Unknown => confirm(secrets, &ns, &name, nonce.as_deref()).await,
        known => known,
    };
    let secret = match &pod {
        PodCreated::Yes { name, uid } => {
            secret.metadata.owner_references = Some(vec![pod_owner(name, uid)]);
            secrets
                .create_worker_secret(&secret)
                .await
                .map_err(|e| format!("create Secret {}: {e}", secret.name_any()))
        }
        PodCreated::No | PodCreated::Unknown => Ok(()),
    };
    Spawned { out, pod, secret }
}

/// Reads the pod back after a create whose answer was lost. It is ours when
/// it carries the nonce this create chose.
async fn confirm<S: WorkerSecrets + ?Sized>(
    secrets: &S,
    ns: &str,
    name: &str,
    nonce: Option<&str>,
) -> PodCreated {
    match secrets.get_worker_pod(ns, name).await {
        Ok(Some(p)) if p.annotations().get(ANN_WORKER_NONCE).map(String::as_str) == nonce => {
            match p.metadata.uid {
                Some(uid) => PodCreated::Yes {
                    name: name.to_owned(),
                    uid,
                },
                None => PodCreated::Unknown,
            }
        }
        Ok(_) => PodCreated::No,
        Err(error) => {
            tracing::warn!(namespace = %ns, pod = %name, %error, "reading back a pod whose create outcome is unknown");
            PodCreated::Unknown
        }
    }
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

    fn get_worker_pod(
        &self,
        ns: &str,
        name: &str,
    ) -> BoxFuture<'static, Result<Option<Pod>, String>> {
        let (api, name) = (Api::<Pod>::namespaced(self.clone(), ns), name.to_owned());
        Box::pin(async move { api.get_opt(&name).await.map_err(|e| e.to_string()) })
    }
}
