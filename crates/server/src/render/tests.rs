//! Snapshots (insta), conflict cases and property tests.

use std::path::PathBuf;

use buildkit_controller_api::v1alpha1::BuildKitPool;
use insta::assert_snapshot;
use proptest::prelude::*;
use serde_json::{Value, json};

use super::fixtures::{
    default_config, default_inputs, fake_tls, render_all, render_scenario, scenarios,
};
use super::*;

/// A fixture pool; the test specs are fixed and valid.
fn pool(name: &str, spec: Value) -> BuildKitPool {
    super::fixtures::pool(name, spec).unwrap()
}

fn conflict_names(r: &RenderError) -> Vec<&str> {
    r.conflict_fields()
}

#[test]
fn snapshots_every_scenario_and_object() {
    for s in scenarios().unwrap() {
        for (kind, y) in render_scenario(&s).unwrap() {
            insta::with_settings!({prepend_module_to_snapshot => false}, {
                assert_snapshot!(format!("{}.{kind}", s.name), y);
            });
        }
    }
}

#[test]
fn snapshot_ca_bundle() {
    let y = serde_saphyr::to_string(&ca_bundle_config_map(
        "buildkit-controller-ca",
        &default_config(),
        "PEM",
    ))
    .unwrap();
    insta::with_settings!({prepend_module_to_snapshot => false}, {
        assert_snapshot!("ca-bundle.configmap", y);
    });
}

#[test]
fn committed_fixtures_match_render() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/render");
    for (name, want) in render_all().unwrap() {
        let have = std::fs::read_to_string(dir.join(&name)).unwrap_or_default();
        assert_eq!(
            have, want,
            "{name} is stale; run `cargo xtask render-verify`"
        );
    }
}

