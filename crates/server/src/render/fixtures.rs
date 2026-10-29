//! The scenario matrix behind the insta snapshots and the YAML fixtures that
//! `cargo xtask render-verify` dry-runs against a real API server.

use std::collections::BTreeMap;

use buildkit_controller_api::v1alpha1::{BuildKitPool, BuildKitPoolSpec};
use serde_json::{Value, json};

use super::{
    PodInputs, RenderConfig, RenderError, WorkerNetwork, WorkerTlsPem, ca_bundle_config_map,
    network_policy, service_account, worker_pod, worker_secret,
};

/// Namespace the pools of every scenario live in.
pub const POOL_NAMESPACE: &str = "render-verify";
/// Namespace of the (imaginary) controller.
pub const CONTROLLER_NAMESPACE: &str = "buildkit-system";
/// Name of the CA Secret the bundle `ConfigMap` is derived from.
pub const CA_SECRET_NAME: &str = "buildkit-controller-ca";

const FAKE_CERT: &str =
    "-----BEGIN CERTIFICATE-----\nZml4dHVyZS1ub3QtYS1yZWFsLWNlcnQ=\n-----END CERTIFICATE-----\n";

/// A fixture that failed to build.
#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    /// A scenario spec is not a valid pool spec.
    #[error("scenario spec: {0}")]
    Spec(#[from] serde_json::Error),
    /// Rendering failed.
    #[error(transparent)]
    Render(#[from] RenderError),
    /// YAML serialisation failed.
    #[error("yaml: {0}")]
    Yaml(String),
}

/// One rendered configuration.
#[derive(Clone, Debug)]
pub struct Scenario {
    /// Scenario name, also the pool name and the fixture file prefix.
    pub name: &'static str,
    /// The pool.
    pub pool: BuildKitPool,
    /// Per-pod inputs.
    pub inputs: PodInputs,
    /// Controller settings.
    pub config: RenderConfig,
}

/// A pool named `name` in [`POOL_NAMESPACE`] with the given spec fields.
///
/// # Errors
/// [`FixtureError::Spec`] when `spec` is not a valid `BuildKitPoolSpec`.
pub fn pool(name: &str, spec: Value) -> Result<BuildKitPool, FixtureError> {
    let spec: BuildKitPoolSpec = serde_json::from_value(spec)?;
    let mut p = BuildKitPool::new(name, spec);
    p.metadata.namespace = Some(POOL_NAMESPACE.into());
    p.metadata.uid = Some("2f6c1f4e-3b0a-4c43-9a55-5d4f0c7b1a10".into());
    Ok(p)
}

/// Default config and free-pod inputs.
#[must_use]
pub fn default_config() -> RenderConfig {
    RenderConfig {
        controller_namespace: CONTROLLER_NAMESPACE.into(),
        worker_network: WorkerNetwork::default(),
        cache_host: None,
    }
}

/// Default per-pod inputs (slot 0, free).
#[must_use]
pub fn default_inputs() -> PodInputs {
    PodInputs {
        slot: 0,
        internal_ca_fingerprint: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .into(),
        worker_cert_not_after: "2026-11-19T12:00:00Z".into(),
        nonce: "00000000000000aa".into(),
    }
}

fn scenario(name: &'static str, spec: Value) -> Result<Scenario, FixtureError> {
    Ok(Scenario {
        name,
        pool: pool(name, spec)?,
        inputs: default_inputs(),
        config: default_config(),
    })
}

fn full_template() -> Value {
    json!({
        "platforms": ["linux/amd64"],
        "template": {
            "metadata": {
                "labels": {"team": "ci", "cost-center": "1234"},
                "annotations": {"example.com/note": "full"}
            },
            "spec": {
                "nodeSelector": {"disktype": "ssd"},
                "tolerations": [{"key": "dedicated", "operator": "Equal", "value": "builds", "effect": "NoSchedule"}],
                "affinity": {"nodeAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": {"nodeSelectorTerms": [
                    {"matchExpressions": [{"key": "pool", "operator": "In", "values": ["builders"]}]}
                ]}}},
                "topologySpreadConstraints": [{"maxSkew": 1, "topologyKey": "topology.kubernetes.io/zone", "whenUnsatisfiable": "ScheduleAnyway", "labelSelector": {"matchLabels": {"app.kubernetes.io/name": "buildkitd"}}}],
                "runtimeClassName": "gvisor",
                "priorityClassName": "build-low",
                "imagePullSecrets": [{"name": "regcred"}],
                "volumes": [
                    {"name": "registry-ca", "configMap": {"name": "registry-ca"}},
                    {"name": "netrc", "secret": {"secretName": "netrc"}},
                    {"name": "scratch", "emptyDir": {"medium": "Memory", "sizeLimit": "512Mi"}},
                    {"name": "bundle", "projected": {"sources": [
                        {"configMap": {"name": "registry-ca"}},
                        {"secret": {"name": "netrc"}},
                        {"downwardAPI": {"items": [{"path": "labels", "fieldRef": {"fieldPath": "metadata.labels"}}]}}
                    ]}}
                ],
                "containers": [{
                    "name": "buildkitd",
                    "image": "registry.example.com/buildkit:v0.34.0-rootless",
                    "imagePullPolicy": "Always",
                    "resources": {
                        "requests": {"cpu": "2", "memory": "4Gi", "ephemeral-storage": "10Gi"},
                        "limits": {"cpu": "8", "memory": "16Gi", "ephemeral-storage": "100Gi"}
                    },
                    "env": [
                        {"name": "HTTPS_PROXY", "value": "http://proxy.internal:3128"},
                        {"name": "NODE_NAME", "valueFrom": {"fieldRef": {"fieldPath": "spec.nodeName"}}},
                        {"name": "NETRC_USER", "valueFrom": {"secretKeyRef": {"name": "netrc", "key": "user"}}}
                    ],
                    "volumeMounts": [
                        {"name": "registry-ca", "mountPath": "/etc/ssl/registry", "readOnly": true},
                        {"name": "netrc", "mountPath": "/home/user/.netrc", "subPath": "netrc", "readOnly": true},
                        {"name": "scratch", "mountPath": "/scratch"}
                    ]
                }]
            }
        },
        "workers": {
            "buildkitdConfig": "[worker.oci]\n  max-parallelism = 4\n",
            "cache": {"sizeLimit": "50Gi"}
        }
    })
}

