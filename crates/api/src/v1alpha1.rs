//! `buildkit.smrt-devops.net/v1alpha1` holds the `BuildKitPool` custom resource.
//!
//! `cargo xtask crdgen` generates the schema from these types, and nobody edits the CRD
//! YAML by hand. Defaults live in the `Default` impls, schemars mirrors them into the schema,
//! and a unit test checks that the two agree.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Affinity, EnvVar, LocalObjectReference, ResourceRequirements, Toleration,
    TopologySpreadConstraint, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube_derive::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ApiError;

/// API group served by the controller.
pub const GROUP: &str = "buildkit.smrt-devops.net";
/// API version of this module.
pub const VERSION: &str = "v1alpha1";

/// Longest pool name that still leaves room for `-<slot>` in a pod name.
pub const MAX_POOL_NAME_LEN: usize = 52;

const DURATION_PATTERN: &str = r"^([0-9]+h)?([0-9]+m)?([0-9]+s)?$";

/// A duration such as `2m`, `1h30m` or `90s` (`^([0-9]+h)?([0-9]+m)?([0-9]+s)?$`, non-empty).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
#[schemars(extend("minLength" = 1, "maxLength" = 32, "pattern" = r"^([0-9]+h)?([0-9]+m)?([0-9]+s)?$"))]
pub struct Duration(String);

impl Duration {
    /// Wraps a duration string without validating it. [`Duration::seconds`] validates.
    #[must_use]
    pub fn new(s: &str) -> Self {
        Self(s.to_owned())
    }

    /// The textual form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse into whole seconds using the CRD grammar (hours, minutes, seconds in order).
    pub fn seconds(&self) -> Result<u64, ApiError> {
        parse_duration(&self.0).ok_or_else(|| ApiError::Invalid {
            field: "duration".into(),
            reason: format!("`{}` does not match {DURATION_PATTERN}", self.0),
        })
    }
}

fn parse_duration(s: &str) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let mut rest = s;
    let mut total: u64 = 0;
    for (unit, mult) in [('h', 3600u64), ('m', 60), ('s', 1)] {
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 && rest[digits..].starts_with(unit) {
            let n: u64 = rest[..digits].parse().ok()?;
            total = total.checked_add(n.checked_mul(mult)?)?;
            rest = &rest[digits + 1..];
        }
    }
    rest.is_empty().then_some(total)
}

fn dur(s: &str) -> Duration {
    Duration::new(s)
}

/// `imagePullPolicy` values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum PullPolicy {
    /// Always pull.
    Always,
    /// Pull when absent.
    #[default]
    IfNotPresent,
    /// Never pull.
    Never,
}

impl PullPolicy {
    /// The `imagePullPolicy` value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Always => "Always",
            Self::IfNotPresent => "IfNotPresent",
            Self::Never => "Never",
        }
    }
}

/// Day of week for a scaling window.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub enum Day {
    /// Monday.
    Mon,
    /// Tuesday.
    Tue,
    /// Wednesday.
    Wed,
    /// Thursday.
    Thu,
    /// Friday.
    Fri,
    /// Saturday.
    Sat,
    /// Sunday.
    Sun,
}

impl Day {
    /// All seven days, the default for a window.
    pub const ALL: [Day; 7] = [
        Day::Mon,
        Day::Tue,
        Day::Wed,
        Day::Thu,
        Day::Fri,
        Day::Sat,
        Day::Sun,
    ];
}

