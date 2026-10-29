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
    let cluster = Cluster::for_suite("bk-alloc-verify", &work)?;
    cluster.create(None, "120s")?;
    status(&mut crate::kube::kind_load(PAUSE, cluster.name()))?;
    status(&mut crate::kube::kind_load(
        crate::image::WORKER_IMAGE,
        cluster.name(),
    ))?;
    cluster.apply_crds()?;
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
