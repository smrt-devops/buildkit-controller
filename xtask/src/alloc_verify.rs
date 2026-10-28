//! `cargo xtask alloc-verify` runs the allocation service against a real API
//! server on a throwaway kind cluster. The scenarios live in the ignored test
//! `crates/server/tests/alloc_verify.rs`. This task provides the cluster.
//!
//! The cluster uses a private kubeconfig under `target/alloc-verify`, which
//! the test reads from `BK_TEST_KUBECONFIG`. The task never reads or writes
//! the user's `~/.kube/config`, and it deletes the cluster when it exits.

use anyhow::Result;

use crate::kube::Cluster;
use crate::proc::status;

/// Pause image for the harness's warm pods. The task loads it into kind
/// because Docker Hub DNS inside kind is unreliable.
pub(crate) const PAUSE: &str = "registry.k8s.io/pause:3.10";

pub fn alloc_verify() -> Result<()> {
    let work = crate::root().join("target/alloc-verify");
    std::fs::create_dir_all(&work)?;
    crate::image::ensure(PAUSE)?;
    let cluster = Cluster::new(
        &super::kind_cluster("bk-alloc-verify"),
        work.join("kubeconfig"),
    );
    cluster.create(None, "120s")?;
    status(&mut crate::kube::kind_load(PAUSE, cluster.name()))?;
    apply_crd(&cluster, &work)?;
    status(
        cluster
            .server_test("alloc_verify")
            .env("ALLOC_VERIFY_PAUSE_IMAGE", PAUSE)
            .env("ALLOC_VERIFY_WORK", &work),
    )?;
    println!("alloc-verify: all scenarios passed");
    Ok(())
}

/// Applies the generated CRD and waits until it is Established.
pub(crate) fn apply_crd(cluster: &Cluster, work: &std::path::Path) -> Result<()> {
    let crd = work.join("crd.yaml");
    std::fs::write(&crd, crate::render_crd()?)?;
    cluster.kubectl(&["apply", "--wait", "-f", &crd.to_string_lossy()])?;
    cluster.wait_crd_established()
}