/// `BuildKitPool` spec.
#[derive(CustomResource, Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "buildkit.smrt-devops.net",
    version = "v1alpha1",
    kind = "BuildKitPool",
    plural = "buildkitpools",
    singular = "buildkitpool",
    shortname = "bkp",
    category = "buildkit",
    namespaced,
    status = "BuildKitPoolStatus",
    printcolumn = r#"{"name":"Min","type":"integer","jsonPath":".spec.scaling.min"}"#,
    printcolumn = r#"{"name":"Max","type":"integer","jsonPath":".spec.scaling.max"}"#,
    printcolumn = r#"{"name":"Warm","type":"integer","jsonPath":".status.warm"}"#,
    printcolumn = r#"{"name":"Allocated","type":"integer","jsonPath":".status.allocated"}"#,
    printcolumn = r#"{"name":"Waiting","type":"integer","jsonPath":".status.waiting"}"#,
    printcolumn = r#"{"name":"Platforms","type":"string","jsonPath":".status.platforms"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
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
pub struct BuildKitPoolSpec {
    /// Platforms the pool builds for (`os/arch[/vN]`, at most 8).
    #[serde(default)]
    #[schemars(
        length(max = 8),
        extend("x-kubernetes-list-type" = "set"),
        inner(length(max = 32), regex(pattern = r"^[a-z0-9]+/[a-z0-9]+(/v[0-9]+)?$"))
    )]
    pub platforms: Vec<String>,
    /// The part of the worker pod that the user controls. The reconciler sets the rest.
    #[serde(default)]
    pub template: PodTemplate,
    /// Worker pod settings.
    #[serde(default)]
    pub workers: Workers,
    /// Network policy settings for worker pods.
    #[serde(default)]
    pub network: Network,
    /// Pool size and schedule.
    #[serde(default)]
    pub scaling: Scaling,
    /// Who may allocate from the pool.
    #[serde(default)]
    pub access: Access,
    /// Allocation lifetimes and queueing.
    #[serde(default)]
    pub allocation: AllocationSpec,
}

/// Name of the one container a template may contain.
pub const CONTAINER_NAME: &str = "buildkitd";
/// Volume names the controller owns; templates may not use them.
pub const RESERVED_VOLUMES: [&str; 2] = ["certs", "state"];
/// Mount paths (and everything below them) the controller owns.
pub const RESERVED_MOUNT_PATHS: [&str; 4] = [
    "/certs",
    "/etc/buildkit",
    "/home/user/.local/share/buildkit",
    "/run/user/1000",
];
/// Env var name prefixes the controller owns.
pub const RESERVED_ENV_PREFIXES: [&str; 2] = ["BUILDKITD_", "ROOTLESSKIT_"];
/// Label keys the controller owns.
pub const RESERVED_LABEL_KEYS: [&str; 2] =
    ["app.kubernetes.io/managed-by", "app.kubernetes.io/name"];
/// Label and annotation key prefix the controller owns.
pub const RESERVED_KEY_PREFIX: &str = "buildkit.smrt-devops.net/";

/// `spec.template` is the subset of a `PodTemplateSpec` that the user controls.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct PodTemplate {
    /// Extra pod labels and annotations.
    pub metadata: TemplateMeta,
    /// Pod spec subset.
    pub spec: TemplateSpec,
}

/// `spec.template.metadata`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct TemplateMeta {
    /// Pod labels. The API server rejects keys that the controller owns.
    #[schemars(extend(
        "x-kubernetes-validations" = [{"rule": "self.all(k, k != 'app.kubernetes.io/managed-by' && k != 'app.kubernetes.io/name' && !k.startsWith('buildkit.smrt-devops.net/'))", "message": "label key is reserved by the controller"}]
    ))]
    pub labels: BTreeMap<String, String>,
    /// Pod annotations. The API server rejects keys with the controller-owned prefix.
    #[schemars(extend(
        "x-kubernetes-validations" = [{"rule": "self.all(k, !k.startsWith('buildkit.smrt-devops.net/'))", "message": "annotation key prefix buildkit.smrt-devops.net/ is reserved"}]
    ))]
    pub annotations: BTreeMap<String, String>,
}

