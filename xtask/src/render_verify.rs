//! `cargo xtask render-verify` writes the render fixtures and dry-runs every
//! object against a real API server, because unit tests cannot catch
//! API-server rejections.

use std::path::{Path, PathBuf};
use std::process::Output;

use anyhow::{Context, Result, bail};
use buildkit_controller::render::fixtures::{CONTROLLER_NAMESPACE, POOL_NAMESPACE, render_all};

use super::root;
use crate::kube::Cluster;

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

/// Install the CRDs, namespaces and prerequisites.
fn prepare(cluster: &Cluster) -> Result<()> {
    let version = cluster.kubectl(&["version", "-o", "json"])?;
    for l in version.lines().filter(|l| l.contains("gitVersion")) {
        println!("{}", l.trim());
    }
    cluster.apply_crds().context("CRD apply")?;
    for ns in [POOL_NAMESPACE, CONTROLLER_NAMESPACE] {
        cluster
            .kubectl(&["create", "namespace", ns])
            .context("create namespace")?;
    }
    cluster.apply(PREREQS).context("prereqs")
}

/// Server-side dry run of one fixture. A `ServiceAccount` is also applied
/// for real afterwards: pod admission looks it up even on a dry run.
fn check(cluster: &Cluster, file: &Path) -> Result<(bool, String)> {
    let arg = file.to_string_lossy().into_owned();
    let out = cluster.kubectl_raw(
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
        let real = cluster.kubectl_raw(&["apply", "-f", &arg], None)?;
        return Ok((real.status.success(), stderr(&real)));
    }
    Ok((out.status.success(), stderr(&out)))
}

pub fn render_verify() -> Result<()> {
    let root = root();
    let fixtures = write_fixtures(&root.join(FIXTURES))?;
    println!("wrote {} fixtures to {FIXTURES}", fixtures.len());

    let work = root.join("target/render-verify");
    std::fs::create_dir_all(&work)?;
    let cluster = Cluster::new(
        &super::kind_cluster("bk-render-verify"),
        work.join("kubeconfig"),
    );
    cluster.create(None, "120s")?;
    prepare(&cluster)?;

    // Apply ServiceAccounts first so the pods find theirs.
    let mut ordered: Vec<&PathBuf> = fixtures.iter().collect();
    ordered.sort_by_key(|p| !p.to_string_lossy().ends_with(".serviceaccount.yaml"));
    let (mut passed, mut failed) = (0usize, Vec::new());
    for file in ordered {
        let name = file
            .file_name()
            .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        let (good, why) = check(&cluster, file)?;
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