#[allow(clippy::too_many_lines)]
/// A template mutation that must be rejected, and the field it must name.
fn conflict_cases() -> Vec<(&'static str, Value, &'static str)> {
    let c = |extra: Value| json!({"template": {"spec": extra}});
    let ctr = |extra: Value| {
        let mut base = json!({"name": "buildkitd", "resources": {"limits": {"memory": "1Gi", "ephemeral-storage": "1Gi"}}});
        if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        c(json!({"containers": [base]}))
    };
    vec![
        (
            "label-managed-by",
            json!({"template": {"metadata": {"labels": {"app.kubernetes.io/managed-by": "me"}}}}),
            "spec.template.metadata.labels[app.kubernetes.io/managed-by]",
        ),
        (
            "label-name",
            json!({"template": {"metadata": {"labels": {"app.kubernetes.io/name": "x"}}}}),
            "spec.template.metadata.labels[app.kubernetes.io/name]",
        ),
        (
            "label-prefix",
            json!({"template": {"metadata": {"labels": {"buildkit.smrt-devops.net/state": "free"}}}}),
            "spec.template.metadata.labels[buildkit.smrt-devops.net/state]",
        ),
        (
            "annotation-prefix",
            json!({"template": {"metadata": {"annotations": {"buildkit.smrt-devops.net/alloc": "x"}}}}),
            "spec.template.metadata.annotations[buildkit.smrt-devops.net/alloc]",
        ),
        (
            "annotation-multus",
            json!({"template": {"metadata": {"annotations": {"k8s.v1.cni.cncf.io/networks": "default/macvlan"}}}}),
            "spec.template.metadata.annotations[k8s.v1.cni.cncf.io/networks]",
        ),
        (
            "annotation-calico",
            json!({"template": {"metadata": {"annotations": {"cni.projectcalico.org/ipAddrs": "[\"10.0.0.9\"]"}}}}),
            "spec.template.metadata.annotations[cni.projectcalico.org/ipAddrs]",
        ),
        (
            "annotation-cilium",
            json!({"template": {"metadata": {"annotations": {"network.cilium.io/ipv4-pool": "p"}}}}),
            "spec.template.metadata.annotations[network.cilium.io/ipv4-pool]",
        ),
        (
            "annotation-kata",
            json!({"template": {"metadata": {"annotations": {"io.katacontainers.config.hypervisor.kernel_params": "x"}}}}),
            "spec.template.metadata.annotations[io.katacontainers.config.hypervisor.kernel_params]",
        ),
        (
            "volume-certs",
            c(json!({"volumes": [{"name": "certs", "emptyDir": {}}]})),
            "spec.template.spec.volumes[0].name",
        ),
        (
            "volume-state",
            c(json!({"volumes": [{"name": "state", "emptyDir": {}}]})),
            "spec.template.spec.volumes[0].name",
        ),
        (
            "volume-hostpath",
            c(json!({"volumes": [{"name": "h", "hostPath": {"path": "/"}}]})),
            "spec.template.spec.volumes[0].hostPath",
        ),
        (
            "volume-projected-token",
            c(
                json!({"volumes": [{"name": "t", "projected": {"sources": [{"serviceAccountToken": {"path": "t"}}]}}]}),
            ),
            "spec.template.spec.volumes[0].projected.sources[0].serviceAccountToken",
        ),
        (
            "container-name",
            c(json!({"containers": [{"name": "other"}]})),
            "spec.template.spec.containers",
        ),
        (
            "containers-two",
            c(json!({"containers": [{"name": "buildkitd"}, {"name": "side"}]})),
            "spec.template.spec.containers",
        ),
        (
            "containers-none",
            c(json!({"containers": []})),
            "spec.template.spec.containers",
        ),
        (
            "env-buildkitd",
            ctr(json!({"env": [{"name": "BUILDKITD_FLAGS", "value": "x"}]})),
            "spec.template.spec.containers[0].env[0].name",
        ),
        (
            "env-valuefrom-resource-field",
            ctr(
                json!({"env": [{"name": "LIMIT", "valueFrom": {"resourceFieldRef": {"resource": "limits.memory"}}}]}),
            ),
            "spec.template.spec.containers[0].env[0].valueFrom.resourceFieldRef",
        ),
        (
            "env-valuefrom-file-key",
            ctr(
                json!({"env": [{"name": "F", "valueFrom": {"fileKeyRef": {"path": "p", "key": "k", "volumeName": "v"}}}]}),
            ),
            "spec.template.spec.containers[0].env[0].valueFrom.fileKeyRef",
        ),
        (
            "env-rootlesskit",
            ctr(json!({"env": [{"name": "ROOTLESSKIT_FLAGS", "value": "x"}]})),
            "spec.template.spec.containers[0].env[0].name",
        ),
        (
            "mount-certs",
            ctr(json!({"volumeMounts": [{"name": "v", "mountPath": "/certs/extra"}]})),
            "spec.template.spec.containers[0].volumeMounts[0].mountPath",
        ),
        (
            "mount-etc-buildkit",
            ctr(json!({"volumeMounts": [{"name": "v", "mountPath": "/etc/buildkit"}]})),
            "spec.template.spec.containers[0].volumeMounts[0].mountPath",
        ),
        (
            "mount-state",
            ctr(
                json!({"volumeMounts": [{"name": "v", "mountPath": "/home/user/.local/share/buildkit/x"}]}),
            ),
            "spec.template.spec.containers[0].volumeMounts[0].mountPath",
        ),
        (
            "mount-run-user",
            ctr(json!({"volumeMounts": [{"name": "v", "mountPath": "/run/user/1000"}]})),
            "spec.template.spec.containers[0].volumeMounts[0].mountPath",
        ),
        (
            "mount-relative",
            ctr(json!({"volumeMounts": [{"name": "v", "mountPath": "rel"}]})),
            "spec.template.spec.containers[0].volumeMounts[0].mountPath",
        ),
        (
            "mount-dotdot",
            ctr(json!({"volumeMounts": [{"name": "v", "mountPath": "/a/../certs"}]})),
            "spec.template.spec.containers[0].volumeMounts[0].mountPath",
        ),
        (
            "mount-unknown-volume",
            ctr(json!({"volumeMounts": [{"name": "nope", "mountPath": "/data"}]})),
            "spec.template.spec.containers[0].volumeMounts[0].name",
        ),
        (
            "limits-missing-memory",
            c(
                json!({"containers": [{"name": "buildkitd", "resources": {"limits": {"ephemeral-storage": "1Gi"}}}]}),
            ),
            "spec.template.spec.containers[0].resources.limits[memory]",
        ),
        (
            "limits-missing-storage",
            c(
                json!({"containers": [{"name": "buildkitd", "resources": {"limits": {"memory": "1Gi"}}}]}),
            ),
            "spec.template.spec.containers[0].resources.limits[ephemeral-storage]",
        ),
        (
            "limits-absent",
            c(json!({"containers": [{"name": "buildkitd", "resources": {}}]})),
            "spec.template.spec.containers[0].resources.limits[memory]",
        ),
        (
            "arch-disagrees",
            json!({"platforms": ["linux/amd64"], "template": {"spec": {"nodeSelector": {"kubernetes.io/arch": "arm64"}}}}),
            "spec.template.spec.nodeSelector[kubernetes.io/arch]",
        ),
        (
            "priority-system",
            c(json!({"priorityClassName": "system-node-critical"})),
            "spec.template.spec.priorityClassName",
        ),
        (
            "mount-bidirectional",
            ctr(
                json!({"volumeMounts": [{"name": "v", "mountPath": "/data", "mountPropagation": "Bidirectional"}]}),
            ),
            "spec.template.spec.containers[0].volumeMounts[0].mountPropagation",
        ),
        (
            "mount-path-control",
            ctr(json!({"volumeMounts": [{"name": "v", "mountPath": "/data\u{7f}"}]})),
            "spec.template.spec.containers[0].volumeMounts[0].mountPath",
        ),
        (
            "mount-subpath-control",
            ctr(json!({"volumeMounts": [{"name": "v", "mountPath": "/data", "subPath": "a\nb"}]})),
            "spec.template.spec.containers[0].volumeMounts[0].subPath",
        ),
        (
            "projected-two-sources",
            c(
                json!({"volumes": [{"name": "t", "projected": {"sources": [{"secret": {"name": "s"}, "configMap": {"name": "c"}}]}}]}),
            ),
            "spec.template.spec.volumes[0].projected.sources[0]",
        ),
        (
            "env-valuefrom-two",
            ctr(
                json!({"env": [{"name": "A", "valueFrom": {"fieldRef": {"fieldPath": "metadata.name"}, "secretKeyRef": {"name": "s", "key": "k"}}}]}),
            ),
            "spec.template.spec.containers[0].env[0].valueFrom",
        ),
        (
            "config-not-toml",
            json!({"workers": {"buildkitdConfig": "[worker.oci\n"}}),
            "spec.workers.buildkitdConfig",
        ),
        (
            "config-registry-not-table",
            json!({"workers": {"buildkitdConfig": "registry = 1\n"}}),
            "spec.workers.buildkitdConfig",
        ),
    ]
}

