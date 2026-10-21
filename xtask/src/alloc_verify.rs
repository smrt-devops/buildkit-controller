//! `cargo xtask alloc-verify` runs the allocation service against a real API
//! server on a throwaway kind cluster. The scenarios live in the ignored test
//! `crates/server/tests/alloc_verify.rs`. This task provides the cluster.
//!
//! The cluster uses a private kubeconfig under `target/alloc-verify`. The task
//! never reads or writes the user's `~/.kube/config`, and it deletes the
//! cluster on every exit path.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, ensure};

static CLUSTER: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| super::kind_cluster("bk-alloc-verify"));
/// Pause image for the harness's warm pods. The task loads it into kind
/// because Docker Hub DNS inside kind is unreliable.
const PAUSE: &str = "registry.k8s.io/pause:3.10";

struct Cluster {
    kubeconfig: PathBuf,
}

impl Cluster {
    fn kind(&self, args: &[&str]) -> Result<std::process::Output> {
        crate::kube::kind(&self.kubeconfig)
            .args(args)
            .arg("--kubeconfig")
            .arg(&self.kubeconfig)
            .output()
            .context("running kind")
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        let _ = crate::kube::kind(&self.kubeconfig)
            .args([
                "delete",
                "cluster",
                "--name",
                CLUSTER.as_str(),
                "--kubeconfig",
            ])
            .arg(&self.kubeconfig)
            .status();
    }
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let st = cmd.status().with_context(|| what.to_owned())?;
    ensure!(st.success(), "{what} failed");
    Ok(())
}

pub fn alloc_verify() -> Result<()> {
    let root = crate::root();
    let work = root.join("target/alloc-verify");
    std::fs::create_dir_all(&work)?;
    let kubeconfig = work.join("kubeconfig");

    let have = Command::new("docker")
        .args(["image", "inspect", PAUSE])
        .output()
        .context("docker")?;
    if !have.status.success() {
        run(Command::new("docker").args(["pull", PAUSE]), "docker pull")?;
    }
    let cluster = Cluster { kubeconfig };
    let _ = cluster.kind(&["delete", "cluster", "--name", CLUSTER.as_str()]);
    let created = cluster.kind(&[
        "create",
        "cluster",
        "--name",
        CLUSTER.as_str(),
        "--wait",
        "120s",
    ])?;
    ensure!(
        created.status.success(),
        "kind create: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    run(
        &mut crate::kube::kind_load(PAUSE, CLUSTER.as_str()),
        "kind load docker-image",
    )?;
    let crd = work.join("crd.yaml");
    std::fs::write(&crd, crate::render_crd()?)?;
    run(
        crate::kube::kubectl(&cluster.kubeconfig)
            .args(["apply", "--wait", "-f"])
            .arg(&crd),
        "kubectl apply crd",
    )?;
    run(
        crate::kube::kubectl(&cluster.kubeconfig).args([
            "wait",
            "--for=condition=Established",
            "crd/buildkitpools.buildkit.smrt-devops.net",
            "--timeout=60s",
        ]),
        "kubectl wait crd",
    )?;
    run(
        Command::new("cargo")
            .current_dir(&root)
            .env("KUBECONFIG", &cluster.kubeconfig)
            .env("ALLOC_VERIFY_PAUSE_IMAGE", PAUSE)
            .env("ALLOC_VERIFY_WORK", &work)
            .args([
                "test",
                "--locked",
                "-p",
                "buildkit-controller",
                "--test",
                "alloc_verify",
                "--",
                "--ignored",
                "--nocapture",
            ]),
        "alloc_verify scenarios",
    )?;
    println!("alloc-verify: all scenarios passed");
    Ok(())
}
