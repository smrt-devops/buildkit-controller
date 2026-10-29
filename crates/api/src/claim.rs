//! `BuildKitClaim` v1alpha1: a request for one builder, queued and bound by
//! the leader. Every allocation is a claim: the API creates claims for CI
//! callers in the controller's namespace, and in-cluster callers create
//! their own.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube_derive::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v1alpha1::{Duration, VERSION};

/// Longest claim name that leaves room for [`SECRET_SUFFIX`] in a Secret name.
pub const MAX_CLAIM_NAME_LEN: usize = 253 - SECRET_SUFFIX.len();
/// Suffix of the Secret a claim gets.
pub const SECRET_SUFFIX: &str = "-buildkit";

/// The Secret name for claim `name`.
#[must_use]
pub fn secret_name(name: &str) -> String {
    format!("{name}{SECRET_SUFFIX}")
}

/// `BuildKitClaim` spec. Immutable after create.
#[derive(
    CustomResource, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema,
)]
#[kube(
    group = "buildkit.smrt-devops.net",
    version = "v1alpha1",
    kind = "BuildKitClaim",
    plural = "buildkitclaims",
    singular = "buildkitclaim",
    shortname = "bkc",
    category = "buildkit",
    namespaced,
    status = "BuildKitClaimStatus",
    printcolumn = r#"{"name":"Pool","type":"string","jsonPath":".spec.poolRef.name"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Secret","type":"string","jsonPath":".status.secretName"}"#,
    printcolumn = r#"{"name":"Expires","type":"string","jsonPath":".status.expiresAt"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
    crates(
        kube_core = "::kube_core",
        k8s_openapi = "::k8s_openapi",
        schemars = "::schemars",
        serde = "::serde",
        serde_json = "::serde_json"
    )
)]
#[serde(rename_all = "camelCase")]
#[schemars(extend("x-kubernetes-validations" = [{"rule": "self == oldSelf", "message": "spec is immutable; create a new claim"}]))]
pub struct BuildKitClaimSpec {
    /// The pool to allocate from.
    pub pool_ref: PoolRef,
    /// Lifetime of the allocation. Defaults to the pool's `allocation.defaultTTL`;
    /// the pool's `allocation.maxTTL` caps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("x-kubernetes-validations" = [{"rule": "duration(self) >= duration('60s')", "message": "ttl must be at least 60s"}]))]
    pub ttl: Option<Duration>,
    /// How long the claim may stay `Pending` before it fails with `QueueTimeout`.
    /// Defaults to the pool's `allocation.queueTimeout`; its `maxQueueTimeout` caps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_timeout: Option<Duration>,
    /// Cache tag, as `cacheKey` of `POST /v1/allocations`. Unset uses the pool name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        length(min = 1, max = 63),
        regex(pattern = r"^[A-Za-z0-9]([A-Za-z0-9._-]*[A-Za-z0-9])?$")
    )]
    pub cache_key: Option<String>,
    /// Whether to use the cache, as `cache` of `POST /v1/allocations`. Unset means `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<crate::dto::CacheChoice>,
    /// PEM SPKI ECDSA P-256 public key. The leader signs the allocation certificate
    /// for it and writes the certificate into status. Without it the leader generates
    /// the key and writes the Secret `<name>-buildkit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 1024))]
    pub public_key: Option<String>,
    /// The allocation owner the API authenticated. Honoured only in the controller's
    /// namespace; elsewhere the owner is `claim:<namespace>/<name>#<uid>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 1024))]
    pub owner: Option<String>,
    /// The caller's cache scope id, written by the API with `owner`. Honoured only in
    /// the controller's namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub cache_scope: Option<String>,
    /// The caller's cache tier, written by the API with `owner`. Honoured only in the
    /// controller's namespace; there a claim without it may only read the cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_tier: Option<CacheTier>,
}

/// Whether an allocation may write the cache or only read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CacheTier {
    /// Pull and push.
    Trusted,
    /// Pull only.
    Untrusted,
}

impl CacheTier {
    /// The value as written on the worker pod.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Untrusted => "untrusted",
        }
    }
}

/// Reference to a `BuildKitPool`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PoolRef {
    /// Pool namespace.
    #[schemars(
        length(min = 1, max = 63),
        regex(pattern = r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?$")
    )]
    pub namespace: String,
    /// Pool name.
    #[schemars(
        length(min = 1, max = 52),
        regex(pattern = r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?$")
    )]
    pub name: String,
}

