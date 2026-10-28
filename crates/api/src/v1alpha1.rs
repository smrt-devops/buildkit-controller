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
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
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
/// Largest value accepted for `scaling.min`, `scaling.max` and window `min`.
pub const MAX_SCALE: i32 = 500;

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
    /// Cache volume settings.
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
    /// Size limit of the `emptyDir`. When unset, the ephemeral-storage limit bounds it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_limit: Option<Quantity>,
    /// Persistent state volumes for cache affinity. Unset turns the
    /// feature off. A volume is shared only by callers of one cache scope, see `local.scope`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "non_null::<LocalCache>")]
    pub local: Option<LocalCache>,
    /// Shared registry cache that clients import and export. Every caller
    /// this pool admits, and anyone who can push to the repository, can write cache that
    /// later callers read.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "non_null::<RegistryCache>")]
    pub registry: Option<RegistryCache>,
}

/// `spec.workers.cache.local`: one PVC per cached slot, mounted at the buildkitd state dir.
///
/// A volume holds the whole buildkitd state: the content store, the cache records and
/// any image built on it. Each volume belongs to one cache scope (`scope`). A pod whose
/// caller is in another scope never gets the volume as it is: the volume is emptied
/// before buildkitd starts. buildkitd runs with build history off
/// (`[history] maxEntries = 0`) and `history.db` is deleted at every start, so no
/// build logs or build arguments reach a later allocation and `buildx history`
/// shows nothing on these pools.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
#[schemars(extend("x-kubernetes-validations" = [{"rule": "!has(self.gcKeepStorage) || quantity(string(self.gcKeepStorage)).compareTo(quantity(string(self.size))) <= 0", "message": "gcKeepStorage must not exceed size"}]))]
pub struct LocalCache {
    /// Storage request of each volume, from 1Gi to 10Ti.
    #[schemars(schema_with = "plain_quantity", extend("x-kubernetes-validations" = [{"rule": "quantity(string(self)).compareTo(quantity('1Gi')) >= 0 && quantity(string(self)).compareTo(quantity('10Ti')) <= 0", "message": "size must be between 1Gi and 10Ti"}]))]
    pub size: Quantity,
    /// Storage class of the volumes; unset uses the cluster default. Use one with
    /// `volumeBindingMode: WaitForFirstConsumer`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 253), extend("pattern" = r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*$"))]
    pub storage_class: Option<String>,
    /// Slots `0..min(maxVolumes, scaling.max)` get a volume; other slots use an `emptyDir`.
    #[schemars(range(min = 1, max = 500))]
    pub max_volumes: i32,
    /// `soft` prefers a free pod whose volume served the request's key; `off` ignores keys.
    pub affinity: CacheAffinity,
    /// Recent keys tracked per volume in the `cache-keys` annotation.
    #[schemars(range(min = 1, max = 32))]
    pub keys_per_volume: i32,
    /// buildkitd GC bound (`[worker.oci] maxUsedSpace`). Unset uses 80% of `size`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "opt_quantity")]
    pub gc_keep_storage: Option<Quantity>,
    /// Who shares a volume. `identity` (the default) gives each caller its own cache. An
    /// OIDC token is scoped to its issuer and its `repository_id` claim (GitHub Actions),
    /// else its `repository` claim, else its `sub`. A `ServiceAccount` is scoped to its
    /// namespace and name, and a `BuildKitClaim` to its namespace. `pool` lets every caller
    /// the pool admits read and write the same volumes, so use it only when they are one
    /// trust domain.
    pub scope: CacheScope,
}

impl Default for LocalCache {
    fn default() -> Self {
        Self {
            size: Quantity("20Gi".to_owned()),
            storage_class: None,
            max_volumes: 10,
            affinity: CacheAffinity::Soft,
            keys_per_volume: 8,
            gc_keep_storage: None,
            scope: CacheScope::Identity,
        }
    }
}

