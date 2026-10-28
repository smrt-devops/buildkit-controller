//! `cargo xtask`: repository automation.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

mod alloc_verify;
mod api_verify;
mod claim_verify;
mod docs;
mod drain_verify;
mod e2e;
mod e2e_issuer;
mod edge_verify;
mod gateway;
mod helm_test;
mod helm_verify;
mod image;
mod kube;
mod perf;
mod proxy_verify;
mod reconcile_verify;
mod render_verify;
mod runtime_verify;
mod soak;
use alloc_verify::alloc_verify;
use api_verify::api_verify;
use claim_verify::claim_verify;
use docs::docs;
use drain_verify::drain_verify;
use e2e::{e2e, e2e_up};
use edge_verify::edge_verify;
use helm_test::helm_test;
use helm_verify::helm_verify;
use image::{image, prune};
use perf::{perf, perf_loopback};
use proxy_verify::proxy_verify;
use reconcile_verify::reconcile_verify;
use render_verify::render_verify;
use runtime_verify::runtime_verify;
use soak::soak;

/// Repository-relative path of the generated CRD.
const CRD_PATH: &str = "helm/buildkit-controller/crds/buildkitpools.yaml";
/// The claim CRD is a chart template so `claims.enabled` can gate it (Helm never templates `crds/`).
const CLAIM_CRD_PATH: &str = "helm/buildkit-controller/templates/crd-buildkitclaims.yaml";

/// Repository automation tasks.
#[derive(Debug, Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Task,
}

/// Available tasks.
#[derive(Debug, Subcommand)]
enum Task {
    /// Print the workspace root directory.
    Root,
    /// Write the `BuildKitPool` CRD YAML into the Helm chart.
    Crdgen {
        /// Do not write. Exit non-zero if the file is missing or stale.
        #[arg(long)]
        check: bool,
    },
    /// Apply the CRD to a throwaway kind cluster and dry-run the fixtures under crates/api/tests/fixtures.
    CrdVerify,
    /// Write the render fixtures under crates/server/tests/fixtures/render and server-side dry-run them on a throwaway kind cluster.
    RenderVerify,
    /// Run two controller replicas on a throwaway kind cluster. Checks that one replica leads, leadership fails over, readiness is correct and the lease is released cleanly.
    RuntimeVerify,
    /// Run the controller on a throwaway kind cluster and test the mTLS edge end to end with buildctl.
    EdgeVerify,
    /// Build a Linux container image of the controller for the host architecture, without network access.
    Image {
        /// Image tag.
        #[arg(long, default_value = "buildkit-controller:dev")]
        tag: String,
        /// Also `kind load` the image into this cluster.
        #[arg(long)]
        load_kind: Option<String>,
        /// Remove the target volumes of checkouts that no longer exist, and the old shared volume, instead of building.
        #[arg(long)]
        prune: bool,
    },
    /// Run the allocation service against a throwaway kind cluster. Covers exclusive claims, the queue, cancellation, owner-only release and the client certificate.
    AllocVerify,
    /// Serve the public HTTPS API against a throwaway kind cluster. Covers OIDC and `ServiceAccount` auth, pools, release, the queue and metrics.
    ApiVerify,
    /// Run the reconciler against a throwaway kind cluster. Covers warm pods, the pool maximum, idle and expiry handling, CA repair and replacement, the sandbox, slot holds and the absence of idle polling.
    ReconcileVerify,
    /// Run `helm lint` and golden `helm template` tests of the chart. Needs no cluster.
    HelmTest {
        /// Rewrite the golden files.
        #[arg(long)]
        update: bool,
    },
    /// Install the chart on a throwaway kind cluster with one pool and check the replicas, the leader, the pool and readiness.
    HelmVerify,
    /// Install the chart with two replicas on a throwaway kind cluster and check that stopping a replica never cuts a live build: a long build survives a rollout, the Lease moves within seconds, and a queued request gets 503 `shutting_down`.
    DrainVerify,
    /// Install the chart with claims on a throwaway kind cluster and check the claim lifecycle, a real build through a claim Secret, refusal, expiry and a lost worker.
    ClaimVerify,
    /// Soak the certificate lifecycle on a throwaway kind cluster. Runs continuous builds through leaf renewals, a serving certificate swap, a leader failover, and deletion and corruption of the CA Secret.
    Soak,
    /// Build the documentation site with mdBook into target/book/html. Fails on a broken repository link.
    Docs,
    /// Install the chart behind Envoy Gateway with PROXY protocol on, on a throwaway kind cluster, and check that the logs and the per-address caps see client addresses, that a build through the edge works, and that an untrusted pod cannot forge its address.
    ProxyVerify,
    /// Run the end-to-end suite on a kind cluster, exposed through Envoy Gateway. Covers chart install, the Gateway API routes, OIDC and `TokenReview`, builds, the registry cache, queueing, a long poll, multi-platform builds and the Envoy extras.
    E2e,
    /// Install the chart on kind behind Envoy Gateway, trust GitHub OIDC for REPO (owner/name) and leave the cluster running. Used by the CI Action job.
    E2eUp {
        /// Repository whose workflow tokens may allocate.
        repo: String,
    },
    /// Measure the plan's efficiency targets on a throwaway kind cluster: allocation latency, edge connect overhead, edge throughput, idle RSS and idle API requests. Method in docs/performance.md.
    Perf {
        /// Run the in-process loopback benchmark (no kind, no Docker) instead of the kind rig.
        #[arg(long)]
        loopback: bool,
        /// Allocation batches. Each batch makes `perf::BATCH` allocations against warm pods.
        #[arg(long, default_value_t = 25)]
        batches: usize,
        /// Length of the idle window in seconds.
        #[arg(long, default_value_t = 300)]
        idle_secs: u64,
    },
}

