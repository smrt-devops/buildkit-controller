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
mod kind_all;
mod kube;
mod perf;
mod proc;
mod proxy_verify;
mod reconcile_verify;
mod registry_verify;
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
use registry_verify::registry_verify;
use render_verify::render_verify;
use runtime_verify::runtime_verify;
use soak::soak;

/// Repository-relative path of the generated pool CRD.
const CRD_PATH: &str = "helm/buildkit-controller/crds/buildkitpools.yaml";
/// Repository-relative path of the generated claim CRD. Both CRDs sit in the
/// chart's `crds/`: Helm installs them on first install, and an upgrade
/// applies them by hand first.
const CLAIM_CRD_PATH: &str = "helm/buildkit-controller/crds/buildkitclaims.yaml";

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
    /// Run the cache registry role on a throwaway kind cluster with two real buildkitd pods. Checks a `mode=max` registry cache export, a cache hit from a fresh daemon over mTLS, and that a read-only grant cannot push.
    RegistryVerify,
    /// Run `helm lint` and golden `helm template` tests of the chart. Needs no cluster.
    HelmTest {
        /// Rewrite the golden files.
        #[arg(long)]
        update: bool,
    },
    /// Install the chart on a throwaway kind cluster with one pool and check the replicas, the leader, the pool and readiness.
    HelmVerify,
    /// Install the chart with two replicas on a throwaway kind cluster and check that stopping a replica never cuts a live build: a long build survives a rollout, the Lease moves within seconds after the allocator has stopped, and a pending (202) claim survives a rollout of both replicas and binds once the worker is free.
    DrainVerify {
        /// Run the build past the old 300 s drain budget (390 s), as the nightly job does.
        #[arg(long)]
        long: bool,
    },
    /// Install the chart with claims on a throwaway kind cluster and check the claim lifecycle, a real build through a claim Secret, refusal, expiry and a lost worker.
    ClaimVerify,
    /// Soak the certificate lifecycle on a throwaway kind cluster. Runs continuous builds through leaf renewals, a serving certificate swap, a leader failover, and deletion and corruption of the CA Secret.
    Soak,
    /// Build the documentation site with mdBook into target/book/html. Fails on a broken repository link.
    Docs,
    /// Install the chart behind Envoy Gateway with PROXY protocol on, on a throwaway kind cluster, and check that the logs and the per-address caps see client addresses, that a build through the edge works, and that an untrusted pod cannot forge its address.
    ProxyVerify,
    /// Run the end-to-end suite on a kind cluster, exposed through Envoy Gateway. Covers chart install, the Gateway API routes, OIDC and `TokenReview`, builds, an external registry cache, the built-in cache registry for trusted and untrusted callers, the v0.1.0 volume sweep, queueing, a long poll, multi-platform builds and the Envoy extras.
    E2e,
    /// Run every kind suite but soak and perf in one go: build once, two shared clusters, suites side by side. Logs go to target/kind-all/<suite>.log.
    KindAll {
        /// Run only these suites (comma-separated).
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        /// Leave the clusters running; run a suite on one with `BK_KIND_CLUSTER=<name> cargo xtask <suite>`.
        #[arg(long)]
        keep: bool,
        /// Delete the clusters a `--keep` run left, and run nothing.
        #[arg(long)]
        down: bool,
        /// How many suites run at once on the shared cluster; 0 runs all of them.
        #[arg(long, default_value_t = 0)]
        jobs: usize,
        /// Also run soak and perf, each on its own cluster, once the others are done.
        #[arg(long)]
        nightly: bool,
    },
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

/// Placeholders every fixture under `xtask/fixtures` may use, so an image bump
/// in code reaches the manifests too.
fn image_placeholders() -> [(&'static str, &'static str); 2] {
    [
        ("WORKER_IMAGE", image::WORKER_IMAGE),
        ("SOCAT_IMAGE", gateway::SOCAT.0),
    ]
}

/// Reads `xtask/fixtures/<rel>`, fills in the image placeholders and `subs`,
/// and writes the result under `work`. Returns the written path.
pub(crate) fn fixture(rel: &str, work: &Path, subs: &[(&str, String)]) -> Result<PathBuf> {
    let src = root().join("xtask/fixtures").join(rel);
    let mut text =
        std::fs::read_to_string(&src).with_context(|| format!("reading {}", src.display()))?;
    for (from, to) in image_placeholders() {
        text = text.replace(from, to);
    }
    for (from, to) in subs {
        text = text.replace(from, to);
    }
    let name = Path::new(rel).file_name().context("fixture file name")?;
    let path = work.join(name);
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Builds the controller binary for the host and returns its path, wherever
/// `CARGO_TARGET_DIR` or `build.target-dir` puts it.
pub(crate) fn build_controller() -> Result<PathBuf> {
    let out = Command::new("cargo")
        .current_dir(root())
        .args(["build", "--locked", "-p", "buildkit-controller"])
        .args(["--bin", "buildkit-controller"])
        .arg("--message-format=json-render-diagnostics")
        .stderr(std::process::Stdio::inherit())
        .output()
        .context("running cargo build")?;
    if !out.status.success() {
        bail!("cargo build failed");
    }
    executable(&String::from_utf8_lossy(&out.stdout), "buildkit-controller")
}

/// The `executable` of the `compiler-artifact` message for binary `name` in
/// `cargo build --message-format=json` output.
fn executable(messages: &str, name: &str) -> Result<PathBuf> {
    messages
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == name)
        .find_map(|m| m["executable"].as_str().map(PathBuf::from))
        .with_context(|| format!("cargo build reported no executable for {name}"))
}