/// `spec.template.spec` holds the `PodSpec` fields that the user controls.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
#[schemars(extend("x-kubernetes-validations" = [
    {"rule": "!has(self.containers[0].volumeMounts) || self.containers[0].volumeMounts.all(m, has(self.volumes) && self.volumes.exists(v, v.name == m.name))", "message": "every volumeMount must name a template volume"}
]))]
pub struct TemplateSpec {
    /// Node selector.
    #[schemars(extend("maxProperties" = 16))]
    pub node_selector: BTreeMap<String, String>,
    /// Tolerations.
    #[schemars(length(max = 16))]
    pub tolerations: Vec<Toleration>,
    /// Affinity.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "non_null::<Affinity>")]
    pub affinity: Option<Affinity>,
    /// Topology spread constraints.
    #[schemars(length(max = 8))]
    pub topology_spread_constraints: Vec<TopologySpreadConstraint>,
    /// Runtime class, e.g. `gvisor` or `kata`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(
        length(min = 1, max = 253),
        regex(pattern = r"^[a-z0-9]([-a-z0-9.]*[a-z0-9])?$")
    )]
    pub runtime_class_name: Option<String>,
    /// Priority class. Names starting with `system-` are refused: they would let untrusted builds preempt cluster-critical pods.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(
        length(min = 1, max = 253),
        regex(pattern = r"^[a-z0-9]([-a-z0-9.]*[a-z0-9])?$"),
        extend("x-kubernetes-validations" = [{"rule": "!self.startsWith('system-')", "message": "priorityClassName must not start with system-"}])
    )]
    pub priority_class_name: Option<String>,
    /// Pull secrets.
    #[schemars(length(max = 8))]
    pub image_pull_secrets: Vec<LocalObjectReference>,
    /// Volumes limited to `configMap`, `secret`, `emptyDir` and `projected`.
    #[schemars(schema_with = "template_volumes")]
    pub volumes: Vec<Volume>,
    /// Exactly one container, named `buildkitd`.
    #[schemars(length(min = 1, max = 1), extend("x-kubernetes-validations" = [{"rule": "size(self) == 1 && self[0].name == 'buildkitd'", "message": "containers must be exactly one item named buildkitd"}]))]
    pub containers: Vec<BuildkitdContainer>,
}

impl Default for TemplateSpec {
    fn default() -> Self {
        Self {
            node_selector: BTreeMap::new(),
            tolerations: Vec::new(),
            affinity: None,
            topology_spread_constraints: Vec::new(),
            runtime_class_name: None,
            priority_class_name: None,
            image_pull_secrets: Vec::new(),
            volumes: Vec::new(),
            containers: vec![BuildkitdContainer::default()],
        }
    }
}

/// Keep only the listed `properties` of a schema node (no-op when absent).
fn retain_props(node: Option<&mut serde_json::Value>, keep: &[&str]) {
    if let Some(props) = node
        .and_then(|n| n.get_mut("properties"))
        .and_then(|p| p.as_object_mut())
    {
        props.retain(|k, _| keep.contains(&k.as_str()));
    }
}

/// Set `maxLength` on the string property at `pointer`.
fn cap_len(value: &mut serde_json::Value, pointer: &str, max: u64) {
    if let Some(node) = value.pointer_mut(pointer).and_then(|n| n.as_object_mut()) {
        node.insert("maxLength".into(), max.into());
    }
}

/// Maximum `volumes` entries.
pub const MAX_VOLUMES: usize = 16;
/// Maximum `env` entries.
pub const MAX_ENV: usize = 32;
/// Maximum `volumeMounts` entries.
pub const MAX_MOUNTS: usize = 16;
/// Longest volume or mount name (DNS-1123 label).
pub const MAX_VOLUME_NAME: usize = 63;
/// Longest env var name.
pub const MAX_ENV_NAME: usize = 128;
/// Longest mount path.
pub const MAX_MOUNT_PATH: usize = 256;
/// Maximum `projected.sources` entries per volume.
pub const MAX_PROJECTED_SOURCES: usize = 16;

fn template_volumes(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut value = Volume::json_schema(generator).to_value();
    retain_props(
        Some(&mut value),
        &["name", "configMap", "secret", "emptyDir", "projected"],
    );
    retain_props(
        value.pointer_mut("/properties/projected/properties/sources/items"),
        &["configMap", "secret", "downwardAPI"],
    );
    if let Some(sources) = value
        .pointer_mut("/properties/projected/properties/sources")
        .and_then(|n| n.as_object_mut())
    {
        sources.insert("maxItems".into(), MAX_PROJECTED_SOURCES.into());
    }
    cap_len(&mut value, "/properties/name", MAX_VOLUME_NAME as u64);
    schemars::json_schema!({
        "type": "array",
        "maxItems": MAX_VOLUMES,
        "items": value,
        "x-kubernetes-validations": [
            {"rule": "self.all(v, v.name != 'certs' && v.name != 'state')", "message": "volume names certs and state are reserved"},
            {"rule": "self.all(v, !has(v.projected) || !has(v.projected.sources) || v.projected.sources.all(s, [has(s.configMap), has(s.secret), has(s.downwardAPI)].exists_one(x, x)))", "message": "each projected source must set exactly one of configMap, secret or downwardAPI"}
        ]
    })
}

