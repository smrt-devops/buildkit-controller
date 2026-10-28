//! `cargo xtask edge-verify` tests the mTLS edge in a throwaway kind cluster.
//! The controller runs in-cluster as a Deployment, the task allocates a
//! hand-rendered worker pod to a test identity, and a `buildctl` client pod
//! dials the edge Service with allocation certs issued by the PKI. The
//! scenarios are:
//!
//! a. `debug workers` and a small build with a `RUN` succeed through the edge.
//! b. The edge refuses the allocation cert when dialled straight at the worker
//!    pod IP.
//! c. The edge refuses an expired cert and another allocation's cert, with the
//!    TLS alert and the `buildkit_edge_connections_total` result.
//! d. Deleting the pod cuts a live session, and the task measures the latency.
//! e. A 512 MiB COPY through the edge, compared with one direct to buildkitd,
//!    in MB/s.
//!
//! The controller's RBAC is the chart's (`templates/rbac.yaml`, rendered with
//! `helm template`), so the "no 403s" check covers the RBAC that ships. Every
//! kind and kubectl call gets the private kubeconfig under `target/edge-verify`.
//! The task never reads or writes `~/.kube/config`, and it deletes the cluster
//! on every exit path.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use buildkit_controller::pki::{AllocationId, CaBundle, LeafKey, WorkerIdentity};
use buildkit_controller::render::fixtures::pool;
use buildkit_controller::render::{
    BUILDKIT_PORT, Claim, PodInputs, WorkerTlsPem, service_account, worker_pod, worker_secret,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{CRD_PATH, root};
use crate::image::WORKER_IMAGE;
use crate::kube::Cluster;
use crate::proc::{Spawned, status};
const SYSTEM_NS: &str = "buildkit-system";
const NS: &str = "bk-edge";
const POOL: &str = "edge";
/// The worker pod's nonce; the edge dials `<NONCE>.<pod>.<pool>.<ns>`.
const NONCE: &str = "0123456789abcdef";
const CONTROLLER_IMAGE: &str = "buildkit-controller:dev";
const ALLOC: &str = "mfrggzdfmztwq2lknnwg23tpoa";
const OTHER_ALLOC: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";
/// 512 MiB.
const BIG: u64 = 512 * 1024 * 1024;
/// Bound for scenario d.
const REVOKE_BOUND: Duration = Duration::from_secs(10);
const EDGE_ADDR: &str = "tcp://buildkit-controller.buildkit-system.svc:8444";
const METRICS: &str = "http://buildkit-controller.buildkit-system.svc:8080/metrics";

/// A `buildctl` client pod (`PodSecurity` "restricted"). The pod in the pool
/// namespace dials the edge. The pod in the controller namespace carries the
/// controller's name label, which the worker `NetworkPolicy` admits, so it can
/// reach buildkitd directly (scenario b and the direct throughput baseline).
fn client_pod(ns: &str, labels: &str) -> String {
    format!(
        "\
apiVersion: v1
kind: Pod
metadata: {{name: client, namespace: {ns}, labels: {{{labels}}}}}
spec:
  securityContext:
    runAsNonRoot: true
    runAsUser: 1000
    seccompProfile: {{type: RuntimeDefault}}
  containers:
    - name: client
      image: {WORKER_IMAGE}
      imagePullPolicy: Never
      command: [sleep, infinity]
      securityContext:
        allowPrivilegeEscalation: false
        capabilities: {{drop: [ALL]}}
      volumeMounts:
        - {{name: certs, mountPath: /certs}}
        - {{name: work, mountPath: /work}}
  volumes:
    - name: certs
      secret: {{secretName: client-certs}}
    - name: work
      emptyDir: {{}}
"
    )
}

/// Copies busybox and the musl loader into small/ and long/ and writes their
/// Dockerfiles (`FROM scratch`, so nothing is pulled). Also writes two 512 MiB
/// random contexts in separate directories so neither run reuses the other's
/// transfer.
const PREPARE: &str = r#"set -e
for d in small long; do
  mkdir -p /work/$d/bin /work/$d/lib
  cp /bin/busybox /work/$d/bin/
  cp -L /lib/ld-musl-*.so.1 /lib/libc.musl-*.so.1 /work/$d/lib/
  printf 'FROM scratch\nCOPY lib/ /lib/\nCOPY bin/ /bin/\n' > /work/$d/Dockerfile
done
echo 'RUN ["/bin/busybox", "sh", "-c", "/bin/busybox echo built-through-edge > /ok; /bin/busybox cat /ok"]' >> /work/small/Dockerfile
echo 'RUN ["/bin/busybox", "sleep", "600"]' >> /work/long/Dockerfile
for d in big-edge big-direct; do
  mkdir -p /work/$d
  head -c 536870912 /dev/urandom > /work/$d/big
  printf 'FROM scratch\nCOPY big /big\n' > /work/$d/Dockerfile
done
"#;

/// The controller's `ServiceAccount` and RBAC, rendered from the chart for the
/// release `bk` in [`SYSTEM_NS`]. `tls_secret` is the chart's
/// `edge.tls.secretName`, which the RBAC lets the controller read.
pub(crate) fn chart_rbac(tls_secret: &str) -> Result<String> {
    let chart = root().join("helm/buildkit-controller");
    let rbac = crate::proc::stdout(
        crate::kube::helm(&crate::kube::sandbox())
            .args(["template", "bk"])
            .arg(&chart)
            .args(["-n", SYSTEM_NS])
            .args(["--show-only", "templates/serviceaccount.yaml"])
            .args(["--show-only", "templates/rbac.yaml"])
            .args(["--set", "fullnameOverride=buildkit-controller"])
            .args(["--set", "gateway.enabled=false"])
            .args(["--set", "edge.endpoint=tcp://bk.example.com:8444"])
            .args(["--set", "edge.serverName=bk.example.com"])
            .args(["--set-string", &format!("edge.tls.secretName={tls_secret}")]),
    )
    .context("rendering the chart's RBAC")?;
    Ok(rbac + "\n---\n")
}

/// The in-cluster controller of `xtask/fixtures/edge-verify`, without its
/// `ServiceAccount` and RBAC; apply [`chart_rbac`] with it.
pub(crate) fn controller_fixture() -> Result<String> {
    std::fs::read_to_string(root().join("xtask/fixtures/edge-verify/controller.yaml"))
        .context("reading the controller fixture")
}

/// Runs `script` in the client pod and returns (success, stdout+stderr).
fn client(env: &Cluster, script: &str) -> Result<(bool, String)> {
    client_in(env, NS, script)
}

/// Runs `script` in the client pod of namespace `ns`.
fn client_in(env: &Cluster, ns: &str, script: &str) -> Result<(bool, String)> {
    let out = env.kubectl_raw(
        &["-n", ns, "exec", "client", "--", "sh", "-c", script],
        None,
    )?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok((out.status.success(), text))
}

/// Sum of `name` samples whose labels contain `label`.
fn metric(env: &Cluster, name: &str, label: &str) -> Result<f64> {
    let (ok, text) = client(env, &format!("wget -qO- {METRICS}"))?;
    ensure!(ok, "metrics fetch failed: {text}");
    Ok(text
        .lines()
        .filter(|l| l.starts_with(&format!("{name}{{")) && l.contains(label))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .fold(0.0, |a, b| a + b))
}

/// Dumps controller state when dropped unless the run succeeded. The cluster
/// it holds is deleted after that.
struct Cleanup(Cluster, std::cell::Cell<bool>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        if !self.1.get() {
            for args in [
                &["get", "pods", "-A", "-o", "wide"][..],
                &["-n", SYSTEM_NS, "describe", "pods"][..],
                &[
                    "-n",
                    SYSTEM_NS,
                    "logs",
                    "deploy/buildkit-controller",
                    "--tail=60",
                ][..],
            ] {
                if let Ok(o) = self.0.kubectl_raw(args, None) {
                    println!(
                        "{}{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    );
                }
            }
        }
    }
}

fn rfc3339(t: OffsetDateTime) -> Result<String> {
    t.format(&Rfc3339).context("formatting time")
}

fn yaml<T: serde::Serialize>(o: &T) -> Result<String> {
    serde_saphyr::to_string(o).context("serialising")
}

/// buildctl global flags for one endpoint and cert pair under /certs.
fn flags(addr: &str, server_name: &str, ca: &str, cert: &str) -> String {
    format!(
        "buildctl --addr {addr} --tlsservername {server_name} --tlscacert /certs/{ca} \
         --tlscert /certs/{cert}.crt --tlskey /certs/{cert}.key"
    )
}

fn build(flags: &str, dir: &str) -> String {
    format!(
        "{flags} build --progress=plain --no-cache --frontend dockerfile.v0 \
         --local context=/work/{dir} --local dockerfile=/work/{dir}"
    )
}

struct Report {
    lines: Vec<String>,
    failed: usize,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool, detail: &str) {
        let verdict = if ok { "PASS" } else { "FAIL" };
        let line = format!("{verdict} {name}: {detail}");
        println!("{line}");
        self.lines.push(line);
        if !ok {
            self.failed += 1;
        }
    }
}

