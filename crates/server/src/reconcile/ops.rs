//! The Kubernetes calls the executor makes, behind [`KubeOps`] so the
//! executor can be tested against a fake. [`KubeClient`] is the real one.
//! No call is retried here, and [`Timed`] bounds each one.

use std::fmt::Debug;
use std::future::Future;

use buildkit_controller_api::v1alpha1::{BuildKitPool, BuildKitPoolStatus};
use k8s_openapi::NamespaceResourceScope;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Secret};
use k8s_openapi::api::events::v1::Event;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams, Preconditions};
use kube::{Client, Resource};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use futures::future::BoxFuture;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;

use super::plan::WaiterRef;
use super::spawn::{WorkerSecrets, bounded};
use crate::render::{ANN_ALLOC, CONTROLLER, LABEL_STATE};

/// Field manager of every server-side apply.
pub const FIELD_MANAGER: &str = "buildkit-controller";

/// A failed call, classified by what the executor does about it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OpError {
    /// HTTP 409, the name is taken or a precondition (uid, resourceVersion) failed.
    #[error("conflict: {0}")]
    Conflict(String),
    /// HTTP 422, a JSON-patch `test` failed.
    #[error("invalid: {0}")]
    Invalid(String),
    /// 404.
    #[error("not found")]
    NotFound,
    /// Any other 4xx (403 quota or admission, 400): the server refused the
    /// call and changed nothing.
    #[error("{message} ({code})")]
    Refused {
        /// HTTP status.
        code: u16,
        /// Server message.
        message: String,
    },
    /// 5xx, timeout or transport: the outcome is unknown.
    #[error("{0}")]
    Other(String),
}

impl From<kube::Error> for OpError {
    fn from(e: kube::Error) -> Self {
        match e {
            kube::Error::Api(s) => match s.code {
                409 => Self::Conflict(s.message),
                422 => Self::Invalid(s.message),
                404 => Self::NotFound,
                code @ 400..=499 => Self::Refused {
                    code,
                    message: s.message,
                },
                _ => Self::Other(format!("{} ({})", s.message, s.code)),
            },
            other => Self::Other(other.to_string()),
        }
    }
}

/// A namespaced object the executor applies.
pub trait Applied:
    Resource<DynamicType = (), Scope = NamespaceResourceScope>
    + Serialize
    + DeserializeOwned
    + Clone
    + Debug
    + Send
    + Sync
    + 'static
{
}

impl<K> Applied for K where
    K: Resource<DynamicType = (), Scope = NamespaceResourceScope>
        + Serialize
        + DeserializeOwned
        + Clone
        + Debug
        + Send
        + Sync
        + 'static
{
}

/// Result of a call.
pub type OpResult<T = ()> = Result<T, OpError>;

/// Every Kubernetes write of a pass.
pub trait KubeOps: WorkerSecrets + Send + Sync {
    /// Server-side apply (`force`) of `obj` in its namespace.
    fn apply<K: Applied>(&self, obj: &K) -> impl Future<Output = OpResult> + Send;
    /// POST a pod; returns the created pod.
    fn create_pod(&self, ns: &str, pod: &Pod) -> impl Future<Output = OpResult<Pod>> + Send;
    /// `[test uid, test P alloc == expected, replace P alloc/state draining]`;
    /// returns the patched pod's resourceVersion.
    fn retire(
        &self,
        ns: &str,
        pod: &str,
        uid: &str,
        expected: &str,
    ) -> impl Future<Output = OpResult<Option<String>>> + Send;
    /// DELETE a pod with a uid (and optionally resourceVersion) precondition.
    fn delete_pod(
        &self,
        ns: &str,
        pod: &str,
        uid: &str,
        resource_version: Option<String>,
        grace_seconds: Option<u32>,
    ) -> impl Future<Output = OpResult> + Send;
    /// Server-side apply of the pool status subresource.
    fn patch_status(
        &self,
        ns: &str,
        pool: &str,
        status: &BuildKitPoolStatus,
    ) -> impl Future<Output = OpResult> + Send;
    /// `[test replica, test updatedAt, remove]` on one `status.waiters` entry.
    fn prune_waiter(
        &self,
        ns: &str,
        pool: &str,
        entry: &WaiterRef,
    ) -> impl Future<Output = OpResult> + Send;
    /// POST a Secret (CA create, CA backup).
    fn create_secret(&self, secret: &Secret) -> impl Future<Output = OpResult> + Send;
    /// PUT a Secret; `metadata.resourceVersion` is the precondition.
    fn replace_secret(&self, secret: &Secret) -> impl Future<Output = OpResult> + Send;
    /// POST an `events.k8s.io/v1` Event.
    fn create_event(&self, event: &Event) -> impl Future<Output = OpResult> + Send;
    /// GET a PVC; `None` on 404.
    fn get_pvc(
        &self,
        ns: &str,
        name: &str,
    ) -> impl Future<Output = OpResult<Option<PersistentVolumeClaim>>> + Send;
    /// POST a PVC.
    fn create_pvc(&self, pvc: &PersistentVolumeClaim) -> impl Future<Output = OpResult> + Send;
    /// DELETE a PVC with a uid precondition.
    fn delete_pvc(&self, ns: &str, name: &str, uid: &str) -> impl Future<Output = OpResult> + Send;
    /// LIST the PVCs in `ns` that match `selector`.
    fn list_pvcs(
        &self,
        ns: &str,
        selector: &str,
    ) -> impl Future<Output = OpResult<Vec<PersistentVolumeClaim>>> + Send;
}

