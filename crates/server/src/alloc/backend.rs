//! The Kubernetes calls of allocation, behind a trait so the front door and
//! the binder are testable without an API server. Every call is bounded by
//! `kube.requestTimeout`.

use std::time::Duration;

use buildkit_controller_api::claim::{BuildKitClaim, BuildKitClaimStatus};
use futures::future::BoxFuture;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Secret};
use kube::api::{DeleteParams, Patch, PatchParams, PostParams, Preconditions};
use kube::{Api, Client};
use serde_json::{Value, json};

/// Outcome of a JSON patch (claim or retire).
#[derive(Debug, Clone)]
pub enum PatchOutcome {
    /// HTTP 200, the patched pod.
    Applied(Box<Pod>),
    /// 422 (a `test` failed), 409, 404.
    Lost,
    /// Timeout, 5xx, 408 or 429, so the outcome is unknown.
    Unknown,
    /// Any other error.
    Failed(String),
}

/// Outcome of a create, delete or status write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write {
    /// 2xx.
    Done,
    /// 409 `AlreadyExists` on a create.
    Exists,
    /// 404, or a 409 from a failed uid precondition: the object is gone or replaced.
    Gone,
    /// Timeout, transport error, 5xx, 408 or 429: the write may or may not have landed.
    Unknown,
    /// Refused for good (another 4xx), with the server's reason.
    Refused(String),
}

/// The calls allocation makes.
pub trait Backend: Send + Sync + 'static {
    /// `application/json-patch+json` on a pod.
    fn patch_pod(&self, ns: &str, name: &str, patch: Value) -> BoxFuture<'static, PatchOutcome>;
    /// `DELETE` with a uid precondition; 404/409 count as done (`Ok`).
    fn delete_pod(
        &self,
        ns: &str,
        name: &str,
        uid: &str,
        grace: Option<u32>,
    ) -> BoxFuture<'static, Result<(), String>>;
    /// JSON merge patch setting one annotation on a PVC.
    fn annotate_pvc(
        &self,
        ns: &str,
        name: &str,
        key: &str,
        value: &str,
    ) -> BoxFuture<'static, Result<(), String>>;
    /// `POST` a claim.
    fn create_claim(&self, claim: BuildKitClaim) -> BoxFuture<'static, Write>;
    /// `GET` a claim; `Ok(None)` on 404.
    fn get_claim(
        &self,
        ns: &str,
        name: &str,
    ) -> BoxFuture<'static, Result<Option<BuildKitClaim>, Write>>;
    /// `DELETE` a claim with a uid precondition.
    fn delete_claim(&self, ns: &str, name: &str, uid: &str) -> BoxFuture<'static, Write>;
    /// Merge-patch a claim's status, if the claim is still at `resource_version`
    /// (a 409 otherwise, which is [`Write::Gone`]).
    fn patch_claim_status(
        &self,
        ns: &str,
        name: &str,
        resource_version: &str,
        status: &BuildKitClaimStatus,
    ) -> BoxFuture<'static, Write>;
    /// `GET` a Secret; `Ok(None)` on 404.
    fn get_secret(&self, ns: &str, name: &str)
    -> BoxFuture<'static, Result<Option<Secret>, Write>>;
    /// `POST` a Secret.
    fn create_secret(&self, secret: Secret) -> BoxFuture<'static, Write>;
    /// `DELETE` a Secret with a uid precondition.
    fn delete_secret(&self, ns: &str, name: &str, uid: &str) -> BoxFuture<'static, Write>;
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

    fn api<K>(&self, ns: &str) -> Api<K>
    where
        K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope>,
        K::DynamicType: Default,
    {
        Api::namespaced(self.client.clone(), ns)
    }
}

/// Sorts a failed call. Throttling (429) and request timeouts (408) count as
/// unknown, like a 5xx: the client has no retry layer, so treating them as
/// final would abandon a write that a later try would make.
pub(super) fn write_of(r: Result<kube::Error, tokio::time::error::Elapsed>) -> Write {
    match r {
        Ok(kube::Error::Api(s)) => match s.code {
            409 if s.reason == "AlreadyExists" => Write::Exists,
            404 | 409 => Write::Gone,
            408 | 429 => Write::Unknown,
            c if c >= 500 => Write::Unknown,
            _ => Write::Refused(format!("http {} {}: {}", s.code, s.reason, s.message)),
        },
        // A timeout, or a transport failure after the request may have been sent.
        Err(_) | Ok(kube::Error::HyperError(_) | kube::Error::Service(_)) => Write::Unknown,
        Ok(e) => Write::Refused(e.to_string()),
    }
}

/// A failed JSON patch: a failed `test` is a 422.
fn patch_of(r: Result<kube::Error, tokio::time::error::Elapsed>) -> PatchOutcome {
    if let Ok(kube::Error::Api(s)) = &r
        && s.code == 422
    {
        return PatchOutcome::Lost;
    }
    match write_of(r) {
        Write::Done | Write::Exists | Write::Gone => PatchOutcome::Lost,
        Write::Unknown => PatchOutcome::Unknown,
        Write::Refused(m) => PatchOutcome::Failed(m),
    }
}

