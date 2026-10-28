//! The Kubernetes writes of allocation, behind a trait so the engine is
//! testable without an API server. Every call is bounded by `kube.requestTimeout`.

use std::time::Duration;

use buildkit_controller_api::v1alpha1::BuildKitPool;
use futures::future::BoxFuture;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams, Preconditions};
use kube::{Api, Client};
use serde_json::{Value, json};

use crate::reconcile::spawn::{PodCreated, PodOutcome, WorkerSecrets, bounded};

/// Outcome of a JSON patch (claim or retire).
#[derive(Debug, Clone)]
pub enum PatchOutcome {
    /// HTTP 200, the patched pod.
    Applied(Box<Pod>),
    /// 422 (a `test` failed), 409, 404.
    Lost,
    /// Timeout or 5xx, so the outcome is unknown.
    Unknown,
    /// Any other error.
    Failed(String),
}

/// Outcome of a cold-path create.
#[derive(Debug, Clone)]
pub enum CreateOutcome {
    /// HTTP 201, the created pod.
    Created(Box<Pod>),
    /// 409 `AlreadyExists`.
    Exists,
    /// 403 `exceeded quota`.
    Quota,
    /// Timeout or 5xx.
    Unknown,
    /// Admission, invalid, other.
    Failed(String),
}

/// The writes allocation makes.
pub trait Backend: WorkerSecrets + Send + Sync + 'static {
    /// `application/json-patch+json` on a pod.
    fn patch_pod(&self, ns: &str, name: &str, patch: Value) -> BoxFuture<'static, PatchOutcome>;
    /// `POST` a pod.
    fn create_pod(&self, ns: &str, pod: Pod) -> BoxFuture<'static, CreateOutcome>;
    /// `DELETE` with a uid precondition; 404/409 count as done (`Ok`).
    fn delete_pod(
        &self,
        ns: &str,
        name: &str,
        uid: &str,
        grace: Option<u32>,
    ) -> BoxFuture<'static, Result<(), String>>;
    /// Server-side apply of this replica's `status.waiters` entry.
    fn apply_waiters(
        &self,
        ns: &str,
        pool: &str,
        replica: &str,
        count: i32,
        updated_at: &str,
    ) -> BoxFuture<'static, Result<(), String>>;
    /// `GET` a PVC; `Ok(None)` on 404.
    fn get_pvc(
        &self,
        ns: &str,
        name: &str,
    ) -> BoxFuture<'static, Result<Option<PersistentVolumeClaim>, String>>;
    /// `POST` a PVC; 409 counts as created.
    fn create_pvc(&self, ns: &str, pvc: PersistentVolumeClaim) -> BoxFuture<'static, PvcCreate>;
    /// `DELETE` a PVC with a uid precondition; 404/409 count as done.
    fn delete_pvc(&self, ns: &str, name: &str, uid: &str)
    -> BoxFuture<'static, Result<(), String>>;
    /// JSON merge patch setting one annotation on a PVC.
    fn annotate_pvc(
        &self,
        ns: &str,
        name: &str,
        key: &str,
        value: &str,
    ) -> BoxFuture<'static, Result<(), String>>;
}

/// Outcome of a cold-path volume create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PvcCreate {
    /// HTTP 201 or 409.
    Created,
    /// 403 `exceeded quota`.
    Quota,
    /// Anything else.
    Failed(String),
}

/// [`Backend`] on a real API server.
pub struct KubeBackend {
    client: Client,
    timeout: Duration,
}

impl KubeBackend {
    /// `timeout` = `kube.requestTimeout`.
    #[must_use]
    pub fn new(client: Client, timeout: Duration) -> Self {
        Self { client, timeout }
    }
}

enum Class {
    Lost,
    Exists,
    Quota,
    Unknown,
    Failed(String),
}

fn classify(r: Result<kube::Error, tokio::time::error::Elapsed>) -> Class {
    match r {
        Err(_) => Class::Unknown,
        Ok(kube::Error::Api(s)) => match s.code {
            409 if s.reason == "AlreadyExists" => Class::Exists,
            422 | 409 | 404 => Class::Lost,
            403 if s.message.contains("exceeded quota") => Class::Quota,
            c if c >= 500 => Class::Unknown,
            c => Class::Failed(format!("http {c} {}: {}", s.reason, s.message)),
        },
        // Transport failures after the request may have been sent.
        Ok(e) => Class::Unknown.or_failed(e),
    }
}

