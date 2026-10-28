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
        json!({"workers": {"buildkitdConfig": "x"}}),
        json!({"template": {"metadata": {"labels": {"a": "b"}}}}),
        json!({"template": {"spec": {"containers": [{"name": "buildkitd", "image": "other", "resources": {"limits": {"memory": "1Gi", "ephemeral-storage": "1Gi"}}}]}}}),
        json!({"platforms": ["linux/arm64"]}),
        json!({"workers": {"cache": {"sizeLimit": "1Gi"}}}),
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
    let s = worker_secret(&p, &default_inputs(), &fake_tls()).unwrap();
    let data = s.data.unwrap();
    assert_eq!(
        data.keys().map(String::as_str).collect::<Vec<_>>(),
        ["buildkitd.toml", "ca.crt", "tls.crt", "tls.key"]
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
    let secret = worker_secret(&p, &a, &fake_tls()).unwrap();
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

fn state_volume(p: &k8s_openapi::api::core::v1::Pod) -> k8s_openapi::api::core::v1::Volume {
    let vs = p.spec.as_ref().unwrap().volumes.as_ref().unwrap();
    vs.iter().find(|v| v.name == "state").unwrap().clone()
}

fn cached_pool(max_volumes: i32, max: i32) -> BuildKitPool {
    pool(
        "c",
        json!({"scaling": {"max": max}, "workers": {"cache": {"local": {"maxVolumes": max_volumes}}}}),
    )
}

#[test]
fn cached_slots_mount_their_volume_and_others_keep_the_empty_dir() {
    let p = cached_pool(2, 5);
    assert_eq!(cache_limit(&p), 2);
    for (slot, want) in [(0, Some("c-cache-0")), (1, Some("c-cache-1")), (2, None)] {
        let mut i = default_inputs();
        i.slot = slot;
        let v = state_volume(&worker_pod(&p, &i).unwrap());
        let claim = v
            .persistent_volume_claim
            .as_ref()
            .map(|c| c.claim_name.as_str());
        assert_eq!(claim, want, "slot {slot}");
        assert_eq!(v.empty_dir.is_none(), want.is_some(), "slot {slot}");
        assert_eq!(cache_volume(&p, slot).unwrap().is_some(), want.is_some());
    }
    let off = pool("c", json!({}));
    assert_eq!(cache_limit(&off), 0);
    assert!(
        state_volume(&worker_pod(&off, &default_inputs()).unwrap())
            .empty_dir
            .is_some()
    );
    assert!(cache_volume(&off, 0).unwrap().is_none());
}

#[test]
fn cache_limit_is_capped_by_scaling_max() {
    assert_eq!(cache_limit(&cached_pool(10, 3)), 3);
    assert_eq!(cache_limit(&cached_pool(1, 3)), 1);
}

#[test]
fn cache_local_changes_the_hash_only_when_set() {
    let off = pool("c", json!({}));
    let on = cached_pool(2, 5);
    let bigger = pool(
        "c",
        json!({"scaling": {"max": 5}, "workers": {"cache": {"local": {"maxVolumes": 3}}}}),
    );
    assert_ne!(template_hash(&off).unwrap(), template_hash(&on).unwrap());
    assert_ne!(template_hash(&on).unwrap(), template_hash(&bigger).unwrap());
    // The slot's volume source is not a hash input: every slot shares one hash.
    let mut i = default_inputs();
    i.slot = 4;
    let a = worker_pod(&on, &default_inputs()).unwrap();
    let b = worker_pod(&on, &i).unwrap();
    assert_eq!(
        a.metadata.labels.as_ref().unwrap()[LABEL_TEMPLATE_HASH],
        b.metadata.labels.as_ref().unwrap()[LABEL_TEMPLATE_HASH]
    );
}

#[test]
fn volume_hash_tracks_size_and_class_only() {
    let base = buildkit_controller_api::v1alpha1::LocalCache::default();
    let mut more = base.clone();
    more.max_volumes = 99;
    assert_eq!(volume_hash(&base), volume_hash(&more));
    more.size = k8s_openapi::apimachinery::pkg::api::resource::Quantity("30Gi".into());
    assert_ne!(volume_hash(&base), volume_hash(&more));
    let mut class = base.clone();
    class.storage_class = Some("fast".into());
    assert_ne!(volume_hash(&base), volume_hash(&class));
}

#[test]
fn cache_volume_slot_parses_only_this_pools_names() {
    assert_eq!(cache_volume_slot("p", "p-cache-0"), Some(0));
    assert_eq!(cache_volume_slot("p", "p-cache-12"), Some(12));
    for bad in [
        "p-cache-",
        "p-cache-01",
        "p-cache-x",
        "pp-cache-1",
        "q-cache-1",
        "p-cache--1",
    ] {
        assert_eq!(cache_volume_slot("p", bad), None, "{bad}");
    }
}

#[test]
fn pod_cache_keys_round_trip_and_push_keeps_the_newest() {
    let mut pod = worker_pod(&cached_pool(2, 5), &default_inputs()).unwrap();
    assert_eq!(pod_cache_keys(&pod), None);
    set_pod_cache_keys(&mut pod, "a,b");
    assert_eq!(pod_cache_keys(&pod), Some("a,b"));
    assert!(
        keys_contain(Some("a,b"), "b")
            && !keys_contain(Some("ab"), "a")
            && !keys_contain(None, "a")
    );
    assert_eq!(push_key(None, "k", 3), "k");
    assert_eq!(push_key(Some("a,b,c"), "b", 3), "b,a,c");
    assert_eq!(push_key(Some("a,b,c"), "d", 3), "d,a,b");
    assert_eq!(push_key(Some("a,b"), "c", 1), "c");
}

#[test]
fn local_cache_bounds_buildkitd_gc() {
    assert_eq!(quantity_bytes("20Gi"), Some(20 << 30));
    assert_eq!(quantity_bytes("1.5Gi"), Some(3 << 29));
    assert_eq!(quantity_bytes("2G"), Some(2_000_000_000));
    assert_eq!(quantity_bytes("1e9"), None);
    let mut p = cached_pool(2, 5);
    // 80% of the 20Gi default size.
    assert_eq!(
        buildkitd_config(&p),
        "[worker.oci]\n  maxUsedSpace = \"17179869184\"\n  gc = true\n[history]\n  maxEntries = 0\n"
    );
    // A `gc = false` in the section is replaced; `gc` elsewhere and `gckeepstorage` stay.
    p.spec.workers.buildkitd_config =
        "debug = true\n[worker.oci]\n  gc = false\n  gckeepstorage = 1\n[worker.containerd]\n  gc = false\n"
            .into();
    p.spec.workers.cache.local.as_mut().unwrap().gc_keep_storage = Some(
        k8s_openapi::apimachinery::pkg::api::resource::Quantity("1Gi".into()),
    );
    assert_eq!(
        buildkitd_config(&p),
        "debug = true\n[worker.oci]\n  maxUsedSpace = \"1073741824\"\n  gc = true\n  gckeepstorage = 1\n[worker.containerd]\n  gc = false\n[history]\n  maxEntries = 0\n"
    );
    p.spec.workers.buildkitd_config = "debug = true".into();
    assert_eq!(
        buildkitd_config(&p),
        "debug = true\n[worker.oci]\n  maxUsedSpace = \"1073741824\"\n  gc = true\n[history]\n  maxEntries = 0\n"
    );
    // A pool that sets maxUsedSpace keeps it; history is turned off all the same.
    p.spec.workers.buildkitd_config = "[worker.oci]\nmaxUsedSpace = \"5GB\"".into();
    assert_eq!(
        buildkitd_config(&p),
        "[worker.oci]\nmaxUsedSpace = \"5GB\"\n[history]\n  maxEntries = 0\n"
    );
    // A [history] of the pool's own loses its maxEntries and keeps the rest.
    p.spec.workers.buildkitd_config =
        "[history]\n  maxAge = 3600\n  maxEntries = 50\n[worker.oci]\nmaxUsedSpace = \"5GB\"\n"
            .into();
    assert_eq!(
        buildkitd_config(&p),
        "[history]\n  maxEntries = 0\n  maxAge = 3600\n[worker.oci]\nmaxUsedSpace = \"5GB\"\n"
    );
    p.spec.workers.buildkitd_config = "debug = true".into();
    p.spec.workers.cache.local = None;
    assert_eq!(buildkitd_config(&p), "debug = true");
}

fn pod_ann<'a>(p: &'a k8s_openapi::api::core::v1::Pod, k: &str) -> Option<&'a str> {
    p.metadata.annotations.as_ref()?.get(k).map(String::as_str)
}

