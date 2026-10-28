//! `cargo xtask api-verify` runs the public HTTPS API against a real API server
//! on a throwaway kind cluster. The scenarios live in the ignored test
//! `crates/server/tests/api_verify.rs`, which runs the controller components on
//! the host with a mock OIDC issuer and real `ServiceAccount` tokens. This task
//! provides the cluster.
//!
//! Every kind and kubectl call gets `--kubeconfig` with a private file under
//! `target/api-verify`. Only `kind load` takes none, because it talks to
//! Docker. The task never reads or writes the user's `~/.kube/config`, and it
//! deletes the cluster on every exit path.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

static CLUSTER: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| super::kind_cluster("bk-api-verify"));
/// Image for the harness's warm pods. The task loads it into kind because
/// Docker Hub DNS inside kind is unreliable.
const PAUSE: &str = "registry.k8s.io/pause:3.10";

struct Cluster {
    kubeconfig: PathBuf,
}

impl Cluster {
    fn kind(&self, args: &[&str]) -> Command {
        let mut c = crate::kube::kind(&self.kubeconfig);
        c.args(args).arg("--kubeconfig").arg(&self.kubeconfig);
        c
    }

    fn kubectl(&self, args: &[&str]) -> Command {
        let mut c = crate::kube::kubectl(&self.kubeconfig);
        c.arg("--kubeconfig").arg(&self.kubeconfig).args(args);
        c
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        let _ = self
            .kind(&["delete", "cluster", "--name", CLUSTER.as_str()])
            .status();
    }
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let st = cmd.status().with_context(|| what.to_owned())?;
    ensure!(st.success(), "{what} failed");
    Ok(())
}

fn ensure_image(image: &str) -> Result<()> {
    let have = Command::new("docker")
        .args(["image", "inspect", image])
        .output()
        .context("docker")?;
    if !have.status.success() {
        run(Command::new("docker").args(["pull", image]), "docker pull")?;
    }
    Ok(())
}

pub fn api_verify() -> Result<()> {
    let root = crate::root();
    let work = root.join("target/api-verify");
    std::fs::create_dir_all(&work)?;
    ensure_image(PAUSE)?;
    let cluster = Cluster {
        kubeconfig: work.join("kubeconfig"),
    };
    let _ = cluster
        .kind(&["delete", "cluster", "--name", CLUSTER.as_str()])
        .output();
    let created = cluster
        .kind(&[
            "create",
            "cluster",
            "--name",
            CLUSTER.as_str(),
            "--wait",
            "120s",
        ])
        .output()
        .context("kind create")?;
    ensure!(
        created.status.success(),
        "kind create: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    run(
        &mut crate::kube::kind_load(PAUSE, CLUSTER.as_str()),
        "kind load docker-image",
    )?;
    apply_crd(&cluster, &work)?;
    run(
        Command::new("cargo")
            .current_dir(&root)
            .env("KUBECONFIG", &cluster.kubeconfig)
            .env("API_VERIFY_KUBECONFIG", &cluster.kubeconfig)
            .env("API_VERIFY_PAUSE_IMAGE", PAUSE)
            .env("API_VERIFY_WORK", &work)
            .args([
                "test",
                "--locked",
                "-p",
                "buildkit-controller",
                "--test",
                "api_verify",
                "--",
                "--ignored",
                "--nocapture",
            ]),
        "api_verify scenarios",
    )?;
    println!("api-verify: all scenarios passed");
    Ok(())
}

fn apply_crd(cluster: &Cluster, work: &Path) -> Result<()> {
    let crd = work.join("crd.yaml");
    std::fs::write(&crd, crate::render_crd()?)?;
    let crd = crd.to_str().context("non-UTF-8 work path")?;
    run(
        &mut cluster.kubectl(&["apply", "--wait", "-f", crd]),
        "kubectl apply crd",
    )?;
    crate::kube::wait_crd_established(&cluster.kubeconfig)
}