fn container_env(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut value = EnvVar::json_schema(generator).to_value();
    retain_props(
        value.pointer_mut("/properties/valueFrom"),
        &["configMapKeyRef", "secretKeyRef", "fieldRef"],
    );
    cap_len(&mut value, "/properties/name", MAX_ENV_NAME as u64);
    schemars::json_schema!({
        "type": "array",
        "maxItems": MAX_ENV,
        "items": value,
        "x-kubernetes-validations": [
            {"rule": "self.all(e, !e.name.startsWith('BUILDKITD_') && !e.name.startsWith('ROOTLESSKIT_'))", "message": "env names BUILDKITD_* and ROOTLESSKIT_* are reserved"},
            {"rule": "self.all(e, !has(e.valueFrom) || [has(e.valueFrom.configMapKeyRef), has(e.valueFrom.secretKeyRef), has(e.valueFrom.fieldRef)].exists_one(x, x))", "message": "env valueFrom must set exactly one of configMapKeyRef, secretKeyRef or fieldRef"}
        ]
    })
}

fn container_mounts(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut value = VolumeMount::json_schema(generator).to_value();
    cap_len(&mut value, "/properties/name", MAX_VOLUME_NAME as u64);
    cap_len(&mut value, "/properties/mountPath", MAX_MOUNT_PATH as u64);
    cap_len(&mut value, "/properties/subPath", MAX_MOUNT_PATH as u64);
    schemars::json_schema!({
        "type": "array",
        "maxItems": MAX_MOUNTS,
        "items": value,
        "x-kubernetes-validations": [
            {"rule": "self.all(m, m.mountPath.startsWith('/') && m.mountPath.substring(1).split('/').all(s, s != '' && s != '.' && s != '..'))", "message": "mountPath must be absolute and normalised: every segment non-empty and not '.' or '..'"},
            {"rule": "self.all(m, ['/certs', '/etc/buildkit', '/home/user/.local/share/buildkit', '/run/user/1000'].all(p, m.mountPath != p && !m.mountPath.startsWith(p + '/')))", "message": "mountPath is under a controller-owned path"},
            {"rule": "self.all(m, !m.mountPath.matches(r'[\\x00-\\x1f\\x7f]') && (!has(m.subPath) || !m.subPath.matches(r'[\\x00-\\x1f\\x7f]')))", "message": "mountPath and subPath must not contain control characters"},
            {"rule": "self.all(m, !has(m.mountPropagation) || m.mountPropagation != 'Bidirectional')", "message": "mountPropagation Bidirectional needs a privileged container"}
        ]
    })
}

/// The `buildkitd` container, limited to the `Container` fields that the user controls.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct BuildkitdContainer {
    /// Must be `buildkitd`.
    #[schemars(length(min = 1, max = 63))]
    pub name: String,
    /// Container image for buildkitd.
    #[schemars(length(min = 1, max = 512))]
    pub image: String,
    /// Image pull policy.
    pub image_pull_policy: PullPolicy,
    /// Resource requests and limits. `limits` must contain `memory` and `ephemeral-storage`.
    #[schemars(extend("x-kubernetes-validations" = [{"rule": "has(self.limits) && 'memory' in self.limits && 'ephemeral-storage' in self.limits", "message": "resources.limits must set memory and ephemeral-storage"}]))]
    pub resources: ResourceRequirements,
    /// Environment variables. The API server rejects names with controller-owned prefixes.
    #[schemars(schema_with = "container_env")]
    pub env: Vec<EnvVar>,
    /// Extra mounts of template volumes.
    #[schemars(schema_with = "container_mounts")]
    pub volume_mounts: Vec<VolumeMount>,
}