/// The outcome of a bounded call.
async fn done<T>(t: Duration, f: impl Future<Output = Result<T, kube::Error>>) -> Result<T, Write> {
    match tokio::time::timeout(t, f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(write_of(Ok(e))),
        Err(e) => Err(write_of(Err(e))),
    }
}

fn delete_params(uid: &str, grace: Option<u32>) -> DeleteParams {
    DeleteParams {
        preconditions: Some(Preconditions {
            uid: Some(uid.to_owned()),
            resource_version: None,
        }),
        grace_period_seconds: grace,
        ..DeleteParams::default()
    }
}

fn written<T>(r: Result<T, Write>) -> Write {
    r.map_or_else(|w| w, |_| Write::Done)
}

impl Backend for KubeBackend {
    fn patch_pod(&self, ns: &str, name: &str, patch: Value) -> BoxFuture<'static, PatchOutcome> {
        let (api, name, t) = (self.api::<Pod>(ns), name.to_owned(), self.timeout);
        Box::pin(async move {
            let patch: json_patch::Patch = match serde_json::from_value(patch) {
                Ok(p) => p,
                Err(e) => return PatchOutcome::Failed(e.to_string()),
            };
            let (pp, patch) = (PatchParams::default(), Patch::Json::<()>(patch));
            match tokio::time::timeout(t, api.patch(&name, &pp, &patch)).await {
                Ok(Ok(pod)) => PatchOutcome::Applied(Box::new(pod)),
                Ok(Err(e)) => patch_of(Ok(e)),
                Err(e) => patch_of(Err(e)),
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
        let (api, name, t) = (self.api::<Pod>(ns), name.to_owned(), self.timeout);
        let dp = delete_params(uid, grace);
        Box::pin(async move {
            match written(done(t, api.delete(&name, &dp)).await) {
                Write::Done | Write::Gone => Ok(()),
                w => Err(format!("{w:?}")),
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
        let api = self.api::<PersistentVolumeClaim>(ns);
        let (name, t) = (name.to_owned(), self.timeout);
        let patch = json!({"metadata": {"annotations": {key: value}}});
        Box::pin(async move {
            let pp = PatchParams::default();
            match written(done(t, api.patch(&name, &pp, &Patch::Merge(patch))).await) {
                Write::Done => Ok(()),
                w => Err(format!("{w:?}")),
            }
        })
    }

    fn create_claim(&self, claim: BuildKitClaim) -> BoxFuture<'static, Write> {
        let ns = claim.metadata.namespace.clone().unwrap_or_default();
        let (api, t) = (self.api::<BuildKitClaim>(&ns), self.timeout);
        Box::pin(async move { written(done(t, api.create(&PostParams::default(), &claim)).await) })
    }

    fn get_claim(
        &self,
        ns: &str,
        name: &str,
    ) -> BoxFuture<'static, Result<Option<BuildKitClaim>, Write>> {
        let (api, name, t) = (self.api::<BuildKitClaim>(ns), name.to_owned(), self.timeout);
        Box::pin(async move { done(t, api.get_opt(&name)).await })
    }

    fn delete_claim(&self, ns: &str, name: &str, uid: &str) -> BoxFuture<'static, Write> {
        let (api, name, t) = (self.api::<BuildKitClaim>(ns), name.to_owned(), self.timeout);
        let dp = delete_params(uid, None);
        Box::pin(async move { written(done(t, api.delete(&name, &dp)).await) })
    }

    fn patch_claim_status(
        &self,
        ns: &str,
        name: &str,
        resource_version: &str,
        status: &BuildKitClaimStatus,
    ) -> BoxFuture<'static, Write> {
        let (api, name, t) = (self.api::<BuildKitClaim>(ns), name.to_owned(), self.timeout);
        let body = Patch::Merge(json!({
            "metadata": {"resourceVersion": resource_version},
            "status": status,
        }));
        Box::pin(async move {
            written(done(t, api.patch_status(&name, &PatchParams::default(), &body)).await)
        })
    }

    fn get_secret(
        &self,
        ns: &str,
        name: &str,
    ) -> BoxFuture<'static, Result<Option<Secret>, Write>> {
        let (api, name, t) = (self.api::<Secret>(ns), name.to_owned(), self.timeout);
        Box::pin(async move { done(t, api.get_opt(&name)).await })
    }

    fn create_secret(&self, secret: Secret) -> BoxFuture<'static, Write> {
        let ns = secret.metadata.namespace.clone().unwrap_or_default();
        let (api, t) = (self.api::<Secret>(&ns), self.timeout);
        Box::pin(async move { written(done(t, api.create(&PostParams::default(), &secret)).await) })
    }

    fn delete_secret(&self, ns: &str, name: &str, uid: &str) -> BoxFuture<'static, Write> {
        let (api, name, t) = (self.api::<Secret>(ns), name.to_owned(), self.timeout);
        let dp = delete_params(uid, None);
        Box::pin(async move { written(done(t, api.delete(&name, &dp)).await) })
    }
}