#[test]
fn every_enforced_field_conflict_is_typed_and_named() {
    for (name, spec, field) in conflict_cases() {
        let mut p = pool("p", spec);
        // Volume mounts need a volume to isolate the path rule.
        if name.starts_with("mount-") && name != "mount-unknown-volume" {
            p.spec.template.spec.volumes =
                vec![serde_json::from_value(json!({"name": "v", "emptyDir": {}})).unwrap()];
        }
        let err = worker_pod(&p, &default_inputs()).unwrap_err();
        assert!(
            conflict_names(&err).contains(&field),
            "{name}: {field} not in {err}"
        );
        assert!(matches!(err, RenderError::TemplateConflict(_)), "{name}");
        assert_eq!(
            template_hash(&p).unwrap_err(),
            err,
            "{name}: hash must refuse too"
        );
        insta::with_settings!({prepend_module_to_snapshot => false}, {
            assert_snapshot!(format!("conflict-{name}"), err.to_string());
        });
    }
}

#[test]
fn several_conflicts_are_all_reported() {
    let p = pool(
        "p",
        json!({"template": {"metadata": {"labels": {"app.kubernetes.io/name": "x"}}, "spec": {"volumes": [{"name": "certs", "emptyDir": {}}]}}}),
    );
    let err = worker_pod(&p, &default_inputs()).unwrap_err();
    insta::with_settings!({prepend_module_to_snapshot => false}, {
        assert_snapshot!("conflict-multiple", err.to_string());
    });
}

#[test]
fn missing_metadata_and_long_names_are_errors() {
    let mut p = pool("p", json!({}));
    p.metadata.uid = None;
    assert_eq!(
        worker_pod(&p, &default_inputs()).unwrap_err(),
        RenderError::MissingMetadata("uid")
    );
    let long = pool(&"x".repeat(53), json!({}));
    assert!(matches!(
        worker_pod(&long, &default_inputs()).unwrap_err(),
        RenderError::NameTooLong(_)
    ));
}

#[test]
fn hash_excludes_ca_fingerprint_slot_and_claim() {
    let p = pool("p", json!({}));
    let base = worker_pod(&p, &default_inputs()).unwrap();
    let mut other = default_inputs();
    other.internal_ca_fingerprint = "f".repeat(64);
    other.slot = 7;
    other.worker_cert_not_after = "2030-01-01T00:00:00Z".into();
    let b = worker_pod(&p, &other).unwrap();
    let hash = |p: &k8s_openapi::api::core::v1::Pod| {
        p.metadata.labels.as_ref().unwrap()[LABEL_TEMPLATE_HASH].clone()
    };
    assert_eq!(hash(&base), hash(&b));
    assert_eq!(hash(&base), template_hash(&p).unwrap());
    assert_eq!(hash(&base).len(), 10);
    assert_ne!(
        base.metadata.annotations.as_ref().unwrap()[ANN_INTERNAL_CA],
        b.metadata.annotations.as_ref().unwrap()[ANN_INTERNAL_CA]
    );
    assert_eq!(
        b.metadata.annotations.as_ref().unwrap()[ANN_INTERNAL_CA].len(),
        16
    );
}

#[test]
fn hash_covers_template_and_buildkitd_config() {
    let base = template_hash(&pool("p", json!({}))).unwrap();
    for spec in [
        json!({"workers": {"buildkitdConfig": "debug = true\n"}}),
        json!({"template": {"metadata": {"labels": {"a": "b"}}}}),
        json!({"template": {"spec": {"containers": [{"name": "buildkitd", "image": "other", "resources": {"limits": {"memory": "1Gi", "ephemeral-storage": "1Gi"}}}]}}}),
        json!({"platforms": ["linux/arm64"]}),
        json!({"workers": {"stateSizeLimit": "1Gi"}}),
    ] {
        assert_ne!(
            base,
            template_hash(&pool("p", spec.clone())).unwrap(),
            "{spec}"
        );
    }
    // Scaling and network settings are not part of the pod.
    let same = pool(
        "p",
        json!({"scaling": {"min": 3}, "network": {"egress": {"internet": "Deny"}}}),
    );
    assert_eq!(base, template_hash(&same).unwrap());
}

#[test]
fn worker_secret_keys() {
    let p = pool(
        "p",
        json!({"workers": {"buildkitdConfig": "debug = true\n"}}),
    );
    let s = worker_secret(&p, &default_inputs(), &fake_tls(), None).unwrap();
    let data = s.data.unwrap();
    assert_eq!(
        data.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "buildkitd.toml",
            "ca.crt",
            "client.crt",
            "client.key",
            "tls.crt",
            "tls.key"
        ]
    );
    assert_eq!(data["buildkitd.toml"].0, b"debug = true\n");
    assert_eq!(s.type_.as_deref(), Some("Opaque"));
    assert_eq!(s.metadata.name.as_deref(), Some("p-0-00000000000000aa"));
    assert_eq!(config_hash(""), "e3b0c44298fc1c14");
}

#[test]
fn tls_debug_never_prints_material() {
    let t = fake_tls();
    assert!(!format!("{t:?}").contains("fixture-key"));
}

fn spec_from_labels(pairs: &[(String, String)]) -> BuildKitPool {
    let obj: serde_json::Map<String, Value> = pairs
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    pool(
        "p",
        json!({"template": {"metadata": {"labels": Value::Object(obj.clone()), "annotations": Value::Object(obj.clone())}, "spec": {"nodeSelector": Value::Object(obj)}}}),
    )
}

/// The same object with its keys written in a different order.
fn ordered_json(pairs: &[(String, String)]) -> String {
    let body = pairs
        .iter()
        .map(|(k, v)| format!("{}:{}", Value::String(k.clone()), Value::String(v.clone())))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{body}}}")
}

fn key() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9]{0,8}(\\.[a-z]{1,4})?/?[a-z]{1,6}"
}

