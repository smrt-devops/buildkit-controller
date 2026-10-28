//! The only place that builds `kind`, `kubectl` and `helm` commands.
//!
//! Every command gets `KUBECONFIG=<private path>` in its environment, so a
//! misplaced `--kubeconfig` (say, after `--` in `kubectl exec`) can never fall
//! back to `~/.kube/config`, which may point at a real cluster.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn tool(program: &str, kubeconfig: &Path) -> Command {
    let mut c = Command::new(program);
    c.env("KUBECONFIG", kubeconfig);
    c
}

/// `kind` bound to `kubeconfig`.
pub(crate) fn kind(kubeconfig: &Path) -> Command {
    tool("kind", kubeconfig)
}

/// `kubectl` bound to `kubeconfig`.
pub(crate) fn kubectl(kubeconfig: &Path) -> Command {
    tool("kubectl", kubeconfig)
}

/// `helm` bound to `kubeconfig`.
pub(crate) fn helm(kubeconfig: &Path) -> Command {
    tool("helm", kubeconfig)
}

/// A kubeconfig path that is never created, for commands that need no cluster
/// (`kind load`, `helm template`). Pointing them here keeps them off `~/.kube/config`.
pub(crate) fn sandbox() -> PathBuf {
    crate::root().join("target/kube-sandbox/kubeconfig")
}

/// `kind load docker-image <image> --name <cluster>`.
pub(crate) fn kind_load(image: &str, cluster: &str) -> Command {
    let mut c = kind(&sandbox());
    c.args(["load", "docker-image", image, "--name", cluster]);
    c
}

/// The tool by name, for callers that pick it at run time.
pub(crate) fn by_name(program: &str, kubeconfig: &Path) -> Command {
    tool(program, kubeconfig)
}

/// What one `kubectl get -o json` says about a single condition.
#[derive(Debug, PartialEq, Eq)]
enum Observed {
    /// The object has no `status` (or no `status.conditions`) yet.
    NoConditions,
    /// `status.conditions` exists but has no entry of the wanted type; holds the types seen.
    Missing(Vec<String>),
    /// The condition's `status` field.
    Status(String),
}

impl Observed {
    fn is_met(&self) -> bool {
        matches!(self, Observed::Status(s) if s == "True")
    }
}

impl std::fmt::Display for Observed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Observed::NoConditions => write!(f, "no status.conditions yet"),
            Observed::Missing(seen) => write!(f, "condition absent (have: {})", seen.join(", ")),
            Observed::Status(s) => write!(f, "status={s}"),
        }
    }
}

/// Reads condition `kind` out of an object's JSON. Missing status or conditions is
/// "not yet", never an error: `kubectl wait` fails hard on that window instead of waiting.
fn observe(json: &str, kind: &str) -> anyhow::Result<Observed> {
    let v: serde_json::Value = serde_json::from_str(json)?;
    let Some(conds) = v.pointer("/status/conditions").and_then(|c| c.as_array()) else {
        return Ok(Observed::NoConditions);
    };
    let found = conds.iter().find(|c| {
        c["type"]
            .as_str()
            .is_some_and(|t| t.eq_ignore_ascii_case(kind))
    });
    Ok(match found {
        Some(c) => Observed::Status(c["status"].as_str().unwrap_or("").to_owned()),
        None => Observed::Missing(
            conds
                .iter()
                .filter_map(|c| c["type"].as_str().map(ToOwned::to_owned))
                .collect(),
        ),
    })
}

/// Polls until `resource` (e.g. `crd/x`, `buildkitpool/y`) reports condition `kind`=True.
///
/// Use this instead of `kubectl wait --for=condition=...` on an object that was just
/// applied: before the API server writes any status, kubectl exits with
/// `.status.conditions accessor error` rather than waiting.
pub(crate) fn wait_condition(
    kubeconfig: &Path,
    resource: &str,
    namespace: Option<&str>,
    kind: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last: String;
    loop {
        let mut c = kubectl(kubeconfig);
        c.arg("--kubeconfig")
            .arg(kubeconfig)
            .args(["get", resource]);
        if let Some(ns) = namespace {
            c.args(["-n", ns]);
        }
        let out = c.args(["-o", "json"]).output()?;
        if out.status.success() {
            let obs = observe(&String::from_utf8_lossy(&out.stdout), kind)?;
            if obs.is_met() {
                return Ok(());
            }
            last = obs.to_string();
        } else {
            last = format!(
                "kubectl get failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        if Instant::now() >= deadline {
            anyhow::bail!("{resource} not {kind} after {timeout:?}; last observed: {last}");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Waits for the project's CRD to be Established.
pub(crate) fn wait_crd_established(kubeconfig: &Path) -> anyhow::Result<()> {
    wait_condition(
        kubeconfig,
        "crd/buildkitpools.buildkit.smrt-devops.net",
        None,
        "Established",
        Duration::from_mins(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kubeconfig_env(c: &Command) -> Option<std::ffi::OsString> {
        c.get_envs()
            .find(|(k, _)| *k == "KUBECONFIG")
            .and_then(|(_, v)| v.map(ToOwned::to_owned))
    }

    #[test]
    fn every_builder_sets_kubeconfig() {
        let kc = Path::new("/tmp/private/kubeconfig");
        for (name, c) in [
            ("kind", kind(kc)),
            ("kubectl", kubectl(kc)),
            ("helm", helm(kc)),
            ("by_name", by_name("kubectl", kc)),
        ] {
            assert_eq!(
                kubeconfig_env(&c).as_deref(),
                Some(kc.as_os_str()),
                "{name}"
            );
        }
        let load = kind_load("img", "c");
        assert_eq!(
            kubeconfig_env(&load).as_deref(),
            Some(sandbox().as_os_str())
        );
        assert!(sandbox().components().any(|c| c.as_os_str() == "target"));
    }

    #[test]
    fn no_direct_command_new_for_cluster_tools() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.file_name().is_some_and(|n| n == "kube.rs")
                || p.extension().is_none_or(|e| e != "rs")
            {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            for tool in ["kind", "kubectl", "helm"] {
                let needle = format!("Command::new(\"{tool}\")");
                assert!(
                    !text.contains(&needle),
                    "{} builds {tool} directly",
                    p.display()
                );
            }
        }
    }

    #[test]
    fn missing_status_is_not_yet() {
        let o = observe(r#"{"metadata":{"name":"x"}}"#, "Established").unwrap();
        assert_eq!(o, Observed::NoConditions);
        assert!(!o.is_met());
        let o = observe(r#"{"status":{}}"#, "Established").unwrap();
        assert_eq!(o, Observed::NoConditions);
    }

    #[test]
    fn absent_condition_lists_what_exists() {
        let j = r#"{"status":{"conditions":[{"type":"NamesAccepted","status":"True"}]}}"#;
        let o = observe(j, "Established").unwrap();
        assert_eq!(o, Observed::Missing(vec!["NamesAccepted".into()]));
        assert!(!o.is_met());
    }

    #[test]
    fn false_is_not_met_and_true_is() {
        let f = r#"{"status":{"conditions":[{"type":"Established","status":"False"}]}}"#;
        let o = observe(f, "Established").unwrap();
        assert_eq!(o, Observed::Status("False".into()));
        assert!(!o.is_met());
        let t = r#"{"status":{"conditions":[{"type":"NamesAccepted","status":"True"},{"type":"Established","status":"True"}]}}"#;
        assert!(observe(t, "established").unwrap().is_met());
    }

    #[test]
    fn bad_json_is_an_error() {
        assert!(observe("not json", "Ready").is_err());
    }
}