/// `spec.workers.cache.registry`: a shared registry cache that clients import and export
///. Every caller this pool admits can write cache that later callers read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RegistryCache {
    /// Image name of the cache, without tag or digest, for example `ghcr.io/org/cache`.
    #[schemars(length(min = 1, max = 255), extend("x-kubernetes-validations" = [{"rule": "self.matches(r'^[a-z0-9]([a-z0-9._-]*[a-z0-9])?(:[0-9]{1,5})?(/[a-z0-9]([a-z0-9._-]*[a-z0-9])?)*$')", "message": "repository must be an image name without tag or digest"}]))]
    pub repository: String,
    /// Whether allocations return the registry cache flags.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Export mode of `--cache-to`.
    #[serde(default)]
    pub mode: CacheMode,
    /// Ref template. Placeholders: `{repository}`, `{cacheKey}`, `{pool}`.
    #[serde(default = "default_ref", rename = "ref")]
    #[schemars(length(min = 1, max = 255), extend("x-kubernetes-validations" = [{"rule": "self.replace('{repository}', 'r.example/x').replace('{cacheKey}', 'k').replace('{pool}', 'p').matches(r'^[a-z0-9]([a-z0-9._-]*[a-z0-9])?(:[0-9]{1,5})?(/[a-z0-9]([a-z0-9._-]*[a-z0-9])?)*(:[A-Za-z0-9_][A-Za-z0-9_.-]{0,127})?$')", "message": "ref may use only {repository}, {cacheKey} and {pool} and must render to an image reference"}]))]
    pub ref_template: String,
    /// Key for requests that name no `cacheKey`. Unset uses the pool name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 63), extend("pattern" = r"^[A-Za-z0-9]([A-Za-z0-9._-]*[A-Za-z0-9])?$"))]
    pub default_key: Option<String>,
    /// Read-only fallbacks after the request's own ref: a cache key, rendered through `ref`,
    /// or an image reference (an entry with `/` or `:`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 8), schema_with = "import_from")]
    pub import_from: Vec<String>,
}

fn yes() -> bool {
    true
}

fn default_ref() -> String {
    "{repository}:{cacheKey}".to_owned()
}

fn import_from(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "array",
        "maxItems": 8,
        "items": {"type": "string", "minLength": 1, "maxLength": 255},
        "x-kubernetes-validations": [
            {"rule": "self.all(e, (size(e) <= 63 && e.matches(r'^[A-Za-z0-9]([A-Za-z0-9._-]*[A-Za-z0-9])?$')) || ((e.contains('/') || e.contains(':')) && e.matches(r'^[a-z0-9]([a-z0-9._-]*[a-z0-9])?(:[0-9]{1,5})?(/[a-z0-9]([a-z0-9._-]*[a-z0-9])?)*(:[A-Za-z0-9_][A-Za-z0-9_.-]{0,127})?(@sha256:[a-f0-9]{64})?$')))", "message": "importFrom entries must be cache keys or image references"}
        ]
    })
}

/// `cache.registry.mode`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CacheMode {
    /// Export every intermediate layer.
    #[default]
    Max,
    /// Export only the final image's layers.
    Min,
}

impl CacheMode {
    /// The buildx `mode=` value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Max => "max",
            Self::Min => "min",
        }
    }
}

/// `cache.local.scope`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CacheScope {
    /// One cache per caller identity.
    #[default]
    Identity,
    /// One cache for every caller the pool admits.
    Pool,
}

/// `cache.local.affinity`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CacheAffinity {
    /// Prefer a free pod whose volume served the key; spill to any free pod.
    #[default]
    Soft,
    /// Ignore keys; prefer any cached slot.
    Off,
}

/// Resolved buildx flags of the registry cache.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RegistryCacheFlags {
    /// `--cache-to` value.
    pub to: String,
    /// `--cache-from` values, in order.
    pub from: Vec<String>,
}

/// `status.cache`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CacheStatus {
    /// Registry flags for `registry.defaultKey`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<RegistryCacheFlags>,
}

impl RegistryCache {
    /// `ref` with its placeholders filled in.
    #[must_use]
    pub fn render_ref(&self, pool: &str, key: &str) -> String {
        self.ref_template
            .replace("{repository}", &self.repository)
            .replace("{cacheKey}", key)
            .replace("{pool}", pool)
    }

    /// `--cache-to` and `--cache-from` values for `key`.
    #[must_use]
    pub fn resolve(&self, pool: &str, key: &str) -> RegistryCacheFlags {
        let own = self.render_ref(pool, key);
        let mut refs = vec![own.clone()];
        for e in &self.import_from {
            let r = if e.contains(['/', ':']) {
                e.clone()
            } else {
                self.render_ref(pool, e)
            };
            if !refs.contains(&r) {
                refs.push(r);
            }
        }
        RegistryCacheFlags {
            to: format!(
                "type=registry,ref={own},mode={},image-manifest=true,oci-mediatypes=true",
                self.mode.as_str()
            ),
            from: refs
                .iter()
                .map(|r| format!("type=registry,ref={r}"))
                .collect(),
        }
    }
}