proptest! {
    #[test]
    fn hash_is_stable_under_map_ordering(
        map in proptest::collection::btree_map(key(), "[a-z0-9]{0,8}", 1..8),
        seed in any::<u64>(),
    ) {
        let sorted: Vec<(String, String)> = map.into_iter().collect();
        let mut shuffled = sorted.clone();
        // Deterministic permutation that rotates and reverses by the seed.
        let n = shuffled.len();
        shuffled.rotate_left(usize::try_from(seed % n as u64).unwrap_or(0));
        if seed % 2 == 0 { shuffled.reverse(); }
        let body = |p: &[(String, String)]| {
            let o = ordered_json(p);
            format!(r#"{{"template":{{"metadata":{{"labels":{o},"annotations":{o}}},"spec":{{"nodeSelector":{o}}}}}}}"#)
        };
        let a: buildkit_controller_api::v1alpha1::BuildKitPoolSpec = serde_json::from_str(&body(&sorted)).unwrap();
        let b: buildkit_controller_api::v1alpha1::BuildKitPoolSpec = serde_json::from_str(&body(&shuffled)).unwrap();
        let mut pa = spec_from_labels(&sorted);
        let mut pb = spec_from_labels(&sorted);
        pa.spec = a;
        pb.spec = b;
        prop_assert_eq!(template_hash(&pa).unwrap(), template_hash(&pb).unwrap());
        let pod_a = serde_json::to_string(&worker_pod(&pa, &default_inputs()).unwrap()).unwrap();
        let pod_b = serde_json::to_string(&worker_pod(&pb, &default_inputs()).unwrap()).unwrap();
        prop_assert_eq!(pod_a, pod_b);
    }

    #[test]
    fn enforced_fields_always_win(
        labels in proptest::collection::btree_map("x-[a-z]{1,8}", "[a-z]{0,6}", 0..5),
        selector in proptest::collection::btree_map("x-[a-z]{1,8}", "[a-z]{0,6}", 0..5),
        image in "[a-z]{1,10}/[a-z]{1,10}:[a-z0-9.]{1,8}",
        envs in proptest::collection::vec("[A-Z]{1,6}_[A-Z]{1,6}", 0..4),
        platform in proptest::option::of(prop_oneof![Just("linux/amd64"), Just("linux/arm64/v8")]),
    ) {
        let env: Vec<Value> = envs.iter().filter(|n| !n.starts_with("BUILDKITD_") && !n.starts_with("ROOTLESSKIT_")).map(|n| json!({"name": n, "value": "v"})).collect();
        let spec = json!({
            "platforms": platform.into_iter().collect::<Vec<_>>(),
            "template": {
                "metadata": {"labels": labels, "annotations": {"free": "form"}},
                "spec": {"nodeSelector": selector, "containers": [{
                    "name": "buildkitd", "image": image, "env": env,
                    "resources": {"limits": {"memory": "1Gi", "ephemeral-storage": "1Gi"}}
                }]}
            }
        });
        let pod = worker_pod(&pool("p", spec), &default_inputs()).unwrap();
        let ps = pod.spec.unwrap();
        prop_assert_eq!(ps.service_account_name.as_deref(), Some("p-buildkit-worker"));
        prop_assert_eq!(ps.automount_service_account_token, Some(false));
        prop_assert_eq!(ps.enable_service_links, Some(false));
        prop_assert_eq!((ps.host_network, ps.host_pid, ps.host_ipc), (Some(false), Some(false), Some(false)));
        prop_assert_eq!(ps.restart_policy.as_deref(), Some("Never"));
        prop_assert_eq!(ps.termination_grace_period_seconds, Some(5));
        let sc = ps.security_context.unwrap();
        prop_assert_eq!((sc.run_as_user, sc.run_as_group, sc.fs_group, sc.run_as_non_root), (Some(1000), Some(1000), Some(1000), Some(true)));
        prop_assert_eq!(ps.containers.len(), 1);
        let c = &ps.containers[0];
        prop_assert_eq!(c.name.as_str(), "buildkitd");
        prop_assert_eq!(c.command.as_ref(), None);
        prop_assert_eq!(c.args.as_ref().map(Vec::len), Some(7));
        prop_assert_eq!(c.security_context.as_ref().and_then(|s| s.privileged), Some(false));
        prop_assert!(c.readiness_probe.is_some());
        let md = pod.metadata;
        let l = md.labels.unwrap();
        prop_assert_eq!(l[LABEL_MANAGED_BY].as_str(), "buildkit-controller");
        prop_assert_eq!(l[LABEL_NAME].as_str(), "buildkitd");
        prop_assert_eq!(l[LABEL_POOL].as_str(), "p");
        prop_assert_eq!(l[LABEL_STATE].as_str(), "free");
        let ann = md.annotations.unwrap();
        prop_assert_eq!(ann[ANN_ALLOC].as_str(), "free");
        if let Some(arch) = platform.map(|p| p.split('/').nth(1).unwrap_or_default()) {
            let ns = ps.node_selector.unwrap();
            prop_assert_eq!(ns["kubernetes.io/arch"].as_str(), arch);
        }
    }

    #[test]
    fn reserved_keys_always_conflict(suffix in "[a-z]{1,10}", annotation in any::<bool>()) {
        let key = format!("buildkit.smrt-devops.net/{suffix}");
        let spec = if annotation {
            json!({"template": {"metadata": {"annotations": {key.clone(): "x"}}}})
        } else {
            json!({"template": {"metadata": {"labels": {key.clone(): "x"}}}})
        };
        let err = worker_pod(&pool("p", spec), &default_inputs()).unwrap_err();
        prop_assert!(err.conflict_fields().iter().any(|f| f.contains(&key)));
    }
}

/// The `ipBlock`s of the internet rules, as (cidr, except).
fn internet_blocks(
    np: &k8s_openapi::api::networking::v1::NetworkPolicy,
) -> Vec<(String, Vec<String>)> {
    np.spec
        .as_ref()
        .and_then(|s| s.egress.as_ref())
        .into_iter()
        .flatten()
        .flat_map(|r| r.to.iter().flatten())
        .filter_map(|p| p.ip_block.as_ref())
        .filter(|b| b.cidr.ends_with("/0"))
        .map(|b| (b.cidr.clone(), b.except.clone().unwrap_or_default()))
        .collect()
}

#[test]
fn ipv6_internet_stays_closed_until_cluster_ranges_are_named() {
    let p = pool("p", json!({}));
    let mut cfg = default_config();
    let blocks = internet_blocks(&network_policy(&p, &cfg).unwrap());
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    assert_eq!(blocks[0].0, "0.0.0.0/0");

    cfg.worker_network.cluster_cidrs = vec!["34.118.224.0/20".into(), "2600:1f18::/40".into()];
    let blocks = internet_blocks(&network_policy(&p, &cfg).unwrap());
    let v4 = &blocks.iter().find(|b| b.0 == "0.0.0.0/0").unwrap().1;
    let v6 = &blocks.iter().find(|b| b.0 == "::/0").unwrap().1;
    assert!(v4.contains(&"34.118.224.0/20".to_owned()) && !v4.iter().any(|c| c.contains(':')));
    for c in [
        "2600:1f18::/40",
        "fc00::/7",
        "fe80::/10",
        "64:ff9b::/96",
        "64:ff9b:1::/48",
    ] {
        assert!(v6.contains(&c.to_owned()), "{c} missing from {v6:?}");
    }
}

#[test]
fn pod_mounts_its_own_secret_and_names_its_nonce() {
    let p = pool("p", json!({}));
    let a = default_inputs();
    let b = PodInputs {
        nonce: "00000000000000bb".into(),
        ..default_inputs()
    };
    let (pa, pb) = (worker_pod(&p, &a).unwrap(), worker_pod(&p, &b).unwrap());
    let mounted = |pod: &k8s_openapi::api::core::v1::Pod| {
        pod.spec
            .as_ref()
            .and_then(|s| s.volumes.as_ref())
            .and_then(|v| v.iter().find(|v| v.name == "certs"))
            .and_then(|v| v.secret.as_ref())
            .and_then(|s| s.secret_name.clone())
    };
    let secret = worker_secret(&p, &a, &fake_tls(), None).unwrap();
    assert_eq!(mounted(&pa), secret.metadata.name);
    assert_eq!(mounted(&pb).as_deref(), Some("p-0-00000000000000bb"));
    let ann = |pod: &k8s_openapi::api::core::v1::Pod| {
        pod.metadata.annotations.as_ref().unwrap()[ANN_WORKER_NONCE].clone()
    };
    assert_eq!(ann(&pa), a.nonce);
    assert_eq!(ann(&pb), b.nonce);
    // The nonce is per pod, so it stays out of the template hash.
    let hash = |pod: &k8s_openapi::api::core::v1::Pod| {
        pod.metadata.labels.as_ref().unwrap()[LABEL_TEMPLATE_HASH].clone()
    };
    assert_eq!(hash(&pa), hash(&pb));
}

const HOST: &str = "bk-registry.bk.svc:5000";

/// The parsed `buildkitd.toml` of a pool whose own config is `cfg`, with the cache.
fn cached_config(cfg: &str) -> toml_edit::DocumentMut {
    let p = pool("p", json!({"workers": {"buildkitdConfig": cfg}}));
    assert!(validate_template(&p.spec).is_empty(), "{cfg}");
    let out = buildkitd_config(&p.spec.workers.buildkitd_config, Some(HOST));
    out.parse()
        .unwrap_or_else(|e| panic!("{cfg:?} rendered invalid TOML {out:?}: {e}"))
}

#[test]
fn the_cache_registry_entry_uses_the_worker_client_leaf_and_the_internal_ca() {
    assert_eq!(
        buildkitd_config("", Some(HOST)),
        "[registry.\"bk-registry.bk.svc:5000\"]\nca = [\"/certs/ca.crt\"]\nkeypair = [{ key = \"/certs/client.key\", cert = \"/certs/client.crt\" }]\n"
    );
    // Without a cache host the config passes through unread.
    assert_eq!(buildkitd_config("worker = 1\n", None), "worker = 1\n");
    let entry = |d: &toml_edit::DocumentMut| {
        let e = &d["registry"][HOST];
        (
            e["ca"].as_array().map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            }),
            e["keypair"].as_array().map(toml_edit::Array::len),
        )
    };
    let ours = (Some("/certs/ca.crt".to_owned()), Some(1));
    for cfg in [
        "[worker.oci]\nmax-parallelism = 4\n",
        // Other registries stay.
        "[registry.\"docker.io\"]\nmirrors = [\"mirror.internal\"]\n",
        // An entry of the pool's own for the cache host is replaced.
        "[registry.\"bk-registry.bk.svc:5000\"]\ninsecure = true\nca = [\"/x\"]\n",
        // Dotted keys and inline tables.
        "registry.\"docker.io\".mirrors = [\"m\"]\n",
        "registry = { \"docker.io\" = { mirrors = [\"m\"] } }\n",
    ] {
        let d = cached_config(cfg);
        assert_eq!(entry(&d), ours, "{cfg}");
    }
    let d = cached_config("[registry.\"docker.io\"]\nmirrors = [\"mirror.internal\"]\n");
    assert_eq!(
        d["registry"]["docker.io"]["mirrors"][0].as_str(),
        Some("mirror.internal")
    );
    let d = cached_config("[registry.\"bk-registry.bk.svc:5000\"]\ninsecure = true\n");
    assert_eq!(d["registry"][HOST]["insecure"].as_bool(), Some(true));
}

