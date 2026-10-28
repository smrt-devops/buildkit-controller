//! `cargo xtask reconcile-verify`: the reconciler against a real API server.
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
//! Every kind and kubectl call passes `--kubeconfig target/reconcile-verify/...`.
//! The task never reads or writes `~/.kube/config`. It kills the controller and
//! deletes the cluster on every exit path.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result, ensure};

static CLUSTER: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| super::kind_cluster("bk-reconcile-verify"));
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

/// Kills the controller and deletes the cluster when dropped.
struct Cleanup {
    kubeconfig: PathBuf,
    controller: Option<Child>,
}

impl Cleanup {
    fn kind(&self, args: &[&str]) -> Result<std::process::Output> {
        crate::kube::kind(&self.kubeconfig)
            .args(args)
            .arg("--kubeconfig")
            .arg(&self.kubeconfig)
            .output()
            .context("running kind")
    }

    fn kubectl(&self, args: &[&str]) -> Result<String> {
        let out = crate::kube::kubectl(&self.kubeconfig)
            .arg("--kubeconfig")
            .arg(&self.kubeconfig)
            .args(args)
            .output()
            .context("running kubectl")?;
        ensure!(
            out.status.success(),
            "kubectl {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(c) = &mut self.controller {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = self.kind(&["delete", "cluster", "--name", CLUSTER.as_str()]);
    }
}

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// A kubeconfig that authenticates as the controller's `ServiceAccount`, so the
/// audit policy can pick out its requests.
fn controller_kubeconfig(c: &Cleanup, work: &Path) -> Result<PathBuf> {
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
    let built = Command::new("cargo")
        .args(["build", "--locked", "-p", "buildkit-controller"])
        .current_dir(&root)
        .status()?;
    ensure!(built.success(), "cargo build failed");
    let bin = root.join("target/debug/buildkit-controller");

    let mut c = Cleanup {
        kubeconfig: work.join("kubeconfig"),
        controller: None,
    };
    let _ = c.kind(&["delete", "cluster", "--name", CLUSTER.as_str()]);
    let kc = kind_config.display().to_string();
    let created = c.kind(&[
        "create",
        "cluster",
        "--name",
        CLUSTER.as_str(),
        "--config",
        &kc,
        "--wait",
        "180s",
    ])?;
    ensure!(
        created.status.success(),
        "kind create: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let loaded = crate::kube::kind_load(IMAGE, CLUSTER.as_str()).status()?;
    ensure!(loaded.success(), "kind load docker-image failed");
    let crd = work.join("crd.yaml");
    std::fs::write(&crd, crate::render_crd()?)?;
    c.kubectl(&["apply", "-f", &crd.display().to_string()])?;
    crate::kube::wait_crd_established(&c.kubeconfig)?;
    let ctrl_kc = controller_kubeconfig(&c, &work)?;

    let ops = free_port()?;
    let config = work.join("controller.yaml");
    std::fs::write(
        &config,
        format!(
            "listen:\n  ops: 127.0.0.1:{ops}\n  api: 127.0.0.1:{}\n  edge: 127.0.0.1:{}\nedge:\n  endpoint: tcp://bk.example.com:8444\n  serverName: bk.example.com\nworkerNetwork:\n  extraDeniedCIDRs: [203.0.113.0/24]\n  dns:\n    namespace: rv-dns\n    podLabels: {{rv-dns: \"yes\"}}\n    cidrs: [198.51.100.53/32]\nshutdown:\n  delay: 0s\n  drainTimeout: 5s\n",
            free_port()?,
            free_port()?
        ),
    )?;
    let log = std::fs::File::create(work.join("controller.log"))?;
    c.controller = Some(
        Command::new(&bin)
            .arg("--config")
            .arg(&config)
            .env("KUBECONFIG", &ctrl_kc)
            .env("POD_NAME", "rv-0")
            .env("POD_NAMESPACE", NAMESPACE)
            .env("LOG_LEVEL", "info,buildkit_controller=debug")
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()
            .context("starting the controller")?,
    );
    std::thread::sleep(std::time::Duration::from_secs(5));
    let early = c
        .controller
        .as_mut()
        .map(Child::try_wait)
        .transpose()?
        .flatten();
    ensure!(early.is_none(), "controller exited at startup: {early:?}");
    let tests = Command::new("cargo")
        .current_dir(&root)
        .env("KUBECONFIG", &c.kubeconfig)
        .env("RV_OPS", format!("127.0.0.1:{ops}"))
        .env("RV_NODE", format!("{}-control-plane", CLUSTER.as_str()))
        .env("RV_NAMESPACE", NAMESPACE)
        .env("RV_CONFIG", &config)
        .env("RV_IMAGE", IMAGE)
        .env("RV_BIN", &bin)
        .env("RV_CTRL_KUBECONFIG", &ctrl_kc)
        .args([
            "test",
            "--locked",
            "-p",
            "buildkit-controller",
            "--test",
            "reconcile_verify",
            "--",
            "--ignored",
            "--nocapture",
        ])
        .status()?;
    ensure!(
        tests.success(),
        "reconcile_verify scenarios failed (controller log: {})",
        work.join("controller.log").display()
    );
    let exited = c
        .controller
        .as_mut()
        .map(Child::try_wait)
        .transpose()?
        .flatten();
    ensure!(
        exited.is_none(),
        "controller exited during the scenarios: {exited:?} (log: {})",
        work.join("controller.log").display()
    );
    println!("reconcile-verify: all scenarios passed");
    Ok(())
}
