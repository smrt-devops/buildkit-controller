//! The kind cluster an xtask task created for an ignored scenario test.
//!
//! The kubeconfig comes only from `BK_TEST_KUBECONFIG`, which xtask sets. Its
//! current context must be a kind context (`kind-*`) whose API server is on a
//! loopback address, as kind publishes it. Anything else fails before the first
//! request, so a test run by hand can never reach the cluster that
//! `KUBECONFIG` or `~/.kube/config` names.

#![allow(dead_code, reason = "each test binary uses a different part")]

use std::ffi::OsString;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Command;

use kube::config::{KubeConfigOptions, Kubeconfig};

pub const VAR: &str = "BK_TEST_KUBECONFIG";

/// The checked harness kubeconfig. Panics with the reason when it is unusable.
pub fn kubeconfig() -> (PathBuf, Kubeconfig) {
    load(std::env::var_os(VAR), true).unwrap_or_else(|e| panic!("{e}"))
}

/// A client for the harness cluster.
pub async fn client() -> kube::Client {
    let (_, kc) = kubeconfig();
    let config = kube::Config::from_custom_kubeconfig(kc, &KubeConfigOptions::default())
        .await
        .unwrap();
    let host = config.cluster_url.host().unwrap_or_default();
    assert!(
        is_loopback(host),
        "API server {} is not loopback",
        config.cluster_url
    );
    kube::Client::try_from(config).unwrap()
}

/// `kubectl` on the harness cluster, with nothing inherited that could pick another.
pub fn kubectl() -> Command {
    let (path, kc) = kubeconfig();
    let mut c = scrubbed("kubectl");
    c.arg("--kubeconfig")
        .arg(path)
        .arg("--context")
        .arg(kc.current_context.unwrap_or_default());
    c
}

/// `program` (the controller binary) bound to `kubeconfig`, which must name a
/// loopback API server. The controller finds its cluster through `KUBECONFIG`.
pub fn controller(program: &Path, kubeconfig: &Path) -> Command {
    load(Some(kubeconfig.into()), false).unwrap_or_else(|e| panic!("{e}"));
    let mut c = scrubbed(program);
    c.env("KUBECONFIG", kubeconfig);
    c
}

fn scrubbed(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut c = Command::new(program);
    for (name, _) in std::env::vars_os() {
        let n = name.to_string_lossy();
        if n.starts_with("KUBE") || n.starts_with("HELM_KUBE") || n == "HELM_NAMESPACE" {
            c.env_remove(&name);
        }
    }
    c
}

/// Reads and checks the kubeconfig at `var`. With `kind_context`, the current
/// context must also be named `kind-*`.
pub fn load(var: Option<OsString>, kind_context: bool) -> Result<(PathBuf, Kubeconfig), String> {
    let path = var
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| format!("{VAR} is unset: run this test through its `cargo xtask` task"))?;
    let kc = Kubeconfig::read_from(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    check(&kc, kind_context).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((path, kc))
}

/// The current context's API server must be loopback, and with `kind_context`
/// the context must be a kind one.
pub fn check(kc: &Kubeconfig, kind_context: bool) -> Result<(), String> {
    let name = kc.current_context.as_deref().ok_or("no current-context")?;
    if kind_context && !name.starts_with("kind-") {
        return Err(format!("current-context {name:?} is not a kind cluster"));
    }
    let ctx = kc
        .contexts
        .iter()
        .find(|c| c.name == name)
        .and_then(|c| c.context.as_ref())
        .ok_or_else(|| format!("context {name:?} not found"))?;
    let server = kc
        .clusters
        .iter()
        .find(|c| c.name == ctx.cluster)
        .and_then(|c| c.cluster.as_ref())
        .and_then(|c| c.server.as_deref())
        .ok_or_else(|| format!("cluster {:?} has no server", ctx.cluster))?;
    let authority = server
        .split_once("://")
        .map_or(server, |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    if is_loopback(host) {
        Ok(())
    } else {
        Err(format!("API server {server} is not on a loopback address"))
    }
}

fn is_loopback(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kubeconfig(context: &str, server: &str) -> Kubeconfig {
        Kubeconfig::from_yaml(&format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster: {{server: \"{server}\"}}\n\
             contexts:\n- name: {context}\n  context: {{cluster: c, user: u}}\n\
             users:\n- name: u\n  user: {{}}\ncurrent-context: {context}\n"
        ))
        .unwrap()
    }

    #[test]
    fn unset_or_empty_variable_is_refused() {
        assert!(load(None, true).unwrap_err().contains(VAR));
        assert!(load(Some(OsString::new()), true).unwrap_err().contains(VAR));
    }

    #[test]
    fn only_a_loopback_kind_context_is_accepted() {
        assert!(check(&kubeconfig("kind-bk", "https://127.0.0.1:6443"), true).is_ok());
        assert!(check(&kubeconfig("kind-bk", "https://[::1]:6443"), true).is_ok());
        assert!(check(&kubeconfig("kind-bk", "https://localhost:6443/"), true).is_ok());
        let eks = "https://ABC.gr7.eu-west-1.eks.amazonaws.com";
        assert!(check(&kubeconfig("kind-bk", eks), true).is_err());
        assert!(check(&kubeconfig("kind-bk", "https://10.0.0.1:443"), true).is_err());
        let prod = kubeconfig(
            "arn:aws:eks:eu-west-1:1:cluster/prod",
            "https://127.0.0.1:6443",
        );
        assert!(check(&prod, true).is_err());
        assert!(check(&prod, false).is_ok());
    }
}