pub(crate) fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(PathBuf::new, Path::to_path_buf)
}

/// Render the CRD exactly as it is committed.
fn render_crd() -> Result<String> {
    let yaml = serde_saphyr::to_string(&buildkit_controller_api::v1alpha1::crd())
        .context("serialising CRD")?;
    Ok(format!(
        "# Generated by `cargo xtask crdgen`; do not edit.\n{yaml}"
    ))
}

/// Render the claim CRD template, gated on `claims.enabled`.
fn render_claim_crd() -> Result<String> {
    let mut crd = buildkit_controller_api::claim::crd();
    // Keep the CRD, and so every claim, when the feature is turned off or the
    // release is uninstalled.
    crd.metadata.annotations = Some(std::collections::BTreeMap::from([(
        "helm.sh/resource-policy".to_owned(),
        "keep".to_owned(),
    )]));
    let yaml = serde_saphyr::to_string(&crd).context("serialising claim CRD")?;
    Ok(format!(
        "{{{{- if .Values.claims.enabled }}}}\n# Generated by `cargo xtask crdgen`; do not edit.\n{yaml}{{{{- end }}}}\n"
    ))
}

fn crdgen(check: bool) -> Result<()> {
    write_or_check(CRD_PATH, &render_crd()?, check)?;
    write_or_check(CLAIM_CRD_PATH, &render_claim_crd()?, check)
}

fn write_or_check(rel: &str, want: &str, check: bool) -> Result<()> {
    let path = root().join(rel);
    if check {
        let have = std::fs::read_to_string(&path).unwrap_or_default();
        if have != want {
            bail!("{rel} is stale; run `cargo xtask crdgen`");
        }
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, want).with_context(|| format!("writing {}", path.display()))
}

/// Name of the throwaway kind cluster used by `crd-verify`.
fn verify_cluster() -> String {
    kind_cluster("bk-crd-verify")
}
/// Repository-relative directory of the `crd-verify` fixtures.
const FIXTURES: &str = "crates/api/tests/fixtures";

/// Deletes the verification cluster when dropped, including on failure.
struct ClusterGuard(PathBuf);

impl Drop for ClusterGuard {
    fn drop(&mut self) {
        let _ = crate::kube::kind(&self.0)
            .args(["delete", "cluster", "--name", verify_cluster().as_str()])
            .arg("--kubeconfig")
            .arg(&self.0)
            .status();
    }
}

fn run(cmd: &mut Command) -> Result<std::process::Output> {
    cmd.output()
        .with_context(|| format!("running {}", cmd.get_program().display()))
}

pub(crate) fn yaml_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "yaml"))
        .collect();
    v.sort();
    Ok(v)
}

