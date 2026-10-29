//! `cargo xtask claim-verify` installs the chart on a throwaway kind cluster
//! with `claims.namespaces` set and one pool that admits claims from namespace
//! `claim-ok`, then checks the claim lifecycle end to end:
//!
//! 1. A claim in `claim-ok` binds and its Secret appears.
//! 2. A pod that mounts the Secret runs a real `buildctl build` through the edge.
//! 3. Deleting the claim releases the worker.
//! 4. A claim from `claim-denied`, served by the controller but not admitted
//!    by the pool, gets `Failed`, reason `PoolNotFound`. A claim from
//!    `claim-unlisted`, outside `claims.namespaces`, gets `NamespaceNotServed`.
//! 5. A 60 s claim expires and loses its Secret.
//! 6. Deleting a bound claim's worker pod makes the claim `Lost`.
//! 7. A claim with an invalid `cacheKey` is rejected at admission; c1 carries a valid one.
//! 8. With both workers bound, claims queue in order (`queuePosition`, the
//!    pool's `status.waiting`), one fails at its `queueTimeout`, the next binds
//!    once a worker frees; a tenant's written `owner` is ignored.
//!
//! The cluster name is `bk-claim-verify`, plus `-$BK_KIND_SUFFIX` when that is set.
//! Every call uses the private kubeconfig under `target/claim-verify`, and
//! the cluster is deleted on every exit path.

use std::time::Duration;

use anyhow::{Context, Result, ensure};

use crate::image::WORKER_IMAGE;
use crate::kube::Cluster;
use crate::proc::until;
use crate::root;

const NS: &str = "bk-claim";
const POOL_NS: &str = "bk-claim-pool";
const OK_NS: &str = "claim-ok";
const DENIED_NS: &str = "claim-denied";
const UNLISTED_NS: &str = "claim-unlisted";
const ANN_OWNER: &str = "buildkit.smrt-devops.net/owner";
const ANN_ALLOC: &str = "buildkit.smrt-devops.net/alloc";

pub(crate) fn claim_verify() -> Result<()> {
    let work = root().join("target/claim-verify");
    std::fs::create_dir_all(&work)?;
    let env = &Cluster::for_suite("bk-claim-verify", &work)?;
    let result = verify(env);
    if result.is_err() {
        for args in [
            vec!["get", "pods,bkc", "-A", "-o", "wide"],
            vec![
                "logs",
                "-n",
                NS,
                "-l",
                "app.kubernetes.io/name=buildkit-controller",
                "--tail=40",
            ],
            vec!["get", "bkc", "-A", "-o", "yaml"],
            vec!["logs", "-n", OK_NS, "build"],
        ] {
            if let Ok(o) = env.kubectl(&args) {
                eprintln!("--- kubectl {args:?}\n{o}");
            }
        }
    }
    result
}

fn claim_yaml(ns: &str, name: &str, ttl: &str, extra: &str) -> String {
    format!(
        "apiVersion: buildkit.smrt-devops.net/v1alpha1\nkind: BuildKitClaim\nmetadata: {{name: {name}, namespace: {ns}}}\nspec:\n  poolRef: {{namespace: {POOL_NS}, name: verify}}\n  ttl: {ttl}\n{extra}"
    )
}

fn apply(env: &Cluster, work: &std::path::Path, file: &str, yaml: &str) -> Result<String> {
    let path = work.join(file);
    std::fs::write(&path, yaml)?;
    env.kubectl(&["apply", "-f", &path.to_string_lossy()])
}

/// `phase reason` of a claim.
fn state(env: &Cluster, ns: &str, name: &str) -> Result<String> {
    env.kubectl(&[
        "get",
        "bkc",
        name,
        "-n",
        ns,
        "-o",
        "jsonpath={.status.phase} {.status.conditions[?(@.type==\"Bound\")].reason}",
    ])
}

fn wait_phase(
    env: &Cluster,
    ns: &str,
    name: &str,
    phase: &str,
    timeout: Duration,
) -> Result<String> {
    until(&format!("claim {ns}/{name} {phase}"), timeout, || {
        let s = state(env, ns, name)?;
        Ok(s.starts_with(phase).then_some(s))
    })
}

