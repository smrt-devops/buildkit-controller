//! `cargo xtask helm-verify`: install the chart on a throwaway kind cluster
//! `cargo xtask helm-verify` installs the chart on a throwaway kind cluster
//! with one pool and checks that both replicas are Ready, exactly one is the
//! leader, the pool is Ready with its minimum warm pods Ready, and `/readyz`
//! returns 200 on every replica. The controller namespace enforces
//! `PodSecurity` "restricted".
//!
//! The cluster name is `bk-helm-verify`, plus `-$BK_KIND_SUFFIX` when that is set.
//! Every kind, kubectl and helm call uses the private kubeconfig under
//! `target/helm-verify`. The task never touches `~/.kube/config`, and it
//! deletes the cluster on every exit path.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::image::WORKER_IMAGE;
use crate::root;

fn cluster() -> String {
    super::kind_cluster("bk-helm-verify")
}
const NS: &str = "bk-helm";
const POOL_NS: &str = "bk-helm-pool";
const CONTROLLER_IMAGE: &str = "buildkit-controller:dev";
const LEASE: &str = "buildkit-controller-leader";

pub(crate) struct Env {
    pub(crate) kubeconfig: PathBuf,
}

impl Env {
    pub(crate) fn tool(&self, program: &str, args: &[&str]) -> Result<String> {
        let out = crate::kube::by_name(program, &self.kubeconfig)
            .args(args)
            .arg("--kubeconfig")
            .arg(&self.kubeconfig)
            .output()
            .with_context(|| format!("running {program}"))?;
        ensure!(
            out.status.success(),
            "{program} {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub(crate) fn kubectl(&self, args: &[&str]) -> Result<String> {
        self.tool("kubectl", args)
    }
}

/// Poll `f` until it returns `Some`, or fail after `timeout` and report `what`.
pub(crate) fn until<T>(
    what: &str,
    timeout: Duration,
    mut f: impl FnMut() -> Result<Option<T>>,
) -> Result<T> {
    let start = Instant::now();
    loop {
        match f() {
            Ok(Some(v)) => return Ok(v),
            Err(e) if start.elapsed() > timeout => bail!("{what}: {e:#}"),
            Ok(None) | Err(_) => {}
        }
        ensure!(start.elapsed() <= timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// Deletes the cluster when dropped, including on failure.
struct Cleanup(Env);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self
            .0
            .tool("kind", &["delete", "cluster", "--name", cluster().as_str()]);
    }
}

pub(crate) fn helm_verify() -> Result<()> {
    let work = root().join("target/helm-verify");
    std::fs::create_dir_all(&work)?;
    let cleanup = Cleanup(Env {
        kubeconfig: work.join("kubeconfig"),
    });
    let env = &cleanup.0;
    let result = verify(env);
    if result.is_err() {
        for args in [
            vec!["get", "pods", "-A", "-o", "wide"],
            vec![
                "logs",
                "-n",
                NS,
                "-l",
                "app.kubernetes.io/name=buildkit-controller",
                "--tail=30",
            ],
            vec!["get", "secrets,leases", "-n", NS],
            vec!["get", "buildkitpool", "-n", POOL_NS, "-o", "yaml"],
            vec!["describe", "pods", "-n", POOL_NS],
        ] {
            if let Ok(o) = env.kubectl(&args) {
                eprintln!("--- kubectl {args:?}\n{o}");
            }
        }
    }
    result
}

#[allow(clippy::too_many_lines, reason = "one linear scenario script")]
fn verify(env: &Env) -> Result<()> {
    let root = root();
    let _ = env.tool("kind", &["delete", "cluster", "--name", cluster().as_str()]);
    env.tool(
        "kind",
        &[
            "create",
            "cluster",
            "--name",
            cluster().as_str(),
            "--wait",
            "120s",
        ],
    )?;

    crate::image::image(CONTROLLER_IMAGE, Some(cluster().as_str()))?;
    let ok = |c: &mut Command| -> Result<()> {
        ensure!(
            c.status().with_context(|| format!("{c:?}"))?.success(),
            "{c:?} failed"
        );
        Ok(())
    };
    crate::image::ensure_worker()?;
    ok(&mut crate::kube::kind_load(
        WORKER_IMAGE,
        cluster().as_str(),
    ))?;

    env.kubectl(&["create", "namespace", NS])?;
    env.kubectl(&[
        "label",
        "namespace",
        NS,
        "pod-security.kubernetes.io/enforce=restricted",
    ])?;
    env.kubectl(&["create", "namespace", POOL_NS])?;

    let chart = root
        .join("helm/buildkit-controller")
        .to_string_lossy()
        .into_owned();
    let values = root
        .join("xtask/fixtures/helm-verify/values.yaml")
        .to_string_lossy()
        .into_owned();
    env.tool(
        "helm",
        &[
            "install",
            "bk",
            &chart,
            "-n",
            NS,
            "-f",
            &values,
            "--timeout",
            "5m",
        ],
    )?;
    println!("installed chart");

    let pods = until("two Ready replicas", Duration::from_mins(2), || {
        let names = env.kubectl(&[
            "get", "pods", "-n", NS, "-l", "app.kubernetes.io/name=buildkit-controller",
            "-o", "jsonpath={range .items[?(@.status.containerStatuses[0].ready==true)]}{.metadata.name}{\"\\n\"}{end}",
        ])?;
        let v: Vec<String> = names.lines().map(str::to_owned).collect();
        Ok((v.len() == 2).then_some(v))
    })?;
    println!("PASS both replicas Ready: {pods:?}");

    let mut leaders = 0;
    for p in &pods {
        let base = format!("/api/v1/namespaces/{NS}/pods/{p}:8080/proxy");
        // `kubectl get --raw` fails on any non-2xx, so success is a 200.
        env.kubectl(&["get", "--raw", &format!("{base}/readyz")])?;
        let metrics = env.kubectl(&["get", "--raw", &format!("{base}/metrics")])?;
        if metrics.lines().any(|l| l.trim() == "buildkit_leader 1") {
            leaders += 1;
        }
    }
    println!("PASS /readyz 200 on both replicas");
    ensure!(
        leaders == 1,
        "expected exactly one leader by metrics, got {leaders}"
    );
    let holder = env.kubectl(&[
        "get",
        "lease",
        LEASE,
        "-n",
        NS,
        "-o",
        "jsonpath={.spec.holderIdentity}",
    ])?;
    ensure!(
        pods.iter().any(|p| holder.starts_with(p.as_str())),
        "lease holder {holder:?} is not a replica"
    );
    println!("PASS one leader (buildkit_leader=1 on one replica; lease holder {holder})");

    // Apply the pool only now that the controller is Ready, to rule out a startup race.
    let pool_values = root
        .join("xtask/fixtures/helm-verify/pool.yaml")
        .to_string_lossy()
        .into_owned();
    env.tool(
        "helm",
        &[
            "upgrade",
            "bk",
            &chart,
            "-n",
            NS,
            "-f",
            &values,
            "-f",
            &pool_values,
        ],
    )?;
    println!("applied pool after controller Ready");

    let pool_state = until("pool Ready with warm pods", Duration::from_mins(5), || {
        let s = env.kubectl(&[
            "get",
            "buildkitpool",
            "verify",
            "-n",
            POOL_NS,
            "-o",
            "jsonpath={.status.conditions[?(@.type==\"Ready\")].status} warm={.status.warm}",
        ])?;
        Ok(s.starts_with("True warm=1").then_some(s))
    })?;
    let ready_pods = env.kubectl(&[
        "get",
        "pods",
        "-n",
        POOL_NS,
        "-o",
        "jsonpath={range .items[*]}{.metadata.name}={.status.containerStatuses[0].ready} {end}",
    ])?;
    ensure!(
        ready_pods.contains("=true"),
        "no Ready worker pod: {ready_pods}"
    );
    let logs = env.kubectl(&[
        "logs",
        "-n",
        NS,
        "-l",
        "app.kubernetes.io/name=buildkit-controller",
        "--tail=-1",
    ])?;
    ensure!(
        !shows_rbac_denial(&logs),
        "controller logs show an RBAC denial"
    );
    println!("PASS no RBAC denial (Forbidden / is forbidden / \"code\":403) in controller logs");
    println!("PASS pool Ready: {pool_state}; worker pods: {ready_pods}");
    println!("helm-verify: all checks passed");
    Ok(())
}

/// Whether controller logs contain a Kubernetes API authorization denial.
/// Matches the kube error text, not a bare "403", which also appears in
/// microsecond timestamps, UIDs and resourceVersions.
fn shows_rbac_denial(logs: &str) -> bool {
    ["Forbidden", "is forbidden", "\"code\":403"]
        .iter()
        .any(|needle| logs.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::shows_rbac_denial;

    #[test]
    fn bare_403_in_timestamps_and_ids_is_not_a_denial() {
        let logs = "2026-10-20T10:52:21.403512Z  INFO reconcile pool=p uid=4031-aa rv=\"94037\"";
        assert!(!shows_rbac_denial(logs));
    }

    #[test]
    fn kube_forbidden_errors_are_denials() {
        assert!(shows_rbac_denial(
            r#"pods "x" is forbidden: User "system:serviceaccount:ns:sa" cannot list"#
        ));
        assert!(shows_rbac_denial(r#"{"kind":"Status","code":403}"#));
        assert!(shows_rbac_denial("ApiError: Forbidden (ErrorResponse)"));
    }
}