/// [`KubeOps`] over a `kube::Client`.
#[derive(Clone)]
pub struct KubeClient(pub Client);

impl KubeClient {
    fn api<K: Applied>(&self, ns: &str) -> Api<K> {
        Api::namespaced(self.0.clone(), ns)
    }
}

fn ns_of<K: Resource>(obj: &K) -> String {
    obj.meta().namespace.clone().unwrap_or_default()
}

fn name_of<K: Resource>(obj: &K) -> String {
    obj.meta().name.clone().unwrap_or_default()
}

/// JSON-pointer escape of one path segment.
fn escape(seg: &str) -> String {
    seg.replace('~', "~0").replace('/', "~1")
}

/// The retire patch.
#[must_use]
pub fn retire_patch(uid: &str, expected: &str) -> serde_json::Value {
    json!([
        {"op": "test", "path": "/metadata/uid", "value": uid},
        {"op": "test", "path": format!("/metadata/annotations/{}", escape(ANN_ALLOC)), "value": expected},
        {"op": "replace", "path": format!("/metadata/annotations/{}", escape(ANN_ALLOC)), "value": "draining"},
        {"op": "replace", "path": format!("/metadata/labels/{}", escape(LABEL_STATE)), "value": "draining"},
    ])
}

/// The waiter prune patch.
#[must_use]
pub fn prune_patch(entry: &WaiterRef) -> serde_json::Value {
    let base = format!("/status/waiters/{}", entry.index);
    json!([
        {"op": "test", "path": format!("{base}/replica"), "value": entry.replica},
        {"op": "test", "path": format!("{base}/updatedAt"), "value": entry.updated_at},
        {"op": "remove", "path": base},
    ])
}

fn json_patch(v: serde_json::Value) -> OpResult<json_patch::Patch> {
    serde_json::from_value(v).map_err(|e| OpError::Other(e.to_string()))
}

impl KubeOps for KubeClient {
    async fn apply<K: Applied>(&self, obj: &K) -> OpResult {
        let pp = PatchParams::apply(FIELD_MANAGER).force();
        self.api::<K>(&ns_of(obj))
            .patch(&name_of(obj), &pp, &Patch::Apply(obj))
            .await?;
        Ok(())
    }

    async fn create_pod(&self, ns: &str, pod: &Pod) -> OpResult<Pod> {
        let pp = PostParams {
            field_manager: Some(FIELD_MANAGER.into()),
            ..PostParams::default()
        };
        Ok(self.api::<Pod>(ns).create(&pp, pod).await?)
    }

    async fn retire(
        &self,
        ns: &str,
        pod: &str,
        uid: &str,
        expected: &str,
    ) -> OpResult<Option<String>> {
        let patch = json_patch(retire_patch(uid, expected))?;
        let pp = PatchParams {
            field_manager: Some(FIELD_MANAGER.into()),
            ..PatchParams::default()
        };
        let p = self
            .api::<Pod>(ns)
            .patch(pod, &pp, &Patch::<()>::Json(patch))
            .await?;
        Ok(p.metadata.resource_version)
    }