impl Default for BuildkitdContainer {
    fn default() -> Self {
        let q = |s: &str| Quantity(s.to_owned());
        Self {
            name: CONTAINER_NAME.into(),
            image: "moby/buildkit:v0.34.0-rootless".into(),
            image_pull_policy: PullPolicy::default(),
            resources: ResourceRequirements {
                requests: Some(BTreeMap::from([
                    ("cpu".to_owned(), q("500m")),
                    ("memory".to_owned(), q("1Gi")),
                    ("ephemeral-storage".to_owned(), q("1Gi")),
                ])),
                limits: Some(BTreeMap::from([
                    ("memory".to_owned(), q("4Gi")),
                    ("ephemeral-storage".to_owned(), q("20Gi")),
                ])),
                ..ResourceRequirements::default()
            },
            env: Vec::new(),
            volume_mounts: Vec::new(),
        }
    }
}

/// `spec.workers`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct Workers {
    /// buildkitd TOML configuration.
    #[schemars(length(max = 65536))]
    pub buildkitd_config: String,
    /// How long a pod may take to become Ready, from 10s to 30m.
    #[schemars(extend("x-kubernetes-validations" = [{"rule": "duration(self) >= duration('10s') && duration(self) <= duration('30m')", "message": "startTimeout must be between 10s and 30m"}]))]
    pub start_timeout: Duration,
    /// Build cache settings.
    pub cache: Cache,
}

impl Default for Workers {
    fn default() -> Self {
        Self {
            buildkitd_config: String::new(),
            start_timeout: dur("2m"),
            cache: Cache::default(),
        }
    }
}

/// `spec.workers.cache`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct Cache {
    /// Size limit of the buildkitd state `emptyDir`. When unset, the
    /// ephemeral-storage limit bounds it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_limit: Option<Quantity>,
    /// Who shares a cache in the built-in registry. `identity` (the default) gives each
    /// caller its own cache. An OIDC token is scoped to its issuer and its `repository_id`
    /// claim (GitHub Actions), else its `repository` claim, else its `sub`. A
    /// `ServiceAccount` is scoped to its namespace and name, and a `BuildKitClaim` to its
    /// namespace. `pool` gives every caller the pool admits one cache, so use it only
    /// when they are one trust domain.
    pub scope: CacheScope,
    /// Callers whose builds may write the cache, in the syntax of `access.rules`; the
    /// rest only read it. Unset trusts OIDC tokens whose `event_name` claim is `push`,
    /// `schedule`, `workflow_dispatch` or `merge_group`, and every `ServiceAccount` and
    /// `BuildKitClaim` caller. `[]` trusts nobody. A token whose `event_name` is
    /// `pull_request` or `pull_request_target` only reads, whatever these rules say.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 64))]
    pub writers: Option<Vec<AccessRule>>,
    /// Bytes the pool's caches may hold in the built-in registry together. Over it, the
    /// least recently used caches go first. Unset uses the registry's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "max_size")]
    pub max_size: Option<Quantity>,
}

fn max_size(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "x-kubernetes-int-or-string": true,
        "x-kubernetes-validations": [
            {"rule": "quantity(string(self)).compareTo(quantity('1Mi')) >= 0", "message": "maxSize must be at least 1Mi"}
        ]
    })
}

/// `cache.scope`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CacheScope {
    /// One cache per caller identity.
    #[default]
    Identity,
    /// One cache for every caller the pool admits.
    Pool,
}

/// A cache key: a label value of 1 to 63 characters.
#[must_use]
pub fn is_cache_key(s: &str) -> bool {
    let edge = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
    (1..=63).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && edge(s.chars().next())
        && edge(s.chars().last())
}

/// `spec.network`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct Network {
    /// Egress policy of worker pods.
    pub egress: Egress,
}

/// `spec.network.egress`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct Egress {
    /// Whether workers may reach the internet.
    pub internet: Internet,
    /// CIDR ranges re-opened through the egress deny list (for example a private registry or proxy), so they apply to every pool, not just under `Deny`. The controller refuses (`Ready=False/InvalidSpec`) an entry of `0.0.0.0/0`, `::/0` or `64:ff9b::/96` (all of IPv4 through NAT64), or one that overlaps link-local (`169.254.0.0/16`, `fe80::/10`), loopback, a cloud metadata endpoint (`168.63.129.16`, `100.100.100.200`, `fd00:ec2::254`, `fd20:ce::254`, `fd00:a9fe:a9fe::1`, `fd00:c1::a9fe:a9fe`, `fd00:42::42`), the NAT64 local-use prefix `64:ff9b:1::/48`, the cluster's own ranges or the operator's `workerNetwork.extraDeniedCIDRs`, unless the operator lists a range holding it in `workerNetwork.permittedCIDRs`. An entry inside `::ffff:0:0/96` or `64:ff9b::/96` is checked as the IPv4 range it reaches. A refused entry is left out of the pool's network policy.
    #[serde(rename = "allowCIDRs")]
    #[schemars(
        length(max = 32),
        extend("x-kubernetes-validations" = [{"rule": "self.all(c, isCIDR(c) && cidr(c) == cidr(c).masked())", "message": "allowCIDRs entries must be CIDR ranges with no bits set beyond the prefix length"}]),
        inner(length(max = 64))
    )]
    pub allow_cidrs: Vec<String>,
}

