//! `cargo xtask helm-test` runs `helm lint` and compares `helm template` output
//! with golden files for each values file under `xtask/fixtures/helm-test`. It
//! also checks that the chart rejects every file under `invalid/`, through
//! `values.schema.json`, `required` or `fail`. An invalid file whose first line
//! is `# expect: <text>` must fail with an error that contains that text, so it
//! cannot pass by failing for another reason. `--update` rewrites the goldens.
//!
//! `helm template` runs as if the cluster served the Gateway API and Envoy
//! Gateway kinds the chart renders only when they exist ([`API_VERSIONS`]), so
//! the goldens show them. `helm lint` takes no API versions and skips them.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::{root, yaml_files};

const CHART: &str = "helm/buildkit-controller";
const FIXTURES: &str = "xtask/fixtures/helm-test";
/// Kinds the chart renders only when the cluster serves them.
const API_VERSIONS: [&str; 4] = [
    "gateway.networking.k8s.io/v1/HTTPRoute",
    "gateway.networking.k8s.io/v1/BackendTLSPolicy",
    "gateway.networking.k8s.io/v1/TLSRoute",
    "gateway.envoyproxy.io/v1alpha1/BackendTrafficPolicy",
];

/// The text after `# expect:` on the first line of `values`, if any.
fn expected_error(values: &Path) -> Result<Option<String>> {
    let text = std::fs::read_to_string(values).with_context(|| values.display().to_string())?;
    Ok(text
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("# expect:"))
        .map(|s| s.trim().to_owned()))
}

fn helm(root: &Path, sub: &str, values: &Path) -> Result<std::process::Output> {
    let mut c = crate::kube::helm(&crate::kube::sandbox());
    c.current_dir(root).arg(sub);
    if sub == "template" {
        c.args(["bk", CHART, "--namespace", "buildkit-system"]);
        for v in API_VERSIONS {
            c.args(["--api-versions", v]);
        }
    } else {
        c.arg(CHART);
    }
    c.arg("-f").arg(values).output().context("running helm")
}

pub(crate) fn helm_test(update: bool) -> Result<()> {
    let root = root();
    let dir = root.join(FIXTURES);
    let mut failures = Vec::new();
    let mut passed = 0usize;
    for values in yaml_files(&dir)? {
        let name = values
            .file_stem()
            .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        let lint = helm(&root, "lint", &values)?;
        if !lint.status.success() {
            failures.push(format!(
                "lint {name}: {}",
                String::from_utf8_lossy(&lint.stdout)
            ));
            continue;
        }
        let out = helm(&root, "template", &values)?;
        if !out.status.success() {
            failures.push(format!(
                "template {name}: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
            continue;
        }
        let golden = dir.join("golden").join(format!("{name}.yaml"));
        if update {
            std::fs::write(&golden, &out.stdout)?;
        } else if std::fs::read(&golden).ok().as_deref() != Some(out.stdout.as_slice()) {
            failures.push(format!("golden {name}: output differs from {}; run `cargo xtask helm-test --update` and review", golden.display()));
            continue;
        }
        println!("PASS {name}");
        passed += 1;
    }
    for values in yaml_files(&dir.join("invalid"))? {
        let name = values
            .file_stem()
            .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        let out = helm(&root, "template", &values)?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if out.status.success() {
            failures.push(format!("invalid/{name}: was accepted"));
        } else if let Some(want) = expected_error(&values)?
            && !stderr.contains(&want)
        {
            failures.push(format!(
                "invalid/{name}: rejected without {want:?}: {stderr}"
            ));
        } else {
            println!("PASS invalid/{name} rejected");
            passed += 1;
        }
    }
    println!("helm-test: {passed} passed, {} failed", failures.len());
    if !failures.is_empty() {
        bail!("{}", failures.join("\n"));
    }
    Ok(())
}