impl Class {
    fn or_failed(self, e: kube::Error) -> Self {
        match e {
            kube::Error::HyperError(_) | kube::Error::Service(_) => self,
            other => Self::Failed(other.to_string()),
        }
    }
}

impl Backend for KubeBackend {
    fn patch_pod(&self, ns: &str, name: &str, patch: Value) -> BoxFuture<'static, PatchOutcome> {
        let (api, name, t) = (self.pods(ns), name.to_owned(), self.timeout);
        Box::pin(async move {
            let patch: json_patch::Patch = match serde_json::from_value(patch) {
                Ok(p) => p,
                Err(e) => return PatchOutcome::Failed(e.to_string()),
            };
            let (pp, patch) = (PatchParams::default(), Patch::Json::<()>(patch));
            match tokio::time::timeout(t, api.patch(&name, &pp, &patch)).await {
                Ok(Ok(pod)) => PatchOutcome::Applied(Box::new(pod)),
                Ok(Err(e)) => from_class(classify(Ok(e))),
                Err(e) => from_class(classify(Err(e))),
            }
        })
    }

    fn create_pod(&self, ns: &str, pod: Pod) -> BoxFuture<'static, CreateOutcome> {
        let (api, t) = (self.pods(ns), self.timeout);
        Box::pin(async move {
            let r = tokio::time::timeout(t, api.create(&PostParams::default(), &pod)).await;
            let class = match r {
                Ok(Ok(pod)) => return CreateOutcome::Created(Box::new(pod)),
                Ok(Err(e)) => classify(Ok(e)),
                Err(e) => classify(Err(e)),
            };
            match class {
                Class::Exists => CreateOutcome::Exists,
                Class::Quota => CreateOutcome::Quota,
                Class::Unknown => CreateOutcome::Unknown,
                Class::Lost => CreateOutcome::Failed("conflict on create".into()),
                Class::Failed(m) => CreateOutcome::Failed(m),
            }
        })
    }

    fn delete_pod(
        &self,
        ns: &str,
        name: &str,
        uid: &str,
        grace: Option<u32>,
    ) -> BoxFuture<'static, Result<(), String>> {
        let (api, name, t) = (self.pods(ns), name.to_owned(), self.timeout);
        let dp = DeleteParams {
            preconditions: Some(Preconditions {
                uid: Some(uid.to_owned()),
                resource_version: None,
            }),
            grace_period_seconds: grace,
            ..DeleteParams::default()
        };
        Box::pin(async move {
            match tokio::time::timeout(t, api.delete(&name, &dp)).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(kube::Error::Api(s))) if s.code == 404 || s.code == 409 => Ok(()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("timeout".into()),
            }
        })
    }

    fn apply_waiters(
        &self,
        ns: &str,
        pool: &str,
        replica: &str,
        count: i32,
        updated_at: &str,
    ) -> BoxFuture<'static, Result<(), String>> {
        let api: Api<BuildKitPool> = Api::namespaced(self.client.clone(), ns);
        let (pool, t) = (pool.to_owned(), self.timeout);
        let pp = PatchParams::apply(&format!("buildkit-controller/{replica}")).force();
        let body = json!({
            "apiVersion": "buildkit.smrt-devops.net/v1alpha1",
            "kind": "BuildKitPool",
            "metadata": {"name": pool},
            "status": {"waiters": [{"replica": replica, "count": count, "updatedAt": updated_at}]},
        });
        Box::pin(async move {
            match tokio::time::timeout(t, api.patch_status(&pool, &pp, &Patch::Apply(body))).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("timeout".into()),
            }
        })
    }

    fn get_pvc(
        &self,
        ns: &str,
        name: &str,
    ) -> BoxFuture<'static, Result<Option<PersistentVolumeClaim>, String>> {
        let (api, name, t) = (self.pvcs(ns), name.to_owned(), self.timeout);
        Box::pin(async move {
            match tokio::time::timeout(t, api.get_opt(&name)).await {
                Ok(r) => r.map_err(|e| e.to_string()),
                Err(_) => Err("timeout".into()),
            }
        })
    }

    fn create_pvc(&self, ns: &str, pvc: PersistentVolumeClaim) -> BoxFuture<'static, PvcCreate> {
        let (api, t) = (self.pvcs(ns), self.timeout);
        Box::pin(async move {
            match tokio::time::timeout(t, api.create(&PostParams::default(), &pvc)).await {
                Ok(Ok(_)) => PvcCreate::Created,
                Ok(Err(kube::Error::Api(s))) if s.code == 409 => PvcCreate::Created,
                Ok(Err(kube::Error::Api(s)))
                    if s.code == 403 && s.message.contains("exceeded quota") =>
                {
                    PvcCreate::Quota
                }
                Ok(Err(e)) => PvcCreate::Failed(e.to_string()),
                Err(_) => PvcCreate::Failed("timeout".into()),
            }
        })
    }

    fn delete_pvc(
        &self,
        ns: &str,
        name: &str,
        uid: &str,
    ) -> BoxFuture<'static, Result<(), String>> {
        let (api, name, t) = (self.pvcs(ns), name.to_owned(), self.timeout);
        let dp = DeleteParams {
            preconditions: Some(Preconditions {
                uid: Some(uid.to_owned()),
                resource_version: None,
            }),
            ..DeleteParams::default()
        };
        Box::pin(async move {
            match tokio::time::timeout(t, api.delete(&name, &dp)).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(kube::Error::Api(s))) if s.code == 404 || s.code == 409 => Ok(()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("timeout".into()),
            }
        })
    }

    fn annotate_pvc(
        &self,
        ns: &str,
        name: &str,
        key: &str,
        value: &str,
    ) -> BoxFuture<'static, Result<(), String>> {
        let (api, name, t) = (self.pvcs(ns), name.to_owned(), self.timeout);
        let patch = json!({"metadata": {"annotations": {key: value}}});
        Box::pin(async move {
            let pp = PatchParams::default();
            match tokio::time::timeout(t, api.patch(&name, &pp, &Patch::Merge(patch))).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("timeout".into()),
            }
        })
    }
}