impl Cache {
    /// The registry layer when it is set and enabled.
    #[must_use]
    pub fn registry_enabled(&self) -> Option<&RegistryCache> {
        self.registry.as_ref().filter(|r| r.enabled)
    }

    /// Key of a request without `cacheKey`: `registry.defaultKey`, else the pool name.
    #[must_use]
    pub fn default_key<'a>(&'a self, pool: &'a str) -> &'a str {
        self.registry
            .as_ref()
            .and_then(|r| r.default_key.as_deref())
            .unwrap_or(pool)
    }
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

fn is_name_component(s: &str) -> bool {
    let b = s.as_bytes();
    let alnum = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    b.first().is_some_and(alnum)
        && b.last().is_some_and(alnum)
        && b.iter()
            .all(|c| alnum(c) || matches!(c, b'.' | b'_' | b'-'))
}

/// An image name without tag or digest.
#[must_use]
pub fn is_image_name(s: &str) -> bool {
    let mut parts = s.split('/');
    let host = parts.next().unwrap_or_default();
    let host_ok = match host.split_once(':') {
        Some((h, port)) => {
            is_name_component(h)
                && (1..=5).contains(&port.len())
                && port.bytes().all(|c| c.is_ascii_digit())
        }
        None => is_name_component(host),
    };
    s.len() <= 255 && host_ok && parts.all(is_name_component)
}

/// An image name with an optional tag and an optional `@sha256:` digest.
#[must_use]
pub fn is_image_ref(s: &str) -> bool {
    let (rest, digest_ok) = match s.split_once('@') {
        Some((r, d)) => (
            r,
            d.strip_prefix("sha256:").is_some_and(|h| {
                h.len() == 64
                    && h.bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            }),
        ),
        None => (s, true),
    };
    let slash = rest.rfind('/').map_or(0, |i| i + 1);
    let (name, tag) = match rest[slash..].rfind(':') {
        Some(i) if slash > 0 || !is_image_name(rest) => {
            (&rest[..slash + i], Some(&rest[slash + i + 1..]))
        }
        _ => (rest, None),
    };
    let tag_ok = tag.is_none_or(|t| {
        let b = t.as_bytes();
        (1..=128).contains(&b.len())
            && (b[0].is_ascii_alphanumeric() || b[0] == b'_')
            && b.iter()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
    });
    digest_ok && tag_ok && is_image_name(name)
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
    /// CIDR ranges re-opened through the egress deny list (for example a private registry or proxy), so they apply to every pool, not just under `Deny`.
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
/// A Quantity that may not be null (kube-core marks optional int-or-string fields nullable
/// unless `nullable` is already set), so a `null` cannot skip
/// the default and the CEL rule.
fn plain_quantity(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"x-kubernetes-int-or-string": true, "nullable": false})
}

fn opt_quantity(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"x-kubernetes-int-or-string": true})
}

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

/// `BuildKitPool` status. The leader owns every field except `waiters`, which
/// each API replica owns for its own entry.
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
    /// Sum of the non-stale `waiters[].count` values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<i32>,
    /// Queued allocation requests per API replica.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64), extend("x-kubernetes-list-type" = "map", "x-kubernetes-list-map-keys" = ["replica"]))]
    pub waiters: Vec<Waiter>,
    /// Platforms from `spec.platforms`, or derived from the template's arch selector.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
    /// Edge endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// TLS server name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    /// Resolved cache flags for the pool's default key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheStatus>,
    /// Conditions (`Ready`, `Degraded`, `CAExpiringSoon`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(extend("x-kubernetes-list-type" = "map", "x-kubernetes-list-map-keys" = ["type"]))]
    pub conditions: Vec<Condition>,
}

/// One API replica's queue length (`status.waiters`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Waiter {
    /// Replica (pod) name. It is the list key.
    pub replica: String,
    /// Queued POSTs on that replica.
    pub count: i32,
    /// When the replica last wrote the entry. Entries older than 90 s are stale.
    pub updated_at: Time,
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

