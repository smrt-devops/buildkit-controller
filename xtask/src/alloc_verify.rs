//! `cargo xtask alloc-verify` runs allocation (front door, allocator and, for
//! the cold path, the reconciler) against a real API server on a throwaway
//! kind cluster. The scenarios live in the ignored test
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
    crate::image::ensure_worker()?;
    let cluster = Cluster::new(
        &super::kind_cluster("bk-alloc-verify"),
        work.join("kubeconfig"),
    );
    cluster.create(None, "120s")?;
    status(&mut crate::kube::kind_load(PAUSE, cluster.name()))?;
    status(&mut crate::kube::kind_load(
        crate::image::WORKER_IMAGE,
        cluster.name(),
    ))?;
    apply_crd(&cluster, &work)?;
    status(
        cluster
            .server_test("alloc_verify")
            .env("ALLOC_VERIFY_PAUSE_IMAGE", PAUSE)
            .env("ALLOC_VERIFY_WORKER_IMAGE", crate::image::WORKER_IMAGE)
            .env("ALLOC_VERIFY_WORK", &work),
    )?;
    println!("alloc-verify: all scenarios passed");
    Ok(())
}

/// Applies the generated CRDs (pool and claim) and waits until they are Established.
pub(crate) fn apply_crd(cluster: &Cluster, work: &std::path::Path) -> Result<()> {
    let crd = work.join("crd.yaml");
    let claim = crate::claim_crd_yaml(false)?;
    std::fs::write(&crd, format!("{}---\n{claim}", crate::render_crd()?))?;
    cluster.kubectl(&["apply", "--wait", "-f", &crd.to_string_lossy()])?;
    cluster.wait_crd_established()?;
    cluster.wait_condition(
        "crd/buildkitclaims.buildkit.smrt-devops.net",
        None,
        "Established",
        std::time::Duration::from_mins(1),
    )
}
