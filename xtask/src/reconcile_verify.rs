//! `cargo xtask reconcile-verify` tests the reconciler against a real API server.
//!
//! The task creates the kind cluster `bk-reconcile-verify` with an API-server
//! audit policy that logs only the controller's own `ServiceAccount`. It runs
//! the controller binary on the host with a token kubeconfig for that account,
//! then runs the ignored scenarios in `crates/server/tests/reconcile_verify.rs`.
//!
//! Only the reconciler is under test, but the binary runs with every real hook
//! and the default config (self-issued edge and API certs, empty
//! `edge.tls.secretName`, no OIDC issuers). The config file sets only the
//! listen addresses (free loopback ports), the required
//! `edge.endpoint` and `serverName`, a short shutdown, and non-default
//! `workerNetwork` values that scenario S7 looks for literally in the applied
//! `NetworkPolicy`. The controller must still be running when the scenarios end.
//! The scenarios get the binary (`RV_BIN`) and its kubeconfig
//! (`RV_CTRL_KUBECONFIG`) to start and kill a second replica of their own.
//!
//! Every kind and kubectl call uses the private kubeconfig under
//! `target/reconcile-verify`, which the scenarios read from `BK_TEST_KUBECONFIG`.
//! The task never reads or writes `~/.kube/config`. It kills the controller and
//! deletes the cluster on every exit path.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};

use crate::kube::Cluster;
use crate::proc::Spawned;

/// Namespace of the controller (Lease, CA Secret, Events).
const NAMESPACE: &str = "bk-reconcile";
const SA: &str = "buildkit-controller";
/// The worker image the scenarios' pools run. The task loads it into kind
/// because Docker Hub DNS inside kind is unreliable on this host.
const IMAGE: &str = crate::image::WORKER_IMAGE;

/// Logs every request of the controller's `ServiceAccount` at `Metadata`
/// (verb, resource, name, response code) and nothing else.
const AUDIT_POLICY: &str = "apiVersion: audit.k8s.io/v1
kind: Policy
omitStages: [RequestReceived]
rules:
- level: Metadata
  users: [\"system:serviceaccount:bk-reconcile:buildkit-controller\"]
- level: None
";

pub(crate) const KIND_CONFIG: &str = "kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
- role: control-plane
  extraMounts:
  - hostPath: POLICY_DIR
    containerPath: /etc/kubernetes/policies
    readOnly: true
  kubeadmConfigPatches:
  - |
    kind: ClusterConfiguration
    apiServer:
      extraArgs:
        audit-log-path: /var/log/kubernetes/audit.log
        audit-policy-file: /etc/kubernetes/policies/audit.yaml
      extraVolumes:
      - name: audit-policies
        hostPath: /etc/kubernetes/policies
        mountPath: /etc/kubernetes/policies
        readOnly: true
        pathType: DirectoryOrCreate
      - name: audit-logs
        hostPath: /var/log/kubernetes
        mountPath: /var/log/kubernetes
        readOnly: false
        pathType: DirectoryOrCreate
";

/// A kubeconfig that authenticates as the controller's `ServiceAccount`, so the
/// audit policy can pick out its requests.
fn controller_kubeconfig(c: &Cluster, work: &Path) -> Result<PathBuf> {
    c.kubectl(&["create", "namespace", NAMESPACE])?;
    c.kubectl(&["-n", NAMESPACE, "create", "serviceaccount", SA])?;
    c.kubectl(&[
        "create",
        "clusterrolebinding",
        "bk-reconcile-verify",
        "--clusterrole=cluster-admin",
        &format!("--serviceaccount={NAMESPACE}:{SA}"),
    ])?;
    let token = c.kubectl(&["-n", NAMESPACE, "create", "token", SA, "--duration=3h"])?;
    let server = c.kubectl(&[
        "config",
        "view",
        "--minify",
        "-o",
        "jsonpath={.clusters[0].cluster.server}",
    ])?;
    let ca = c.kubectl(&[
        "config",
        "view",
        "--minify",
        "--raw",
        "-o",
        "jsonpath={.clusters[0].cluster.certificate-authority-data}",
    ])?;
    let path = work.join("controller.kubeconfig");
    std::fs::write(
        &path,
        format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: kind\n  cluster:\n    server: {server}\n    certificate-authority-data: {ca}\nusers:\n- name: controller\n  user:\n    token: {}\ncontexts:\n- name: controller\n  context: {{cluster: kind, user: controller}}\ncurrent-context: controller\n",
            token.trim()
        ),
    )?;
    Ok(path)
}