fn bad(field: &str, reason: impl Into<String>) -> ApiError {
    ApiError::Invalid {
        field: field.to_owned(),
        reason: reason.into(),
    }
}

fn validate_registry(r: &RegistryCache) -> Result<(), ApiError> {
    const P: &str = "spec.workers.cache.registry";
    if !is_image_name(&r.repository) {
        return Err(bad(
            &format!("{P}.repository"),
            "must be an image name without tag or digest",
        ));
    }
    let sample = r
        .ref_template
        .replace("{repository}", "r.example/x")
        .replace("{cacheKey}", "k")
        .replace("{pool}", "p");
    if r.ref_template.len() > 255 || sample.contains('@') || !is_image_ref(&sample) {
        return Err(bad(
            &format!("{P}.ref"),
            "may use only {repository}, {cacheKey} and {pool} and must render to an image reference",
        ));
    }
    if r.default_key.as_deref().is_some_and(|k| !is_cache_key(k)) {
        return Err(bad(&format!("{P}.defaultKey"), "must be a cache key"));
    }
    let entry_ok = |e: &String| {
        is_cache_key(e) || (e.len() <= 255 && e.contains(['/', ':']) && is_image_ref(e))
    };
    if r.import_from.len() > 8 || !r.import_from.iter().all(entry_ok) {
        return Err(bad(
            &format!("{P}.importFrom"),
            "at most 8 cache keys or image references",
        ));
    }
    Ok(())
}

fn is_label(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=63).contains(&b.len())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
}

fn is_subdomain(s: &str) -> bool {
    s.len() <= 253 && s.split('.').all(is_label)
}

/// An absolute, normalised path, as the CEL rule on `volumeMounts` requires. It has a leading
/// `/` followed by segments that are non-empty and neither `.` nor `..`. Such a
/// path is already clean, so prefix checks on it match what the runtime mounts.
fn is_clean_abs_path(p: &str) -> bool {
    p.chars().count() <= MAX_MOUNT_PATH
        && p.strip_prefix('/')
            .is_some_and(|rest| rest.split('/').all(|s| !matches!(s, "" | "." | "..")))
}