/// The TLS error line if there is one, otherwise the last non-empty line.
fn last_line(s: &str) -> &str {
    let mut lines = s.lines().rev();
    lines
        .clone()
        .find(|l| l.contains("tls:"))
        .or_else(|| lines.find(|l| !l.trim().is_empty()))
        .unwrap_or_default()
        .trim()
}

pub(crate) fn edge_verify() -> Result<()> {
    let root = root();
    let work = root.join("target/edge-verify");
    std::fs::create_dir_all(&work)?;
    let cleanup = Cleanup(
        Cluster::new(
            &super::kind_cluster("bk-edge-verify"),
            work.join("kubeconfig"),
        ),
        std::cell::Cell::new(false),
    );
    let env = &cleanup.0;
    env.create(None, "120s")?;

    super::image::image(CONTROLLER_IMAGE, Some(env.name()))?;
    crate::image::ensure_worker()?;
    status(&mut crate::kube::kind_load(WORKER_IMAGE, env.name()))?;

    let crd = root.join(CRD_PATH).to_string_lossy().into_owned();
    env.kubectl(&["apply", "--server-side", "-f", &crd])?;
    env.wait_crd_established()?;
    env.kubectl(&["create", "namespace", SYSTEM_NS])?;
    // The controller pod must pass PodSecurity "restricted".
    env.kubectl(&[
        "label",
        "namespace",
        SYSTEM_NS,
        "pod-security.kubernetes.io/enforce=restricted",
    ])?;
    env.kubectl(&["create", "namespace", NS])?;

    let now = OffsetDateTime::now_utc().replace_nanosecond(0)?;
    let ca = CaBundle::generate(now)?;
    secret_from_files(
        env,
        SYSTEM_NS,
        "buildkit-controller-ca",
        &work.join("ca"),
        ca.to_secret_data(),
    )?;
    env.apply(&(chart_rbac("")? + &controller_fixture()?))?;

    let pod_ip = deploy(env, &work, &ca, now)?;

    let mut r = Report {
        lines: Vec::new(),
        failed: 0,
    };
    let rates = scenarios(env, &pod_ip, &mut r)?;
    revocation(env, &mut r)?;
    controller_health(env, &mut r)?;

    std::fs::write(work.join("result.txt"), r.lines.join("\n") + "\n")?;
    println!(
        "edge-verify: {} passed, {} failed (edge {:.1} MB/s, direct {:.1} MB/s)",
        r.lines.len() - r.failed,
        r.failed,
        rates.first().copied().unwrap_or_default(),
        rates.get(1).copied().unwrap_or_default()
    );
    if r.failed > 0 {
        let logs = env.kubectl(&[
            "-n",
            SYSTEM_NS,
            "logs",
            "deploy/buildkit-controller",
            "--tail=40",
        ]);
        println!("{}", logs.unwrap_or_else(|e| e.to_string()));
        bail!("{} edge-verify scenarios failed", r.failed);
    }
    cleanup.1.set(true);
    Ok(())
}