/// Apply the generated CRD to a throwaway kind cluster and server-side dry-run
/// every fixture. The API server must admit `accept/*.yaml` and refuse `reject/*.yaml`.
fn crd_verify() -> Result<()> {
    let root = root();
    let work = root.join("target/crd-verify");
    std::fs::create_dir_all(&work)?;
    let kubeconfig = work.join("kubeconfig");
    // Delete any leftover cluster first. The guard removes this run's cluster afterwards.
    let _ = crate::kube::kind(&kubeconfig)
        .args(["delete", "cluster", "--name", verify_cluster().as_str()])
        .arg("--kubeconfig")
        .arg(&kubeconfig)
        .status();
    let _guard = ClusterGuard(kubeconfig.clone());
    let created = run(crate::kube::kind(&kubeconfig)
        .args([
            "create",
            "cluster",
            "--name",
            verify_cluster().as_str(),
            "--wait",
            "120s",
        ])
        .arg("--kubeconfig")
        .arg(&kubeconfig))?;
    if !created.status.success() {
        bail!(
            "kind create failed: {}",
            String::from_utf8_lossy(&created.stderr)
        );
    }
    let kubectl = |args: &[&str]| -> Result<std::process::Output> {
        run(crate::kube::kubectl(&kubeconfig).args(args))
    };
    let version = kubectl(&["version", "-o", "json"])?;
    println!(
        "{}",
        String::from_utf8_lossy(&version.stdout)
            .lines()
            .filter(|l| l.contains("gitVersion"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let crd_file = root.join(CRD_PATH);
    let crd_arg = crd_file.to_string_lossy().into_owned();
    let applied = kubectl(&["apply", "--server-side", "-f", &crd_arg])?;
    if !applied.status.success() {
        bail!("CRD rejected: {}", String::from_utf8_lossy(&applied.stderr));
    }
    crate::kube::wait_crd_established(&kubeconfig).context("CRD not established")?;
    let (mut passed, mut failed) = (0usize, Vec::new());
    for (dir, want_ok) in [("accept", true), ("reject", false)] {
        for file in yaml_files(&root.join(FIXTURES).join(dir))? {
            let arg = file.to_string_lossy().into_owned();
            let out = kubectl(&["apply", "--dry-run=server", "--validate=strict", "-f", &arg])?;
            let ok = out.status.success();
            let verdict = if ok == want_ok { "PASS" } else { "FAIL" };
            let name = file
                .file_stem()
                .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
            let why = String::from_utf8_lossy(&out.stderr);
            let why = why.lines().map(str::trim).collect::<Vec<_>>().join(" | ");
            println!(
                "{verdict} {dir}/{name} -> {}{}",
                if ok { "admitted" } else { "rejected" },
                if ok || verdict == "FAIL" {
                    String::new()
                } else {
                    format!(": {why}")
                }
            );
            if ok == want_ok {
                passed += 1;
            } else {
                failed.push(format!("{dir}/{name}: {why}"));
            }
        }
    }
    println!("crd-verify: {passed} passed, {} failed", failed.len());
    if !failed.is_empty() {
        bail!(
            "{} fixtures misbehaved:\n{}",
            failed.len(),
            failed.join("\n")
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Task::Root => {
            println!("{}", root().display());
            Ok(())
        }
        Task::Crdgen { check } => crdgen(check),
        Task::CrdVerify => crd_verify(),
        Task::RenderVerify => render_verify(),
        Task::RuntimeVerify => runtime_verify(),
        Task::EdgeVerify => edge_verify(),
        Task::Image { prune: true, .. } => prune(),
        Task::Image { tag, load_kind, .. } => image(&tag, load_kind.as_deref()),
        Task::AllocVerify => alloc_verify(),
        Task::ApiVerify => api_verify(),
        Task::ReconcileVerify => reconcile_verify(),
        Task::HelmTest { update } => helm_test(update),
        Task::HelmVerify => helm_verify(),
        Task::DrainVerify => drain_verify(),
        Task::ClaimVerify => claim_verify(),
        Task::Soak => soak(),
        Task::Docs => docs(),
        Task::ProxyVerify => proxy_verify(),
        Task::E2e => e2e(),
        Task::E2eUp { repo } => e2e_up(&repo),
        Task::Perf { loopback: true, .. } => perf_loopback(),
        Task::Perf {
            batches, idle_secs, ..
        } => perf(batches, idle_secs),
    }
}

/// A kind cluster name: `base`, plus `-$BK_KIND_SUFFIX` when that is set, so
/// parallel checkouts can run the same task without sharing a cluster.
fn kind_cluster(base: &str) -> String {
    match std::env::var("BK_KIND_SUFFIX") {
        Ok(s) if !s.is_empty() => format!("{base}-{s}"),
        _ => base.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tasks() {
        assert!(Cli::try_parse_from(["xtask", "root"]).is_ok());
        assert!(Cli::try_parse_from(["xtask", "crdgen", "--check"]).is_ok());
        assert!(Cli::try_parse_from(["xtask", "crd-verify"]).is_ok());
        assert!(Cli::try_parse_from(["xtask", "render-verify"]).is_ok());
        assert!(Cli::try_parse_from(["xtask", "runtime-verify"]).is_ok());
        assert!(Cli::try_parse_from(["xtask", "reconcile-verify"]).is_ok());
        assert!(Cli::try_parse_from(["xtask", "soak"]).is_ok());
        assert!(Cli::try_parse_from(["xtask", "drain-verify"]).is_ok());
        assert!(Cli::try_parse_from(["xtask", "proxy-verify"]).is_ok());
        assert!(Cli::try_parse_from(["xtask", "perf", "--idle-secs", "60"]).is_ok());
    }
}
