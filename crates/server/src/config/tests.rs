use super::*;

const MIN: &str = "edge:\n  endpoint: tcp://bk.example.com:8444\n  serverName: bk.example.com\n";

fn errs(yaml: &str) -> Vec<String> {
    match Config::from_yaml(yaml) {
        Err(ConfigError::Invalid(v)) => v,
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn minimal_config_gets_spec_defaults() {
    let c = Config::from_yaml(MIN).unwrap();
    let t = c.election_timing();
    assert_eq!(t.lease_duration, Duration::from_secs(15));
    assert_eq!(t.renew_interval, Duration::from_secs(2));
    assert_eq!(t.renew_deadline(), Duration::from_secs(10));
    assert_eq!(c.listen.ops.port(), 8080);
    assert_eq!(c.worker_network.denied_cidrs.len(), 11);
}

#[test]
fn unknown_keys_are_errors() {
    let e = Config::from_yaml(&format!("{MIN}bogus: 1\n")).unwrap_err();
    assert!(matches!(e, ConfigError::Parse(_)), "{e}");
}

#[test]
fn edge_is_required_and_all_errors_are_listed() {
    let v = errs("auth:\n  oidc:\n  - url: http://x\n");
    assert!(v.iter().any(|m| m.contains("edge.endpoint")), "{v:?}");
    assert!(v.iter().any(|m| m.contains("edge.serverName")), "{v:?}");
    assert!(v.iter().any(|m| m.contains("must be https")), "{v:?}");
    assert!(v.iter().any(|m| m.contains("audiences")), "{v:?}");
}

#[test]
fn fence_margin_must_cover_request_timeout() {
    let v = errs(&format!(
        "{MIN}kube:\n  requestTimeout: 8s\nleaderElection:\n  fenceMargin: 5s\n  leaseDuration: 60s\n"
    ));
    assert!(v.iter().any(|m| m.contains("fenceMargin")), "{v:?}");
}

#[test]
fn renew_budget_rule() {
    // 6 + 5 >= 15 - 5
    let v = errs(&format!("{MIN}leaderElection:\n  renewInterval: 6s\n"));
    assert!(
        v.iter()
            .any(|m| m.contains("renewInterval + requestTimeout")),
        "{v:?}"
    );
    // 2 + 5 < 10 holds with defaults
    Config::from_yaml(MIN).unwrap();
}

#[test]
fn duplicate_oidc_and_bad_cidr_and_dns() {
    let y = format!(
        "{MIN}auth:\n  oidc:\n  - {{url: 'https://a', audiences: [x]}}\n  - {{url: 'https://a', audiences: [x]}}\nworkerNetwork:\n  extraDeniedCIDRs: [nope]\n  dns:\n    podLabels: {{}}\n"
    );
    let v = errs(&y);
    assert!(v.iter().any(|m| m.contains("duplicated")), "{v:?}");
    assert!(
        v.iter().any(|m| m.contains("not a canonical CIDR")),
        "{v:?}"
    );
    assert!(v.iter().any(|m| m.contains("podLabels or cidrs")), "{v:?}");
}

#[test]
fn bad_endpoint_and_allocation_domain() {
    let v =
        errs("edge:\n  endpoint: https://x\n  serverName: a.b\n  allocationDomain: 'bad name'\n");
    assert!(v.iter().any(|m| m.contains("tcp://")), "{v:?}");
    assert!(v.iter().any(|m| m.contains("allocationDomain")), "{v:?}");
}

#[test]
fn bad_duration_is_reported() {
    let v = errs(&format!("{MIN}shutdown:\n  delay: soon\n"));
    assert!(v.iter().any(|m| m.contains("shutdown.delay")), "{v:?}");
}

#[test]
fn drain_timeout_is_unset_by_default_and_bounded_by_the_connection_lifetime() {
    let c = Config::from_yaml(MIN).unwrap();
    assert_eq!(c.shutdown_delay(), Duration::from_secs(5));
    assert_eq!(c.drain_timeout(), None);
    assert_eq!(c.drain_budget(), MAX_CONNECTION_LIFETIME);
    assert_eq!(MAX_CONNECTION_LIFETIME, Duration::from_mins(1445));
}

#[test]
fn drain_timeout_when_set_is_a_hard_cap() {
    let c = Config::from_yaml(&format!("{MIN}shutdown:\n  drainTimeout: 90s\n")).unwrap();
    assert_eq!(c.drain_timeout(), Some(Duration::from_secs(90)));
    assert_eq!(c.drain_budget(), Duration::from_secs(90));
    let v = errs(&format!("{MIN}shutdown:\n  drainTimeout: later\n"));
    assert!(
        v.iter().any(|m| m.contains("shutdown.drainTimeout")),
        "{v:?}"
    );
}

/// The drain bound follows the longest connection the edge lets live: the CRD's
/// `maxTTL` cap plus the cert margin past the allocation's expiry.
#[test]
fn max_connection_lifetime_matches_the_ttl_cap_and_cert_margin() {
    use buildkit_controller_api::v1alpha1::BuildKitPoolSpec;
    let spec = |ttl: &str| {
        let mut s: BuildKitPoolSpec = serde_json::from_value(serde_json::json!(
            {"access": {"rules": [{"serviceAccount": {"name": "builder"}}]}}
        ))
        .unwrap();
        s.allocation.max_ttl = CrdDuration::new(ttl);
        s.validate()
    };
    assert!(spec("24h").is_ok());
    assert!(
        spec("24h0m1s").is_err(),
        "the CRD cap moved; update MAX_CONNECTION_LIFETIME"
    );
    let margin = Duration::from_secs(
        u64::try_from(crate::alloc::pods::CERT_MARGIN.whole_seconds()).unwrap(),
    );
    assert_eq!(MAX_CONNECTION_LIFETIME, Duration::from_hours(24) + margin);
}

#[test]
fn settings_require_pod_env() {
    let dir = std::env::temp_dir().join(format!("bkc-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("c.yaml");
    std::fs::write(&path, MIN).unwrap();
    let cli = Cli::try_parse_from(["x", "--config", path.to_str().unwrap()]).unwrap();
    let e = Settings::load(cli.clone(), |_| None)
        .unwrap_err()
        .to_string();
    assert!(e.contains("POD_NAME") && e.contains("POD_NAMESPACE"), "{e}");
    let ok = Settings::load(cli, |k| Some(format!("v-{k}"))).unwrap();
    assert_eq!(ok.cli.pod_name, "v-POD_NAME");
    assert_eq!(ok.cli.log_format, LogFormat::Json);
    std::fs::remove_dir_all(dir).ok();
}

fn test_ca_pem() -> String {
    rcgen::generate_simple_self_signed(vec!["ca.test".to_owned()])
        .unwrap()
        .cert
        .pem()
}

fn oidc_yaml(extra: &str) -> String {
    format!("{MIN}auth:\n  oidc:\n  - url: https://i.example\n    audiences: [a]\n{extra}")
}

#[test]
fn ca_bundle_inline_and_file_parse() {
    let pem = test_ca_pem();
    let indented = pem.replace('\n', "\n      ");
    let c = Config::from_yaml(&oidc_yaml(&format!("    caBundle: |\n      {indented}\n"))).unwrap();
    assert_eq!(c.auth.oidc[0].ca_certs().unwrap().len(), 1);

    let path = std::env::temp_dir().join(format!("bkc-config-ca-{}.pem", std::process::id()));
    std::fs::write(&path, &pem).unwrap();
    let c = Config::from_yaml(&oidc_yaml(&format!(
        "    caBundleFile: {}\n",
        path.display()
    )))
    .unwrap();
    assert_eq!(c.auth.oidc[0].ca_certs().unwrap().len(), 1);
    std::fs::remove_file(&path).unwrap();
    assert!(
        Config::from_yaml(&oidc_yaml("")).unwrap().auth.oidc[0]
            .ca_certs()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn ca_bundle_both_set_is_rejected() {
    let v = errs(&oidc_yaml("    caBundle: x\n    caBundleFile: /y\n"));
    assert!(v.iter().any(|m| m.contains("mutually exclusive")), "{v:?}");
}

#[test]
fn ca_bundle_bad_pem_is_rejected() {
    let v = errs(&oidc_yaml("    caBundle: not a pem\n"));
    assert!(v.iter().any(|m| m.contains("caBundle")), "{v:?}");
    let junk = "-----BEGIN CERTIFICATE-----\\nAAAA\\n-----END CERTIFICATE-----\\n";
    let v = errs(&oidc_yaml(&format!("    caBundle: \"{junk}\"\n")));
    assert!(v.iter().any(|m| m.contains("invalid certificate")), "{v:?}");
}

#[test]
fn ca_bundle_unreadable_file_is_rejected() {
    let v = errs(&oidc_yaml("    caBundleFile: /nonexistent/ca.pem\n"));
    assert!(v.iter().any(|m| m.contains("/nonexistent/ca.pem")), "{v:?}");
}

#[test]
fn test_leaf_lifetimes_default_off_and_parse() {
    let c = Config::from_yaml(MIN).unwrap();
    assert_eq!(c.leaf_validity(), crate::pki::LeafValidity::default());
    assert_eq!(c.test_worker_thresholds(), None);
    let c = Config::from_yaml(&format!(
        "{MIN}pki:\n  testLeafLifetimes:\n    workerValidity: 12m\n    workerRenewBefore: 8m\n    workerMinLeft: 6m30s\n    edgeValidity: 4m\n"
    ))
    .unwrap();
    let v = c.leaf_validity();
    assert_eq!(v.worker_serving, time::Duration::minutes(12));
    assert_eq!(v.edge_serving, time::Duration::minutes(4));
    assert_eq!(v.edge_client, time::Duration::minutes(4));
    assert_eq!(
        c.test_worker_thresholds(),
        Some((time::Duration::minutes(8), time::Duration::seconds(390)))
    );
}

#[test]
fn test_leaf_lifetimes_must_be_ordered() {
    let e = errs(&format!(
        "{MIN}pki:\n  testLeafLifetimes:\n    workerValidity: 12m\n    workerRenewBefore: 5m\n    workerMinLeft: 6m\n    edgeValidity: 0s\n"
    ));
    assert!(
        e.iter()
            .any(|m| m.contains("workerMinLeft < workerRenewBefore")),
        "{e:?}"
    );
    assert!(
        e.iter()
            .any(|m| m.contains("edgeValidity must be at least 180s")),
        "{e:?}"
    );
}

fn leaf_yaml(edge: &str) -> String {
    format!(
        "{MIN}pki:\n  testLeafLifetimes:\n    workerValidity: 12m\n    workerRenewBefore: 8m\n    workerMinLeft: 6m30s\n    edgeValidity: {edge}\n"
    )
}

/// Review fix. At or under 2m30s the edge leaves are due for renewal at
/// issue (5 m backdate, renew at 2/3), so the loader would spin.
#[test]
fn test_edge_validity_must_leave_renewal_in_the_future() {
    for bad in ["2m30s", "2m59s"] {
        let e = errs(&leaf_yaml(bad));
        assert!(
            e.iter()
                .any(|m| m.contains("edgeValidity must be at least 180s")),
            "{bad}: {e:?}"
        );
    }
    assert!(Config::from_yaml(&leaf_yaml("3m")).is_ok());
}

#[derive(Clone, Default)]
struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Review fix. The test-only knob is announced at startup.
#[test]
fn test_leaf_lifetimes_are_warned_about() {
    let buf = Buf::default();
    let sub = tracing_subscriber::fmt()
        .with_writer({
            let b = buf.clone();
            move || b.clone()
        })
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(sub, || {
        assert!(!Config::from_yaml(MIN).unwrap().warn_test_knobs());
        assert!(
            Config::from_yaml(&leaf_yaml("4m"))
                .unwrap()
                .warn_test_knobs()
        );
    });
    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert_eq!(out.lines().count(), 1, "{out}");
    assert!(
        out.contains("WARN") && out.contains("edge_validity=\"4m\"") && out.contains("TEST ONLY"),
        "{out}"
    );
}

#[test]
fn worker_network_cidrs_must_be_canonical_strict_subsets() {
    let y = format!(
        "{MIN}workerNetwork:\n  extraDeniedCIDRs: ['34.1.2.3/24', '0.0.0.0/0', '::ffff:10.0.0.0/104', '10.0.0.0/08', '0.0.0.0/00']\n  clusterCIDRs: ['::/0', 'fd00::1/8', '2600:1f18::/32']\n  dns:\n    cidrs: ['169.254.20.10/24']\n"
    );
    let v = errs(&y);
    for bad in [
        "34.1.2.3/24",
        "10.0.0.0/08",
        "0.0.0.0/00",
        "::ffff:10.0.0.0/104",
        "fd00::1/8",
        "169.254.20.10/24",
    ] {
        assert!(
            v.iter()
                .any(|m| m.contains(bad) && m.contains("not a canonical CIDR")),
            "{bad}: {v:?}"
        );
    }
    for whole in ["0.0.0.0/0", "::/0"] {
        assert!(
            v.iter()
                .any(|m| m.contains(whole) && m.contains("whole address family")),
            "{whole}: {v:?}"
        );
    }
    assert_eq!(v.len(), 8, "{v:?}");
    let ok = Config::from_yaml(&format!(
        "{MIN}workerNetwork:\n  clusterCIDRs: ['10.96.0.0/12', '2600:1f18:1234:5600::/56']\n"
    ))
    .unwrap();
    assert_eq!(ok.worker_network.cluster_cidrs.len(), 2);
}

#[test]
fn sa_audiences_must_not_name_the_api_server() {
    for aud in [
        "https://kubernetes.default.svc",
        "https://kubernetes.default.svc.cluster.local",
    ] {
        let v = errs(&format!(
            "{MIN}auth:\n  serviceAccount:\n    audiences: [buildkit-controller, '{aud}']\n"
        ));
        assert!(v.iter().any(|m| m.contains("API server audience")), "{v:?}");
    }
}

#[test]
fn review_limit_must_be_positive() {
    let v = errs(&format!(
        "{MIN}auth:\n  serviceAccount:\n    maxConcurrentReviews: 0\n"
    ));
    assert!(
        v.iter().any(|m| m.contains("maxConcurrentReviews")),
        "{v:?}"
    );
    let c = Config::from_yaml(MIN).unwrap();
    assert_eq!(c.auth.service_account.max_concurrent_reviews, 32);
}

#[test]
fn connection_limits_default_and_zero_rules() {
    let c = Config::from_yaml(MIN).unwrap();
    assert_eq!(
        (c.api.max_connections, c.api.max_connections_per_ip),
        (4096, 128)
    );
    assert_eq!(
        (c.edge.max_handshakes, c.edge.max_handshakes_per_ip),
        (1024, 32)
    );
    let v = errs(&format!(
        "{MIN}  maxHandshakes: 0\n  maxHandshakesPerIp: 0\napi:\n  maxConnections: 0\n  maxConnectionsPerIp: 0\n"
    ));
    for key in ["edge.maxHandshakes ", "api.maxConnections "] {
        assert!(v.iter().any(|m| m.contains(key)), "{key}: {v:?}");
    }
    // A per-address cap of 0 disables it (for clients behind a proxy or SNAT).
    assert!(!v.iter().any(|m| m.contains("PerIp")), "{v:?}");
    let c = Config::from_yaml(&format!(
        "{MIN}  maxHandshakesPerIp: 0\napi:\n  maxConnectionsPerIp: 0\nauth:\n  serviceAccount:\n    maxReviewsPerIp: 0\n"
    ))
    .unwrap();
    assert_eq!(c.auth.service_account.max_reviews_per_ip, 0);
}

#[test]
fn claims_are_off_unless_enabled() {
    assert!(!Config::from_yaml(MIN).unwrap().claims.enabled);
    let v = errs(&format!("{MIN}claims:\n  enabled: true\n"));
    assert!(v.iter().any(|m| m.contains("claims.namespaces")), "{v:?}");
    let c = Config::from_yaml(&format!(
        "{MIN}claims:\n  enabled: true\n  namespaces: [ci]\n"
    ))
    .unwrap();
    assert!(c.claims.enabled && c.claims.namespaces == ["ci"]);
    let e = Config::from_yaml(&format!("{MIN}claims:\n  enable: true\n")).unwrap_err();
    assert!(matches!(e, ConfigError::Parse(_)), "{e}");
}

#[test]
fn watch_namespaces_default_to_cluster_wide() {
    let c = Config::from_yaml(MIN).unwrap();
    assert!(c.watched_namespaces().is_empty());
    let c =
        Config::from_yaml(&format!("{MIN}watchNamespace: \"\"\nwatchNamespaces: []\n")).unwrap();
    assert!(c.watched_namespaces().is_empty());
}

#[test]
fn watch_namespaces_list_and_deprecated_alias() {
    let c = Config::from_yaml(&format!("{MIN}watchNamespaces: [team-a, team-b]\n")).unwrap();
    assert_eq!(c.watched_namespaces(), ["team-a", "team-b"]);
    let c = Config::from_yaml(&format!("{MIN}watchNamespace: ci\n")).unwrap();
    assert_eq!(c.watched_namespaces(), ["ci"]);
}

#[test]
fn watch_namespace_and_watch_namespaces_are_exclusive() {
    let v = errs(&format!("{MIN}watchNamespace: ci\nwatchNamespaces: [ci]\n"));
    assert!(v.iter().any(|m| m.contains("both set")), "{v:?}");
}

#[test]
fn watch_namespaces_must_be_unique_namespace_names() {
    let v = errs(&format!(
        "{MIN}watchNamespaces: [ci, ci, \"\", Bad_Name, -x]\n"
    ));
    assert!(
        v.iter().any(|m| m.contains("`ci` is listed twice")),
        "{v:?}"
    );
    for bad in ["``", "`Bad_Name`", "`-x`"] {
        assert!(
            v.iter()
                .any(|m| m.contains(&format!("{bad} is not a namespace name"))),
            "{bad}: {v:?}"
        );
    }
    let v = errs(&format!("{MIN}watchNamespace: Bad\n"));
    assert!(v.iter().any(|m| m.contains("`Bad` is not")), "{v:?}");
}

#[test]
fn request_timeout_must_be_below_the_read_timeout() {
    let v = errs(&format!("{MIN}kube:\n  requestTimeout: 90s\n"));
    assert!(
        v.iter()
            .any(|m| m.contains("kube.requestTimeout (90s)")
                && m.contains("client read timeout (90s)")),
        "{v:?}"
    );
    let ok = format!(
        "{MIN}kube:\n  requestTimeout: 89s\nleaderElection:\n  fenceMargin: 89s\n  leaseDuration: 600s\n"
    );
    let errors = match Config::from_yaml(&ok) {
        Ok(_) => Vec::new(),
        Err(ConfigError::Invalid(v)) => v,
        Err(e) => panic!("{e}"),
    };
    assert!(
        !errors.iter().any(|m| m.contains("read timeout")),
        "{errors:?}"
    );
}