#[test]
fn buildkitd_config_that_cannot_take_the_registry_entry_is_refused() {
    for cfg in [
        "[registry\n",
        "registry = 1\n",
        "[[registry]]\n",
        "[registry]\n[registry]\n",
    ] {
        let p = pool("p", json!({"workers": {"buildkitdConfig": cfg}}));
        let c = validate_template(&p.spec);
        assert!(
            c.iter().any(|c| c.field == "spec.workers.buildkitdConfig"),
            "{cfg:?}: {c:?}"
        );
        assert_eq!(buildkitd_config(cfg, Some(HOST)), cfg);
    }
}

#[test]
fn the_worker_secret_carries_the_registry_entry_and_the_hash_does_not() {
    let p = pool("p", json!({}));
    let toml = |host| {
        let s = worker_secret(&p, &default_inputs(), &fake_tls(), host).unwrap();
        String::from_utf8(s.data.unwrap()["buildkitd.toml"].0.clone()).unwrap()
    };
    assert!(toml(Some(HOST)).contains("[registry.\"bk-registry.bk.svc:5000\"]"));
    assert_eq!(toml(None), "");
    // The pod and its hash are the same with or without the cache.
    let pod = worker_pod(&p, &default_inputs()).unwrap();
    assert!(pod.spec.as_ref().unwrap().init_containers.is_none());
    assert!(state_volume(&pod).empty_dir.is_some());
}