/// `spec.network.egress.internet`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Internet {
    /// Allow internet egress.
    #[default]
    Allow,
    /// Deny internet egress.
    Deny,
}

/// `spec.scaling`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
#[schemars(extend("x-kubernetes-validations" = [
    {"rule": "self.min <= self.max", "message": "scaling.min must not exceed scaling.max"},
    {"rule": "self.schedule.all(w, w.min <= self.max)", "message": "every schedule window min must not exceed scaling.max"}
]))]
pub struct Scaling {
    /// Minimum warm pods.
    #[schemars(range(min = 0, max = 500))]
    pub min: i32,
    /// Maximum pods.
    #[schemars(range(min = 1, max = 500))]
    pub max: i32,
    /// Idle time before scale-down, from 1m to 24h.
    #[schemars(extend("x-kubernetes-validations" = [{"rule": "duration(self) >= duration('1m') && duration(self) <= duration('24h')", "message": "idleTimeout must be between 1m and 24h"}]))]
    pub idle_timeout: Duration,
    /// IANA time zone for schedule windows. The controller checks it.
    #[schemars(length(max = 64))]
    pub time_zone: String,
    /// Scheduled minimum overrides.
    #[schemars(length(max = 32))]
    pub schedule: Vec<Window>,
}

impl Default for Scaling {
    fn default() -> Self {
        Self {
            min: 0,
            max: 10,
            idle_timeout: dur("10m"),
            time_zone: "UTC".into(),
            schedule: Vec::new(),
        }
    }
}

fn all_days() -> Vec<Day> {
    Day::ALL.to_vec()
}

/// A scheduled minimum.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(extend("x-kubernetes-validations" = [{"rule": "self.start != self.end", "message": "window start and end must differ"}]))]
pub struct Window {
    /// Days the window opens. Defaults to all seven.
    #[serde(default = "all_days")]
    #[schemars(length(max = 7), extend("x-kubernetes-list-type" = "set"))]
    pub days: Vec<Day>,
    /// Opening wall time, `HH:MM`.
    #[schemars(length(max = 5), regex(pattern = r"^([01][0-9]|2[0-3]):[0-5][0-9]$"))]
    pub start: String,
    /// Closing wall time, `HH:MM`. A time earlier than `start` closes the window on the next day.
    #[schemars(length(max = 5), regex(pattern = r"^([01][0-9]|2[0-3]):[0-5][0-9]$"))]
    pub end: String,
    /// Minimum warm pods while open.
    #[schemars(range(min = 0, max = 500))]
    pub min: i32,
}

/// `spec.access`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct Access {
    /// Access rules. An empty list denies everyone.
    #[schemars(length(max = 64))]
    pub rules: Vec<AccessRule>,
}

/// Schema of `T` without the `null` alternative, so the apiserver prunes explicit nulls.
fn non_null<T: JsonSchema>(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    T::json_schema(generator)
}

/// One access rule. Set exactly one of `oidc`, `serviceAccount` or `claims`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(extend("x-kubernetes-validations" = [{"rule": "[has(self.oidc), has(self.serviceAccount), has(self.claims)].filter(x, x).size() == 1", "message": "exactly one of oidc, serviceAccount or claims must be set"}]))]
pub struct AccessRule {
    /// Match OIDC token claims.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "non_null::<OidcRule>")]
    pub oidc: Option<OidcRule>,
    /// Match a Kubernetes service account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "non_null::<ServiceAccountRule>")]
    pub service_account: Option<ServiceAccountRule>,
    /// Match `BuildKitClaim` objects by their namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "non_null::<ClaimsRule>")]
    pub claims: Option<ClaimsRule>,
}