/// Claim lifecycle phase. `Expired`, `Lost` and `Failed` are terminal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ClaimPhase {
    /// Waiting for a worker.
    #[default]
    Pending,
    /// A worker is allocated; status (and the Secret, without `publicKey`) holds its credentials.
    Bound,
    /// The allocation's TTL ran out; the Secret is gone.
    Expired,
    /// The worker pod went away before expiry; the Secret is gone.
    Lost,
    /// The claim cannot be served (queue timeout, pool not found or access denied, bad TTL,
    /// Secret conflict).
    Failed,
}

impl ClaimPhase {
    /// The phase as written in status.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Bound => "Bound",
            Self::Expired => "Expired",
            Self::Lost => "Lost",
            Self::Failed => "Failed",
        }
    }

    /// True for `Expired`, `Lost` and `Failed`.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Expired | Self::Lost | Self::Failed)
    }
}

/// `BuildKitClaim` status, written by the controller only.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BuildKitClaimStatus {
    /// Current phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<ClaimPhase>,
    /// Allocation id (set once bound).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocation_id: Option<String>,
    /// Edge endpoint, for example `tcp://bk.example.com:8444`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// TLS server name of the edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    /// Secret in the claim's namespace holding `tls.crt`, `tls.key`, `ca.crt` and `endpoint`.
    /// Unset for a claim with `spec.publicKey`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_name: Option<String>,
    /// The worker pod the claim is bound to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod: Option<String>,
    /// The allocation client certificate (PEM; public).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate: Option<String>,
    /// RFC 3339 UTC expiry of the allocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Cache flags of the allocation, as in the `201` response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<crate::dto::CacheResponse>,
    /// Generation of the spec this status describes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// Conditions (`Bound`).
    #[serde(default)]
    #[schemars(extend("x-kubernetes-list-type" = "map", "x-kubernetes-list-map-keys" = ["type"]))]
    pub conditions: Vec<Condition>,
}

/// The claim CRD with object-level validation.
///
/// # Panics
/// Does not panic in practice, because the generated CRD always has one version with a schema.
#[must_use]
pub fn crd()
-> k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition {
    use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::ValidationRule;
    use kube_core::CustomResourceExt;
    let mut crd = BuildKitClaim::crd();
    if let Some(root) = crd
        .spec
        .versions
        .iter_mut()
        .find(|v| v.name == VERSION)
        .and_then(|v| v.schema.as_mut())
        .and_then(|s| s.open_api_v3_schema.as_mut())
    {
        root.x_kubernetes_validations = Some(vec![ValidationRule {
            rule: format!("self.metadata.name.size() <= {MAX_CLAIM_NAME_LEN}"),
            message: Some(format!(
                "claim name must be at most {MAX_CLAIM_NAME_LEN} characters"
            )),
            ..Default::default()
        }]);
        root.required = Some(vec!["spec".into()]);
    }
    crd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_name_fits() {
        let long = "a".repeat(MAX_CLAIM_NAME_LEN);
        assert_eq!(secret_name(&long).len(), 253);
        assert_eq!(secret_name("b"), "b-buildkit");
    }

    #[test]
    fn crd_has_immutable_spec_and_name_rule() {
        let y = serde_json::to_string(&crd()).unwrap_or_default();
        assert!(y.contains("self == oldSelf"));
        assert!(y.contains("self.metadata.name.size() <= 244"));
        assert!(y.contains("duration(self) >= duration('60s')"));
        assert!(!y.contains("not supported yet"));
        let v = serde_json::to_value(crd()).unwrap_or_default();
        let schema = &v["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"];
        let spec = &schema["spec"]["properties"];
        for f in ["queueTimeout", "publicKey", "owner", "cacheScope"] {
            assert!(spec.get(f).is_some(), "spec.{f}");
        }
        assert_eq!(spec["publicKey"]["maxLength"], 1024);
        assert_eq!(spec["owner"]["minLength"], 1);
        let status = &schema["status"]["properties"];
        for f in ["pod", "certificate", "cache"] {
            assert!(status.get(f).is_some(), "status.{f}");
        }
        // The queue place is computed from the store when asked, never stored.
        assert!(status.get("queuePosition").is_none());
        let cols = &v["spec"]["versions"][0]["additionalPrinterColumns"];
        assert!(
            cols.as_array()
                .is_some_and(|c| c.iter().all(|c| c["name"] != "Position")),
            "{cols}"
        );
    }

    #[test]
    fn status_round_trips() {
        let s = BuildKitClaimStatus {
            phase: Some(ClaimPhase::Bound),
            secret_name: Some("x-buildkit".into()),
            ..Default::default()
        };
        let v = serde_json::to_value(&s).unwrap_or_default();
        assert_eq!(v["phase"], "Bound");
        assert_eq!(v["secretName"], "x-buildkit");
        assert!(ClaimPhase::Failed.is_terminal() && !ClaimPhase::Pending.is_terminal());
    }
}
