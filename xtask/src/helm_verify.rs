//! `cargo xtask helm-verify` installs the chart on a throwaway kind cluster
//! with `watchNamespaces` set to two pool namespaces and checks that both
//! replicas are Ready, exactly one is the leader, and `/readyz` returns 200 on
//! every replica. It then checks that the pool in each watched namespace is
//! Ready with its warm pod and binds a claim, that a pool in a third namespace
//! is ignored, that the controller cannot create pods or Secrets in that
//! namespace or in kube-system, and that its logs show no RBAC denial. The
//! controller namespace enforces `PodSecurity` "restricted".
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
/// The second watched pool namespace.
const POOL_NS_B: &str = "bk-helm-pool-b";
/// Not in `watchNamespaces`.
const STRAY_NS: &str = "bk-helm-stray";
/// The namespace in `claims.namespaces`.
const APPS_NS: &str = "bk-helm-apps";
const CONTROLLER_SA: &str = "system:serviceaccount:bk-helm:buildkit-controller";
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
            vec!["get", "buildkitpool", "-A", "-o", "yaml"],
            vec!["get", "bkc", "-A", "-o", "yaml"],
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
    for ns in [POOL_NS, POOL_NS_B, STRAY_NS, APPS_NS] {
        env.kubectl(&["create", "namespace", ns])?;
    }

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
    println!("applied pools after controller Ready");
    let stray = root
        .join("xtask/fixtures/helm-verify/stray-pool.yaml")
        .to_string_lossy()
        .into_owned();
    env.kubectl(&["apply", "-f", &stray])?;
    println!("applied pool {STRAY_NS}/stray outside watchNamespaces");

    for ns in [POOL_NS, POOL_NS_B] {
        let pool_state = until(
            &format!("pool {ns}/verify Ready with warm pods"),
            Duration::from_mins(5),
            || {
                let s = env.kubectl(&[
                    "get",
                    "buildkitpool",
                    "verify",
                    "-n",
                    ns,
                    "-o",
                    "jsonpath={.status.conditions[?(@.type==\"Ready\")].status} warm={.status.warm}",
                ])?;
                Ok(s.starts_with("True warm=1").then_some(s))
            },
        )?;
        let ready_pods = env.kubectl(&[
            "get",
            "pods",
            "-n",
            ns,
            "-o",
            "jsonpath={range .items[*]}{.metadata.name}={.status.containerStatuses[0].ready} {end}",
        ])?;
        ensure!(
            ready_pods.contains("=true"),
            "no Ready worker pod in {ns}: {ready_pods}"
        );
        println!("PASS pool {ns}/verify Ready: {pool_state}; worker pods: {ready_pods}");
    }

    // An allocation from each pool, through a claim.
    let work = root.join("target/helm-verify");
    for (claim, ns) in [("claim-a", POOL_NS), ("claim-b", POOL_NS_B)] {
        let path = work.join(format!("{claim}.yaml"));
        std::fs::write(
            &path,
            format!(
                "apiVersion: buildkit.smrt-devops.net/v1alpha1\nkind: BuildKitClaim\nmetadata: {{name: {claim}, namespace: {APPS_NS}}}\nspec:\n  poolRef: {{namespace: {ns}, name: verify}}\n  ttl: 10m\n"
            ),
        )?;
        env.kubectl(&["apply", "-f", &path.to_string_lossy()])?;
        let s = until(
            &format!("claim {APPS_NS}/{claim} Bound"),
            Duration::from_mins(3),
            || {
                let s = env.kubectl(&[
                    "get",
                    "bkc",
                    claim,
                    "-n",
                    APPS_NS,
                    "-o",
                    "jsonpath={.status.phase}",
                ])?;
                Ok((s == "Bound").then_some(s))
            },
        )?;
        env.kubectl(&["get", "secret", &format!("{claim}-buildkit"), "-n", APPS_NS])?;
        println!("PASS claim {APPS_NS}/{claim} on {ns}/verify {s}; Secret present");
    }

    // By now the controller has had minutes to act on the stray pool.
    let stray_state = env.kubectl(&[
        "get",
        "buildkitpool",
        "stray",
        "-n",
        STRAY_NS,
        "-o",
        "jsonpath={.status}",
    ])?;
    let stray_pods = env.kubectl(&["get", "pods", "-n", STRAY_NS, "-o", "name"])?;
    ensure!(
        stray_state.trim().is_empty() && stray_pods.trim().is_empty(),
        "pool {STRAY_NS}/stray was acted on: status {stray_state:?}, pods {stray_pods:?}"
    );
    println!("PASS pool {STRAY_NS}/stray ignored: no status, no pods");

    for (verb, resource, ns, want) in [
        ("create", "pods", POOL_NS, "yes"),
        ("create", "secrets", POOL_NS_B, "yes"),
        ("create", "pods", STRAY_NS, "no"),
        ("create", "secrets", STRAY_NS, "no"),
        ("create", "pods", "kube-system", "no"),
        ("create", "secrets", "kube-system", "no"),
        ("delete", "pods", "kube-system", "no"),
        (
            "list",
            "buildkitpools.buildkit.smrt-devops.net",
            STRAY_NS,
            "no",
        ),
    ] {
        let got = can_i(env, verb, resource, ns)?;
        ensure!(
            got == want,
            "auth can-i {verb} {resource} -n {ns} as the controller: {got}, want {want}"
        );
        println!("PASS can-i {verb} {resource} -n {ns}: {got}");
    }

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
    println!("helm-verify: all checks passed");
    Ok(())
}

/// `kubectl auth can-i` as the controller's service account: "yes" or "no".
fn can_i(env: &Env, verb: &str, resource: &str, ns: &str) -> Result<String> {
    // can-i exits 1 for "no", so this does not go through `Env::kubectl`.
    let out = crate::kube::kubectl(&env.kubeconfig)
        .args([
            "auth",
            "can-i",
            verb,
            resource,
            "-n",
            ns,
            "--as",
            CONTROLLER_SA,
        ])
        .arg("--kubeconfig")
        .arg(&env.kubeconfig)
        .output()
        .context("running kubectl auth can-i")?;
    let answer = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    ensure!(
        answer == "yes" || answer == "no",
        "kubectl auth can-i {verb} {resource} -n {ns}: {answer}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(answer)
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
