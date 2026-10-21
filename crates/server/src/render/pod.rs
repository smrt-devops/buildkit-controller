//! The worker pod: the user template merged with the
//! enforced fields.

use std::collections::BTreeMap;

use buildkit_controller_api::v1alpha1::{BuildKitPool, CONTAINER_NAME};
use k8s_openapi::api::core::v1::{
    AppArmorProfile, Container, ContainerPort, EmptyDirVolumeSource, ExecAction, KeyToPath, Pod,
    PodSecurityContext, PodSpec, Probe, SeccompProfile, SecretVolumeSource, SecurityContext,
    Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

use super::validate::{check, single_arch};
use super::{
    ANN_ALLOC, ANN_ALLOCATED_AT, ANN_EXPIRES_AT, ANN_INTERNAL_CA, ANN_OWNER,
    ANN_WORKER_CERT_NOT_AFTER, ANN_WORKER_NONCE, BUILDKIT_PORT, CONTROLLER, INTERNAL_CA_LABEL_LEN,
    LABEL_MANAGED_BY, LABEL_NAME, LABEL_POOL, LABEL_STATE, LABEL_TEMPLATE_HASH, PodInputs,
    RenderError, WORKER_NAME, owner_ref, pool_identity, template_hash, worker_object_name,
    worker_pod_name, worker_secret_name,
};

const SOCKET: &str = "unix:///run/user/1000/buildkit/buildkitd.sock";
const RUN_AS: i64 = 1000;
const CERTS_VOLUME: &str = "certs";
use super::cache::{STATE_VOLUME, is_cached_slot, mount_cache_volume};

fn s(v: &str) -> String {
    v.to_owned()
}

fn mount(name: &str, path: &str) -> VolumeMount {
    VolumeMount {
        name: s(name),
        mount_path: s(path),
        ..VolumeMount::default()
    }
}

fn non_empty<T>(v: Vec<T>) -> Option<Vec<T>> {
    (!v.is_empty()).then_some(v)
}

fn buildkitd_args() -> Vec<String> {
    [
        format!("--addr={SOCKET}"),
        format!("--addr=tcp://0.0.0.0:{BUILDKIT_PORT}"),
        "--tlscacert=/certs/ca.crt".into(),
        "--tlscert=/certs/tls.crt".into(),
        "--tlskey=/certs/tls.key".into(),
        "--config=/etc/buildkit/buildkitd.toml".into(),
        "--oci-worker-no-process-sandbox".into(),
    ]
    .to_vec()
}

fn container(pool: &BuildKitPool) -> Result<Container, RenderError> {
    let user = pool
        .spec
        .template
        .spec
        .containers
        .first()
        .ok_or_else(|| RenderError::Serialize("no container".into()))?;
    let mut mounts = vec![
        VolumeMount {
            read_only: Some(true),
            ..mount(CERTS_VOLUME, "/certs")
        },
        VolumeMount {
            read_only: Some(true),
            sub_path: Some(s("buildkitd.toml")),
            ..mount(CERTS_VOLUME, "/etc/buildkit/buildkitd.toml")
        },
        mount(STATE_VOLUME, "/home/user/.local/share/buildkit"),
    ];
    mounts.extend(user.volume_mounts.iter().cloned());
    Ok(Container {
        name: s(CONTAINER_NAME),
        image: Some(user.image.clone()),
        image_pull_policy: Some(format!("{:?}", user.image_pull_policy)),
        command: None,
        args: Some(buildkitd_args()),
        ports: Some(vec![ContainerPort {
            container_port: BUILDKIT_PORT,
            name: Some(s("buildkit")),
            protocol: Some(s("TCP")),
            ..ContainerPort::default()
        }]),
        security_context: Some(SecurityContext {
            privileged: Some(false),
            ..SecurityContext::default()
        }),
        readiness_probe: Some(Probe {
            exec: Some(ExecAction {
                command: Some(
                    ["buildctl", "--addr", SOCKET, "debug", "workers"]
                        .map(s)
                        .to_vec(),
                ),
            }),
            period_seconds: Some(1),
            timeout_seconds: Some(5),
            failure_threshold: Some(3),
            ..Probe::default()
        }),
        resources: Some(user.resources.clone()),
        env: non_empty(user.env.clone()),
        volume_mounts: Some(mounts),
        ..Container::default()
    })
}

fn volumes(pool: &BuildKitPool) -> Vec<Volume> {
    let item = |k: &str| KeyToPath {
        key: s(k),
        path: s(k),
        mode: None,
    };
    let mut v = vec![
        Volume {
            name: s(CERTS_VOLUME),
            secret: Some(SecretVolumeSource {
                // Per pod, `<pod>-<nonce>`; [`worker_pod`] sets it so the spec
                // (and the template hash) stays pod-independent.
                secret_name: None,
                default_mode: Some(0o440),
                items: Some(
                    ["tls.crt", "tls.key", "ca.crt", "buildkitd.toml"]
                        .map(item)
                        .to_vec(),
                ),
                optional: None,
            }),
            ..Volume::default()
        },
        Volume {
            name: s(STATE_VOLUME),
            empty_dir: Some(EmptyDirVolumeSource {
                medium: None,
                size_limit: pool.spec.workers.cache.size_limit.clone(),
            }),
            ..Volume::default()
        },
    ];
    v.extend(pool.spec.template.spec.volumes.iter().cloned());
    v
}

/// The merged pod spec. It is slot-independent, so it is what the template hash covers.
pub fn pod_spec(pool: &BuildKitPool) -> Result<PodSpec, RenderError> {
    let name = pool_name(pool)?;
    check(&pool.spec)?;
    let t = &pool.spec.template.spec;
    let mut node_selector = t.node_selector.clone();
    if let Some(arch) = single_arch(&pool.spec.platforms) {
        node_selector
            .entry(s("kubernetes.io/arch"))
            .or_insert_with(|| s(arch));
    }
    let unconfined = || s("Unconfined");
    Ok(PodSpec {
        service_account_name: Some(worker_object_name(name)),
        automount_service_account_token: Some(false),
        enable_service_links: Some(false),
        host_network: Some(false),
        host_pid: Some(false),
        host_ipc: Some(false),
        restart_policy: Some(s("Never")),
        termination_grace_period_seconds: Some(5),
        security_context: Some(PodSecurityContext {
            run_as_user: Some(RUN_AS),
            run_as_group: Some(RUN_AS),
            run_as_non_root: Some(true),
            fs_group: Some(RUN_AS),
            seccomp_profile: Some(SeccompProfile {
                type_: unconfined(),
                localhost_profile: None,
            }),
            app_armor_profile: Some(AppArmorProfile {
                type_: unconfined(),
                localhost_profile: None,
            }),
            ..PodSecurityContext::default()
        }),
        node_selector: (!node_selector.is_empty()).then_some(node_selector),
        tolerations: non_empty(t.tolerations.clone()),
        affinity: t.affinity.clone(),
        topology_spread_constraints: non_empty(t.topology_spread_constraints.clone()),
        runtime_class_name: t.runtime_class_name.clone(),
        priority_class_name: t.priority_class_name.clone(),
        image_pull_secrets: non_empty(t.image_pull_secrets.clone()),
        volumes: Some(volumes(pool)),
        containers: vec![container(pool)?],
        ..PodSpec::default()
    })
}

/// Pool name only (the spec does not need a namespace or uid).
fn pool_name(pool: &BuildKitPool) -> Result<&str, RenderError> {
    pool.metadata
        .name
        .as_deref()
        .ok_or(RenderError::MissingMetadata("name"))
}

/// The worker pod for one slot.
pub fn worker_pod(pool: &BuildKitPool, inputs: &PodInputs) -> Result<Pod, RenderError> {
    let (name, ns, uid) = pool_identity(pool)?;
    let mut spec = pod_spec(pool)?;
    let hash = template_hash(pool)?;
    let pod_name = worker_pod_name(name, inputs.slot);
    let secret = worker_secret_name(&pod_name, &inputs.nonce);
    for v in spec.volumes.iter_mut().flatten() {
        if let (CERTS_VOLUME, Some(src)) = (v.name.as_str(), v.secret.as_mut()) {
            src.secret_name = Some(secret.clone());
        }
    }

    let mut labels: BTreeMap<String, String> = pool.spec.template.metadata.labels.clone();
    labels.extend([
        (s(LABEL_MANAGED_BY), s(CONTROLLER)),
        (s(LABEL_NAME), s(WORKER_NAME)),
        (s(LABEL_POOL), s(name)),
        (
            s(LABEL_STATE),
            s(if inputs.claim.is_some() {
                "allocated"
            } else {
                "free"
            }),
        ),
        (s(LABEL_TEMPLATE_HASH), hash),
    ]);

    let mut annotations: BTreeMap<String, String> = pool.spec.template.metadata.annotations.clone();
    let fp: String = inputs
        .internal_ca_fingerprint
        .chars()
        .take(INTERNAL_CA_LABEL_LEN)
        .collect();
    annotations.extend([
        (
            s(ANN_WORKER_CERT_NOT_AFTER),
            inputs.worker_cert_not_after.clone(),
        ),
        (s(ANN_INTERNAL_CA), fp),
        (s(ANN_WORKER_NONCE), inputs.nonce.clone()),
    ]);
    match &inputs.claim {
        None => {
            annotations.insert(s(ANN_ALLOC), s("free"));
        }
        Some(c) => annotations.extend([
            (s(ANN_ALLOC), c.alloc_id.clone()),
            (s(ANN_OWNER), c.owner.clone()),
            (s(ANN_ALLOCATED_AT), c.allocated_at.clone()),
            (s(ANN_EXPIRES_AT), c.expires_at.clone()),
        ]),
    }

    let mut pod = Pod {
        metadata: ObjectMeta {
            name: Some(pod_name),
            namespace: Some(s(ns)),
            labels: Some(labels),
            annotations: Some(annotations),
            owner_references: Some(vec![owner_ref(name, uid)]),
            ..ObjectMeta::default()
        },
        spec: Some(spec),
        status: None,
    };
    if is_cached_slot(pool, inputs.slot) {
        mount_cache_volume(&mut pod, name, inputs.slot);
    }
    Ok(pod)
}
