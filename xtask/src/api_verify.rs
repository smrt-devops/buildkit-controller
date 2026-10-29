//! `cargo xtask api-verify` runs the public HTTPS API against a real API server
//! on a throwaway kind cluster. The scenarios live in the ignored test
//! `crates/server/tests/api_verify.rs`, which runs the controller components on
//! the host with a mock OIDC issuer and real `ServiceAccount` tokens. This task
//! provides the cluster.
//!
//! The cluster uses a private kubeconfig under `target/api-verify`, which the
//! test reads from `BK_TEST_KUBECONFIG`. The task never reads or writes the
//! user's `~/.kube/config`, and it deletes the cluster when it exits.

use anyhow::Result;

use crate::alloc_verify::PAUSE;
use crate::kube::Cluster;
use crate::proc::status;

pub fn api_verify() -> Result<()> {
    let work = crate::root().join("target/api-verify");
    std::fs::create_dir_all(&work)?;
    crate::image::ensure(PAUSE)?;
    let cluster = Cluster::for_suite("bk-api-verify", &work)?;
    cluster.create(None, "120s")?;
    status(&mut crate::kube::kind_load(PAUSE, cluster.name()))?;
    cluster.apply_crds()?;
    status(
        cluster
            .server_test("api_verify")
            .env("API_VERIFY_PAUSE_IMAGE", PAUSE)
            .env("API_VERIFY_WORK", &work),
    )?;
    println!("api-verify: all scenarios passed");
    Ok(())
}
