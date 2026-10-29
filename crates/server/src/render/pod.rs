//! The worker pod: the user template merged with the
//! enfored fields.

use std::collections::BTreeMap;

use buildkit_controller_api::v1alpha1::{BuildKitPool, BuildkitdContainer, CONTAINER_NAME};
use k8s_openapi::api::core::v1::{
    AppArmorProfile, Container, ContainerPort, EmptyDirVolumeSource, ExecAction, KeyToPath, Pod,
    PodSecurityContext, PodSpec, Probe, SeccompProfile, SecretVolumeSource, SecurityContext,
    Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

use super::validate::{check, single_arch};
use super::{
    ANN_ALLOC, ANN_INTERNAL_CA, ANN_WORKER_CERT_NOT_AFTER, ANN_WORKER_NONCE, BUILDKIT_PORT,
    CONTROLLER, INTERNAL_CA_LABEL_LEN, LABEL_MANAGED_BY, LABEL_NAME, LABEL_POOL, LABEL_STATE,
    LABEL_TEMPLATE_HASH, PodInputs, RenderError, TemplateConflictField, WORKER_NAME, owner_ref,
    pool_identity, template_hash, worker_object_name, worker_pod_name, worker_secret_name,
};

const SOCKET: &str = "unix:///run/user/1000/buildkit/buildkitd.sock";
const RUN_AS: i64 = 1000;
const CERTS_VOLUME: &str = "certs";
/// Name of the pod volume that holds the buildkitd state dir.
const STATE_VOLUME: &str = "state";

const STATE_DIR: &str = "/home/user/.local/share/buildkit";
/// Failed startup probes (one a second) before kubelet gives up: past the
/// longest `startTimeout` (30m), so the reconciler retires a slow pod first.
const STARTUP_PROBE_LIMIT: i32 = 31 * 60;

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

fn container(user: &BuildkitdContainer) -> Container {
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
        mount(STATE_VOLUME, STATE_DIR),
    ];
    mounts.extend(user.volume_mounts.iter().cloned());
    let workers = || ExecAction {
        command: Some(
            ["buildctl", "--addr", SOCKET, "debug", "workers"]
                .map(s)
                .to_vec(),
        ),
    };
    Container {
        name: s(CONTAINER_NAME),
        image: Some(user.image.clone()),
        image_pull_policy: Some(s(user.image_pull_policy.as_str())),
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
        // Probed every second until the first success, so a cold start is
        // quick, and slowly after that: an exec probe costs a process start,
        // and an allocated pod no longer needs its readiness.
        startup_probe: Some(Probe {
            exec: Some(workers()),
            period_seconds: Some(1),
            timeout_seconds: Some(5),
            failure_threshold: Some(STARTUP_PROBE_LIMIT),
            ..Probe::default()
        }),
        readiness_probe: Some(Probe {
            exec: Some(workers()),
            period_seconds: Some(10),
            timeout_seconds: Some(5),
            failure_threshold: Some(3),
            ..Probe::default()
        }),
        resources: Some(user.resources.clone()),
        env: non_empty(user.env.clone()),
        volume_mounts: Some(mounts),
        ..Container::default()
    }
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
                    [
                        "tls.crt",
                        "tls.key",
                        "ca.crt",
                        "client.crt",
                        "client.key",
                        "buildkitd.toml",
                    ]
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
                size_limit: pool.spec.workers.state_size_limit.clone(),
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
    let main = container(t.containers.first().ok_or_else(|| {
        RenderError::TemplateConflict(vec![TemplateConflictField {
            field: s("spec.template.spec.containers"),
            reason: s("must be exactly one container named buildkitd"),
        }])
    })?);
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
        containers: vec![main],
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
        (s(LABEL_STATE), s("free")),
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
    annotations.insert(s(ANN_ALLOC), s("free"));

    Ok(Pod {
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
    })
}
