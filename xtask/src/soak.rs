//! `cargo xtask soak` is the certificate lifecycle soak
//! on a throwaway kind cluster. Two controller replicas run in-cluster with
//! short test-only leaf lifetimes (`pki.testLeafLifetimes`). A client pod runs
//! builds back to back, each with a fresh allocation from the API (a
//! `ServiceAccount` token) and a `buildctl` build through the edge. Meanwhile
//! this task forces, in turn:
//!
//! a. Leaf renewals. Worker serving, edge serving (the same as API serving) and
//!    edge client certs each renew at least once on every replica.
//! b. An edge serving-cert swap to a user `kubernetes.io/tls` Secret, an
//!    in-place update of that Secret, and a swap back to self-issued.
//! c. A leader failover (graceful delete of the leader pod).
//! d. Deletion of the CA Secret.
//! e. Corruption of the CA Secret.
//!
//! Scenarios a to c must not fail a single build. In d and e only builds already
//! in flight at the event may fail, the CA must heal within one reconcile, and
//! the task measures the recovery time (from the event to the end of the first
//! good build started after it). Every kind and kubectl call gets the private
//! kubeconfig under `target/soak`, and the task deletes the cluster on every
//! exit path.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use buildkit_controller::pki::{CaBundle, EdgeNames};
use time::OffsetDateTime;

use super::edge_verify::{Env, run_ok};
use super::{CRD_PATH, root};
use crate::image::WORKER_IMAGE;

static CLUSTER: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| super::kind_cluster("bk-soak"));
const SYSTEM_NS: &str = "buildkit-system";
const NS: &str = "bk-soak";
const CURL_IMAGE: &str = "curlimages/curl:8.11.1";
const CONTROLLER_IMAGE: &str = "buildkit-controller:dev";
const CA_SECRET: &str = "buildkit-controller-ca";
const USER_TLS: &str = "soak-user-tls";
const SERVER_NAME: &str = "bk.example.com";
const SERVICE: &str = "buildkit-controller";
/// Bound for "heals within one reconcile".
const HEAL_BOUND: Duration = Duration::from_secs(30);

/// The controller config; `{secret}` is `edge.tls.secretName`.
fn config(secret: &str) -> String {
    format!(
        "    edge:
      endpoint: tcp://{SERVICE}.{SYSTEM_NS}.svc:8444
      serverName: {SERVER_NAME}
      tls: {{secretName: \"{secret}\"}}
    auth:
      serviceAccount: {{enabled: true, audiences: [buildkit-controller]}}
    pki:
      # TEST ONLY: renewals within minutes.
      testLeafLifetimes: {{workerValidity: 12m, workerRenewBefore: 8m, workerMinLeft: 6m30s, edgeValidity: 4m}}
    shutdown:
      delay: 3s
      drainTimeout: 20s
"
    )
}

/// The edge-verify controller fixture (least-privilege RBAC, restricted
/// securityContext, the worker `NetworkPolicy` label) with two replicas, the
/// API port and the soak config.
fn controller_manifest(secret: &str) -> Result<String> {
    let base = std::fs::read_to_string(root().join("xtask/fixtures/edge-verify/controller.yaml"))
        .context("reading the controller fixture")?;
    let mut m = base;
    for (from, to) in [
        ("    edge:\n      endpoint: tcp://bk.example.com:8444\n      serverName: bk.example.com\n    shutdown:\n      delay: 0s\n", config(secret)),
        ("  replicas: 1\n", "  replicas: 2\n".to_owned()),
        (
            "            - {name: edge, containerPort: 8444}\n",
            "            - {name: edge, containerPort: 8444}\n            - {name: api, containerPort: 8443}\n".to_owned(),
        ),
        (
            "    - {name: edge, port: 8444, targetPort: edge}\n",
            "    - {name: edge, port: 8444, targetPort: edge}\n    - {name: api, port: 8443, targetPort: api}\n".to_owned(),
        ),
    ] {
        ensure!(m.contains(from), "controller fixture changed: {from:?} not found");
        m = m.replacen(from, &to, 1);
    }
    Ok(m)
}

/// The controller's `ConfigMap` alone, for the serving-cert switch.
fn configmap(secret: &str) -> String {
    format!(
        "apiVersion: v1\nkind: ConfigMap\nmetadata: {{name: {SERVICE}, namespace: {SYSTEM_NS}}}\n\
         data:\n  config.yaml: |\n{}",
        config(secret)
    )
}

/// One line of the client log.
#[derive(Debug)]
struct Build {
    start: u64,
    end: u64,
    ok: bool,
    retries: u32,
    /// When the 201 arrived (pod clock). Without a 201, the last POST attempt
    /// (fail:post), else `end`, else `start`.
    allocated: u64,
    line: String,
}

fn parse_log(text: &str) -> Vec<Build> {
    text.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let _n = w.next()?;
            let start = w.next()?.parse().ok()?;
            let end = w.next()?.parse().ok()?;
            let status = w.next()?;
            let ok = status == "ok";
            let retries = w
                .next()
                .and_then(|r| r.strip_prefix("retries="))
                .and_then(|r| r.parse().ok())
                .unwrap_or(0);
            let rest: Vec<&str> = w.collect();
            let stamp = |key: &str| {
                rest.iter()
                    .find_map(|t| t.strip_prefix(key))
                    .and_then(|t| t.parse().ok())
            };
            // The 201 time, else the last POST attempt (fail:post), else the end.
            let allocated = stamp("allocated=")
                .or_else(|| stamp("lastpost="))
                .unwrap_or(if status == "fail:post" { end } else { start });
            Some(Build {
                start,
                end,
                ok,
                retries,
                allocated,
                line: l.to_owned(),
            })
        })
        .collect()
}