/// Creates the pool, the allocated worker pod, the client certs and the
/// client pod. Returns the worker pod IP.
fn deploy(env: &Cluster, work: &Path, ca: &CaBundle, now: OffsetDateTime) -> Result<String> {
    // The pool must exist because the edge checks the pod's ownerReference uid.
    env.apply(&format!(
        "apiVersion: buildkit.smrt-devops.net/v1alpha1\nkind: BuildKitPool\n\
         metadata: {{name: {POOL}, namespace: {NS}}}\nspec: {{}}\n"
    ))?;
    let uid = env.kubectl(&[
        "-n",
        NS,
        "get",
        "buildkitpool",
        POOL,
        "-o",
        "jsonpath={.metadata.uid}",
    ])?;
    let mut p = pool(POOL, serde_json::json!({}))?;
    p.metadata.namespace = Some(NS.into());
    p.metadata.uid = Some(uid);

    let serving = ca.issue_worker_serving(&worker_identity()?, now)?;
    let tls = WorkerTlsPem::from_pki(&serving, ca.internal_ca())?;
    let expires = now + Duration::from_hours(2);
    let inputs = PodInputs {
        slot: 0,
        internal_ca_fingerprint: ca.internal_ca().fingerprint_hex(),
        worker_cert_not_after: rfc3339(serving_not_after(now))?,
        nonce: NONCE.into(),
        claim: Some(Claim {
            alloc_id: ALLOC.into(),
            owner: "sa:bk-edge/edge-verify".into(),
            allocated_at: rfc3339(now)?,
            expires_at: rfc3339(expires)?,
        }),
    };
    env.apply(&yaml(&service_account(&p)?)?)?;
    env.apply(&yaml(&worker_secret(&p, &inputs, &tls)?)?)?;
    env.apply(&yaml(&worker_pod(&p, &inputs)?)?)?;
    let worker = format!("{POOL}-0");
    env.kubectl(&[
        "-n",
        NS,
        "wait",
        "--for=condition=Ready",
        &format!("pod/{worker}"),
        "--timeout=180s",
    ])?;
    let pod_uid = env.kubectl(&[
        "-n",
        NS,
        "get",
        "pod",
        &worker,
        "-o",
        "jsonpath={.metadata.uid}",
    ])?;
    let pod_ip = env.kubectl(&[
        "-n",
        NS,
        "get",
        "pod",
        &worker,
        "-o",
        "jsonpath={.status.podIP}",
    ])?;

    client_certs(env, work, ca, now, expires, &pod_uid)?;
    env.apply(&client_pod(NS, "app: edge-verify-client"))?;
    env.apply(&client_pod(
        SYSTEM_NS,
        "app.kubernetes.io/name: buildkit-controller",
    ))?;
    env.kubectl(&[
        "-n",
        SYSTEM_NS,
        "rollout",
        "status",
        "deploy/buildkit-controller",
        "--timeout=120s",
    ])?;
    for ns in [NS, SYSTEM_NS] {
        env.kubectl(&[
            "-n",
            ns,
            "wait",
            "--for=condition=Ready",
            "pod/client",
            "--timeout=120s",
        ])?;
        let (ok, out) = client_in(env, ns, PREPARE)?;
        ensure!(ok, "preparing build contexts in {ns}: {out}");
    }
    Ok(pod_ip)
}