#[test]
fn workers_may_reach_the_cache_registry_and_nothing_else_new() {
    let p = pool("p", json!({"network": {"egress": {"internet": "Deny"}}}));
    let mut cfg = default_config();
    let rules = |cfg: &RenderConfig| {
        network_policy(&p, cfg)
            .unwrap()
            .spec
            .unwrap()
            .egress
            .unwrap()
    };
    let without = rules(&cfg);
    cfg.registry_host = Some(HOST.into());
    let with = rules(&cfg);
    assert_eq!(with.len(), without.len() + 1);
    let r = with.iter().find(|r| !without.contains(r)).unwrap();
    let to = &r.to.as_ref().unwrap()[0];
    assert_eq!(
        to.pod_selector
            .as_ref()
            .unwrap()
            .match_labels
            .as_ref()
            .unwrap()[LABEL_NAME],
        REGISTRY_NAME
    );
    assert_eq!(
        to.namespace_selector
            .as_ref()
            .unwrap()
            .match_labels
            .as_ref()
            .unwrap()["kubernetes.io/metadata.name"],
        default_config().controller_namespace
    );
    let ports = r.ports.as_ref().unwrap();
    assert_eq!(ports.len(), 1);
    assert_eq!(
        ports[0].port,
        Some(k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(5000))
    );
}

#[test]
fn quantities_and_scope_ids_parse() {
    assert_eq!(quantity_bytes("20Gi"), Some(20 << 30));
    assert_eq!(quantity_bytes("1.5Gi"), Some(3 << 29));
    assert_eq!(quantity_bytes("2G"), Some(2_000_000_000));
    assert_eq!(quantity_bytes("1e9"), None);
    let id = scope_id("[\"pool\"]");
    assert!(is_scope_id(&id), "{id}");
    for bad in [
        "",
        "ABCDEF0123456789abcdef0123456789",
        "abc",
        "../../../../../../../../../../..",
    ] {
        assert!(!is_scope_id(bad), "{bad}");
    }
}

fn state_volume(p: &k8s_openapi::api::core::v1::Pod) -> k8s_openapi::api::core::v1::Volume {
    let vs = p.spec.as_ref().unwrap().volumes.as_ref().unwrap();
    vs.iter().find(|v| v.name == "state").unwrap().clone()
}

/// A network with the pod, service and node ranges of a dual-stack cluster.
fn cluster_net() -> WorkerNetwork {
    WorkerNetwork {
        cluster_cidrs: vec![
            "10.244.0.0/16".into(),
            "10.96.0.0/12".into(),
            "fd00:10:244::/56".into(),
        ],
        ..WorkerNetwork::default()
    }
}

#[test]
fn allow_cidr_overlapping_a_protected_range_is_refused() {
    let net = cluster_net();
    for (entry, hit) in [
        // Inside a protected range.
        ("169.254.169.254/32", "169.254.0.0/16"),
        ("168.63.129.16/32", "168.63.129.16/32"),
        ("127.0.0.1/32", "127.0.0.0/8"),
        ("fe80::1/128", "fe80::/10"),
        ("::1/128", "::1/128"),
        // Holding a protected range.
        ("169.0.0.0/8", "169.254.0.0/16"),
        ("168.63.0.0/16", "168.63.129.16/32"),
        ("fe00::/7", "fe80::/10"),
        ("::/120", "::1/128"),
        // Holding or inside a cluster range.
        ("10.0.0.0/8", "10.244.0.0/16"),
        ("10.244.3.0/24", "10.244.0.0/16"),
        ("10.100.0.10/32", "10.96.0.0/12"),
        ("fd00:10::/32", "fd00:10:244::/56"),
        ("fd00:10:244:7::/64", "fd00:10:244::/56"),
        // IPv4-mapped IPv6 names the IPv4 hosts.
        ("::ffff:169.254.169.254/128", "169.254.0.0/16"),
        ("::ffff:10.244.1.0/120", "10.244.0.0/16"),
        // An IPv6 range holding the whole IPv4-mapped block holds every IPv4 range.
        ("::ffff:0:0/95", "169.254.0.0/16"),
    ] {
        let why = refused_allow_cidr(entry, &net).unwrap_or_else(|| panic!("{entry} allowed"));
        assert!(
            why.starts_with("overlaps ")
                && why.ends_with(&format!(
                    "{hit}; an operator can permit it in workerNetwork.permittedCIDRs"
                )),
            "{entry}: {why}"
        );
    }
}

