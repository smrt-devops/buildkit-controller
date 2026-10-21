//! `BuildKitClaim` v1alpha1: a namespaced request
//! for one builder, served by the leader through the allocation engine.

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
    /// Cache key of both cache layers, as `cacheKey` of `POST /v1/allocations`. Unset uses the pool's default key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        length(min = 1, max = 63),
        regex(pattern = r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?$")
    )]
    pub cache_key: Option<String>,
    /// Which cache layers to use, as `cache` of `POST /v1/allocations`. Unset means `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<crate::dto::CacheChoice>,
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
    /// A worker is allocated and the Secret holds its credentials.
    Bound,
    /// The allocation's TTL ran out; the Secret is gone.
    Expired,
    /// The worker pod went away before expiry; the Secret is gone.
    Lost,
    /// The claim cannot be served (pool not found or access denied, bad TTL, Secret conflict).
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_name: Option<String>,
    /// RFC 3339 UTC expiry of the allocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
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