/// Claims rule: claims in any listed namespace may use the pool.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaimsRule {
    /// Namespaces whose claims match (1 to 32 DNS-1123 labels, no wildcards).
    #[schemars(
        length(min = 1, max = 32),
        extend("x-kubernetes-list-type" = "set"),
        inner(length(min = 1, max = 63), regex(pattern = r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?$"))
    )]
    pub namespaces: Vec<String>,
}

/// A claim pattern. `*` matches any run of characters, and the pattern needs at least one character that is not `*`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
#[schemars(extend("maxLength" = 256, "pattern" = r"^.*[^*].*$"))]
pub struct ClaimPattern(pub String);

/// OIDC rule.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OidcRule {
    /// Issuer URL. It must equal a configured issuer byte for byte.
    #[schemars(length(max = 512), regex(pattern = r"^https://"))]
    pub issuer: String,
    /// Claims that must all match.
    #[schemars(extend("minProperties" = 1, "maxProperties" = 16))]
    pub claims: BTreeMap<String, ClaimPattern>,
}

/// Service-account rule.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceAccountRule {
    /// Namespace. When unset, the pool's namespace applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        length(min = 1, max = 63),
        regex(pattern = r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?$")
    )]
    pub namespace: Option<String>,
    /// Name. When unset, any service account in the namespace matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        length(min = 1, max = 253),
        regex(pattern = r"^[a-z0-9]([-a-z0-9.]*[a-z0-9])?$")
    )]
    pub name: Option<String>,
}

/// `spec.allocation`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
#[schemars(extend("x-kubernetes-validations" = [
    {"rule": "duration(self.defaultTTL) <= duration(self.maxTTL)", "message": "defaultTTL must not exceed maxTTL"},
    {"rule": "duration(self.queueTimeout) <= duration(self.maxQueueTimeout)", "message": "queueTimeout must not exceed maxQueueTimeout"}
]))]
pub struct AllocationSpec {
    /// TTL applied when a request omits one. At least 1m.
    #[serde(rename = "defaultTTL")]
    #[schemars(extend("x-kubernetes-validations" = [{"rule": "duration(self) >= duration('1m')", "message": "defaultTTL must be at least 1m"}]))]
    pub default_ttl: Duration,
    /// Largest TTL a request may ask for. At most 24h.
    #[serde(rename = "maxTTL")]
    #[schemars(extend("x-kubernetes-validations" = [{"rule": "duration(self) <= duration('24h')", "message": "maxTTL must be at most 24h"}]))]
    pub max_ttl: Duration,
    /// Queue wait applied when a request omits one. `0s` disables queueing.
    pub queue_timeout: Duration,
    /// Largest queue wait a request may ask for. At most 1h.
    #[schemars(extend("x-kubernetes-validations" = [{"rule": "duration(self) <= duration('1h')", "message": "maxQueueTimeout must be at most 1h"}]))]
    pub max_queue_timeout: Duration,
}

impl Default for AllocationSpec {
    fn default() -> Self {
        Self {
            default_ttl: dur("1h"),
            max_ttl: dur("24h"),
            queue_timeout: dur("2m"),
            max_queue_timeout: dur("10m"),
        }
    }
}

/// `BuildKitPool` status, written by the leader.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BuildKitPoolStatus {
    /// Generation of the spec that this status describes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// Number of free pods that are Ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm: Option<i32>,
    /// Number of free pods that are not Ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starting: Option<i32>,
    /// Number of pods that carry an allocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocated: Option<i32>,
    /// Minimum number of warm pods in effect now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_warm: Option<i32>,
    /// Number of pending claims of this pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<i32>,
    /// Platforms from `spec.platforms`, or derived from the template's arch selector.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
    /// Edge endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// TLS server name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    /// Conditions (`Ready`, `Degraded`, `CAExpiringSoon`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(extend("x-kubernetes-list-type" = "map", "x-kubernetes-list-map-keys" = ["type"]))]
    pub conditions: Vec<Condition>,
}