    async fn delete_pod(
        &self,
        ns: &str,
        pod: &str,
        uid: &str,
        resource_version: Option<String>,
        grace_seconds: Option<u32>,
    ) -> OpResult {
        let dp = DeleteParams {
            grace_period_seconds: grace_seconds,
            preconditions: Some(Preconditions {
                uid: Some(uid.to_owned()),
                resource_version,
            }),
            ..DeleteParams::default()
        };
        self.api::<Pod>(ns).delete(pod, &dp).await?;
        Ok(())
    }

    async fn patch_status(&self, ns: &str, pool: &str, status: &BuildKitPoolStatus) -> OpResult {
        let mut status = serde_json::to_value(status).map_err(|e| OpError::Other(e.to_string()))?;
        // `waiters` belongs to the API replicas; never take ownership of it.
        if let Some(m) = status.as_object_mut() {
            m.remove("waiters");
        }
        let body = json!({
            "apiVersion": BuildKitPool::api_version(&()),
            "kind": BuildKitPool::kind(&()),
            "status": status,
        });
        let pp = PatchParams::apply(FIELD_MANAGER).force();
        Api::<BuildKitPool>::namespaced(self.0.clone(), ns)
            .patch_status(pool, &pp, &Patch::Apply(body))
            .await?;
        Ok(())
    }

    async fn prune_waiter(&self, ns: &str, pool: &str, entry: &WaiterRef) -> OpResult {
        let patch = json_patch(prune_patch(entry))?;
        Api::<BuildKitPool>::namespaced(self.0.clone(), ns)
            .patch_status(pool, &PatchParams::default(), &Patch::<()>::Json(patch))
            .await?;
        Ok(())
    }

    async fn create_secret(&self, secret: &Secret) -> OpResult {
        let pp = PostParams {
            field_manager: Some(CONTROLLER.into()),
            ..PostParams::default()
        };
        self.api::<Secret>(&ns_of(secret))
            .create(&pp, secret)
            .await?;
        Ok(())
    }

    async fn replace_secret(&self, secret: &Secret) -> OpResult {
        let pp = PostParams {
            field_manager: Some(CONTROLLER.into()),
            ..PostParams::default()
        };
        self.api::<Secret>(&ns_of(secret))
            .replace(&name_of(secret), &pp, secret)
            .await?;
        Ok(())
    }

    async fn create_event(&self, event: &Event) -> OpResult {
        self.api::<Event>(&ns_of(event))
            .create(&PostParams::default(), event)
            .await?;
        Ok(())
    }

    async fn get_pvc(&self, ns: &str, name: &str) -> OpResult<Option<PersistentVolumeClaim>> {
        Ok(self.api::<PersistentVolumeClaim>(ns).get_opt(name).await?)
    }

    async fn create_pvc(&self, pvc: &PersistentVolumeClaim) -> OpResult {
        let pp = PostParams {
            field_manager: Some(FIELD_MANAGER.into()),
            ..PostParams::default()
        };
        self.api::<PersistentVolumeClaim>(&ns_of(pvc))
            .create(&pp, pvc)
            .await?;
        Ok(())
    }

    async fn delete_pvc(&self, ns: &str, name: &str, uid: &str) -> OpResult {
        let dp = DeleteParams {
            preconditions: Some(Preconditions {
                uid: Some(uid.to_owned()),
                resource_version: None,
            }),
            ..DeleteParams::default()
        };
        self.api::<PersistentVolumeClaim>(ns)
            .delete(name, &dp)
            .await?;
        Ok(())
    }

    async fn list_pvcs(&self, ns: &str, selector: &str) -> OpResult<Vec<PersistentVolumeClaim>> {
        let lp = ListParams::default().labels(selector);
        Ok(self.api::<PersistentVolumeClaim>(ns).list(&lp).await?.items)
    }
}

/// [`KubeOps`] with every call cut off after a fixed time.
///
/// The kube client sets no read timeout and its retry layer may resend a
/// throttled call for minutes. The leader's fence margin assumes that no call
/// outlives `kube.requestTimeout`, so each call, retries included, is dropped
/// at `timeout` and reported as [`OpError::Other`].
#[derive(Clone)]
pub struct Timed<O> {
    inner: O,
    timeout: std::time::Duration,
}

impl<O: KubeOps> Timed<O> {
    /// Bounds every call of `inner` by `timeout`.
    pub fn new(inner: O, timeout: std::time::Duration) -> Self {
        Self { inner, timeout }
    }

    /// The wrapped ops.
    pub fn inner(&self) -> &O {
        &self.inner
    }

