//! `cargo xtask render-verify`: write the render fixtures and dry-run every
//! `cargo xtask render-verify` writes the render fixtures and dry-runs every
//! object against a real API server, because unit tests cannot catch
//! API-server rejections.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use buildkit_controller::render::fixtures::{CONTROLLER_NAMESPACE, POOL_NAMESPACE, render_all};

use super::{CRD_PATH, root};

/// Name of the throwaway kind cluster.
static CLUSTER: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| super::kind_cluster("bk-render-verify"));
/// Repository-relative fixture directory.
const FIXTURES: &str = "crates/server/tests/fixtures/render";

/// Cluster-scoped objects the pod fixtures reference. The API server checks
/// them at admission, even on a dry run.
const PREREQS: &str = "\
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata: {name: gvisor}
handler: runc
---
apiVersion: scheduling.k8s.io/v1
kind: PriorityClass
metadata: {name: build-low}
value: 10
";

/// Deletes the cluster when dropped, including on failure.
struct ClusterGuard(PathBuf);

impl Drop for ClusterGuard {
    fn drop(&mut self) {
        let _ = crate::kube::kind(&self.0)
            .args(["delete", "cluster", "--name", CLUSTER.as_str()])
            .arg("--kubeconfig")
            .arg(&self.0)
            .status();
    }
}

fn run(cmd: &mut Command) -> Result<Output> {
    cmd.output()
        .with_context(|| format!("running {}", cmd.get_program().display()))
}

fn write_fixtures(dir: &Path) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let fixtures = render_all().context("rendering fixtures")?;
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        if p.extension().is_some_and(|x| x == "yaml") {
            std::fs::remove_file(p)?;
        }
    }
    let mut paths = Vec::new();
    for (name, yaml) in fixtures {
        let path = dir.join(name);
        std::fs::write(&path, yaml).with_context(|| format!("writing {}", path.display()))?;
        paths.push(path);
    }
    Ok(paths)
}

/// `kubectl` bound to the private kubeconfig, never `~/.kube/config`.
struct Kube {
    kubeconfig: PathBuf,
}

impl Kube {
    fn run(&self, args: &[&str], stdin: Option<&str>) -> Result<Output> {
        use std::io::Write;
        let mut cmd = crate::kube::kubectl(&self.kubeconfig);
        cmd.args(args);
        let Some(input) = stdin else {
            return run(&mut cmd);
        };
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("spawning kubectl")?;
        child
            .stdin
            .take()
            .context("kubectl stdin")?
            .write_all(input.as_bytes())?;
        child.wait_with_output().context("waiting for kubectl")
    }

    fn ok(&self, what: &str, args: &[&str], stdin: Option<&str>) -> Result<()> {
        let out = self.run(args, stdin)?;
        if out.status.success() {
            Ok(())
        } else {
            bail!("{what}: {}", String::from_utf8_lossy(&out.stderr))
        }
    }

    /// Install the CRD, namespaces and prerequisites.
    fn prepare(&self, root: &Path) -> Result<()> {
        let version = self.run(&["version", "-o", "json"], None)?;
        for l in String::from_utf8_lossy(&version.stdout)
            .lines()
            .filter(|l| l.contains("gitVersion"))
        {
            println!("{}", l.trim());
        }
        let crd = root.join(CRD_PATH).to_string_lossy().into_owned();
        self.ok("CRD apply", &["apply", "--server-side", "-f", &crd], None)?;
        self.ok(
            "CRD not established",
            &[
                "wait",
                "--for=condition=established",
                "--timeout=60s",
                "crd/buildkitpools.buildkit.smrt-devops.net",
            ],
            None,
        )?;
        for ns in [POOL_NAMESPACE, CONTROLLER_NAMESPACE] {
            self.ok("create namespace", &["create", "namespace", ns], None)?;
        }
        self.ok("prereqs", &["apply", "-f", "-"], Some(PREREQS))
    }

    /// Server-side dry run of one fixture. A `ServiceAccount` is also applied
    /// for real afterwards: pod admission looks it up even on a dry run.
    fn check(&self, file: &Path) -> Result<(bool, String)> {
        let arg = file.to_string_lossy().into_owned();
        let out = self.run(
            &["apply", "--dry-run=server", "--validate=strict", "-f", &arg],
            None,
        )?;
        let stderr = |o: &Output| {
            String::from_utf8_lossy(&o.stderr)
                .lines()
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" | ")
        };
        if out.status.success() && file.to_string_lossy().ends_with(".serviceaccount.yaml") {
            let real = self.run(&["apply", "-f", &arg], None)?;
            return Ok((real.status.success(), stderr(&real)));
        }
        Ok((out.status.success(), stderr(&out)))
    }
}

pub fn render_verify() -> Result<()> {
    let root = root();
    let fixtures = write_fixtures(&root.join(FIXTURES))?;
    println!("wrote {} fixtures to {FIXTURES}", fixtures.len());

    let work = root.join("target/render-verify");
    std::fs::create_dir_all(&work)?;
    let kubeconfig = work.join("kubeconfig");
    let _ = crate::kube::kind(&kubeconfig)
        .args(["delete", "cluster", "--name", CLUSTER.as_str()])
        .arg("--kubeconfig")
        .arg(&kubeconfig)
        .status();
    let _guard = ClusterGuard(kubeconfig.clone());
    let created = run(crate::kube::kind(&kubeconfig)
        .args([
            "create",
            "cluster",
            "--name",
            CLUSTER.as_str(),
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
    let kube = Kube { kubeconfig };
    kube.prepare(&root)?;

    // Apply ServiceAccounts first so the pods find theirs.
    let mut ordered: Vec<&PathBuf> = fixtures.iter().collect();
    ordered.sort_by_key(|p| !p.to_string_lossy().ends_with(".serviceaccount.yaml"));
    let (mut passed, mut failed) = (0usize, Vec::new());
    for file in ordered {
        let name = file
            .file_name()
            .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        let (good, why) = kube.check(file)?;
        if good {
            println!("PASS {name}");
            passed += 1;
        } else {
            println!("FAIL {name}: {why}");
            failed.push(format!("{name}: {why}"));
        }
    }
    println!("render-verify: {passed} passed, {} failed", failed.len());
    if !failed.is_empty() {
        bail!("{} fixtures rejected:\n{}", failed.len(), failed.join("\n"));
    }
    Ok(())
}