impl KubeBackend {
    fn pods(&self, ns: &str) -> Api<Pod> {
        Api::namespaced(self.client.clone(), ns)
    }

    fn pvcs(&self, ns: &str) -> Api<PersistentVolumeClaim> {
        Api::namespaced(self.client.clone(), ns)
    }
}

fn from_class(c: Class) -> PatchOutcome {
    match c {
        Class::Lost | Class::Exists => PatchOutcome::Lost,
        Class::Unknown => PatchOutcome::Unknown,
        Class::Quota => PatchOutcome::Failed("forbidden: exceeded quota".into()),
        Class::Failed(m) => PatchOutcome::Failed(m),
    }
}

impl PodOutcome for CreateOutcome {
    fn pod_created(&self) -> PodCreated {
        match self {
            Self::Created(p) => match (&p.metadata.name, &p.metadata.uid) {
                (Some(name), Some(uid)) => PodCreated::Yes {
                    name: name.clone(),
                    uid: uid.clone(),
                },
                _ => PodCreated::Unknown,
            },
            Self::Exists | Self::Quota | Self::Failed(_) => PodCreated::No,
            Self::Unknown => PodCreated::Unknown,
        }
    }
}

impl WorkerSecrets for KubeBackend {
    fn create_worker_secret(&self, secret: &Secret) -> BoxFuture<'static, Result<(), String>> {
        bounded(self.timeout, self.client.create_worker_secret(secret))
    }

    fn reown_worker_secret(
        &self,
        ns: &str,
        name: &str,
        owner: OwnerReference,
    ) -> BoxFuture<'static, Result<(), String>> {
        bounded(
            self.timeout,
            self.client.reown_worker_secret(ns, name, owner),
        )
    }

    fn delete_worker_secret(&self, ns: &str, name: &str) -> BoxFuture<'static, Result<(), String>> {
        bounded(self.timeout, self.client.delete_worker_secret(ns, name))
    }

    fn get_worker_pod(
        &self,
        ns: &str,
        name: &str,
    ) -> BoxFuture<'static, Result<Option<Pod>, String>> {
        let (f, t) = (self.client.get_worker_pod(ns, name), self.timeout);
        Box::pin(async move {
            tokio::time::timeout(t, f)
                .await
                .unwrap_or_else(|_| Err("timeout".to_owned()))
        })
    }
}
