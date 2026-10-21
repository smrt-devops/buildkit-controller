//! The only place that builds `kind`, `kubectl` and `helm` commands.
//!
//! Every command gets `KUBECONFIG=<private path>` in its environment, so a
//! misplaced `--kubeconfig` (say, after `--` in `kubectl exec`) can never fall
//! back to `~/.kube/config`, which may point at a real cluster.

use std::path::{Path, PathBuf};
use std::process::Command;

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
}