#[allow(clippy::too_many_lines, reason = "one linear setup script")]
pub fn reconcile_verify() -> Result<()> {
    let root = crate::root();
    let work = root.join("target/reconcile-verify");
    let policies = work.join("policies");
    std::fs::create_dir_all(&policies)?;
    std::fs::write(policies.join("audit.yaml"), AUDIT_POLICY)?;
    let kind_config = work.join("kind.yaml");
    std::fs::write(
        &kind_config,
        KIND_CONFIG.replace("POLICY_DIR", &policies.display().to_string()),
    )?;
    crate::image::ensure_worker()?;
    let bin = crate::build_controller()?;

    let c = Cluster::new(
        &super::kind_cluster("bk-reconcile-verify"),
        work.join("kubeconfig"),
    );
    c.create(Some(&kind_config), "180s")?;
    crate::proc::status(&mut crate::kube::kind_load(IMAGE, c.name()))?;
    c.apply_crds()?;
    let ctrl_kc = controller_kubeconfig(&c, &work)?;

    let [ops, api, edge] = crate::proc::free_ports()?;
    let config = work.join("controller.yaml");
    std::fs::write(
        &config,
        format!(
            "listen:\n  ops: 127.0.0.1:{ops}\n  api: 127.0.0.1:{api}\n  edge: 127.0.0.1:{edge}\nedge:\n  endpoint: tcp://bk.example.com:8444\n  serverName: bk.example.com\nworkerNetwork:\n  extraDeniedCIDRs: [203.0.113.0/24]\n  dns:\n    namespace: rv-dns\n    podLabels: {{rv-dns: \"yes\"}}\n    cidrs: [198.51.100.53/32]\nshutdown:\n  delay: 0s\n  drainTimeout: 5s\n",
        ),
    )?;
    let log = std::fs::File::create(work.join("controller.log"))?;
    let mut controller = Command::new(&bin);
    crate::kube::with_kubeconfig(&mut controller, &ctrl_kc)
        .arg("--config")
        .arg(&config)
        .env("POD_NAME", "rv-0")
        .env("POD_NAMESPACE", NAMESPACE)
        .env("LOG_LEVEL", "info,buildkit_controller=debug")
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    // Declared after the cluster, so it is killed before the cluster is deleted.
    let mut controller = Spawned::spawn(&mut controller).context("starting the controller")?;
    std::thread::sleep(std::time::Duration::from_secs(5));
    let early = controller.try_wait()?;
    ensure!(early.is_none(), "controller exited at startup: {early:?}");
    let mut tests = c.server_test("reconcile_verify");
    tests
        .env("RV_OPS", format!("127.0.0.1:{ops}"))
        .env("RV_NODE", format!("{}-control-plane", c.name()))
        .env("RV_NAMESPACE", NAMESPACE)
        .env("RV_CONFIG", &config)
        .env("RV_IMAGE", IMAGE)
        .env("RV_BIN", &bin)
        .env("RV_CTRL_KUBECONFIG", &ctrl_kc);
    // In its own group, so an interrupt stops cargo test and the replicas it starts.
    let tests = Spawned::spawn(&mut tests)?.wait()?;
    ensure!(
        tests.success(),
        "reconcile_verify scenarios failed (controller log: {})",
        work.join("controller.log").display()
    );
    let exited = controller.try_wait()?;
    ensure!(
        exited.is_none(),
        "controller exited during the scenarios: {exited:?} (log: {})",
        work.join("controller.log").display()
    );
    println!("reconcile-verify: all scenarios passed");
    Ok(())
}
