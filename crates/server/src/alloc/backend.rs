//! The Kubernetes calls of allocation, behind a trait so the front door and
//! the binder are testable without an API server. Every call is bounded by
//! `kube.requestTimeout`.

use std::collections::BTreeMap;
use std::time::Duration;

use buildkit_controller_api::claim::{BuildKitClaim, BuildKitClaimStatus};
use futures::future::BoxFuture;
use k8s_openapi::api::core::v1::{Pod, Secret};
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
    /// `DELETE` a claim if it still has `uid` and `resource_version`: a claim
    /// changed since it was read is a 409, which is [`Write::Gone`].
    fn delete_claim_at(
        &self,
        ns: &str,
        name: &str,
        uid: &str,
        resource_version: &str,
    ) -> BoxFuture<'static, Write>;
    /// Merge `annotations` into a claim's metadata, if the claim still has
    /// `uid` (a 409 otherwise, which is [`Write::Gone`]).
    fn annotate_claim(
        &self,
        ns: &str,
        name: &str,
        uid: &str,
        annotations: BTreeMap<String, String>,
    ) -> BoxFuture<'static, Write>;
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

    fn delete_claim_at(
        &self,
        ns: &str,
        name: &str,
        uid: &str,
        resource_version: &str,
    ) -> BoxFuture<'static, Write> {
        let (api, name, t) = (self.api::<BuildKitClaim>(ns), name.to_owned(), self.timeout);
        let mut dp = delete_params(uid, None);
        if let Some(p) = dp.preconditions.as_mut() {
            p.resource_version = Some(resource_version.to_owned());
        }
        Box::pin(async move { written(done(t, api.delete(&name, &dp)).await) })
    }

    fn annotate_claim(
        &self,
        ns: &str,
        name: &str,
        uid: &str,
        annotations: BTreeMap<String, String>,
    ) -> BoxFuture<'static, Write> {
        let (api, name, t) = (self.api::<BuildKitClaim>(ns), name.to_owned(), self.timeout);
        // A uid in the patch is a precondition: a claim recreated since is a 409.
        let body = Patch::Merge(json!({"metadata": {"uid": uid, "annotations": annotations}}));
        Box::pin(
            async move { written(done(t, api.patch(&name, &PatchParams::default(), &body)).await) },
        )
    }

    fn patch_claim_status(
        &self,
        ns: &str,
        name: &str,
        resource_version: &str,
        status: &BuildKitClaimStatus,
    ) -> BoxFuture<'static, Write> {
        let (api, name, t) = (self.api::<BuildKitClaim>(ns), name.to_owned(), self.timeout);
        let body = Patch::Merge(status_patch(resource_version, status));
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

/// Status fields a write may leave unset. A merge patch keeps a field it
/// does not name, so each unset one is sent as `null`: a claim that leaves
/// `Pending` loses its place, and an ended claim no longer names its deleted
/// Secret.
const CLEARABLE: [&str; 10] = [
    "allocationId",
    "endpoint",
    "serverName",
    "secretName",
    "pod",
    "certificate",
    "expiresAt",
    "queuePosition",
    "cache",
    "observedGeneration",
];

/// The merge patch of a claim status write, preconditioned on
/// `resource_version`. Every field the status leaves unset is cleared, so the
/// stored status is exactly `status`.
pub(super) fn status_patch(resource_version: &str, status: &BuildKitClaimStatus) -> Value {
    let mut s = json!(status);
    if let Some(m) = s.as_object_mut() {
        for k in CLEARABLE {
            m.entry(k).or_insert(Value::Null);
        }
    }
    json!({"metadata": {"resourceVersion": resource_version}, "status": s})
}

#[cfg(test)]
mod tests {
    use buildkit_controller_api::claim::{BuildKitClaimStatus, ClaimPhase};
    use serde_json::json;

    use super::status_patch;

    #[test]
    fn leaving_pending_clears_the_queue_position() {
        let bound = BuildKitClaimStatus {
            phase: Some(ClaimPhase::Bound),
            ..BuildKitClaimStatus::default()
        };
        let p = status_patch("7", &bound);
        assert_eq!(p["metadata"]["resourceVersion"], "7");
        assert_eq!(p["status"]["queuePosition"], json!(null));
        assert!(
            p["status"]
                .as_object()
                .is_some_and(|m| m.contains_key("queuePosition"))
        );
        let pending = BuildKitClaimStatus {
            phase: Some(ClaimPhase::Pending),
            queue_position: Some(3),
            ..BuildKitClaimStatus::default()
        };
        assert_eq!(status_patch("7", &pending)["status"]["queuePosition"], 3);
    }

    /// An ended claim's status drops `secretName`: the write must say so, or
    /// the merge keeps the name of the Secret `end()` just deleted. Every
    /// optional status field is covered, so a new one cannot be forgotten.
    #[test]
    fn unset_fields_are_cleared() {
        let ended = BuildKitClaimStatus {
            phase: Some(ClaimPhase::Expired),
            ..BuildKitClaimStatus::default()
        };
        let p = status_patch("7", &ended);
        assert_eq!(p["status"].get("secretName"), Some(&json!(null)));
        let full: BuildKitClaimStatus = serde_json::from_value(json!({
            "phase": "Bound", "allocationId": "a", "endpoint": "e", "serverName": "s",
            "secretName": "x", "pod": "p", "certificate": "c", "expiresAt": "t",
            "queuePosition": 1, "cache": {"key": "k"},
            "observedGeneration": 1, "conditions": []
        }))
        .unwrap();
        let mut set: Vec<String> = json!(full).as_object().unwrap().keys().cloned().collect();
        set.retain(|k| k != "phase" && k != "conditions");
        set.sort();
        let mut clearable: Vec<String> = super::CLEARABLE.iter().map(|s| (*s).to_owned()).collect();
        clearable.sort();
        assert_eq!(set, clearable);
    }
}
