//! `cargo xtask helm-test` runs `helm lint` and compares `helm template` output
//! with golden files for each values file under `xtask/fixtures/helm-test`. It
//! also checks that the chart rejects every file under `invalid/`, through
//! `values.schema.json`, `required` or `fail`. Every invalid file's first line must be
//! `# expect: <text>`, and the error must contain that text, so a fixture cannot
//! pass by failing for another reason. `--update` rewrites the goldens.
//!
//! `helm template` runs as if the cluster served the Envoy Gateway kind the chart
//! renders only when it exists ([`API_VERSIONS`]), so the goldens show it. The
//! Gateway API routes render with no API version, which the goldens pin too.
//! It also renders `examples/helm-values.yaml` and checks the NOTES.txt lines in
//! [`NOTES`].

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::{root, yaml_files};

const CHART: &str = "helm/buildkit-controller";
const FIXTURES: &str = "xtask/fixtures/helm-test";
/// Kinds the chart renders only when the cluster serves them.
const API_VERSIONS: [&str; 1] = ["gateway.envoyproxy.io/v1alpha1/BackendTrafficPolicy"];

/// A NOTES.txt line, whether the install notes of the fixture must show it, and the fixture.
const NOTES: [(&str, bool, &str); 2] = [
    ("workerNetwork.clusterCIDRs is empty", true, "default"),
    ("workerNetwork.clusterCIDRs is empty", false, "all-options"),
];

/// Why an invalid fixture is wrong, or `None` when it was rejected for its `# expect:` reason.
fn judge(expect: Option<&str>, accepted: bool, stderr: &str) -> Option<String> {
    match expect {
        _ if accepted => Some("was accepted".to_owned()),
        None => Some(format!(
            "has no `# expect:` first line; it failed with: {stderr}"
        )),
        Some(want) if !stderr.contains(want) => {
            Some(format!("rejected without {want:?}: {stderr}"))
        }
        Some(_) => None,
    }
}

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
    if sub == "template" || sub == "install" {
        c.args(["bk", CHART, "--namespace", "buildkit-system"]);
        if sub == "install" {
            c.arg("--dry-run=client");
        }
        if sub == "template" {
            for v in API_VERSIONS {
                c.args(["--api-versions", v]);
            }
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
    for (line, want, fixture) in NOTES {
        let values = dir.join(format!("{fixture}.yaml"));
        let out = helm(&root, "install", &values)?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() || stdout.contains(line) != want {
            failures.push(format!(
                "notes {fixture}: expected {line:?} present={want}: {stdout}{}",
                String::from_utf8_lossy(&out.stderr)
            ));
        } else {
            println!("PASS notes {fixture}");
            passed += 1;
        }
    }
    let example = root.join("examples/helm-values.yaml");
    let out = helm(&root, "template", &example)?;
    if out.status.success() {
        println!("PASS examples/helm-values");
        passed += 1;
    } else {
        failures.push(format!(
            "examples/helm-values.yaml: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    for values in yaml_files(&dir.join("invalid"))? {
        let name = values
            .file_stem()
            .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        let out = helm(&root, "template", &values)?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        let expect = expected_error(&values)?;
        if let Some(why) = judge(expect.as_deref(), out.status.success(), &stderr) {
            failures.push(format!("invalid/{name}: {why}"));
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

#[cfg(test)]
mod tests {
    use super::{CHART, judge};

    /// The chart's default image tag is its appVersion, and a checkout of a
    /// release tag installs from this file. It must name the release being cut.
    #[test]
    fn chart_version_is_the_workspace_version() {
        let path = crate::root().join(CHART).join("Chart.yaml");
        let chart: serde_json::Value =
            serde_saphyr::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let workspace = env!("CARGO_PKG_VERSION");
        assert_eq!(chart["version"], workspace, "Chart.yaml version");
        assert_eq!(chart["appVersion"], workspace, "Chart.yaml appVersion");
    }

    #[test]
    fn invalid_fixture_needs_an_expect_line() {
        assert!(judge(None, false, "some error").is_some());
    }

    #[test]
    fn invalid_fixture_must_fail_for_its_reason() {
        assert!(judge(Some("gateway.parentRefs"), false, "other error").is_some());
        assert!(
            judge(
                Some("gateway.parentRefs"),
                false,
                "gateway.parentRefs is required"
            )
            .is_none()
        );
        assert!(judge(Some("x"), true, "").is_some());
    }
}