    async fn run<T>(&self, call: &str, f: impl Future<Output = OpResult<T>>) -> OpResult<T> {
        tokio::time::timeout(self.timeout, f)
            .await
            .unwrap_or_else(|_| {
                Err(OpError::Other(format!(
                    "{call}: no answer within {:?}",
                    self.timeout
                )))
            })
    }
}

impl<O: KubeOps> KubeOps for Timed<O> {
    async fn apply<K: Applied>(&self, obj: &K) -> OpResult {
        self.run("apply", self.inner.apply(obj)).await
    }

    async fn create_pod(&self, ns: &str, pod: &Pod) -> OpResult<Pod> {
        self.run("create pod", self.inner.create_pod(ns, pod)).await
    }

    async fn retire(
        &self,
        ns: &str,
        pod: &str,
        uid: &str,
        expected: &str,
    ) -> OpResult<Option<String>> {
        self.run("retire", self.inner.retire(ns, pod, uid, expected))
            .await
    }

    async fn delete_pod(
        &self,
        ns: &str,
        pod: &str,
        uid: &str,
        resource_version: Option<String>,
        grace_seconds: Option<u32>,
    ) -> OpResult {
        self.run(
            "delete pod",
            self.inner
                .delete_pod(ns, pod, uid, resource_version, grace_seconds),
        )
        .await
    }

    async fn patch_status(&self, ns: &str, pool: &str, status: &BuildKitPoolStatus) -> OpResult {
        self.run("patch status", self.inner.patch_status(ns, pool, status))
            .await
    }

    async fn prune_waiter(&self, ns: &str, pool: &str, entry: &WaiterRef) -> OpResult {
        self.run("prune waiter", self.inner.prune_waiter(ns, pool, entry))
            .await
    }

    async fn create_secret(&self, secret: &Secret) -> OpResult {
        self.run("create secret", self.inner.create_secret(secret))
            .await
    }

    async fn replace_secret(&self, secret: &Secret) -> OpResult {
        self.run("replace secret", self.inner.replace_secret(secret))
            .await
    }

    async fn create_event(&self, event: &Event) -> OpResult {
        self.run("create event", self.inner.create_event(event))
            .await
    }

    async fn get_pvc(&self, ns: &str, name: &str) -> OpResult<Option<PersistentVolumeClaim>> {
        self.run("get pvc", self.inner.get_pvc(ns, name)).await
    }

    async fn create_pvc(&self, pvc: &PersistentVolumeClaim) -> OpResult {
        self.run("create pvc", self.inner.create_pvc(pvc)).await
    }

    async fn delete_pvc(&self, ns: &str, name: &str, uid: &str) -> OpResult {
        self.run("delete pvc", self.inner.delete_pvc(ns, name, uid))
            .await
    }

    async fn list_pvcs(&self, ns: &str, selector: &str) -> OpResult<Vec<PersistentVolumeClaim>> {
        self.run("list pvcs", self.inner.list_pvcs(ns, selector))
            .await
    }
}

impl WorkerSecrets for KubeClient {
    fn create_worker_secret(&self, secret: &Secret) -> BoxFuture<'static, Result<(), String>> {
        self.0.create_worker_secret(secret)
    }

    fn reown_worker_secret(
        &self,
        ns: &str,
        name: &str,
        owner: OwnerReference,
    ) -> BoxFuture<'static, Result<(), String>> {
        self.0.reown_worker_secret(ns, name, owner)
    }

    fn delete_worker_secret(&self, ns: &str, name: &str) -> BoxFuture<'static, Result<(), String>> {
        self.0.delete_worker_secret(ns, name)
    }
}

impl<O: KubeOps> WorkerSecrets for Timed<O> {
    fn create_worker_secret(&self, secret: &Secret) -> BoxFuture<'static, Result<(), String>> {
        bounded(self.timeout, self.inner.create_worker_secret(secret))
    }

    fn reown_worker_secret(
        &self,
        ns: &str,
        name: &str,
        owner: OwnerReference,
    ) -> BoxFuture<'static, Result<(), String>> {
        bounded(
            self.timeout,
            self.inner.reown_worker_secret(ns, name, owner),
        )
    }

    fn delete_worker_secret(&self, ns: &str, name: &str) -> BoxFuture<'static, Result<(), String>> {
        bounded(self.timeout, self.inner.delete_worker_secret(ns, name))
    }
}