fn edge_flags(cert: &str) -> String {
    flags(EDGE_ADDR, "bk.example.com", "client-ca.crt", cert)
}

/// Runs scenarios a, b, c and e. Returns the edge and direct throughput in MB/s.
fn scenarios(env: &Cluster, pod_ip: &str, r: &mut Report) -> Result<Vec<f64>> {
    let edge = edge_flags;
    let direct_addr = format!("tcp://{pod_ip}:{BUILDKIT_PORT}");
    let worker_name = worker_identity()?.dns_name();
    let direct = |cert: &str| flags(&direct_addr, &worker_name, "internal-ca.crt", cert);

    // a.
    let (ok, out) = client(env, &format!("{} debug workers", edge("alloc")))?;
    r.check("a/debug-workers via edge", ok, last_line(&out));
    let (ok, out) = client(env, &build(&edge("alloc"), "small"))?;
    let ran = out.contains("built-through-edge");
    r.check("a/build with RUN via edge", ok && ran, last_line(&out));

    // b.
    let (ok, out) = client_in(
        env,
        SYSTEM_NS,
        &format!("timeout 30 {} debug workers", direct("alloc")),
    )?;
    // A TLS alert from buildkitd, not a timeout or a refused connect.
    r.check(
        "b/alloc cert direct to worker refused",
        !ok && out.contains("remote error: tls:"),
        last_line(&out),
    );

    // c.
    let tls_before = metric(
        env,
        "buildkit_edge_connections_total",
        "result=\"tls_error\"",
    )?;
    let (ok, out) = client(
        env,
        &format!("timeout 30 {} debug workers", edge("expired")),
    )?;
    println!("{out}");
    let tls_after = metric(
        env,
        "buildkit_edge_connections_total",
        "result=\"tls_error\"",
    )?;
    r.check(
        "c/expired cert refused at edge",
        !ok && tls_after > tls_before,
        &format!(
            "tls_error {tls_before}->{tls_after}; client saw alert: {}; {}",
            out.contains("expired certificate"),
            last_line(&out)
        ),
    );
    let mm_before = metric(
        env,
        "buildkit_edge_connections_total",
        "result=\"alloc_mismatch\"",
    )?;
    let (ok, out) = client(env, &format!("timeout 30 {} debug workers", edge("other")))?;
    println!("{out}");
    let mismatch = metric(
        env,
        "buildkit_edge_connections_total",
        "result=\"alloc_mismatch\"",
    )?;
    r.check(
        "c/other allocation's cert refused at edge",
        !ok && mismatch > mm_before,
        &format!(
            "alloc_mismatch {mm_before}->{mismatch}; client saw alert: {}; {}",
            out.contains("revoked certificate"),
            last_line(&out)
        ),
    );

    // e. Runs before d, which deletes the worker.
    let mut rates = Vec::new();
    for (name, ns, fl, dir) in [
        ("edge", NS, edge("alloc"), "big-edge"),
        ("direct", SYSTEM_NS, direct("direct"), "big-direct"),
    ] {
        let bytes = "direction=\"client_to_worker\"";
        let before = metric(env, "buildkit_edge_bytes_total", bytes)?;
        let start = Instant::now();
        let (ok, out) = client_in(env, ns, &build(&fl, dir))?;
        let secs = start.elapsed().as_secs_f64();
        let proxied = metric(env, "buildkit_edge_bytes_total", bytes)? - before;
        // The edge run must carry the whole file; the direct one none of it.
        #[allow(clippy::cast_precision_loss)]
        let carried = if name == "edge" {
            proxied >= BIG as f64
        } else {
            proxied < 1e6
        };
        #[allow(clippy::cast_precision_loss)]
        let mbps = BIG as f64 / 1e6 / secs;
        rates.push(mbps);
        r.check(
            &format!("e/512 MiB COPY {name}"),
            ok && carried,
            &format!("{secs:.1} s, {mbps:.1} MB/s, {proxied:.0} bytes via edge"),
        );
        if !ok {
            println!("{out}");
        }
    }

    Ok(rates)
}