#[test]
fn allow_cidr_outside_protected_ranges_is_rendered() {
    let net = cluster_net();
    for entry in [
        "10.20.0.0/16",
        "192.168.10.0/24",
        "172.16.0.0/12",
        "192.0.2.10/32",
        "2001:db8::/32",
        "fd00:20::/32",
        "169.255.0.0/16",
        "192.0.0.0/3",
        "::ffff:192.0.2.0/120",
    ] {
        assert_eq!(refused_allow_cidr(entry, &net), None, "{entry}");
    }
}

/// RECON-4: without a cluster range of the entry's family the controller
/// cannot tell cluster addresses apart, so an IPv4 entry in a denied (private)
/// range and every IPv6 entry are refused unless permitted. Public IPv4 stays open.
#[test]
fn allow_cidr_of_a_family_without_cluster_ranges_is_refused() {
    let mut net = WorkerNetwork::default();
    for entry in [
        "10.0.0.0/8",
        "10.20.0.0/16",
        "::ffff:10.1.0.0/112",
        "2600::/12",
        "2001:db8::/32",
    ] {
        let why = refused_allow_cidr(entry, &net).unwrap_or_else(|| panic!("{entry} allowed"));
        assert!(
            why.contains("workerNetwork.clusterCIDRs holds no range of its family"),
            "{entry}: {why}"
        );
    }
    assert_eq!(refused_allow_cidr("192.0.2.10/32", &net), None);
    // An IPv4 cluster range opens other IPv4 private ranges, not IPv6.
    net.cluster_cidrs = vec!["10.96.0.0/12".into()];
    assert_eq!(refused_allow_cidr("10.20.0.0/16", &net), None);
    assert!(refused_allow_cidr("2001:db8::/32", &net).is_some());
    net.cluster_cidrs.push("fd00:10:96::/108".into());
    assert_eq!(refused_allow_cidr("2001:db8::/32", &net), None);
    // The operator can permit an entry outright.
    let net = WorkerNetwork {
        permitted_cidrs: vec!["10.20.0.0/16".into()],
        ..WorkerNetwork::default()
    };
    assert_eq!(refused_allow_cidr("10.20.0.0/16", &net), None);
}

#[test]
fn whole_family_and_garbage_are_always_refused() {
    let mut net = cluster_net();
    net.permitted_cidrs = vec!["0.0.0.0/0".into(), "::/0".into()];
    for entry in ["0.0.0.0/0", "::/0", "::ffff:0.0.0.0/96"] {
        let why = refused_allow_cidr(entry, &net).unwrap_or_default();
        assert_eq!(why, "opens a whole address family", "{entry}");
    }
    // A permitted /0 permits nothing.
    assert!(refused_allow_cidr("169.254.169.254/32", &net).is_some());
    for entry in [
        "10.0.0.0",
        "10.0.0.0/33",
        "fe80::/129",
        "x/8",
        "10.0.0.0/-1",
    ] {
        assert_eq!(
            refused_allow_cidr(entry, &net).as_deref(),
            Some("is not a CIDR range"),
            "{entry}"
        );
    }
}

#[test]
fn permitted_cidrs_reopen_only_ranges_they_hold() {
    let mut net = cluster_net();
    net.permitted_cidrs = vec![
        "169.254.20.0/24".into(),
        "10.96.0.10/32".into(),
        "fd00:10:244:1::/64".into(),
    ];
    for entry in [
        "169.254.20.10/32",
        "169.254.20.0/24",
        "10.96.0.10/32",
        "fd00:10:244:1::53/128",
        "::ffff:169.254.20.10/128",
    ] {
        assert_eq!(refused_allow_cidr(entry, &net), None, "{entry}");
    }
    // A wider entry, or one outside the permitted range, stays refused.
    for entry in [
        "169.254.0.0/16",
        "169.254.169.254/32",
        "10.96.0.0/24",
        "fd00:10:244::/56",
    ] {
        assert!(refused_allow_cidr(entry, &net).is_some(), "{entry}");
    }
}

#[test]
fn network_policy_leaves_out_refused_entries() {
    let p = pool(
        "p",
        json!({"network": {"egress": {"internet": "Deny",
            "allowCIDRs": ["192.0.2.10/32", "169.254.169.254/32", "10.244.0.0/24"]}}}),
    );
    let opened = |cfg: &RenderConfig| -> Vec<String> {
        network_policy(&p, cfg)
            .unwrap()
            .spec
            .and_then(|s| s.egress)
            .into_iter()
            .flatten()
            .flat_map(|r| r.to.into_iter().flatten())
            .filter_map(|p| p.ip_block.map(|b| b.cidr))
            .collect()
    };
    let mut cfg = default_config();
    cfg.worker_network = cluster_net();
    assert_eq!(opened(&cfg), vec!["192.0.2.10/32"]);
    // Permitting the cluster entry still leaves the metadata one out.
    cfg.worker_network.permitted_cidrs = vec!["10.244.0.0/24".into()];
    assert_eq!(opened(&cfg), vec!["192.0.2.10/32", "10.244.0.0/24"]);
    // Permitting both renders every entry as its own rule.
    cfg.worker_network
        .permitted_cidrs
        .push("169.254.169.254/32".into());
    assert_eq!(
        opened(&cfg),
        vec!["192.0.2.10/32", "169.254.169.254/32", "10.244.0.0/24"]
    );
    // Withdrawing the permit narrows the policy again.
    cfg.worker_network.permitted_cidrs.clear();
    assert_eq!(opened(&cfg), vec!["192.0.2.10/32"]);
}