fn secret_exists(env: &Cluster, ns: &str, claim: &str) -> Result<bool> {
    let out = env.kubectl(&["get", "secrets", "-n", ns, "-o", "name"])?;
    Ok(out.lines().any(|l| l == format!("secret/{claim}-buildkit")))
}

/// Pool pods as `name owner alloc phase` lines.
fn workers(env: &Cluster) -> Result<String> {
    let esc = |k: &str| k.replace('.', "\\.");
    env.kubectl(&[
        "get", "pods", "-n", POOL_NS, "-o",
        &format!(
            "jsonpath={{range .items[*]}}{{.metadata.name}} {{.metadata.annotations.{}}} {{.metadata.annotations.{}}} {{.status.phase}}{{\"\\n\"}}{{end}}",
            esc(ANN_OWNER),
            esc(ANN_ALLOC)
        ),
    ])
}

#[allow(clippy::too_many_lines, reason = "one linear scenario script")]
fn verify(env: &Cluster) -> Result<()> {
    let root = root();
    let work = root.join("target/claim-verify");
    env.create(None, "120s")?;
    crate::image::load_controller(env)?;
    crate::image::ensure_worker()?;
    crate::proc::status(&mut crate::kube::kind_load(WORKER_IMAGE, env.name()))?;
    for ns in [NS, POOL_NS, OK_NS, DENIED_NS, UNLISTED_NS] {
        env.kubectl(&["create", "namespace", ns])?;
    }
    let chart = root
        .join("helm/buildkit-controller")
        .to_string_lossy()
        .into_owned();
    let values = crate::fixture("claim-verify/values.yaml", &work, &[])?
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
    until("pool Ready with a warm pod", Duration::from_mins(5), || {
        let s = env.kubectl(&[
            "get",
            "buildkitpool",
            "verify",
            "-n",
            POOL_NS,
            "-o",
            "jsonpath={.status.conditions[?(@.type==\"Ready\")].status} warm={.status.warm}",
        ])?;
        Ok(s.starts_with("True warm=1").then_some(()))
    })?;
    // The build reaches the edge through the controller's Service; the build
    // pod waits for the Service itself (see check 2).
    env.kubectl(&[
        "rollout",
        "status",
        "deploy/bkc",
        "-n",
        NS,
        "--timeout=300s",
    ])?;
    println!("installed chart with claims.namespaces; pool Ready, controller rolled out");

    // 7. A cacheKey that starts with a hyphen is rejected at admission.
    let path = work.join("cache-key.yaml");
    std::fs::write(
        &path,
        claim_yaml(OK_NS, "keyed", "10m", "  cacheKey: \"-bad\"\n"),
    )?;
    let out = env.kubectl_raw(&["apply", "-f", &path.to_string_lossy()], None)?;
    let err = String::from_utf8_lossy(&out.stderr);
    ensure!(
        !out.status.success() && err.contains("cacheKey"),
        "bad cacheKey claim was not rejected: {err}"
    );
    println!("PASS invalid cacheKey rejected at admission");

    // 1. Bind and Secret.
    apply(
        env,
        &work,
        "c1.yaml",
        &claim_yaml(OK_NS, "c1", "10m", "  cacheKey: main\n"),
    )?;
    let s = wait_phase(env, OK_NS, "c1", "Bound", Duration::from_mins(3))?;
    ensure!(
        secret_exists(env, OK_NS, "c1")?,
        "Secret c1-buildkit missing"
    );
    println!("PASS claim c1 {s}; Secret c1-buildkit present");

    // 2. A real build through the edge with the Secret as written.
    let build = format!(
        r#"apiVersion: v1
kind: Pod
metadata: {{name: build, namespace: {OK_NS}}}
spec:
  restartPolicy: Never
  containers:
  - name: build
    image: {WORKER_IMAGE}
    imagePullPolicy: Never
    command: [sh, -c]
    args:
    - |
      set -e
      mkdir -p /tmp/w && cd /tmp/w
      echo hello > hello.txt
      printf 'FROM scratch\nCOPY hello.txt /hello.txt\n' > Dockerfile
      bk() {{
        buildctl --addr "$BUILDKIT_HOST" --tlsservername "$BUILDKIT_SERVER_NAME" \
          --tlscacert /certs/ca.crt --tlscert /certs/tls.crt --tlskey /certs/tls.key "$@"
      }}
      # kube-proxy can program a new Service seconds after its endpoints are
      # Ready, longer on a cluster whose API server was just restarted (as
      # runtime-verify does before this suite in kind-all). Until then a
      # connect is refused, so wait for the edge to answer first.
      i=0
      until bk debug workers >/dev/null 2>&1; do
        i=$((i + 1)); [ "$i" -lt 120 ] || exit 1; sleep 1
      done
      bk build --frontend dockerfile.v0 --local context=. --local dockerfile=. \
        --output type=local,dest=/tmp/out
      test "$(cat /tmp/out/hello.txt)" = hello && echo BUILD-OK
    env:
    - name: BUILDKIT_HOST
      valueFrom: {{secretKeyRef: {{name: c1-buildkit, key: endpoint}}}}
    - name: BUILDKIT_SERVER_NAME
      valueFrom: {{secretKeyRef: {{name: c1-buildkit, key: server-name}}}}
    volumeMounts:
    - {{name: certs, mountPath: /certs, readOnly: true}}
  volumes:
  - name: certs
    secret: {{secretName: c1-buildkit}}
"#
    );
    apply(env, &work, "build.yaml", &build)?;
    let phase = until("build pod finished", Duration::from_mins(4), || {
        let p = env.kubectl(&[
            "get",
            "pod",
            "build",
            "-n",
            OK_NS,
            "-o",
            "jsonpath={.status.phase}",
        ])?;
        Ok((p == "Succeeded" || p == "Failed").then_some(p))
    })?;
    let logs = env.kubectl(&["logs", "build", "-n", OK_NS])?;
    ensure!(
        phase == "Succeeded" && logs.contains("BUILD-OK"),
        "build {phase}: {logs}"
    );
    println!("PASS buildctl build through the edge with the claim Secret");

    // 3. Delete releases the worker.
    let owner = format!("claim:{OK_NS}/c1#");
    ensure!(workers(env)?.contains(&owner), "no worker owned by c1");
    env.kubectl(&["delete", "bkc", "c1", "-n", OK_NS])?;
    until("c1 worker released", Duration::from_mins(2), || {
        Ok((!workers(env)?.contains(&owner)).then_some(()))
    })?;
    println!("PASS deleting c1 released its worker");

    // 4. A namespace outside the rule is refused.
    apply(
        env,
        &work,
        "denied.yaml",
        &claim_yaml(DENIED_NS, "d1", "10m", ""),
    )?;
    let s = wait_phase(env, DENIED_NS, "d1", "Failed", Duration::from_mins(1))?;
    ensure!(s.ends_with("PoolNotFound"), "denied claim: {s}");
    ensure!(
        !secret_exists(env, DENIED_NS, "d1")?,
        "denied claim got a Secret"
    );
    println!("PASS claim from {DENIED_NS} refused: {s}");
    apply(
        env,
        &work,
        "unlisted.yaml",
        &claim_yaml(UNLISTED_NS, "u1", "10m", ""),
    )?;
    let s = wait_phase(env, UNLISTED_NS, "u1", "Failed", Duration::from_mins(1))?;
    ensure!(s.ends_with("NamespaceNotServed"), "unlisted claim: {s}");
    println!("PASS claim from {UNLISTED_NS} refused: {s}");

    // 5. Expiry.
    apply(env, &work, "c2.yaml", &claim_yaml(OK_NS, "c2", "60s", ""))?;
    wait_phase(env, OK_NS, "c2", "Bound", Duration::from_mins(3))?;
    let s = wait_phase(env, OK_NS, "c2", "Expired", Duration::from_mins(2))?;
    ensure!(
        !secret_exists(env, OK_NS, "c2")?,
        "expired claim kept its Secret"
    );
    println!("PASS claim c2 {s}; Secret deleted");

    // 6. Deleting the worker pod makes the claim Lost.
    apply(env, &work, "c3.yaml", &claim_yaml(OK_NS, "c3", "10m", ""))?;
    wait_phase(env, OK_NS, "c3", "Bound", Duration::from_mins(3))?;
    let id = env.kubectl(&[
        "get",
        "bkc",
        "c3",
        "-n",
        OK_NS,
        "-o",
        "jsonpath={.status.allocationId}",
    ])?;
    let pod = workers(env)?
        .lines()
        .find(|l| l.split(' ').nth(2) == Some(id.as_str()))
        .and_then(|l| l.split(' ').next().map(str::to_owned))
        .with_context(|| format!("no worker with alloc {id}"))?;
    env.kubectl(&["delete", "pod", &pod, "-n", POOL_NS, "--wait=false"])?;
    let s = wait_phase(env, OK_NS, "c3", "Lost", Duration::from_mins(2))?;
    ensure!(s.ends_with("WorkerLost"), "lost claim: {s}");
    ensure!(
        !secret_exists(env, OK_NS, "c3")?,
        "lost claim kept its Secret"
    );
    std::thread::sleep(Duration::from_secs(10));
    let s = state(env, OK_NS, "c3")?;
    ensure!(s.starts_with("Lost"), "lost claim was rebound: {s}");
    println!("PASS deleting worker {pod} made c3 Lost (WorkerLost), Secret deleted, no rebind");
    queue(env, &work)?;
    println!("claim-verify: all checks passed");
    Ok(())
}