/// Render a CRD exactly as it is committed.
fn render_crd(crd: &impl serde::Serialize) -> Result<String> {
    let yaml = serde_saphyr::to_string(crd).context("serialising CRD")?;
    Ok(format!(
        "# Generated by `cargo xtask crdgen`; do not edit.\n{yaml}"
    ))
}

fn crdgen(check: bool) -> Result<()> {
    let pool = render_crd(&buildkit_controller_api::v1alpha1::crd())?;
    write_or_check(CRD_PATH, &pool, check)?;
    let claim = render_crd(&buildkit_controller_api::claim::crd())?;
    write_or_check(CLAIM_CRD_PATH, &claim, check)
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

/// Repository-relative directory of the `crd-verify` fixtures.
const FIXTURES: &str = "crates/api/tests/fixtures";

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
    let cluster = kube::Cluster::for_suite("bk-crd-verify", &work)?;
    cluster.create(None, "120s")?;
    let version = cluster.kubectl(&["version", "-o", "json"])?;
    println!(
        "{}",
        version
            .lines()
            .filter(|l| l.contains("gitVersion"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    cluster
        .apply_crds()
        .context("CRDs rejected or not established")?;
    let (mut passed, mut failed) = (0usize, Vec::new());
    for (dir, want_ok) in [("accept", true), ("reject", false)] {
        for file in yaml_files(&root.join(FIXTURES).join(dir))? {
            let arg = file.to_string_lossy().into_owned();
            let out = cluster.kubectl_raw(
                &["apply", "--dry-run=server", "--validate=strict", "-f", &arg],
                None,
            )?;
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
        Task::RegistryVerify => registry_verify(),
        Task::HelmTest { update } => helm_test(update),
        Task::HelmVerify => helm_verify(),
        Task::DrainVerify { long } => drain_verify(long),
        Task::ClaimVerify => claim_verify(),
        Task::Soak => soak(),
        Task::Docs => docs(),
        Task::ProxyVerify => proxy_verify(),
        Task::E2e => e2e(),
        Task::E2eUp { repo } => e2e_up(&repo),
        Task::KindAll {
            only,
            keep,
            down,
            jobs,
            nightly,
        } => kind_all::kind_all(&kind_all::Opts {
            only,
            keep,
            down,
            jobs,
            nightly,
        }),
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

    /// Image references live in code; fixtures name them by placeholder, so a
    /// bump cannot leave a fixture on the old tag (pods then sit in
    /// `ErrImageNeverPull` until the suite times out).
    #[test]
    fn fixtures_name_images_by_placeholder() {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        walk(&root().join("xtask/fixtures"), &mut files);
        for p in files {
            // helm-test fixtures are rendered, never installed.
            if p.components().any(|c| c.as_os_str() == "helm-test") {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            for (_, image) in image_placeholders() {
                assert!(!text.contains(image), "{} names {image}", p.display());
            }
        }
    }

    #[test]
    fn fixture_fills_placeholders() {
        let work = std::env::temp_dir().join(format!("xtask-fixture-{}", std::process::id()));
        std::fs::create_dir_all(&work).unwrap();
        let p = fixture(
            "drain-verify/values.yaml",
            &work,
            &[("EDGE_PORT", "20001".into())],
        )
        .unwrap();
        let text = std::fs::read_to_string(p).unwrap();
        assert!(text.contains(image::WORKER_IMAGE), "{text}");
        assert!(text.contains("tcp://localhost:20001"), "{text}");
        assert!(!text.contains("WORKER_IMAGE") && !text.contains("EDGE_PORT"));
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn executable_comes_from_the_build_messages() {
        let msgs = r#"{"reason":"compiler-artifact","target":{"name":"buildkit_controller"},"executable":null}
not json
{"reason":"compiler-artifact","target":{"name":"buildkit-controller"},"executable":"/shared/target/debug/buildkit-controller"}
{"reason":"build-finished","success":true}"#;
        assert_eq!(
            executable(msgs, "buildkit-controller").unwrap(),
            Path::new("/shared/target/debug/buildkit-controller")
        );
        assert!(executable(r#"{"reason":"build-finished"}"#, "buildkit-controller").is_err());
    }

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
