//! Per-pool sandbox objects, the per-pod worker Secret and the
//! CA bundle `ConfigMap`.

use std::collections::BTreeMap;

use buildkit_controller_api::v1alpha1::BuildKitPool;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::{ConfigMap, Secret, ServiceAccount};
use k8s_openapi::api::networking::v1::{
    IPBlock, NetworkPolicy, NetworkPolicyEgressRule, NetworkPolicyIngressRule, NetworkPolicyPeer,
    NetworkPolicyPort, NetworkPolicySpec,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use buildkit_controller_api::v1alpha1::Internet;

use super::{
    BUILDKIT_PORT, CONTROLLER, LABEL_MANAGED_BY, LABEL_NAME, LABEL_POOL, PodInputs, RenderConfig,
    RenderError, WorkerNetwork, WorkerTlsPem, owner_ref, pool_identity, worker_object_name,
    worker_pod_name, worker_secret_name,
};

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// Metadata shared by the pool's sandbox objects, with the name `<pool>-buildkit-worker`,
/// pool namespace, controller labels, ownerReference to the pool.
fn pool_meta(pool: &BuildKitPool) -> Result<ObjectMeta, RenderError> {
    let (name, ns, uid) = pool_identity(pool)?;
    Ok(ObjectMeta {
        name: Some(worker_object_name(name)),
        namespace: Some(ns.to_owned()),
        labels: Some(labels(&[
            (LABEL_MANAGED_BY, CONTROLLER),
            (LABEL_POOL, name),
        ])),
        owner_references: Some(vec![owner_ref(name, uid)]),
        ..ObjectMeta::default()
    })
}

/// The per-pod Secret `<pod>-<nonce>` of the pod [`super::worker_pod`] renders
/// from the same `inputs`: `tls.crt`, `tls.key`, `ca.crt`, `buildkitd.toml`.
/// It carries the pool's ownerReference as rendered; [`crate::reconcile::spawn`]
/// replaces it with the pod's before the Secret is created.
pub fn worker_secret(
    pool: &BuildKitPool,
    inputs: &PodInputs,
    tls: &WorkerTlsPem,
) -> Result<Secret, RenderError> {
    let bytes = |s: &str| ByteString(s.as_bytes().to_vec());
    let mut metadata = pool_meta(pool)?;
    let pod = worker_pod_name(pool_identity(pool)?.0, inputs.slot);
    metadata.name = Some(worker_secret_name(&pod, &inputs.nonce));
    Ok(Secret {
        metadata,
        type_: Some("Opaque".into()),
        data: Some(BTreeMap::from([
            ("tls.crt".to_owned(), bytes(&tls.cert)),
            ("tls.key".to_owned(), bytes(&tls.key)),
            ("ca.crt".to_owned(), bytes(&tls.ca)),
            (
                "buildkitd.toml".to_owned(),
                bytes(&super::buildkitd_config(pool)),
            ),
        ])),
        ..Secret::default()
    })
}

/// `ServiceAccount` `<pool>-buildkit-worker`: no token, no `RoleBinding`.
pub fn service_account(pool: &BuildKitPool) -> Result<ServiceAccount, RenderError> {
    Ok(ServiceAccount {
        metadata: pool_meta(pool)?,
        automount_service_account_token: Some(false),
        ..ServiceAccount::default()
    })
}

fn selector(pairs: BTreeMap<String, String>) -> LabelSelector {
    LabelSelector {
        match_labels: Some(pairs),
        match_expressions: None,
    }
}

fn port(protocol: &str, n: i32) -> NetworkPolicyPort {
    NetworkPolicyPort {
        protocol: Some(protocol.to_owned()),
        port: Some(IntOrString::Int(n)),
        end_port: None,
    }
}

fn block(cidr: &str, except: Option<Vec<String>>) -> NetworkPolicyPeer {
    NetworkPolicyPeer {
        ip_block: Some(IPBlock {
            cidr: cidr.to_owned(),
            except,
        }),
        ..NetworkPolicyPeer::default()
    }
}

fn to(
    peers: Vec<NetworkPolicyPeer>,
    ports: Option<Vec<NetworkPolicyPort>>,
) -> NetworkPolicyEgressRule {
    NetworkPolicyEgressRule {
        to: Some(peers),
        ports,
    }
}

fn dns_rule(net: &WorkerNetwork) -> Option<NetworkPolicyEgressRule> {
    let mut peers = Vec::new();
    if !net.dns.pod_labels.is_empty() {
        peers.push(NetworkPolicyPeer {
            namespace_selector: Some(selector(labels(&[(
                "kubernetes.io/metadata.name",
                &net.dns.namespace,
            )]))),
            pod_selector: Some(selector(net.dns.pod_labels.clone())),
            ip_block: None,
        });
    }
    peers.extend(net.dns.cidrs.iter().map(|c| block(c, None)));
    (!peers.is_empty()).then(|| to(peers, Some(vec![port("UDP", 53), port("TCP", 53)])))
}

/// `NetworkPolicy` `<pool>-buildkit-worker`.
///
/// Leaves out every `allowCIDRs` entry [`super::refused_allow_cidr`] refuses, so the
/// policy never reopens a link-local, metadata, loopback, cluster or extra denied
/// range. Applied while the pool is `InvalidSpec`, it narrows the policy of
/// pods that run under an older, wider one.
pub fn network_policy(
    pool: &BuildKitPool,
    cfg: &RenderConfig,
) -> Result<NetworkPolicy, RenderError> {
    let (name, _, _) = pool_identity(pool)?;
    let net = &cfg.worker_network;
    let mut egress = Vec::new();
    egress.extend(dns_rule(net));
    let spec = &pool.spec.network.egress;
    if spec.internet == Internet::Allow {
        let (v6, v4): (Vec<&String>, Vec<&String>) = net
            .denied_cidrs
            .iter()
            .chain(&net.extra_denied_cidrs)
            .chain(&net.cluster_cidrs)
            .partition(|c| c.contains(':'));
        let except = |v: Vec<&String>| (!v.is_empty()).then(|| v.into_iter().cloned().collect());
        egress.push(to(vec![block("0.0.0.0/0", except(v4))], None));
        // IPv6 opens only once the operator names the cluster's IPv6 ranges (see WorkerNetwork::cluster_cidrs).
        if net.cluster_cidrs.iter().any(|c| c.contains(':')) {
            egress.push(to(vec![block("::/0", except(v6))], None));
        }
    }
    egress.extend(
        spec.allow_cidrs
            .iter()
            .filter(|c| super::refused_allow_cidr(c, net).is_none())
            .map(|c| to(vec![block(c, None)], None)),
    );

    Ok(NetworkPolicy {
        metadata: pool_meta(pool)?,
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(selector(labels(&[
                (LABEL_POOL, name),
                (LABEL_MANAGED_BY, CONTROLLER),
            ]))),
            policy_types: Some(vec!["Ingress".into(), "Egress".into()]),
            ingress: Some(vec![NetworkPolicyIngressRule {
                from: Some(vec![NetworkPolicyPeer {
                    namespace_selector: Some(selector(labels(&[(
                        "kubernetes.io/metadata.name",
                        &cfg.controller_namespace,
                    )]))),
                    pod_selector: Some(selector(labels(&[(LABEL_NAME, CONTROLLER)]))),
                    ip_block: None,
                }]),
                ports: Some(vec![port("TCP", BUILDKIT_PORT)]),
            }]),
            egress: Some(egress),
        }),
    })
}

/// `ConfigMap` `<caSecretName>-bundle` in the controller namespace: `ca.crt` is
/// the client CA certificate only.
#[must_use]
pub fn ca_bundle_config_map(
    ca_secret_name: &str,
    cfg: &RenderConfig,
    client_ca_pem: &str,
) -> ConfigMap {
    ConfigMap {
        metadata: ObjectMeta {
            name: Some(format!("{ca_secret_name}-bundle")),
            namespace: Some(cfg.controller_namespace.clone()),
            labels: Some(labels(&[(LABEL_MANAGED_BY, CONTROLLER)])),
            ..ObjectMeta::default()
        },
        data: Some(BTreeMap::from([(
            "ca.crt".to_owned(),
            client_ca_pem.to_owned(),
        )])),
        ..ConfigMap::default()
    }
}
