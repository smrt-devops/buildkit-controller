//! `cargo xtask helm-verify` installs the chart on a throwaway kind cluster
//! with `watchNamespaces` set to two pool namespaces and the `LoadBalancer`
//! exposure. It checks that the Service is a load balancer and the release has
//! no Gateway API routes, that both replicas are Ready, exactly one is the
//! leader, and `/readyz` returns 200 on every replica. It then checks that the
//! pool in each watched namespace is Ready with its warm pod and binds a claim,
//! that a pool in a third namespace is ignored, that the controller cannot
//! create pods or Secrets in that namespace or in kube-system, and that its logs
//! show no RBAC denial; then that the cache registry becomes Ready with the
//! certificate the leader writes, and its logs show no RBAC denial either. The controller namespace enforces `PodSecurity`
//! "restricted".
//!
//! The cluster name is `bk-helm-verify`, plus `-$BK_KIND_SUFFIX` when that is set.
//! Every kind, kubectl and helm call uses the private kubeconfig under
//! `target/helm-verify`. The task never touches `~/.kube/config`, and it
//! deletes the cluster on every exit path.

use std::time::Duration;

use anyhow::{Context, Result, ensure};

use crate::image::WORKER_IMAGE;
use crate::kube::{Cluster, shows_rbac_denial};
use crate::proc::until;
use crate::root;

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

pub(crate) fn helm_verify() -> Result<()> {
    let work = root().join("target/helm-verify");
    std::fs::create_dir_all(&work)?;
    let env = &Cluster::new(
        &super::kind_cluster("bk-helm-verify"),
        work.join("kubeconfig"),
    );
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
fn verify(env: &Cluster) -> Result<()> {
    let root = root();
    env.create(None, "120s")?;
    crate::image::image(CONTROLLER_IMAGE, Some(env.name()))?;
    crate::image::ensure_worker()?;
    crate::proc::status(&mut crate::kube::kind_load(WORKER_IMAGE, env.name()))?;

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
    env.helm(&[
        "install",
        "bk",
        &chart,
        "-n",
        NS,
        "-f",
        &values,
        "--timeout",
        "5m",
    ])?;
    println!("installed chart");

    // LoadBalancer exposure: the Service is the load balancer and no route is rendered.
    let svc_type = env.kubectl(&[
        "get",
        "svc",
        "buildkit-controller",
        "-n",
        NS,
        "-o",
        "jsonpath={.spec.type}",
    ])?;
    ensure!(
        svc_type == "LoadBalancer",
        "Service type {svc_type}, want LoadBalancer"
    );
    let manifest = env.helm(&["get", "manifest", "bk", "-n", NS])?;
    ensure!(
        !manifest.contains("gateway.networking.k8s.io"),
        "LoadBalancer mode rendered Gateway API objects:\n{manifest}"
    );
    println!("PASS LoadBalancer exposure: Service type LoadBalancer, no Gateway API objects");

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
    let work = root.join("target/helm-verify");
    let pool_values = crate::fixture("helm-verify/pool.yaml", &work, &[])?
        .to_string_lossy()
        .into_owned();
    env.helm(&[
        "upgrade",
        "bk",
        &chart,
        "-n",
        NS,
        "-f",
        &values,
        "-f",
        &pool_values,
    ])?;
    println!("applied pools after controller Ready");
    let stray = crate::fixture("helm-verify/stray-pool.yaml", &work, &[])?
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

    // The cache registry starts once the leader has written its certificate, and
    // reads pods and pools in the watched namespaces without a denial.
    env.kubectl(&[
        "rollout",
        "status",
        "statefulset/buildkit-controller-registry",
        "-n",
        NS,
        "--timeout=180s",
    ])?;
    let logs = env.kubectl(&[
        "logs",
        "-n",
        NS,
        "-l",
        "app.kubernetes.io/name=buildkit-registry",
        "--tail=-1",
    ])?;
    ensure!(
        !shows_rbac_denial(&logs),
        "cache registry logs show an RBAC denial"
    );
    println!("PASS cache registry Ready, no RBAC denial in its logs");
    println!("helm-verify: all checks passed");
    Ok(())
}

/// `kubectl auth can-i` as the controller's service account: "yes" or "no".
fn can_i(env: &Cluster, verb: &str, resource: &str, ns: &str) -> Result<String> {
    // can-i exits 1 for "no", so this does not go through `Cluster::kubectl`.
    let out = env
        .kubectl_raw(
            &[
                "auth",
                "can-i",
                verb,
                resource,
                "-n",
                ns,
                "--as",
                CONTROLLER_SA,
            ],
            None,
        )
        .context("running kubectl auth can-i")?;
    let answer = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    ensure!(
        answer == "yes" || answer == "no",
        "kubectl auth can-i {verb} {resource} -n {ns}: {answer}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(answer)
}