#[test]
fn allow_cidr_overlapping_extra_denied_cidrs_is_refused() {
    let mut net = WorkerNetwork {
        extra_denied_cidrs: vec!["203.0.113.0/24".into(), "2001:db8:5::/48".into()],
        ..WorkerNetwork::default()
    };
    for (entry, hit) in [
        ("203.0.113.7/32", "203.0.113.0/24"),
        ("203.0.0.0/16", "203.0.113.0/24"),
        ("::ffff:203.0.113.7/128", "203.0.113.0/24"),
        ("2001:db8:5:1::/64", "2001:db8:5::/48"),
        ("2001:db8::/32", "2001:db8:5::/48"),
    ] {
        let why = refused_allow_cidr(entry, &net).unwrap_or_else(|| panic!("{entry} allowed"));
        assert!(
            why.starts_with(&format!("overlaps the operator-denied range {hit};")),
            "{entry}: {why}"
        );
    }
    assert_eq!(refused_allow_cidr("203.0.114.0/24", &net), None);
    // The operator can permit a range inside its own deny.
    net.permitted_cidrs = vec!["203.0.113.7/32".into()];
    assert_eq!(refused_allow_cidr("203.0.113.7/32", &net), None);
    assert!(refused_allow_cidr("203.0.113.8/32", &net).is_some());
}

#[test]
fn cloud_metadata_endpoints_are_protected() {
    let net = WorkerNetwork::default();
    for (entry, hit) in [
        // AWS IMDS over IPv6, GCE metadata over IPv6, Akamai metadata over IPv6.
        ("fd00:ec2::254/128", "fd00:ec2::254/128"),
        ("fd00:ec2::/32", "fd00:ec2::254/128"),
        ("fd20:ce::254/128", "fd20:ce::254/128"),
        ("fd00:a9fe:a9fe::1/128", "fd00:a9fe:a9fe::1/128"),
        // Oracle Cloud and Scaleway metadata over IPv6.
        ("fd00:c1::a9fe:a9fe/128", "fd00:c1::a9fe:a9fe/128"),
        ("fd00:42::42/128", "fd00:42::42/128"),
        // Alibaba Cloud.
        ("100.100.100.200/32", "100.100.100.200/32"),
        ("100.64.0.0/10", "100.100.100.200/32"),
    ] {
        let why = refused_allow_cidr(entry, &net).unwrap_or_else(|| panic!("{entry} allowed"));
        assert!(why.contains(&format!(" {hit};")), "{entry}: {why}");
    }
    let mut net = net;
    net.permitted_cidrs = vec!["fd00:ec2::254/128".into()];
    assert_eq!(refused_allow_cidr("fd00:ec2::254/128", &net), None);
}

#[test]
fn nat64_entries_are_checked_as_the_ipv4_they_reach() {
    let net = cluster_net();
    for (entry, hit) in [
        // 64:ff9b::/96 (RFC 6052) embeds the IPv4 address in its last 32 bits.
        ("64:ff9b::a9fe:a9fe/128", "169.254.0.0/16"),
        ("64:ff9b::7f00:1/128", "127.0.0.0/8"),
        ("64:ff9b::af4:0/120", "10.244.0.0/16"),
        ("64:ff9b::a00:0/104", "10.244.0.0/16"),
        // A range holding the whole NAT64 block reaches every IPv4 address.
        ("64:ff9b::/64", "169.254.0.0/16"),
        // The local-use prefix (RFC 8215) places the IPv4 address where the
        // operator chose, so any entry inside it is refused.
        ("64:ff9b:1::/48", "64:ff9b:1::/48"),
        ("64:ff9b:1:ab::/64", "64:ff9b:1::/48"),
        ("64:ff9b:1::808:808/128", "64:ff9b:1::/48"),
    ] {
        let why = refused_allow_cidr(entry, &net).unwrap_or_else(|| panic!("{entry} allowed"));
        assert!(why.contains(&format!(" {hit};")), "{entry}: {why}");
    }
    assert_eq!(
        refused_allow_cidr("64:ff9b::/96", &net).as_deref(),
        Some("opens a whole address family")
    );
    // A public IPv4 destination through NAT64 is allowed, as it is directly.
    assert_eq!(refused_allow_cidr("64:ff9b::808:808/128", &net), None);
    assert_eq!(refused_allow_cidr("64:ff9b::c000:200/120", &net), None);
    // An operator permit in IPv4 form covers the NAT64 form of the same hosts.
    let mut net = net;
    net.permitted_cidrs = vec!["10.244.1.0/24".into()];
    assert_eq!(refused_allow_cidr("64:ff9b::af4:100/120", &net), None);
}

#[test]
fn startup_is_probed_fast_and_readiness_slowly() {
    let spec = pod_spec(&pool("p", json!({}))).unwrap();
    let c = &spec.containers[0];
    let startup = c.startup_probe.as_ref().unwrap();
    let ready = c.readiness_probe.as_ref().unwrap();
    assert_eq!(startup.exec, ready.exec);
    assert_eq!(startup.period_seconds, Some(1));
    // Never shorter than the longest startTimeout (30m): the reconciler retires a slow pod first.
    let budget = startup.period_seconds.unwrap() * startup.failure_threshold.unwrap();
    assert!(budget > 30 * 60, "{budget}");
    assert_eq!(ready.period_seconds, Some(10));
    assert_eq!(c.image_pull_policy.as_deref(), Some("IfNotPresent"));
}