#[test]
fn cache_pools_run_the_init_step_before_buildkitd() {
    let p = cached_pool(2, 5);
    let spec = pod_spec(&p).unwrap();
    let init = spec.init_containers.as_deref().unwrap();
    assert_eq!(init.len(), 1);
    let c = &init[0];
    let main = &spec.containers[0];
    assert_eq!(c.name, CACHE_INIT_NAME);
    assert_eq!((&c.image, &c.resources), (&main.image, &main.resources));
    let cmd = c.command.as_deref().unwrap();
    assert_eq!(&cmd[..3], ["rootlesskit", "sh", "-c"]);
    let script = &cmd[3];
    for want in [
        "d=/home/user/.local/share/buildkit",
        GENERATION_FILE,
        "rm -rf -- \"$d\"/* \"$d\"/.[!.]* \"$d\"/..?*",
        "rm -f -- \"$d/history.db\"",
        "${EXPECT_GENERATION:-$POD_UID}",
    ] {
        assert!(script.contains(want), "{want} missing from {script}");
    }
    // A wipe drops the generation file first and writes it last, so a wipe cut
    // short leaves a volume without a generation, which the next pod empties.
    let pos = |s: &str| script.find(s).unwrap_or_else(|| panic!("{s} missing"));
    let drop_marker = pos("rm -f -- \"$m\"");
    let wipe = pos("rm -rf -- ");
    let write_marker = pos("printf %s \"$g\" > \"$m\"");
    assert!(drop_marker < wipe && wipe < write_marker, "{script}");
    let env: Vec<(&str, &str)> = c
        .env
        .iter()
        .flatten()
        .map(|e| {
            let f = e.value_from.as_ref().unwrap().field_ref.as_ref().unwrap();
            (e.name.as_str(), f.field_path.as_str())
        })
        .collect();
    assert_eq!(
        env,
        [
            (
                "EXPECT_GENERATION",
                "metadata.annotations['buildkit.smrt-devops.net/cache-generation']"
            ),
            ("POD_UID", "metadata.uid"),
        ]
    );
    // The init step mounts the same state volume as buildkitd.
    let m = &c.volume_mounts.as_deref().unwrap()[0];
    let main_m = main
        .volume_mounts
        .iter()
        .flatten()
        .find(|v| v.name == "state")
        .unwrap();
    assert_eq!((&m.name, &m.mount_path), (&main_m.name, &main_m.mount_path));
    // Without cache.local there is no init step.
    assert!(
        pod_spec(&pool("d", json!({})))
            .unwrap()
            .init_containers
            .is_none()
    );
}