/// Scenario d. Deletes the worker under a live build through the edge and
/// measures how long the client session survives.
fn revocation(env: &Cluster, r: &mut Report) -> Result<()> {
    let worker = format!("{POOL}-0");
    let edge = edge_flags;
    let mut child = Spawned::spawn(
        env.kubectl_cmd()
            .args([
                "-n",
                NS,
                "exec",
                "client",
                "--",
                "sh",
                "-c",
                &build(&edge("alloc"), "long"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )
    .context("starting the long build")?;
    // The session is live once the edge counts its connection.
    let active = crate::proc::poll(
        "the long build's edge connection",
        Duration::from_mins(1),
        Duration::from_millis(500),
        || {
            ensure!(child.try_wait()?.is_none(), "the long build exited early");
            let n = metric(env, "buildkit_edge_active_connections", "pool=\"edge\"")?;
            Ok((n >= 1.0).then_some(n))
        },
    )?;
    let gone_before = metric(env, "buildkit_edge_revocations_total", "pod_gone")?;
    let live = child.try_wait()?.is_none();
    let start = Instant::now();
    env.kubectl(&["-n", NS, "delete", "pod", &worker, "--wait=false"])?;
    let mut cut = None;
    while start.elapsed() < Duration::from_mins(1) {
        if child.try_wait()?.is_some() {
            cut = Some(start.elapsed());
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.kill();
    let gone_after = metric(env, "buildkit_edge_revocations_total", "pod_gone")?;
    r.check(
        "d/pod delete cuts the live session",
        live && active >= 1.0 && cut.is_some_and(|d| d <= REVOKE_BOUND) && gone_after > gone_before,
        &format!(
            "live={live} active={active} cut after {} pod_gone {gone_before}->{gone_after}",
            cut.map_or_else(
                || "never".to_owned(),
                |d| format!("{:.2} s", d.as_secs_f64())
            )
        ),
    );
    Ok(())
}

/// The worker serving cert lives 30 days (see pki/leaf.rs).
fn serving_not_after(now: OffsetDateTime) -> OffsetDateTime {
    now + Duration::from_hours(30 * 24)
}

/// Writes `data` to `dir` and creates a generic Secret from those files.
pub(crate) fn secret_from_files(
    env: &Cluster,
    ns: &str,
    name: &str,
    dir: &Path,
    data: std::collections::BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut args = vec![
        "-n".to_owned(),
        ns.to_owned(),
        "create".into(),
        "secret".into(),
        "generic".into(),
        name.into(),
    ];
    for (k, v) in data {
        let path = dir.join(&k);
        std::fs::write(&path, v)?;
        args.push(format!("--from-file={k}={}", path.display()));
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    env.kubectl(&args).map(drop)
}

/// Client certs for the allocation, an expired one, another allocation's, and
/// the internal-CA edge client cert for the direct baseline.
fn client_certs(
    env: &Cluster,
    work: &Path,
    ca: &CaBundle,
    now: OffsetDateTime,
    expires: OffsetDateTime,
    pod_uid: &str,
) -> Result<()> {
    let id = |alloc: &str| -> Result<AllocationId> {
        Ok(AllocationId {
            namespace: NS.parse()?,
            pool: POOL.parse()?,
            pod_uid: pod_uid.parse()?,
            alloc_id: alloc.parse()?,
        })
    };
    let hour = Duration::from_hours(1);
    let certs = [
        (
            "alloc",
            ca.issue_allocation(&id(ALLOC)?, LeafKey::Generate, now, expires)?,
        ),
        (
            "expired",
            ca.issue_allocation(&id(ALLOC)?, LeafKey::Generate, now - 2 * hour, now - hour)?,
        ),
        (
            "other",
            ca.issue_allocation(&id(OTHER_ALLOC)?, LeafKey::Generate, now, expires)?,
        ),
        ("direct", ca.issue_edge_client(now)?),
    ];
    let mut files = std::collections::BTreeMap::new();
    files.insert(
        "client-ca.crt".to_owned(),
        ca.client_ca().cert_pem().as_bytes().to_vec(),
    );
    files.insert(
        "internal-ca.crt".to_owned(),
        ca.internal_ca().cert_pem().as_bytes().to_vec(),
    );
    for (name, c) in &certs {
        files.insert(format!("{name}.crt"), c.cert_pem().as_bytes().to_vec());
        let key = c.private_key_pem().context("generated key missing")?;
        files.insert(format!("{name}.key"), key.into_bytes());
    }
    for ns in [NS, SYSTEM_NS] {
        secret_from_files(env, ns, "client-certs", &work.join(ns), files.clone())?;
    }
    Ok(())
}

/// The controller ran the whole time without restarts and its log has no
/// RBAC refusals. Any denial means the chart's RBAC lacks a grant.
fn controller_health(env: &Cluster, r: &mut Report) -> Result<()> {
    let log = env.kubectl(&["-n", SYSTEM_NS, "logs", "deploy/buildkit-controller"])?;
    let forbidden: Vec<&str> = crate::kube::rbac_denials(&log).collect();
    let restarts = env.kubectl(&[
        "-n",
        SYSTEM_NS,
        "get",
        "pods",
        "-l",
        "app=buildkit-controller",
        "-o",
        "jsonpath={.items[*].status.containerStatuses[*].restartCount}",
    ])?;
    for l in forbidden.iter().take(5) {
        println!("{l}");
    }
    r.check(
        "controller/no 403s, no restarts (restricted PodSecurity, read-only root fs)",
        forbidden.is_empty() && restarts.trim() == "0",
        &format!(
            "{} forbidden log lines of {}, restarts={}",
            forbidden.len(),
            log.lines().count(),
            restarts.trim()
        ),
    );
    Ok(())
}

/// The identity of the one worker pod `deploy` renders, `<POOL>-0`.
fn worker_identity() -> Result<WorkerIdentity> {
    Ok(WorkerIdentity {
        namespace: NS.parse()?,
        pool: POOL.parse()?,
        pod: format!("{POOL}-0"),
        nonce: NONCE.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The controller's RBAC comes from the chart only; a hand-written copy
    /// here would be what edge-verify, soak and perf test instead.
    #[test]
    fn controller_fixture_has_no_rbac_of_its_own() {
        let fixture = controller_fixture().unwrap();
        for kind in [
            "ServiceAccount",
            "ClusterRole",
            "ClusterRoleBinding",
            "Role",
            "RoleBinding",
        ] {
            assert!(
                !fixture.lines().any(|l| l.trim() == format!("kind: {kind}")),
                "controller fixture defines a {kind}"
            );
        }
        assert!(fixture.contains("serviceAccountName: buildkit-controller"));
    }
}