/// Check 8, the queue: with both workers bound, later claims wait in order with
/// their place and the pool's exact `waiting`; one times out, the next binds
/// once a worker frees. A tenant's written `owner` is ignored.
fn queue(env: &Cluster, work: &std::path::Path) -> Result<()> {
    for n in ["c1", "c2", "c3"] {
        env.kubectl(&["delete", "bkc", n, "-n", OK_NS, "--ignore-not-found"])?;
    }
    let forged = "  owner: \"oidc:https://token.actions.githubusercontent.com#repo:x\"\n";
    for n in ["q1", "q2"] {
        apply(
            env,
            work,
            &format!("{n}.yaml"),
            &claim_yaml(OK_NS, n, "10m", forged),
        )?;
        wait_phase(env, OK_NS, n, "Bound", Duration::from_mins(3))?;
    }
    let owners = workers(env)?;
    ensure!(
        owners.contains(&format!("claim:{OK_NS}/q1#")) && !owners.contains("oidc:"),
        "a tenant's written owner was honoured: {owners}"
    );
    println!("PASS a written owner outside the controller namespace is ignored");
    apply(
        env,
        work,
        "q3.yaml",
        &claim_yaml(OK_NS, "q3", "10m", "  queueTimeout: 15s\n"),
    )?;
    std::thread::sleep(Duration::from_secs(1));
    apply(
        env,
        work,
        "q4.yaml",
        &claim_yaml(OK_NS, "q4", "10m", "  queueTimeout: 4m\n"),
    )?;
    let position = |n: &str| {
        env.kubectl(&[
            "get",
            "bkc",
            n,
            "-n",
            OK_NS,
            "-o",
            "jsonpath={.status.phase} {.status.queuePosition}",
        ])
    };
    until(
        "q3 and q4 waiting at places 1 and 2",
        Duration::from_mins(1),
        || Ok((position("q3")? == "Pending 1" && position("q4")? == "Pending 2").then_some(())),
    )?;
    let waiting = until("pool waiting 2", Duration::from_secs(30), || {
        let w = env.kubectl(&[
            "get",
            "buildkitpool",
            "verify",
            "-n",
            POOL_NS,
            "-o",
            "jsonpath={.status.waiting}",
        ])?;
        Ok((w == "2").then_some(w))
    })?;
    println!("PASS q3, q4 Pending at places 1, 2; pool status.waiting {waiting}");
    let s = wait_phase(env, OK_NS, "q3", "Failed", Duration::from_mins(1))?;
    ensure!(s.ends_with("QueueTimeout"), "q3: {s}");
    until("q4 moves up", Duration::from_secs(30), || {
        Ok((position("q4")? == "Pending 1").then_some(()))
    })?;
    println!("PASS q3 {s} at its 15s queue timeout; q4 now at place 1");
    env.kubectl(&["delete", "bkc", "q1", "-n", OK_NS])?;
    let s = wait_phase(env, OK_NS, "q4", "Bound", Duration::from_mins(3))?;
    println!("PASS deleting q1 freed a worker; q4 {s}");
    Ok(())
}
