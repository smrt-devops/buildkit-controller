//! CRD tests for defaults, validation and the generated schema.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use buildkit_controller_api::v1alpha1::{
    BuildKitPool, BuildKitPoolSpec, BuildKitPoolStatus, Day, Internet, PullPolicy, Scaling,
    Workers, crd,
};
use serde_json::{Value, json};

const MINIMAL: &str = r"
apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitPool
metadata: {name: default, namespace: ci}
spec:
  access:
    rules:
    - oidc: {issuer: https://token.actions.githubusercontent.com, claims: {repository_owner: smrt-devops}}
    - serviceAccount: {name: builder}
";

fn pool(spec: &Value) -> BuildKitPool {
    serde_json::from_value(json!({
        "apiVersion": "buildkit.smrt-devops.net/v1alpha1", "kind": "BuildKitPool",
        "metadata": {"name": "p", "namespace": "ci"}, "spec": spec
    }))
    .unwrap()
}

#[test]
fn minimal_pool_fills_defaults() {
    let p: BuildKitPool = serde_saphyr::from_str(MINIMAL).unwrap();
    p.validate().unwrap();
    let s = &p.spec;
    let c = &s.template.spec.containers[0];
    assert_eq!(c.name, "buildkitd");
    assert_eq!(c.image, "moby/buildkit:v0.34.0-rootless");
    assert_eq!(c.image_pull_policy, PullPolicy::IfNotPresent);
    assert!(s.platforms.is_empty());
    assert_eq!(s.network.egress.internet, Internet::Allow);
    assert!(s.network.egress.allow_cidrs.is_empty());
    assert_eq!(s.allocation.queue_timeout.seconds().unwrap(), 120);
    assert_eq!(s.allocation.max_queue_timeout.seconds().unwrap(), 600);
    assert_eq!(s.workers.start_timeout.seconds().unwrap(), 120);
    let req = c.resources.requests.as_ref().unwrap();
    assert_eq!(
        (
            req["cpu"].0.as_str(),
            req["memory"].0.as_str(),
            req["ephemeral-storage"].0.as_str()
        ),
        ("500m", "1Gi", "1Gi")
    );
    let lim = c.resources.limits.as_ref().unwrap();
    assert_eq!(
        (
            lim["memory"].0.as_str(),
            lim["ephemeral-storage"].0.as_str()
        ),
        ("4Gi", "20Gi")
    );
    assert_eq!((s.scaling.min, s.scaling.max), (0, 10));
    assert_eq!(s.scaling.idle_timeout.seconds().unwrap(), 600);
    assert_eq!(s.scaling.time_zone, "UTC");
    assert_eq!(s.allocation.default_ttl.seconds().unwrap(), 3600);
    assert_eq!(s.allocation.max_ttl.seconds().unwrap(), 86400);
    assert_eq!(s.access.rules.len(), 2);
    assert!(
        s.access.rules[1]
            .service_account
            .as_ref()
            .unwrap()
            .namespace
            .is_none()
    );
    // An empty spec denies everyone.
    let empty = pool(&json!({}));
    assert!(empty.spec.access.rules.is_empty());
    assert_eq!(empty.spec, BuildKitPoolSpec::default());
}

#[test]
fn window_days_default_to_all() {
    let p = pool(&json!({"scaling": {"schedule": [{"start": "08:00", "end": "18:00", "min": 2}]}}));
    assert_eq!(p.spec.scaling.schedule[0].days, Day::ALL);
}

#[test]
fn unknown_enum_value_rejected() {
    let r: Result<Workers, _> = serde_json::from_value(json!({"startTimeout": 5}));
    assert!(r.is_err());
    let r: Result<PullPolicy, _> = serde_json::from_value(json!("Sometimes"));
    assert!(r.is_err());
    let r: Result<Internet, _> = serde_json::from_value(json!("Maybe"));
    assert!(r.is_err());
}

/// Walks the schema `properties` and compares every `default` with the Rust default.
fn check_defaults(schema: &Value, rust: &Value, path: &str) {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    for (k, sub) in props {
        let here = format!("{path}/{k}");
        let Some(val) = rust.get(k) else { continue };
        if let Some(d) = sub.get("default") {
            assert_eq!(d, val, "schema default differs from Default impl at {here}");
        }
        check_defaults(sub, val, &here);
    }
}

#[test]
fn schema_defaults_equal_rust_defaults() {
    let v = serde_json::to_value(crd()).unwrap();
    let root = &v["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"];
    check_defaults(
        root,
        &serde_json::to_value(BuildKitPoolSpec::default()).unwrap(),
        "spec",
    );
    // Every block has a default.
    for b in [
        "template",
        "workers",
        "network",
        "scaling",
        "access",
        "allocation",
    ] {
        assert!(
            root["properties"][b].get("default").is_some(),
            "{b} lacks default"
        );
    }
}

#[test]
fn defaults_validate() {
    BuildKitPoolSpec::default().validate().unwrap();
}

#[allow(clippy::needless_pass_by_value)]
fn tmpl(t: Value) -> Value {
    json!({"template": t})
}

fn mount(path: &str) -> Value {
    tmpl(
        json!({"spec": {"volumes": [{"name": "v", "emptyDir": {}}], "containers": [{"name": "buildkitd", "volumeMounts": [{"name": "v", "mountPath": path}]}]}}),
    )
}

fn invalid(spec: &Value) -> String {
    let p = pool(spec);
    p.validate()
        .expect_err(&format!("{spec} should be invalid"))
        .to_string()
}

#[test]
#[allow(clippy::too_many_lines)]
fn validation_rejects_every_listed_case() {
    let oidc = |issuer: &str, claims: &Value| json!({"access": {"rules": [{"oidc": {"issuer": issuer, "claims": claims}}]}});
    let win = |s: &str, e: &str, m: i32| json!({"scaling": {"schedule": [{"start": s, "end": e, "min": m}]}});
    let cases = [
        json!({"workers": {"buildkitdConfig": "x".repeat(65537)}}),
        json!({"workers": {"startTimeout": "9s"}}),
        json!({"workers": {"startTimeout": "31m"}}),
        json!({"workers": {"startTimeout": "5x"}}),
        json!({"workers": {"startTimeout": ""}}),
        json!({"scaling": {"min": -1}}),
        json!({"scaling": {"min": 501, "max": 501}}),
        json!({"scaling": {"max": 0}}),
        json!({"scaling": {"min": 5, "max": 4}}),
        json!({"scaling": {"idleTimeout": "59s"}}),
        json!({"scaling": {"idleTimeout": "25h"}}),
        json!({"scaling": {"timeZone": "x".repeat(65)}}),
        win("8:00", "18:00", 1),
        win("24:00", "18:00", 1),
        win("08:60", "18:00", 1),
        win("08:00", "08:00", 1),
        win("08:00", "18:00", 11),
        win("08:00", "18:00", -1),
        win("08:00", "18:00:00", 1),
        tmpl(
            json!({"spec": {"containers": [{"name": "buildkitd", "env": [{"name": "A", "valueFrom": {"resourceFieldRef": {"resource": "limits.memory"}}}]}]}}),
        ),
        tmpl(
            json!({"spec": {"containers": [{"name": "buildkitd", "env": [{"name": "A", "valueFrom": {"fileKeyRef": {"volumeName": "v", "path": "p", "key": "k"}}}]}]}}),
        ),
        tmpl(
            json!({"spec": {"containers": [{"name": "buildkitd", "env": [{"name": "N".repeat(129), "value": "x"}]}]}}),
        ),
        tmpl(
            json!({"spec": {"containers": [{"name": "buildkitd", "env": (0..33).map(|i| json!({"name": format!("E{i}"), "value": "x"})).collect::<Vec<_>>()}]}}),
        ),
        tmpl(
            json!({"spec": {"volumes": (0..17).map(|i| json!({"name": format!("v{i}"), "emptyDir": {}})).collect::<Vec<_>>()}}),
        ),
        tmpl(json!({"spec": {"volumes": [{"name": "v".repeat(64), "emptyDir": {}}]}})),
        mount("//certs"),
        mount("/data/../etc/buildkit"),
        mount("/.."),
        mount("data"),
        mount("/data/"),
        mount("/data//x"),
        mount(&format!("/{}", "p".repeat(256))),
        json!({"scaling": {"schedule": (0..33).map(|_| json!({"start": "01:00", "end": "02:00", "min": 0})).collect::<Vec<_>>()}}),
        json!({"scaling": {"schedule": [{"days": ["Mon", "Mon"], "start": "01:00", "end": "02:00", "min": 0}]}}),
        json!({"access": {"rules": [{}]}}),
        json!({"access": {"rules": [{"oidc": {"issuer": "https://a", "claims": {"a": "b"}}, "serviceAccount": {}}]}}),
        json!({"access": {"rules": [{"serviceAccount": {}, "claims": {"namespaces": ["ci"]}}]}}),
        json!({"access": {"rules": [{"claims": {"namespaces": []}}]}}),
        json!({"access": {"rules": [{"claims": {"namespaces": ["Bad_NS"]}}]}}),
        json!({"access": {"rules": [{"claims": {"namespaces": (0..33).map(|i| format!("n{i}")).collect::<Vec<_>>()}}]}}),
        oidc("http://a", &json!({"a": "b"})),
        oidc("https://a", &json!({})),
        oidc("https://a", &json!({"a": "*"})),
        oidc("https://a", &json!({"a": "x".repeat(257)})),
        oidc(
            "https://a",
            &(0..17)
                .map(|i| (format!("c{i}"), json!("v")))
                .collect::<serde_json::Map<_, _>>()
                .into(),
        ),
        json!({"access": {"rules": [{"serviceAccount": {"namespace": "Bad_NS"}}]}}),
        json!({"access": {"rules": [{"serviceAccount": {"name": "-bad"}}]}}),
        json!({"access": {"rules": (0..65).map(|_| json!({"serviceAccount": {}})).collect::<Vec<_>>()}}),
        json!({"allocation": {"defaultTTL": "59s"}}),
        json!({"allocation": {"maxTTL": "25h"}}),
        json!({"allocation": {"defaultTTL": "3h", "maxTTL": "2h"}}),
        json!({"allocation": {"queueTimeout": "11m"}}),
        json!({"allocation": {"maxQueueTimeout": "2h", "queueTimeout": "1m"}}),
        json!({"platforms": ["linux/amd64", "linux/amd64"]}),
        json!({"platforms": ["Linux/amd64"]}),
        json!({"platforms": ["linux"]}),
        json!({"platforms": (0..9).map(|i| format!("linux/a{i}")).collect::<Vec<_>>()}),
        json!({"network": {"egress": {"allowCIDRs": ["10.0.0.0"]}}}),
        json!({"network": {"egress": {"allowCIDRs": ["10.0.0.0/33"]}}}),
        json!({"network": {"egress": {"allowCIDRs": ["10.1.2.3/16"]}}}),
        json!({"network": {"egress": {"allowCIDRs": ["10.0.0.0/08"]}}}),
        json!({"network": {"egress": {"allowCIDRs": ["0.0.0.0/00"]}}}),
        json!({"network": {"egress": {"allowCIDRs": ["fd00::1/8"]}}}),
        json!({"network": {"egress": {"allowCIDRs": ["::ffff:10.0.0.0/104"]}}}),
        tmpl(json!({"spec": {"priorityClassName": "system-node-critical"}})),
        tmpl(
            json!({"spec": {"containers": [{"name": "buildkitd", "env": [{"name": "A", "valueFrom": {}}]}]}}),
        ),
        tmpl(
            json!({"spec": {"containers": [{"name": "buildkitd", "env": [{"name": "A", "valueFrom": {"fieldRef": {"fieldPath": "metadata.name"}, "secretKeyRef": {"name": "s", "key": "k"}}}]}]}}),
        ),
        tmpl(json!({"spec": {"volumes": [{"name": "v", "projected": {"sources": [{}]}}]}})),
        mount("/certs\u{0}"),
        mount("/data\u{7f}"),
        tmpl(
            json!({"spec": {"volumes": [{"name": "v", "emptyDir": {}}], "containers": [{"name": "buildkitd", "volumeMounts": [{"name": "v", "mountPath": "/data", "subPath": "a\nb"}]}]}}),
        ),
        tmpl(
            json!({"spec": {"volumes": [{"name": "v", "emptyDir": {}}], "containers": [{"name": "buildkitd", "volumeMounts": [{"name": "v", "mountPath": "/data", "mountPropagation": "Bidirectional"}]}]}}),
        ),
        json!({"network": {"egress": {"allowCIDRs": (0..33).map(|i| format!("10.0.{i}.0/24")).collect::<Vec<_>>()}}}),
        tmpl(json!({"metadata": {"labels": {"app.kubernetes.io/name": "x"}}})),
        tmpl(json!({"metadata": {"labels": {"buildkit.smrt-devops.net/slot": "x"}}})),
        tmpl(json!({"metadata": {"annotations": {"buildkit.smrt-devops.net/a": "x"}}})),
        tmpl(json!({"spec": {"volumes": [{"name": "certs", "emptyDir": {}}]}})),
        tmpl(json!({"spec": {"containers": []}})),
        tmpl(json!({"spec": {"containers": [{"name": "other"}]}})),
        tmpl(json!({"spec": {"containers": [{"name": "buildkitd"}, {"name": "buildkitd"}]}})),
        tmpl(json!({"spec": {"runtimeClassName": "Bad_Name"}})),
        tmpl(
            json!({"spec": {"containers": [{"name": "buildkitd", "resources": {"limits": {"memory": "1Gi"}}}]}}),
        ),
        tmpl(
            json!({"spec": {"containers": [{"name": "buildkitd", "env": [{"name": "BUILDKITD_FLAGS", "value": "x"}]}]}}),
        ),
        tmpl(
            json!({"spec": {"containers": [{"name": "buildkitd", "env": [{"name": "ROOTLESSKIT_X", "value": "x"}]}]}}),
        ),
        tmpl(
            json!({"spec": {"containers": [{"name": "buildkitd", "volumeMounts": [{"name": "nope", "mountPath": "/data"}]}]}}),
        ),
        tmpl(
            json!({"spec": {"volumes": [{"name": "v", "emptyDir": {}}], "containers": [{"name": "buildkitd", "volumeMounts": [{"name": "v", "mountPath": "/certs/x"}]}]}}),
        ),
        tmpl(
            json!({"spec": {"volumes": [{"name": "v", "emptyDir": {}}], "containers": [{"name": "buildkitd", "volumeMounts": [{"name": "v", "mountPath": "/run/user/1000"}]}]}}),
        ),
    ];
    for c in &cases {
        let _ = invalid(c);
    }
}

#[test]
fn long_pool_name_rejected() {
    let mut p = pool(&json!({}));
    p.metadata.name = Some("x".repeat(53));
    assert!(p.validate().is_err());
    p.metadata.name = Some("x".repeat(52));
    p.validate().unwrap();
}

#[test]
fn duration_grammar() {
    use buildkit_controller_api::v1alpha1::Duration;
    assert_eq!(Duration::new("1h30m5s").seconds().unwrap(), 5405);
    assert_eq!(Duration::new("90s").seconds().unwrap(), 90);
    for bad in ["", "m", "1s1m", "1 h", "-1s", "1.5h", "h1"] {
        assert!(Duration::new(bad).seconds().is_err(), "{bad}");
    }
}

#[test]
fn generated_crd_has_columns_names_and_cel() {
    let y = serde_saphyr::to_string(&crd()).unwrap();
    for needle in [
        "group: buildkit.smrt-devops.net",
        "scope: Namespaced",
        "plural: buildkitpools",
        "singular: buildkitpool",
        "- bkp",
        "- buildkit",
        "subresources",
        "jsonPath: .spec.scaling.min",
        "jsonPath: .spec.scaling.max",
        "jsonPath: .status.warm",
        "jsonPath: .status.allocated",
        r#"jsonPath: .status.conditions[?(@.type=="Ready")].status"#,
        "jsonPath: .metadata.creationTimestamp",
        "self.metadata.name.size() <= 52",
        "self.min <= self.max",
        "self.schedule.all(w, w.min <= self.max)",
        "self.start != self.end",
        "[has(self.oidc), has(self.serviceAccount), has(self.claims)].filter(x, x).size() == 1",
        "duration(self.defaultTTL) <= duration(self.maxTTL)",
        "duration(self.queueTimeout) <= duration(self.maxQueueTimeout)",
        "isCIDR(c)",
        "size(self) == 1 && self[0].name == 'buildkitd'",
        "allowCIDRs:",
        "jsonPath: .status.waiting",
        "jsonPath: .status.platforms",
        "x-kubernetes-list-map-keys:",
        "duration(self) >= duration('10s') && duration(self) <= duration('30m')",
        "x-kubernetes-list-type: map",
        "x-kubernetes-list-type: set",
    ] {
        assert!(y.contains(needle), "CRD lacks `{needle}`");
    }
    assert!(!y.contains("x-kubernetes-preserve-unknown-fields"));
    assert_eq!(Scaling::default().max, 10);
}

#[test]
fn status_wire_names_match_spec() {
    use buildkit_controller_api::v1alpha1::BuildKitPoolStatus;
    let wire = json!({
        "observedGeneration": 3, "warm": 2, "starting": 1, "allocated": 0,
        "desiredWarm": 2, "endpoint": "e:443", "serverName": "s",
        "workerTLS": {"notAfter": "2026-01-01T00:00:00Z", "hash": "abcd", "caFingerprint": "ff"},
        "conditions": []
    });
    // A status written before per-pod worker certs still reads; the old
    // pool-wide `workerTLS` is dropped.
    let status: BuildKitPoolStatus = serde_json::from_value(wire).unwrap();
    let back = serde_json::to_value(&status).unwrap();
    assert!(back.get("workerTLS").is_none(), "{back}");
    assert_eq!(back["serverName"], "s");
    let y = serde_saphyr::to_string(&crd()).unwrap();
    assert!(!y.contains("workerTLS") && !y.contains("workerTls"));
}

#[test]
fn access_rule_options_are_not_nullable() {
    let schema = serde_json::to_value(crd()).unwrap();
    let rule = &schema["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["properties"]
        ["access"]["properties"]["rules"]["items"]["properties"];
    for f in ["oidc", "serviceAccount"] {
        assert!(rule[f].get("nullable").is_none(), "{f}: {}", rule[f]);
    }
}

#[test]
fn template_defaults_and_valid_overrides() {
    let p = pool(&json!({
        "platforms": ["linux/amd64", "linux/arm64/v8"],
        "network": {"egress": {"internet": "Deny", "allowCIDRs": ["10.0.0.0/8", "fd00::/8"]}},
        "template": {
            "metadata": {"labels": {"team": "ci"}},
            "spec": {
                "runtimeClassName": "gvisor",
                "volumes": [{"name": "cfg", "configMap": {"name": "c"}}],
                "containers": [{"name": "buildkitd", "volumeMounts": [{"name": "cfg", "mountPath": "/etc/extra"}]}]
            }
        },
        "allocation": {"queueTimeout": "0s"}
    }));
    p.validate().unwrap();
    assert_eq!(p.spec.network.egress.internet, Internet::Deny);
    let out = serde_json::to_value(&p.spec).unwrap();
    assert_eq!(out["network"]["egress"]["allowCIDRs"][1], "fd00::/8");
    assert!(out["network"]["egress"].get("allowCidrs").is_none());
    // Omitted container fields keep their defaults.
    assert_eq!(
        p.spec.template.spec.containers[0].image,
        "moby/buildkit:v0.34.0-rootless"
    );
}

#[test]
fn template_schema_prunes_volume_sources_and_matches_spec_names() {
    let v = serde_json::to_value(crd()).unwrap();
    let spec =
        &v["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["properties"];
    let vol = &spec["template"]["properties"]["spec"]["properties"]["volumes"];
    assert_eq!(vol["maxItems"], 16);
    let props = vol["items"]["properties"].as_object().unwrap();
    let mut keys: Vec<_> = props.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["configMap", "emptyDir", "name", "projected", "secret"]
    );
    let src =
        vol["items"]["properties"]["projected"]["properties"]["sources"]["items"]["properties"]
            .as_object()
            .unwrap();
    let mut keys: Vec<_> = src.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["configMap", "downwardAPI", "secret"]);
    let tspec = &spec["template"]["properties"]["spec"]["properties"];
    let cont = &tspec["containers"]["items"]["properties"];
    let env = &cont["env"];
    assert_eq!(env["maxItems"], 32);
    assert_eq!(env["items"]["properties"]["name"]["maxLength"], 128);
    let mut keys: Vec<_> = env["items"]["properties"]["valueFrom"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["configMapKeyRef", "fieldRef", "secretKeyRef"]);
    let mounts = &cont["volumeMounts"];
    assert_eq!(mounts["maxItems"], 16);
    assert_eq!(mounts["items"]["properties"]["mountPath"]["maxLength"], 256);
    assert_eq!(mounts["items"]["properties"]["name"]["maxLength"], 63);
    assert_eq!(vol["items"]["properties"]["name"]["maxLength"], 63);
    let win = &spec["scaling"]["properties"]["schedule"]["items"]["properties"];
    assert_eq!(win["start"]["maxLength"], 5);
    assert_eq!(win["end"]["maxLength"], 5);
    let meta = &spec["template"]["properties"]["metadata"]["properties"];
    assert!(meta["labels"].get("maxProperties").is_none());
    assert!(meta["annotations"].get("maxProperties").is_none());
    for f in [
        "platforms",
        "template",
        "workers",
        "network",
        "scaling",
        "access",
        "allocation",
    ] {
        assert!(spec.get(f).is_some(), "spec.{f} missing");
    }
    assert!(
        spec["network"]["properties"]["egress"]["properties"]
            .get("allowCIDRs")
            .is_some()
    );
    for f in ["queueTimeout", "maxQueueTimeout", "defaultTTL", "maxTTL"] {
        assert!(spec["allocation"]["properties"].get(f).is_some(), "{f}");
    }
    let st = &v["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["status"]["properties"];
    assert_eq!(st["waiters"]["maxItems"], 64);
    assert_eq!(st["waiters"]["x-kubernetes-list-map-keys"][0], "replica");
    for f in ["waiting", "platforms"] {
        assert!(st.get(f).is_some(), "status.{f}");
    }
    assert!(st.get("workerTLS").is_none());
}

#[test]
fn status_waiters_platforms_roundtrip() {
    let wire = json!({
        "waiting": 2, "platforms": ["linux/amd64"],
        "waiters": [{"replica": "bk-0", "count": 2, "updatedAt": "2026-01-01T00:00:00Z"}]
    });
    let s: BuildKitPoolStatus = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(s.waiters[0].replica, "bk-0");
    assert_eq!(serde_json::to_value(&s).unwrap(), wire);
}

#[test]
fn condition_names_match_spec() {
    use buildkit_controller_api::v1alpha1::{condition, reason};
    assert_eq!(condition::CA_EXPIRING_SOON, "CAExpiringSoon");
    for (got, want) in [
        (reason::TEMPLATE_CONFLICT, "TemplateConflict"),
        (reason::SANDBOX_PENDING, "SandboxPending"),
        (reason::SLOT_TAKEN, "SlotTaken"),
        (reason::CA_REPLACED, "CAReplaced"),
        (reason::CA_UNAVAILABLE, "CAUnavailable"),
    ] {
        assert_eq!(got, want);
    }
}

#[test]
fn mount_path_rule_is_structural() {
    for p in [
        "/./certs",
        "/.",
        "/etc/./buildkit",
        "/home/user/.local/share/./buildkit",
        "/data/.",
        "/./data",
        "//certs",
        "/a/../certs",
        "/..",
        "/data/",
        "/",
        "",
        "relative",
        "./data",
    ] {
        invalid(&mount(p));
    }
    for p in [
        "/data",
        "/home/user/.cache",
        "/scratch/tmp",
        "/a/.b/..c/...",
        "/certsx",
        "/etc/buildkitx",
    ] {
        let spec = mount(p);
        pool(&spec)
            .validate()
            .unwrap_or_else(|e| panic!("{p} should be valid: {e}"));
    }
}

#[test]
fn mount_path_length_counts_characters_not_bytes() {
    // The CRD maxLength counts characters. "é" is 2 bytes but 1 character.
    let at_limit = format!("/{}", "é".repeat(200));
    assert!(pool(&mount(&at_limit)).validate().is_ok());
    let over_limit = format!("/{}", "é".repeat(256));
    assert!(pool(&mount(&over_limit)).validate().is_err());
}

#[test]
fn multibyte_lengths_count_characters_not_bytes() {
    // buildkitdConfig: 65536 characters of 2-byte "é" is 131072 bytes.
    let config = |s: String| pool(&json!({"workers": {"buildkitdConfig": s}}));
    assert!(config("é".repeat(65536)).validate().is_ok());
    assert!(config("é".repeat(65537)).validate().is_err());

    // oidc issuer: "https://" (8 chars) plus 504 "é" is exactly 512 characters.
    let issuer = |rest: usize| {
        pool(&json!({"access": {"rules": [{"oidc": {
            "issuer": format!("https://{}", "é".repeat(rest)),
            "claims": {"repository_owner": "smrt-devops"}
        }}]}}))
    };
    assert!(issuer(504).validate().is_ok());
    assert!(issuer(505).validate().is_err());

    // timeZone (64 characters) and image (512 characters).
    let zone = |n: usize| pool(&json!({"scaling": {"timeZone": "é".repeat(n)}}));
    assert!(zone(64).validate().is_ok());
    assert!(zone(65).validate().is_err());
    let image = |n: usize| {
        pool(
            &json!({"template": {"spec": {"containers": [{"name": "buildkitd", "image": "é".repeat(n)}]}}}),
        )
    };
    assert!(image(512).validate().is_ok());
    assert!(image(513).validate().is_err());
}

#[test]
fn canonical_cidrs_only() {
    use buildkit_controller_api::v1alpha1::is_canonical_cidr;
    for ok in [
        "10.0.0.0/8",
        "10.1.2.3/32",
        "0.0.0.0/0",
        "fd00::/8",
        "64:ff9b::/96",
        "::/0",
    ] {
        assert!(is_canonical_cidr(ok), "{ok}");
    }
    for bad in [
        "10.1.2.3/16",
        "fd00::1/8",
        "::ffff:10.0.0.0/104",
        "10.0.0.0/33",
        "10.0.0.0/+8",
        "10.0.0.0",
        "010.0.0.0/8",
        "10.0.0.0/08",
        "0.0.0.0/00",
        "10.0.0.0/",
    ] {
        assert!(!is_canonical_cidr(bad), "{bad}");
    }
}

#[test]
fn hardened_template_fields_still_accept_valid_values() {
    let p = pool(&json!({
        "network": {"egress": {"allowCIDRs": ["10.2.0.0/16", "2001:db8::/32"]}},
        "template": {"spec": {
            "priorityClassName": "builds-low",
            "volumes": [{"name": "v", "projected": {"sources": [{"secret": {"name": "s"}}, {"configMap": {"name": "c"}}]}}],
            "containers": [{"name": "buildkitd",
                "env": [{"name": "A", "valueFrom": {"secretKeyRef": {"name": "s", "key": "k"}}}],
                "volumeMounts": [{"name": "v", "mountPath": "/data", "subPath": "x", "mountPropagation": "HostToContainer"}]}]
        }}
    }));
    p.validate().expect("valid pool");
}

#[test]
fn claims_rule_validates() {
    pool(&json!({"access": {"rules": [{"claims": {"namespaces": ["ci", "release"]}}]}}))
        .validate()
        .unwrap();
}

#[test]
fn cache_local_is_off_by_default_and_its_defaults_match() {
    use buildkit_controller_api::v1alpha1::LocalCache;
    assert!(BuildKitPoolSpec::default().workers.cache.local.is_none());
    let v = serde_json::to_value(crd()).unwrap();
    let cache = &v["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["properties"]
        ["workers"]["properties"]["cache"];
    let local = &cache["properties"]["local"];
    assert!(local.get("default").is_none(), "local must stay unset");
    assert!(
        local.get("nullable").is_none(),
        "local must not be nullable"
    );
    check_defaults(
        local,
        &serde_json::to_value(LocalCache::default()).unwrap(),
        "spec/workers/cache/local",
    );
    assert_eq!(local["properties"]["size"]["default"], "20Gi");
    assert_eq!(local["properties"]["maxVolumes"]["default"], 10);
    // An empty block turns the feature on with every default.
    let p = pool(&json!({"workers": {"cache": {"local": {}}}}));
    assert_eq!(p.spec.workers.cache.local, Some(LocalCache::default()));
    p.spec.validate().unwrap();
    let p = pool(&json!({"workers": {"cache": {"local": {"maxVolumes": 0}}}}));
    assert!(p.spec.validate().is_err());
}

#[test]
fn cache_local_size_is_not_nullable() {
    let v = serde_json::to_value(crd()).unwrap();
    let size = &v["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["properties"]
        ["workers"]["properties"]["cache"]["properties"]["local"]["properties"]["size"];
    assert_ne!(size.get("nullable"), Some(&json!(true)));
    assert_eq!(size["x-kubernetes-int-or-string"], true);
    assert!(size.get("x-kubernetes-validations").is_some());
}

#[test]
#[allow(clippy::many_single_char_names)]
fn cache_registry_defaults_resolve_and_validate() {
    use buildkit_controller_api::v1alpha1::{
        CacheAffinity, CacheMode, is_image_name, is_image_ref,
    };
    let p = pool(&json!({"workers": {"cache": {
        "registry": {"repository": "ghcr.io/org/cache", "importFrom": ["trunk", "ghcr.io/org/base:main", "trunk"]},
        "local": {}
    }}}));
    p.spec.validate().unwrap();
    let c = &p.spec.workers.cache;
    let r = c.registry_enabled().expect("enabled by default");
    assert_eq!(
        (r.mode, r.ref_template.as_str()),
        (CacheMode::Max, "{repository}:{cacheKey}")
    );
    let l = c.local.as_ref().unwrap();
    assert_eq!(
        (l.affinity, l.keys_per_volume, l.gc_keep_storage.as_ref()),
        (CacheAffinity::Soft, 8, None)
    );
    assert_eq!(c.default_key("app"), "app");
    let f = r.resolve("app", "app-main");
    assert_eq!(
        f.to,
        "type=registry,ref=ghcr.io/org/cache:app-main,mode=max,image-manifest=true,oci-mediatypes=true"
    );
    assert_eq!(
        f.from,
        [
            "type=registry,ref=ghcr.io/org/cache:app-main",
            "type=registry,ref=ghcr.io/org/cache:trunk",
            "type=registry,ref=ghcr.io/org/base:main",
        ]
    );
    let p = pool(&json!({"workers": {"cache": {"registry": {
        "repository": "localhost:5000/c", "ref": "{repository}:{pool}-{cacheKey}", "defaultKey": "dflt", "enabled": false
    }}}}));
    p.spec.validate().unwrap();
    assert!(p.spec.workers.cache.registry_enabled().is_none());
    assert_eq!(p.spec.workers.cache.default_key("app"), "dflt");

    for bad in [
        json!({"repository": "ghcr.io/org/cache:tag"}),
        json!({"repository": "Ghcr.io/x"}),
        json!({"repository": "r.io/x", "ref": "{repository}:{branch}"}),
        json!({"repository": "r.io/x", "ref": "{repository}::{cacheKey}"}),
        json!({"repository": "r.io/x", "defaultKey": "a/b"}),
        json!({"repository": "r.io/x", "importFrom": ["not a key"]}),
        json!({"repository": "r.io/x", "importFrom": ["a","b","c","d","e","f","g","h","i"]}),
    ] {
        let p = pool(&json!({"workers": {"cache": {"registry": bad}}}));
        assert!(p.spec.validate().is_err(), "{bad} must be rejected");
    }
    let p = pool(&json!({"workers": {"cache": {"local": {"keysPerVolume": 33}}}}));
    assert!(p.spec.validate().is_err());
    for ok in ["localhost:5000/a/b", "r.io/x", "cache", "a-b.c_d/e"] {
        assert!(is_image_name(ok), "{ok}");
    }
    for ok in [
        "r.io/x:t",
        "cache:main",
        "localhost:5000/x:v1.2",
        "r.io/x@sha256:0000000000000000000000000000000000000000000000000000000000000000",
    ] {
        assert!(is_image_ref(ok), "{ok}");
    }
    for no in ["r.io/x:", "r.io//x", "-a/b", "r.io/x:t@sha1:00"] {
        assert!(!is_image_ref(no), "{no}");
    }
}