/// Condition types.
pub mod condition {
    /// The spec and CA are valid and the sandbox objects are applied.
    pub const READY: &str = "Ready";
    /// A failure or backoff is active.
    pub const DEGRADED: &str = "Degraded";
    /// True from 365 days before either CA expires.
    pub const CA_EXPIRING_SOON: &str = "CAExpiringSoon";
}

/// Condition reasons.
pub mod reason {
    /// `Ready=True`.
    pub const READY: &str = "Ready";
    /// The controller's validation failed on the time zone.
    pub const INVALID_SPEC: &str = "InvalidSpec";
    /// The template sets a controller-owned key, name, path or arch.
    pub const TEMPLATE_CONFLICT: &str = "TemplateConflict";
    /// The controller has not loaded a valid CA yet.
    pub const CA_UNAVAILABLE: &str = "CAUnavailable";
    /// The worker `ServiceAccount` or `NetworkPolicy` apply failed.
    pub const SANDBOX_PENDING: &str = "SandboxPending";
    /// `Degraded=False`. There is no failure and no create backoff.
    pub const AS_EXPECTED: &str = "AsExpected";
    /// A pod create failed.
    pub const POD_CREATE_FAILED: &str = "PodCreateFailed";
    /// A pod was not Ready within `startTimeout`.
    pub const POD_START_TIMEOUT: &str = "PodStartTimeout";
    /// A pod reached phase `Failed` or `Succeeded`.
    pub const POD_FAILED: &str = "PodFailed";
    /// An `oidc` rule names an unknown issuer.
    pub const UNKNOWN_ISSUER: &str = "UnknownIssuer";
    /// A slot name is held by a pod the pool does not own.
    pub const SLOT_TAKEN: &str = "SlotTaken";
    /// The CA Secret was hard-replaced and pods under the old CA remain.
    pub const CA_REPLACED: &str = "CAReplaced";
    /// `CAExpiringSoon=True`.
    pub const CA_EXPIRING_SOON: &str = "CAExpiringSoon";
}

/// A canonical CIDR range: an IPv4 or IPv6 prefix with no bits set beyond the prefix
/// length, and not an IPv4-mapped IPv6 address. The `NetworkPolicy` API refuses host
/// bits, and the Kubernetes CEL `cidr()` refuses IPv4-mapped addresses.
#[must_use]
pub fn is_canonical_cidr(s: &str) -> bool {
    let Some((addr, prefix)) = s.split_once('/') else {
        return false;
    };
    // Digits only, and no leading zero: the API server refuses "/08" and "/00".
    let digits = !prefix.is_empty() && prefix.bytes().all(|b| b.is_ascii_digit());
    if !digits || (prefix.len() > 1 && prefix.starts_with('0')) {
        return false;
    }
    let Ok(p) = prefix.parse::<u32>() else {
        return false;
    };
    match addr.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(a)) => p <= 32 && u32::from(a).checked_shl(p).unwrap_or(0) == 0,
        Ok(std::net::IpAddr::V6(a)) => {
            p <= 128
                && a.to_ipv4_mapped().is_none()
                && u128::from(a).checked_shl(p).unwrap_or(0) == 0
        }
        Err(_) => false,
    }
}

/// Root-level CEL rule. kube-derive's `validation` attribute needs the `kube`
/// facade crate, which this crate must not depend on, so this crate injects the rule itself.
pub const NAME_RULE: &str = "self.metadata.name.size() <= 52";
/// Message of [`NAME_RULE`].
pub const NAME_RULE_MESSAGE: &str = "pool name must be at most 52 characters";

/// The generated CRD with the root-level name-length rule applied.
///
/// # Panics
/// Does not panic in practice, because the generated CRD always has one version with a schema.
#[must_use]
pub fn crd()
-> k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition {
    use kube_core::CustomResourceExt;
    let mut crd = BuildKitPool::crd();
    if let Some(root) = crd
        .spec
        .versions
        .iter_mut()
        .find(|v| v.name == VERSION)
        .and_then(|v| v.schema.as_mut())
        .and_then(|s| s.open_api_v3_schema.as_mut())
    {
        root.x_kubernetes_validations = Some(vec![
            k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::ValidationRule {
                rule: NAME_RULE.into(),
                message: Some(NAME_RULE_MESSAGE.into()),
                ..Default::default()
            },
        ]);
    }
    crd
}