/// A scenario's pod-clock window and, for d and e, the event.
struct Window {
    name: &'static str,
    from: u64,
    to: u64,
    /// (before, after) the event command, pod clock.
    event: Option<(u64, u64)>,
    heal: Option<Duration>,
    notes: Vec<String>,
}

struct Soak {
    env: Env,
}

/// Deletes the cluster when dropped. Dumps state unless the run passed.
struct Cleanup(Soak, std::cell::Cell<bool>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        if !self.1.get() {
            for args in [
                &["get", "pods", "-A", "-o", "wide"][..],
                &[
                    "-n",
                    SYSTEM_NS,
                    "logs",
                    "-l",
                    "app=buildkit-controller",
                    "--tail=80",
                    "--prefix",
                ][..],
                &["-n", NS, "logs", "client", "--tail=20"][..],
                &["-n", NS, "describe", "pod", "client"][..],
                &[
                    "-n",
                    NS,
                    "logs",
                    "client",
                    "-c",
                    "client",
                    "--previous",
                    "--tail=40",
                ][..],
            ] {
                if let Ok(o) = self.0.env.kubectl_raw(args, None) {
                    println!(
                        "{}{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    );
                }
            }
        }
        let _ = self
            .0
            .env
            .kind(&["delete", "cluster", "--name", CLUSTER.as_str()]);
    }
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

fn wait_for(
    what: &str,
    timeout: Duration,
    mut f: impl FnMut() -> Result<bool>,
) -> Result<Duration> {
    let start = Instant::now();
    loop {
        if f()? {
            return Ok(start.elapsed());
        }
        ensure!(
            start.elapsed() < timeout,
            "timed out after {timeout:?} waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

impl Soak {
    fn k(&self, args: &[&str]) -> Result<String> {
        self.env.kubectl(args)
    }

    /// `date +%s` in the client pod: the clock the client log uses.
    fn pod_now(&self) -> Result<u64> {
        let out = self.k(&[
            "-n", NS, "exec", "client", "-c", "client", "--", "date", "+%s",
        ])?;
        out.trim().parse().context("pod clock")
    }

    fn client_sh(&self, script: &str) -> Result<String> {
        self.k(&[
            "-n", NS, "exec", "client", "-c", "client", "--", "sh", "-c", script,
        ])
    }

    fn builds(&self) -> Result<Vec<Build>> {
        Ok(parse_log(
            &self.client_sh("cat /work/log 2>/dev/null || true")?,
        ))
    }

    /// Waits until `n` good builds started at or after `since`.
    fn good_builds_since(&self, since: u64, n: usize, timeout: Duration) -> Result<()> {
        wait_for(&format!("{n} good builds"), timeout, || {
            Ok(self
                .builds()?
                .iter()
                .filter(|b| b.ok && b.start >= since)
                .count()
                >= n)
        })
        .map(drop)
    }

    fn controller_ips(&self) -> Result<BTreeMap<String, String>> {
        let out = self.k(&[
            "-n",
            SYSTEM_NS,
            "get",
            "pods",
            "-l",
            "app=buildkit-controller",
            "--field-selector=status.phase=Running",
            "-o",
            "jsonpath={range .items[*]}{.metadata.name}={.status.podIP}{\"\\n\"}{end}",
        ])?;
        Ok(out
            .lines()
            .filter_map(|l| l.split_once('='))
            .filter(|(_, ip)| !ip.is_empty())
            .map(|(n, ip)| (n.to_owned(), ip.to_owned()))
            .collect())
    }

    /// `buildkit_cert_not_after_seconds{kind}` of every running replica.
    fn not_after(&self, kind: &str) -> Result<BTreeMap<String, f64>> {
        let mut m = BTreeMap::new();
        for (pod, ip) in self.controller_ips()? {
            let text = self.client_sh(&format!(
                "curl -s --max-time 5 http://{ip}:8080/metrics || true"
            ))?;
            let needle = format!("buildkit_cert_not_after_seconds{{kind=\"{kind}\"}}");
            if let Some(v) = text
                .lines()
                .find(|l| l.starts_with(&needle))
                .and_then(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
            {
                m.insert(pod, v);
            }
        }
        Ok(m)
    }

    fn leader(&self) -> Result<String> {
        Ok(self
            .k(&[
                "-n",
                SYSTEM_NS,
                "get",
                "lease",
                "buildkit-controller-leader",
                "-o",
                "jsonpath={.spec.holderIdentity}",
            ])?
            .trim()
            .to_owned())
    }

    /// Worker pod UIDs (names are slots and get reused).
    fn worker_pods(&self) -> Result<BTreeSet<String>> {
        let out = self.k(&[
            "-n",
            NS,
            "get",
            "pods",
            "-l",
            "app.kubernetes.io/managed-by=buildkit-controller",
            "-o",
            "jsonpath={.items[*].metadata.uid}",
        ])?;
        Ok(out.split_whitespace().map(str::to_owned).collect())
    }

    fn rollout(&self) -> Result<()> {
        self.k(&[
            "-n",
            SYSTEM_NS,
            "rollout",
            "status",
            "deploy/buildkit-controller",
            "--timeout=240s",
        ])
        .map(drop)
    }

    /// Sends requests through the API Service that verify against `ca` only, to
    /// check that the replicas serve that CA's cert. Returns how many of six
    /// tries verified (kube-proxy lags a rollout by a few seconds).
    fn api_serves(&self, ca: &str) -> Result<u32> {
        let out = self.client_sh(&format!(
            "n=0; for i in 1 2 3 4 5 6; do curl -s -o /dev/null --max-time 5 --cacert {ca} \
             https://{SERVICE}.{SYSTEM_NS}.svc:8443/v1/pools && n=$((n+1)); done; echo $n"
        ))?;
        out.trim().parse().context("api check")
    }
}

/// A user serving cert from its own CA, as a
/// `kubernetes.io/tls` Secret with `ca.crt`. Returns (manifest, ca pem).
fn user_tls(ca: &CaBundle, now: OffsetDateTime) -> Result<String> {
    let names = EdgeNames::new(SERVER_NAME, &[], SERVICE, &SYSTEM_NS.parse()?)?;
    let leaf = ca.issue_edge_serving(&names, now)?;
    let key = leaf.private_key_pem().context("generated key missing")?;
    Ok(format!(
        "apiVersion: v1\nkind: Secret\ntype: kubernetes.io/tls\nmetadata: {{name: {USER_TLS}, namespace: {SYSTEM_NS}}}\n\
         data:\n  tls.crt: {}\n  tls.key: {}\n  ca.crt: {}\n",
        b64(leaf.cert_pem().as_bytes()),
        b64(key.as_bytes()),
        b64(ca.client_ca().cert_pem().as_bytes()),
    ))
}

pub(crate) fn soak() -> Result<()> {
    let root = root();
    let work = root.join("target/soak");
    std::fs::create_dir_all(&work)?;
    let env = Env {
        kubeconfig: work.join("kubeconfig"),
    };
    let _ = env.kind(&["delete", "cluster", "--name", CLUSTER.as_str()]);
    let cleanup = Cleanup(Soak { env }, std::cell::Cell::new(false));
    let s = &cleanup.0;
    let created = s.env.kind(&[
        "create",
        "cluster",
        "--name",
        CLUSTER.as_str(),
        "--wait",
        "120s",
    ])?;
    ensure!(
        created.status.success(),
        "kind create failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let user_ca = setup(s, &root)?;
    let t0 = s.pod_now()?;
    s.good_builds_since(t0, 3, Duration::from_mins(5))
        .context("warm-up")?;
    println!("soak: warm-up done, builds flowing");

    let mut w = Vec::new();
    w.push(renewals(s)?);
    w.push(serving_swap(s, &user_ca)?);
    w.push(failover(s)?);
    w.push(ca_event(s, "d/CA Secret deleted", |s| {
        s.k(&["-n", SYSTEM_NS, "delete", "secret", CA_SECRET])
            .map(drop)
    })?);
    w.push(ca_event(s, "e/CA Secret corrupted", |s| {
        let patch = format!(
            "{{\"data\":{{\"client-ca.crt\":\"{}\"}}}}",
            b64(b"corrupted")
        );
        s.k(&["-n", SYSTEM_NS, "patch", "secret", CA_SECRET, "-p", &patch])
            .map(drop)
    })?);

    let stop = s.pod_now()?;
    s.client_sh("touch /work/stop")?;
    wait_for("the client to stop", Duration::from_mins(5), || {
        Ok(s.client_sh("[ -f /work/stopped ] && echo y || true")?
            .trim()
            == "y")
    })?;
    if let Some(last) = w.last_mut() {
        last.to = stop + 1_000;
    }
    let builds = s.builds()?;
    // A restarted client reruns its loop and would hide a gap in the log.
    let restarts = s.k(&[
        "-n",
        NS,
        "get",
        "pod",
        "client",
        "-o",
        "jsonpath={.status.containerStatuses[0].restartCount}",
    ])?;
    ensure!(
        restarts.trim() == "0",
        "the client container restarted {} times",
        restarts.trim()
    );
    std::fs::write(
        work.join("client.log"),
        builds
            .iter()
            .map(|b| b.line.clone() + "\n")
            .collect::<String>(),
    )?;
    let (report, ok) = verdict(&w, &builds);
    print!("{report}");
    std::fs::write(work.join("result.txt"), &report)?;
    if !ok {
        bail!("soak acceptance failed; see target/soak/result.txt and client.log");
    }
    cleanup.1.set(true);
    Ok(())
}

/// Scenario a. Waits until every renewable leaf has renewed on every replica and
/// every worker pod has been replaced. Each worker holds its own certificate,
/// and a free worker retires once that certificate nears `workerMinLeft`.
fn renewals(s: &Soak) -> Result<Window> {
    let from = s.pod_now()?;
    let mut notes = Vec::new();
    for kind in ["edge_serving", "edge_client"] {
        let before = s.not_after(kind)?;
        ensure!(
            before.len() == 2,
            "{kind} not exported by both replicas: {before:?}"
        );
        let took = wait_for(&format!("{kind} renewal"), Duration::from_mins(4), || {
            let now = s.not_after(kind)?;
            Ok(before
                .iter()
                .all(|(p, v)| now.get(p).is_some_and(|n| n > v)))
        })?;
        notes.push(format!(
            "{kind} renewed on both replicas ({:.0} s)",
            took.as_secs_f64()
        ));
    }
    let old = s.worker_pods()?;
    let gone = wait_for(
        "every worker to be replaced with a fresh certificate",
        Duration::from_mins(10),
        || Ok(s.worker_pods()?.is_disjoint(&old)),
    )?;
    notes.push(format!(
        "all {} workers replaced with fresh certificates ({:.0} s)",
        old.len(),
        gone.as_secs_f64()
    ));
    let to = s.pod_now()?;
    s.good_builds_since(to, 2, Duration::from_mins(3))?;
    Ok(Window {
        name: "a/leaf renewals",
        from,
        to: s.pod_now()?,
        event: None,
        heal: None,
        notes,
    })
}

/// Scenario b. Switches from the self-issued cert to a user Secret (rollout),
/// updates the Secret in place, then switches back.
fn serving_swap(s: &Soak, user_ca: &CaBundle) -> Result<Window> {
    let from = s.pod_now()?;
    let mut notes = Vec::new();
    let now = OffsetDateTime::now_utc().replace_nanosecond(0)?;
    s.env.apply(&user_tls(user_ca, now)?)?;
    s.env.apply(&configmap(USER_TLS))?;
    s.k(&[
        "-n",
        SYSTEM_NS,
        "rollout",
        "restart",
        "deploy/buildkit-controller",
    ])?;
    s.rollout()?;
    wait_for(
        "the API to serve the user cert",
        Duration::from_secs(90),
        || Ok(s.api_serves("/user-ca/ca.crt")? == 6),
    )?;
    notes.push("switched to the user Secret (rollout)".into());
    let t = s.pod_now()?;
    s.good_builds_since(t, 3, Duration::from_mins(3))?;
    // Cert-manager style renewal. Same Secret, new leaf, hot-reloaded.
    let before = s.not_after("edge_serving")?;
    s.env
        .apply(&user_tls(user_ca, now + time::Duration::minutes(1))?)?;
    wait_for("user Secret hot reload", Duration::from_mins(2), || {
        let after = s.not_after("edge_serving")?;
        Ok(after.len() == 2
            && after
                .iter()
                .all(|(p, v)| before.get(p).is_some_and(|b| v > b)))
    })?;
    notes.push("user Secret updated in place, hot-reloaded on both replicas".into());
    let t = s.pod_now()?;
    s.good_builds_since(t, 3, Duration::from_mins(3))?;
    s.env.apply(&configmap(""))?;
    s.k(&[
        "-n",
        SYSTEM_NS,
        "rollout",
        "restart",
        "deploy/buildkit-controller",
    ])?;
    s.rollout()?;
    wait_for(
        "the API to serve the self-issued cert",
        Duration::from_secs(90),
        || Ok(s.api_serves("/user-ca/ca.crt")? == 0),
    )?;
    notes.push("back to self-issued (rollout)".into());
    let t = s.pod_now()?;
    s.good_builds_since(t, 3, Duration::from_mins(3))?;
    Ok(Window {
        name: "b/serving-cert swap",
        from,
        to: s.pod_now()?,
        event: None,
        heal: None,
        notes,
    })
}

/// Scenario c. Deletes the leader pod (SIGTERM, the edge drains) and waits for
/// the other replica to take the Lease.
fn failover(s: &Soak) -> Result<Window> {
    let from = s.pod_now()?;
    let old = s.leader()?;
    let start = Instant::now();
    s.k(&["-n", SYSTEM_NS, "delete", "pod", &old, "--wait=false"])?;
    let took = wait_for("a new leader", Duration::from_mins(2), || {
        let l = s.leader()?;
        Ok(!l.is_empty() && l != old)
    })?;
    let new = s.leader()?;
    s.rollout()?;
    let t = s.pod_now()?;
    s.good_builds_since(t, 3, Duration::from_mins(3))?;
    Ok(Window {
        name: "c/leader failover",
        from,
        to: s.pod_now()?,
        event: None,
        heal: None,
        notes: vec![format!(
            "leader {old} deleted; {new} leads after {:.1} s (total {:.1} s)",
            took.as_secs_f64(),
            start.elapsed().as_secs_f64()
        )],
    })
}

/// The CA Secret's `client-ca.crt` and the number of backups.
fn ca_state(s: &Soak) -> Result<(Option<String>, usize)> {
    let crt = s.env.kubectl_raw(
        &[
            "-n",
            SYSTEM_NS,
            "get",
            "secret",
            CA_SECRET,
            "-o",
            "jsonpath={.data.client-ca\\.crt}",
        ],
        None,
    )?;
    let crt = crt
        .status
        .success()
        .then(|| String::from_utf8_lossy(&crt.stdout).trim().to_owned());
    let backups = s.k(&[
        "-n",
        SYSTEM_NS,
        "get",
        "secrets",
        "-l",
        "app.kubernetes.io/component=ca-replaced",
        "-o",
        "jsonpath={.items[*].metadata.name}",
    ])?;
    Ok((crt, backups.split_whitespace().count()))
}

/// Scenarios d and e. Runs `event`, then measures the heal (a new
/// valid-looking CA Secret, plus the backup for corruption) and the build recovery.
fn ca_event(s: &Soak, name: &'static str, event: impl Fn(&Soak) -> Result<()>) -> Result<Window> {
    let from = s.pod_now()?;
    let (crt0, backups0) = ca_state(s)?;
    let old_pods = s.worker_pods()?;
    let crt0 = crt0.context("CA Secret missing before the event")?;
    let corrupt = b64(b"corrupted");
    let before = s.pod_now()?;
    event(s)?;
    let after = s.pod_now()?;
    let heal = wait_for("the CA Secret to heal", Duration::from_mins(2), || {
        let (crt, backups) = ca_state(s)?;
        let fresh = crt.is_some_and(|c| !c.is_empty() && c != crt0 && c != corrupt);
        Ok(fresh && (name.starts_with('d') || backups > backups0))
    })?;
    let mut notes = vec![format!(
        "CA Secret healed after {:.1} s",
        heal.as_secs_f64()
    )];
    if !name.starts_with('d') {
        let (_, b) = ca_state(s)?;
        notes.push(format!("backups {backups0} -> {b}"));
    }
    // Hard rotation. Every worker pod issued under the old internal CA goes.
    let retired = wait_for(
        "pre-rotation worker pods to retire",
        Duration::from_mins(3),
        || Ok(s.worker_pods()?.is_disjoint(&old_pods)),
    )?;
    notes.push(format!(
        "event at pod clock {before}..{after}; all {} pre-rotation worker pods gone {:.1} s after heal",
        old_pods.len(),
        retired.as_secs_f64()
    ));
    s.good_builds_since(after + 1, 3, Duration::from_mins(6))?;
    Ok(Window {
        name,
        from,
        to: s.pod_now()?,
        event: Some((before, after)),
        heal: Some(heal),
        notes,
    })
}

/// Per-scenario counts and the acceptance. Returns (report, passed).
fn verdict(windows: &[Window], builds: &[Build]) -> (String, bool) {
    let mut out = String::new();
    let mut pass = true;
    let _ = writeln!(
        out,
        "scenario | attempted | failed | retries | recovery | heal | verdict"
    );
    for (i, w) in windows.iter().enumerate() {
        let to = windows.get(i + 1).map_or(w.to, |n| n.from);
        // A build belongs to the scenario it ended in. A build allocated just
        // before an event and cut by it is that event's in-flight build.
        let mine: Vec<&Build> = builds
            .iter()
            .filter(|b| b.end >= w.from && b.end < to)
            .collect();
        let failed: Vec<&&Build> = mine.iter().filter(|b| !b.ok).collect();
        let retries: u32 = mine.iter().map(|b| b.retries).sum();
        let (ok, recovery) = match w.event {
            None => (failed.is_empty(), String::from("-")),
            Some((before, after)) => {
                // A build is in flight if its 201 arrived before the event finished.
                // Recovery counts only builds started after the event, from the event start.
                let late = failed.iter().any(|b| b.allocated > after);
                let rec = mine
                    .iter()
                    .filter(|b| b.ok && b.start > after)
                    .map(|b| b.end.saturating_sub(before))
                    .min();
                let healed = w.heal.is_some_and(|h| h <= HEAL_BOUND);
                (
                    !late && healed && rec.is_some(),
                    rec.map_or_else(|| "never".into(), |r| format!("{r} s")),
                )
            }
        };
        pass &= ok;
        let _ = writeln!(
            out,
            "{} | {} | {} | {} | {} | {} | {}",
            w.name,
            mine.len(),
            failed.len(),
            retries,
            recovery,
            w.heal
                .map_or_else(|| "-".into(), |h| format!("{:.1} s", h.as_secs_f64())),
            if ok { "PASS" } else { "FAIL" }
        );
        for n in &w.notes {
            let _ = writeln!(out, "    {n}");
        }
        for b in &failed {
            let _ = writeln!(out, "    failed: {}", b.line);
        }
    }
    let _ = writeln!(out, "soak: {}", if pass { "PASS" } else { "FAIL" });
    (out, pass)
}

/// Installs images, CRD, namespaces, the controller and the client pod. Returns
/// the user serving CA (scenario b), which the client trusts from the start.
fn setup(s: &Soak, root: &std::path::Path) -> Result<CaBundle> {
    super::image::image(CONTROLLER_IMAGE, Some(CLUSTER.as_str()))?;
    crate::image::ensure_worker()?;
    crate::image::ensure(CURL_IMAGE)?;
    for img in [WORKER_IMAGE, CURL_IMAGE] {
        run_ok(&mut crate::kube::kind_load(img, CLUSTER.as_str()))?;
    }

    let crd = root.join(CRD_PATH).to_string_lossy().into_owned();
    s.k(&["apply", "--server-side", "-f", &crd])?;
    crate::kube::wait_crd_established(&s.env.kubeconfig)?;
    s.k(&["create", "namespace", SYSTEM_NS])?;
    s.k(&[
        "label",
        "namespace",
        SYSTEM_NS,
        "pod-security.kubernetes.io/enforce=restricted",
    ])?;
    s.k(&["create", "namespace", NS])?;

    // The controller bootstraps its own CA.
    s.env.apply(&controller_manifest("")?)?;
    s.rollout()?;
    let user_ca = CaBundle::generate(OffsetDateTime::now_utc().replace_nanosecond(0)?)?;
    s.k(&[
        "-n",
        NS,
        "create",
        "secret",
        "generic",
        "soak-user-ca",
        &format!("--from-literal=ca.crt={}", user_ca.client_ca().cert_pem()),
    ])?;
    let script = root.join("xtask/fixtures/soak/client.sh");
    s.k(&[
        "-n",
        NS,
        "create",
        "configmap",
        "soak-client",
        &format!("--from-file=client.sh={}", script.display()),
    ])?;
    let client = include_str!("../fixtures/soak/client.yaml")
        .replace("WORKER_IMAGE", WORKER_IMAGE)
        .replace("CURL_IMAGE", CURL_IMAGE);
    s.env.apply(&client)?;
    s.k(&[
        "-n",
        NS,
        "wait",
        "--for=condition=Ready",
        "pod/client",
        "--timeout=180s",
    ])?;

    Ok(user_ca)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(name: &'static str, from: u64, event: Option<(u64, u64)>) -> Window {
        Window {
            name,
            from,
            to: from + 100,
            event,
            heal: event.map(|_| Duration::from_secs(2)),
            notes: vec![],
        }
    }

    #[test]
    fn verdict_allows_only_in_flight_failures() {
        let log = "1 10 15 ok retries=0\n2 15 20 ok retries=0\n\
                   3 100 112 fail:build retries=0 x\n4 112 130 ok retries=3\n";
        let b = parse_log(log);
        assert_eq!(b.len(), 4);
        let (r, ok) = verdict(&[w("a", 0, None), w("d", 100, Some((110, 111)))], &b);
        assert!(ok, "{r}");
        assert!(r.contains("d | 2 | 1 | 3 | 20 s"), "{r}");
        // A failure that started after the event fails d. Any failure fails a.
        let b = parse_log("1 10 15 fail:post retries=0\n2 120 125 fail:build retries=0\n");
        let (_, ok) = verdict(&[w("a", 0, None)], &b);
        assert!(!ok);
        let (_, ok) = verdict(&[w("d", 100, Some((110, 111)))], &b);
        assert!(!ok);
        // A build started before the event, but its POST (with retries) returned
        // 201 after it. The allocation was made under the new CA, so its
        // failure is not excused.
        let b = parse_log("1 105 125 fail:build retries=4 allocated=118 alloc=x\n");
        assert_eq!(b[0].allocated, 118);
        let (_, ok) = verdict(&[w("d", 100, Some((110, 111)))], &b);
        assert!(!ok);
        let b = parse_log(
            "1 105 125 fail:build retries=0 allocated=109 alloc=x\n2 125 130 ok retries=0 allocated=126\n",
        );
        let (r, ok) = verdict(&[w("d", 100, Some((110, 111)))], &b);
        assert!(ok, "{r}");
        // A fail:post build with no 201 is judged by its last POST attempt, not
        // its start (a long retry loop started before the event and ended after it).
        let b = parse_log("1 105 125 fail:post retries=9 lastpost=120 http=500\n");
        assert_eq!(b[0].allocated, 120);
        let (_, ok) = verdict(&[w("d", 100, Some((110, 111)))], &b);
        assert!(!ok);
        let b = parse_log(
            "1 105 125 fail:post retries=9 lastpost=109 http=500\n2 125 130 ok retries=0 allocated=126\n",
        );
        let (r, ok) = verdict(&[w("d", 100, Some((110, 111)))], &b);
        assert!(ok, "{r}");
        // Old logs without either stamp use the end time.
        assert_eq!(parse_log("1 10 15 fail:post retries=0\n")[0].allocated, 15);
        // Soak run 11 case. A build allocated in c and cut by d's CA deletion
        // is d's in-flight failure, not a failover failure.
        let b = parse_log(
            "1 95 99 ok retries=0 allocated=95\n2 99 110 fail:build retries=0 allocated=99\n\
             3 112 115 ok retries=0 allocated=112\n",
        );
        let (r, ok) = verdict(&[w("c", 90, None), w("d", 100, Some((110, 110)))], &b);
        assert!(ok, "{r}");
        assert!(r.contains("c | 1 | 0") && r.contains("d | 2 | 1"), "{r}");
    }

    #[test]
    fn controller_manifest_applies_the_soak_overrides() {
        let m = controller_manifest(USER_TLS).unwrap();
        assert!(m.contains("replicas: 2"));
        assert!(m.contains("containerPort: 8443"));
        assert!(m.contains("testLeafLifetimes"));
        assert!(m.contains("secretName: \"soak-user-tls\""));
        let client = include_str!("../fixtures/soak/client.yaml");
        for (i, doc) in m
            .split("\n---\n")
            .chain(client.split("\n---\n"))
            .chain([configmap("").as_str()])
            .enumerate()
        {
            let v: Result<serde_json::Value, _> = serde_saphyr::from_str(doc);
            assert!(v.is_ok(), "doc {i}: {v:?}\n{doc}");
        }
        let cm: serde_json::Value = serde_saphyr::from_str(&configmap(USER_TLS)).unwrap();
        let cfg = cm["data"]["config.yaml"].as_str().unwrap();
        let cfg = buildkit_controller::config::Config::from_yaml(cfg);
        assert!(cfg.is_ok(), "{cfg:?}");
    }
}