/// Every scenario, in a fixed order.
///
/// # Errors
/// [`FixtureError::Spec`] when a scenario spec does not deserialize.
pub fn scenarios() -> Result<Vec<Scenario>, FixtureError> {
    // The cache registry entry, beside a registry entry of the pool's own.
    let mut cache = scenario(
        "cache-registry",
        json!({"workers": {"buildkitdConfig": "[registry.\"docker.io\"]\n  mirrors = [\"mirror.internal\"]\n"}}),
    )?;
    cache.config.cache_host = Some(format!("buildkit-registry.{CONTROLLER_NAMESPACE}.svc:5000"));
    let mut nodelocal = scenario("nodelocal-dns", json!({}))?;
    nodelocal.config.worker_network.dns.cidrs = vec!["169.254.20.10/32".into()];
    nodelocal.config.worker_network.extra_denied_cidrs = vec!["203.0.113.7/32".into()];
    let mut dual_stack = scenario("dual-stack-cluster", json!({}))?;
    dual_stack.config.worker_network.cluster_cidrs =
        vec!["34.118.224.0/20".into(), "2600:1f18:1234:5600::/56".into()];
    // allowCIDRs entries inside cluster and link-local ranges the operator permits.
    let mut permitted = scenario(
        "egress-permitted",
        json!({"network": {"egress": {"internet": "Deny", "allowCIDRs": ["169.254.20.10/32", "10.96.0.10/32"]}}}),
    )?;
    permitted.config.worker_network.cluster_cidrs = vec!["10.96.0.0/12".into()];
    permitted.config.worker_network.permitted_cidrs =
        vec!["169.254.20.10/32".into(), "10.96.0.0/24".into()];
    vec![
        scenario("default", json!({})),
        scenario("full-template", full_template()),
        scenario(
            "egress-deny",
            json!({"network": {"egress": {"internet": "Deny"}}}),
        ),
        scenario(
            "egress-allow-list",
            json!({"network": {"egress": {"internet": "Deny", "allowCIDRs": ["10.20.0.0/16", "192.0.2.10/32", "2001:db8::/32"]}}}),
        ),
        scenario(
            "egress-allow-plus-list",
            json!({"network": {"egress": {"internet": "Allow", "allowCIDRs": ["10.20.0.0/16"]}}}),
        ),
        scenario("platform-arm64", json!({"platforms": ["linux/arm64/v8"]})),
        scenario(
            "platform-multi",
            json!({"platforms": ["linux/amd64", "linux/arm64"]}),
        ),
        scenario(
            "template-arch-selector",
            json!({"platforms": ["linux/amd64"], "template": {"spec": {"nodeSelector": {"kubernetes.io/arch": "amd64"}}}}),
        ),
        Ok(cache),
        Ok(nodelocal),
        Ok(dual_stack),
        Ok(permitted),
    ]
    .into_iter()
    .collect()
}

/// Placeholder TLS material; the API server never inspects it.
#[must_use]
pub fn fake_tls() -> WorkerTlsPem {
    WorkerTlsPem {
        cert: FAKE_CERT.into(),
        key: "fixture-key-not-a-real-key\n".into(),
        ca: FAKE_CERT.into(),
        client_cert: FAKE_CERT.into(),
        client_key: "fixture-client-key-not-a-real-key\n".into(),
    }
}

fn yaml<T: serde::Serialize>(o: &T) -> Result<String, FixtureError> {
    serde_saphyr::to_string(o).map_err(|e| FixtureError::Yaml(e.to_string()))
}

/// The pod, Secret, `ServiceAccount` and `NetworkPolicy` of one scenario as
/// `(kind, yaml)`; `kind` is the fixture file suffix.
pub fn render_scenario(s: &Scenario) -> Result<Vec<(&'static str, String)>, FixtureError> {
    Ok(vec![
        ("pod", yaml(&worker_pod(&s.pool, &s.inputs)?)?),
        (
            "secret",
            yaml(&worker_secret(
                &s.pool,
                &s.inputs,
                &fake_tls(),
                s.config.cache_host.as_deref(),
            )?)?,
        ),
        ("serviceaccount", yaml(&service_account(&s.pool)?)?),
        ("networkpolicy", yaml(&network_policy(&s.pool, &s.config)?)?),
    ])
}

/// Every fixture as `(file name, yaml)`, including the CA bundle `ConfigMap`.
pub fn render_all() -> Result<BTreeMap<String, String>, FixtureError> {
    let mut out = BTreeMap::new();
    for s in scenarios()? {
        for (kind, y) in render_scenario(&s)? {
            out.insert(format!("{}.{kind}.yaml", s.name), y);
        }
    }
    out.insert(
        "ca-bundle.configmap.yaml".into(),
        yaml(&ca_bundle_config_map(
            CA_SECRET_NAME,
            &default_config(),
            FAKE_CERT,
        ))?,
    );
    Ok(out)
}