#[test]
fn cached_slot_pods_start_unbound() {
    let p = cached_pool(1, 3);
    let mut i = default_inputs();
    i.slot = 0;
    let pod = worker_pod(&p, &i).unwrap();
    assert_eq!(pod_ann(&pod, KEY_CACHE_SCOPE), Some(""));
    assert_eq!(pod_ann(&pod, KEY_CACHE_GENERATION), None);
    i.slot = 1;
    let pod = worker_pod(&p, &i).unwrap();
    assert_eq!(pod_ann(&pod, KEY_CACHE_SCOPE), None);
    // A pod outside a cached slot serves anyone; a cached one only its scope or any.
    assert!(pod_serves(&pod, false, "a"));
    assert!(!pod_serves(&pod, true, "a"));
}

#[test]
fn bind_pod_keeps_only_the_owner_scope() {
    let b = Binding::parse("s1@g1").unwrap();
    assert_eq!(b.value(), "s1@g1");
    for bad in ["", "s1", "@g", "s@", "s@g@h"] {
        assert_eq!(Binding::parse(bad), None, "{bad}");
    }
    let pod = || {
        let mut i = default_inputs();
        i.slot = 0;
        worker_pod(&cached_pool(1, 2), &i).unwrap()
    };
    // (binding, caller scope) -> (outcome, P cache-scope, P cache-generation)
    let cases = [
        (Some(&b), Some("s1"), Bound::Kept, "s1", Some("g1")),
        (Some(&b), Some("s2"), Bound::Wiped, "s2", None),
        (None, Some("s2"), Bound::Fresh, "s2", None),
        (Some(&b), None, Bound::Kept, "s1", Some("g1")),
        (None, None, Bound::Fresh, "", None),
    ];
    for (binding, scope, want, s, g) in cases {
        let mut p = pod();
        // A stale generation never survives a rebinding.
        set_pod_cache_keys(&mut p, "k");
        p.metadata
            .annotations
            .get_or_insert_default()
            .insert(KEY_CACHE_GENERATION.into(), "stale".into());
        assert_eq!(bind_pod(&mut p, binding, scope), want, "{scope:?}");
        assert_eq!(pod_ann(&p, KEY_CACHE_SCOPE), Some(s));
        assert_eq!(pod_ann(&p, KEY_CACHE_GENERATION), g);
    }
}