fn is_platform(s: &str) -> bool {
    let part = |p: &str| {
        !p.is_empty()
            && p.bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    };
    let mut it = s.split('/');
    let (Some(os), Some(arch), variant, None) = (it.next(), it.next(), it.next(), it.next()) else {
        return false;
    };
    part(os)
        && part(arch)
        && variant.is_none_or(|v| {
            v.strip_prefix('v')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
        })
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

/// Exactly one flag is set.
fn exactly_one(flags: &[bool]) -> bool {
    flags.iter().filter(|f| **f).count() == 1
}

/// A C0 control character or DEL, which the CEL rule on `volumeMounts` refuses.
fn has_control(s: &str) -> bool {
    s.bytes().any(|b| b < 0x20 || b == 0x7f)
}

fn is_hhmm(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 5
        && b[2] == b':'
        && b[..2].iter().chain(&b[3..]).all(u8::is_ascii_digit)
        && (b[0] < b'2' || (b[0] == b'2' && b[1] <= b'3'))
        && b[3] <= b'5'
}

fn check_seconds(field: &str, d: &Duration, lo: u64, hi: u64) -> Result<u64, ApiError> {
    let s = d
        .seconds()
        .map_err(|_| bad(field, "must match ^([0-9]+h)?([0-9]+m)?([0-9]+s)?$"))?;
    if s < lo || s > hi {
        return Err(bad(field, format!("must be between {lo}s and {hi}s")));
    }
    Ok(s)
}

impl BuildKitPoolSpec {
    /// Validates everything the CRD schema and CEL rules enforce and the controller relies on.
    /// Callers that skip the API server use this method.
    /// The controller, not this method, checks the IANA time zone.
    pub fn validate(&self) -> Result<(), ApiError> {
        self.validate_platforms()?;
        self.validate_template()?;
        self.validate_workers()?;
        self.validate_network()?;
        self.validate_scaling()?;
        self.validate_access()?;
        self.validate_allocation()
    }

    fn validate_workers(&self) -> Result<(), ApiError> {
        let w = &self.workers;
        if w.buildkitd_config.chars().count() > 65536 {
            return Err(bad(
                "spec.workers.buildkitdConfig",
                "at most 65536 characters",
            ));
        }
        check_seconds("spec.workers.startTimeout", &w.start_timeout, 10, 30 * 60)?;
        if let Some(l) = &w.cache.local
            && !(1..=500).contains(&l.max_volumes)
        {
            return Err(bad(
                "spec.workers.cache.local.maxVolumes",
                "must be between 1 and 500",
            ));
        }
        if let Some(l) = &w.cache.local
            && !(1..=32).contains(&l.keys_per_volume)
        {
            return Err(bad(
                "spec.workers.cache.local.keysPerVolume",
                "must be between 1 and 32",
            ));
        }
        if let Some(r) = &w.cache.registry {
            validate_registry(r)?;
        }
        Ok(())
    }

    fn validate_platforms(&self) -> Result<(), ApiError> {
        let p = &self.platforms;
        if p.len() > 8 || (1..p.len()).any(|i| p[..i].contains(&p[i])) {
            return Err(bad("spec.platforms", "at most 8, unique"));
        }
        if p.iter().any(|s| !is_platform(s)) {
            return Err(bad("spec.platforms[]", "must match os/arch[/vN]"));
        }
        Ok(())
    }

    fn validate_network(&self) -> Result<(), ApiError> {
        let c = &self.network.egress.allow_cidrs;
        if c.len() > 32 || c.iter().any(|s| !is_canonical_cidr(s)) {
            return Err(bad(
                "spec.network.egress.allowCIDRs",
                "at most 32 CIDR ranges with no bits set beyond the prefix length",
            ));
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn validate_template(&self) -> Result<(), ApiError> {
        let t = &self.template;
        let reserved_key = |k: &String| {
            RESERVED_LABEL_KEYS.contains(&k.as_str()) || k.starts_with(RESERVED_KEY_PREFIX)
        };
        if t.metadata.labels.keys().any(reserved_key) {
            return Err(bad(
                "spec.template.metadata.labels",
                "no controller-owned keys",
            ));
        }
        if t.metadata
            .annotations
            .keys()
            .any(|k| k.starts_with(RESERVED_KEY_PREFIX))
        {
            return Err(bad(
                "spec.template.metadata.annotations",
                "no controller-owned prefix",
            ));
        }
        let s = &t.spec;
        if s.node_selector.len() > 16 {
            return Err(bad("spec.template.spec.nodeSelector", "at most 16 entries"));
        }
        if s.tolerations.len() > 16 {
            return Err(bad("spec.template.spec.tolerations", "at most 16"));
        }
        if s.topology_spread_constraints.len() > 8 {
            return Err(bad(
                "spec.template.spec.topologySpreadConstraints",
                "at most 8",
            ));
        }
        if s.image_pull_secrets.len() > 8 {
            return Err(bad("spec.template.spec.imagePullSecrets", "at most 8"));
        }
        for (field, v) in [
            ("runtimeClassName", &s.runtime_class_name),
            ("priorityClassName", &s.priority_class_name),
        ] {
            if v.as_deref().is_some_and(|n| !is_subdomain(n)) {
                return Err(bad(
                    &format!("spec.template.spec.{field}"),
                    "must be a DNS-1123 subdomain",
                ));
            }
        }
        if s.priority_class_name
            .as_deref()
            .is_some_and(|n| n.starts_with("system-"))
        {
            return Err(bad(
                "spec.template.spec.priorityClassName",
                "must not start with system-",
            ));
        }
        let one_source = |v: &Volume| {
            v.projected
                .as_ref()
                .and_then(|p| p.sources.as_ref())
                .is_none_or(|ss| {
                    ss.len() <= MAX_PROJECTED_SOURCES
                        && ss.iter().all(|s| {
                            exactly_one(&[
                                s.config_map.is_some(),
                                s.secret.is_some(),
                                s.downward_api.is_some(),
                            ])
                        })
                })
        };
        if !s.volumes.iter().all(one_source) {
            return Err(bad(
                "spec.template.spec.volumes[].projected.sources",
                "at most 16, each sets exactly one of configMap, secret or downwardAPI",
            ));
        }
        if s.volumes.len() > MAX_VOLUMES
            || s.volumes.iter().any(|v| {
                RESERVED_VOLUMES.contains(&v.name.as_str())
                    || v.name.chars().count() > MAX_VOLUME_NAME
            })
        {
            return Err(bad(
                "spec.template.spec.volumes",
                "at most 16, names certs and state are reserved",
            ));
        }
        let [c] = s.containers.as_slice() else {
            return Err(bad(
                "spec.template.spec.containers",
                "exactly one container named buildkitd",
            ));
        };
        if c.name != CONTAINER_NAME {
            return Err(bad(
                "spec.template.spec.containers[0].name",
                "must be buildkitd",
            ));
        }
        if !(1..=512).contains(&c.image.chars().count()) {
            return Err(bad(
                "spec.template.spec.containers[0].image",
                "length must be 1..512 characters",
            ));
        }
        let limits = c.resources.limits.as_ref();
        if !["memory", "ephemeral-storage"]
            .iter()
            .all(|k| limits.is_some_and(|l| l.contains_key(*k)))
        {
            return Err(bad(
                "spec.template.spec.containers[0].resources.limits",
                "must set memory and ephemeral-storage",
            ));
        }
        if c.env.len() > MAX_ENV
            || c.env.iter().any(|e| {
                e.name.chars().count() > MAX_ENV_NAME
                    || RESERVED_ENV_PREFIXES.iter().any(|p| e.name.starts_with(p))
                    || e.value_from.as_ref().is_some_and(|s| {
                        !exactly_one(&[
                            s.config_map_key_ref.is_some(),
                            s.secret_key_ref.is_some(),
                            s.field_ref.is_some(),
                        ])
                    })
            })
        {
            return Err(bad(
                "spec.template.spec.containers[0].env",
                "at most 32, name at most 128, no BUILDKITD_ or ROOTLESSKIT_ names, valueFrom exactly one of configMapKeyRef, secretKeyRef or fieldRef",
            ));
        }
        if c.volume_mounts.len() > MAX_MOUNTS {
            return Err(bad(
                "spec.template.spec.containers[0].volumeMounts",
                "at most 16",
            ));
        }
        for m in &c.volume_mounts {
            if has_control(&m.mount_path)
                || m.sub_path
                    .as_deref()
                    .is_some_and(|p| has_control(p) || p.chars().count() > MAX_MOUNT_PATH)
            {
                return Err(bad(
                    "spec.template.spec.containers[0].volumeMounts[]",
                    "mountPath and subPath must have no control characters, subPath at most 256 characters",
                ));
            }
            if m.mount_propagation.as_deref() == Some("Bidirectional") {
                return Err(bad(
                    "spec.template.spec.containers[0].volumeMounts[].mountPropagation",
                    "Bidirectional needs a privileged container",
                ));
            }
            if m.name.chars().count() > MAX_VOLUME_NAME || !is_clean_abs_path(&m.mount_path) {
                return Err(bad(
                    "spec.template.spec.containers[0].volumeMounts[]",
                    "mountPath must be absolute and normalised (no empty, . or .. segment), at most 256 characters",
                ));
            }
            if !s.volumes.iter().any(|v| v.name == m.name) {
                return Err(bad(
                    "spec.template.spec.containers[0].volumeMounts[].name",
                    "must name a template volume",
                ));
            }
            if RESERVED_MOUNT_PATHS
                .iter()
                .any(|p| m.mount_path == *p || m.mount_path.starts_with(&format!("{p}/")))
            {
                return Err(bad(
                    "spec.template.spec.containers[0].volumeMounts[].mountPath",
                    "must not be under a controller-owned path",
                ));
            }
        }
        Ok(())
    }

    fn validate_scaling(&self) -> Result<(), ApiError> {
        let s = &self.scaling;
        if !(0..=MAX_SCALE).contains(&s.min) {
            return Err(bad("spec.scaling.min", "must be 0..500"));
        }
        if !(1..=MAX_SCALE).contains(&s.max) {
            return Err(bad("spec.scaling.max", "must be 1..500"));
        }
        if s.min > s.max {
            return Err(bad("spec.scaling", "min must not exceed max"));
        }
        check_seconds("spec.scaling.idleTimeout", &s.idle_timeout, 60, 24 * 3600)?;
        if s.time_zone.chars().count() > 64 {
            return Err(bad("spec.scaling.timeZone", "at most 64 characters"));
        }
        if s.schedule.len() > 32 {
            return Err(bad("spec.scaling.schedule", "at most 32 windows"));
        }
        for win in &s.schedule {
            if win.days.len() > 7
                || (1..win.days.len()).any(|i| win.days[..i].contains(&win.days[i]))
            {
                return Err(bad("spec.scaling.schedule[].days", "at most 7, unique"));
            }
            if !is_hhmm(&win.start) || !is_hhmm(&win.end) {
                return Err(bad(
                    "spec.scaling.schedule[]",
                    "start and end must be HH:MM",
                ));
            }
            if win.start == win.end {
                return Err(bad("spec.scaling.schedule[]", "start and end must differ"));
            }
            if !(0..=MAX_SCALE).contains(&win.min) || win.min > s.max {
                return Err(bad(
                    "spec.scaling.schedule[].min",
                    "must be 0..500 and not exceed scaling.max",
                ));
            }
        }

        Ok(())
    }

    fn validate_access(&self) -> Result<(), ApiError> {
        if self.access.rules.len() > 64 {
            return Err(bad("spec.access.rules", "at most 64"));
        }
        for r in &self.access.rules {
            match (&r.oidc, &r.service_account, &r.claims) {
                (None, None, Some(c)) => {
                    if !(1..=32).contains(&c.namespaces.len())
                        || c.namespaces.iter().any(|n| !is_label(n))
                    {
                        return Err(bad(
                            "spec.access.rules[].claims.namespaces",
                            "1..32 DNS-1123 labels required",
                        ));
                    }
                }
                (Some(o), None, None) => {
                    if !o.issuer.starts_with("https://") || o.issuer.chars().count() > 512 {
                        return Err(bad(
                            "spec.access.rules[].oidc.issuer",
                            "must start with https:// (max 512 characters)",
                        ));
                    }
                    if !(1..=16).contains(&o.claims.len()) {
                        return Err(bad(
                            "spec.access.rules[].oidc.claims",
                            "1..16 claims required",
                        ));
                    }
                    if o.claims
                        .values()
                        .any(|p| p.0.chars().count() > 256 || p.0.chars().all(|c| c == '*'))
                    {
                        return Err(bad(
                            "spec.access.rules[].oidc.claims",
                            "values must be at most 256 characters and contain a non-* character",
                        ));
                    }
                }
                (None, Some(sa), None) => {
                    if sa.namespace.as_deref().is_some_and(|n| !is_label(n)) {
                        return Err(bad(
                            "spec.access.rules[].serviceAccount.namespace",
                            "must be a DNS-1123 label",
                        ));
                    }
                    if sa.name.as_deref().is_some_and(|n| !is_subdomain(n)) {
                        return Err(bad(
                            "spec.access.rules[].serviceAccount.name",
                            "must be a DNS-1123 subdomain",
                        ));
                    }
                }
                _ => {
                    return Err(bad(
                        "spec.access.rules[]",
                        "exactly one of oidc, serviceAccount or claims",
                    ));
                }
            }
        }

        Ok(())
    }

    fn validate_allocation(&self) -> Result<(), ApiError> {
        let a = &self.allocation;
        let default = check_seconds("spec.allocation.defaultTTL", &a.default_ttl, 60, u64::MAX)?;
        let max = check_seconds("spec.allocation.maxTTL", &a.max_ttl, 0, 24 * 3600)?;
        if default > max {
            return Err(bad("spec.allocation", "defaultTTL must not exceed maxTTL"));
        }
        let queue = check_seconds(
            "spec.allocation.queueTimeout",
            &a.queue_timeout,
            0,
            u64::MAX,
        )?;
        let max_queue = check_seconds(
            "spec.allocation.maxQueueTimeout",
            &a.max_queue_timeout,
            0,
            3600,
        )?;
        if queue > max_queue {
            return Err(bad(
                "spec.allocation",
                "queueTimeout must not exceed maxQueueTimeout",
            ));
        }
        Ok(())
    }
}

impl BuildKitPool {
    /// Validate the pool name plus the spec; see [`BuildKitPoolSpec::validate`].
    pub fn validate(&self) -> Result<(), ApiError> {
        let name = self.metadata.name.as_deref().unwrap_or_default();
        if name.len() > MAX_POOL_NAME_LEN {
            return Err(bad(
                "metadata.name",
                "pool name must be at most 52 characters",
            ));
        }
        self.spec.validate()
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