#[test]
fn won_binding_names_the_generation_on_the_volume() {
    let mut p = {
        let mut i = default_inputs();
        i.slot = 0;
        worker_pod(&cached_pool(1, 2), &i).unwrap()
    };
    p.metadata.uid = Some("u1".into());
    // Unbound pod: it emptied the volume, so its uid is the generation.
    let w = won_binding(&p, "s").unwrap();
    assert_eq!(w.value(), "s@u1");
    // Bound and kept by its own scope: the binding already says so.
    let b = Binding::parse("s@g").unwrap();
    bind_pod(&mut p, Some(&b), Some("s"));
    assert_eq!(won_binding(&p, "s"), None);
    // Bound to `t` on a volume of `s`: emptied, and the pod's uid starts `t`'s generation.
    bind_pod(&mut p, Some(&b), Some("t"));
    assert_eq!(won_binding(&p, "t").unwrap().value(), "t@u1");
}

proptest! {
    /// Whatever the binding and the caller, a pod keeps a generation only when
    /// the binding names the scope the pod serves, so a volume never reaches a
    /// pod of another scope with its contents.
    #[test]
    fn a_kept_volume_always_belongs_to_the_pod_scope(
        bound in proptest::option::of(("[a-c]", "[a-c]")),
        caller in proptest::option::of("[a-c]"),
    ) {
        let mut i = default_inputs();
        i.slot = 0;
        let mut p = worker_pod(&cached_pool(1, 2), &i).unwrap();
        let b = bound.map(|(s, g)| Binding { scope: s, generation: g });
        let out = bind_pod(&mut p, b.as_ref(), caller.as_deref());
        let serves = pod_ann(&p, KEY_CACHE_SCOPE).unwrap().to_owned();
        if let Some(g) = pod_ann(&p, KEY_CACHE_GENERATION) {
            let b = b.as_ref().unwrap();
            prop_assert_eq!(out, Bound::Kept);
            prop_assert_eq!(&b.scope, &serves);
            prop_assert_eq!(&b.generation, g);
            prop_assert!(caller.as_ref().is_none_or(|c| *c == serves));
        } else {
            prop_assert_ne!(out, Bound::Kept);
        }
        if let Some(c) = &caller {
            prop_assert_eq!(&serves, c);
            prop_assert!(pod_serves(&p, true, c));
        }
    }
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
        ("fd00::/8", "fd00:10:244::/56"),
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
    // Without cluster ranges, 10.0.0.0/8 (a private registry network) is open.
    assert_eq!(
        refused_allow_cidr("10.0.0.0/8", &WorkerNetwork::default()),
        None
    );
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
fn network_policy_refuses_instead_of_widening() {
    let p = pool(
        "p",
        json!({"network": {"egress": {"internet": "Deny",
            "allowCIDRs": ["192.0.2.10/32", "169.254.169.254/32", "10.244.0.0/24"]}}}),
    );
    let mut cfg = default_config();
    cfg.worker_network = cluster_net();
    let err = network_policy(&p, &cfg).unwrap_err();
    assert!(matches!(err, RenderError::EgressRefused(_)), "{err}");
    assert_eq!(
        conflict_names(&err),
        vec![
            "spec.network.egress.allowCIDRs[1]",
            "spec.network.egress.allowCIDRs[2]"
        ]
    );
    let msg = err.to_string();
    assert!(
        msg.contains("`169.254.169.254/32` overlaps") && msg.contains("`10.244.0.0/24` overlaps"),
        "{msg}"
    );

    // Permitting the cluster entry still leaves the metadata one refused.
    cfg.worker_network.permitted_cidrs = vec!["10.244.0.0/24".into()];
    assert_eq!(
        conflict_names(&network_policy(&p, &cfg).unwrap_err()),
        vec!["spec.network.egress.allowCIDRs[1]"]
    );
    // Permitting both renders every entry as its own rule.
    cfg.worker_network
        .permitted_cidrs
        .push("169.254.169.254/32".into());
    let np = network_policy(&p, &cfg).unwrap();
    let opened: Vec<String> = np
        .spec
        .and_then(|s| s.egress)
        .into_iter()
        .flatten()
        .flat_map(|r| r.to.into_iter().flatten())
        .filter_map(|p| p.ip_block.map(|b| b.cidr))
        .collect();
    assert_eq!(
        opened,
        vec!["192.0.2.10/32", "169.254.169.254/32", "10.244.0.0/24"]
    );
}
