//! `cargo xtask drain-verify` proves that stopping a controller replica never
//! cuts a live build. It installs the chart on a throwaway kind cluster with two
//! replicas, `shutdown.drainTimeout` unset, and one pool with a single worker,
//! then checks three things:
//!
//! a. A build whose RUN step takes 60 s runs through the edge while the
//!    controller is rolled (`kubectl rollout restart`). The replica carrying the
//!    connection starts draining while the build runs, with a budget above the
//!    old fixed 300 s one, the build succeeds, and the replica exits once the
//!    connection ends. With `--long` (nightly) the RUN step takes 390 s and the
//!    carrier is seen draining for more than 300 s; 390 s leaves room for the
//!    rollout to reach the carrier late.
//! b. The Lease moves to a live replica within 15 s of SIGTERM to the leader,
//!    while the old leader is still in its `shutdown.delay`, and the old
//!    leader logs that its allocator stopped before it released the Lease.
//! c. With the one worker held, a second allocation answers 202 pending. Its
//!    claim survives a rollout of both replicas, stays counted once in the
//!    pool's `status.waiting`, and binds (a long poll answers 200) once the
//!    first allocation is released.
//!
//! The client is the real curl recipe `docs/ci/buildkit-allocate.sh` and real
//! `docker buildx` on the host, through the API and edge that kind publishes on
//! two loopback ports, 28443 and 28444 by default (see [`PORTS`]). The cluster name is `bk-drain-verify`, plus
//! `-$BK_KIND_SUFFIX` when that is set. Every kind, kubectl and helm call uses
//! the private kubeconfig under `target/drain-verify`, and the cluster is
//! deleted on every exit path.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;

use crate::image::copy_busybox;
use crate::kube::{Cluster, HostPorts};
use crate::proc::{Spawned, status, stdout as output, until};
use crate::root;

/// The loopback host ports of the chart's API and edge `NodePorts`: 28443 and
/// 28444 without `BK_KIND_SUFFIX`, else a pair in 22000-23999 per suffix.
pub(crate) static PORTS: std::sync::LazyLock<HostPorts> = std::sync::LazyLock::new(|| {
    HostPorts::get(
        HostPorts {
            api: 28443,
            edge: 28444,
        },
        22000,
    )
});
static API: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| format!("https://localhost:{}", PORTS.api));
const NS: &str = "bk-drain";
const POOL_NS: &str = "drain-ci";
const POOL: &str = "drain";
const DEPLOY: &str = "deploy/buildkit-controller";
const LEASE: &str = "buildkit-controller-leader";
const SELECTOR: &str = "app.kubernetes.io/name=buildkit-controller";
const AUD: &str = "buildkit-controller";
/// Length of the build's RUN step, printed as one tick every 5 s. Long enough
/// for the rollout to reach the carrier while the build runs.
const BUILD_SECS: u64 = 60;
/// The RUN step with `--long`: past [`OLD_BUDGET`], with room for the rollout
/// to reach the carrier late.
const LONG_BUILD_SECS: u64 = 390;
/// The fixed drain budget the controller used to have.
const OLD_BUDGET: Duration = Duration::from_mins(5);
/// The longest a Lease may take to move after SIGTERM to the leader.
const LEASE_MOVE_MAX: Duration = Duration::from_secs(15);

/// `long`: the build outlasts [`OLD_BUDGET`], so check a shows the drain going
/// past it; otherwise the carrier's logged drain budget shows it.
pub(crate) fn drain_verify(long: bool) -> Result<()> {
    let work = root().join("target/drain-verify");
    std::fs::create_dir_all(&work)?;
    let env = &Cluster::for_suite("bk-drain-verify", &work)?;
    let result = run(env, &work, long);
    if result.is_err() {
        diagnose(env);
    }
    result
}

fn diagnose(env: &Cluster) {
    for args in [
        vec!["get", "pods", "-A", "-o", "wide"],
        vec!["get", "lease", LEASE, "-n", NS, "-o", "yaml"],
        vec!["get", "buildkitpool", "-n", POOL_NS, "-o", "yaml"],
    ] {
        if let Ok(o) = env.kubectl(&args) {
            eprintln!("--- kubectl {args:?}\n{o}");
        }
    }
    if let Ok(pods) = controller_pods(env) {
        for p in pods.keys() {
            if let Ok(o) = env.kubectl(&["logs", "-n", NS, p, "--tail=60"]) {
                eprintln!("--- logs {p}\n{o}");
            }
        }
    }
}

fn run(env: &Cluster, work: &Path, long: bool) -> Result<()> {
    let build_secs = if long { LONG_BUILD_SECS } else { BUILD_SECS };
    let ca = install(env, work)?;
    let token = env
        .kubectl(&[
            "create",
            "token",
            "builder",
            "-n",
            POOL_NS,
            "--audience",
            AUD,
            "--duration",
            "2h",
        ])?
        .trim()
        .to_owned();
    let ctx = prepare(work, build_secs)?;
    let c = Client {
        env,
        ca,
        token,
        work: work.to_path_buf(),
        build_secs,
        long,
    };
    build_survives_rollout(&c, &ctx)?;
    lease_moves_on_sigterm(env)?;
    pending_claim_survives_rollout(&c)?;
    println!("drain-verify: all checks passed");
    Ok(())
}

fn install(env: &Cluster, work: &Path) -> Result<PathBuf> {
    let root = root();
    // A leftover cluster of this name holds the ports; delete it before checking them.
    if !env.attached() {
        env.delete();
        PORTS.check_free(env.name())?;
    }
    let ports = [
        ("API_PORT", PORTS.api.to_string()),
        ("EDGE_PORT", PORTS.edge.to_string()),
    ];
    let kind_cfg = crate::fixture("drain-verify/kind.yaml", work, &ports)?;
    env.create(Some(&kind_cfg), "120s")?;
    crate::image::load_controller(env)?;
    crate::image::ensure_worker()?;
    status(&mut crate::kube::kind_load(
        crate::image::WORKER_IMAGE,
        env.name(),
    ))?;

    for ns in [NS, POOL_NS] {
        env.kubectl(&["create", "namespace", ns])?;
    }
    env.kubectl(&["create", "serviceaccount", "builder", "-n", POOL_NS])?;
    let chart = root.join("helm/buildkit-controller");
    let values = crate::fixture("drain-verify/values.yaml", work, &ports)?;
    env.helm(&[
        "install",
        "bk",
        &chart.to_string_lossy(),
        "-n",
        NS,
        "-f",
        &values.to_string_lossy(),
        "--timeout",
        "5m",
    ])?;
    let grace = env.kubectl(&[
        "get",
        DEPLOY,
        "-n",
        NS,
        "-o",
        "jsonpath={.spec.template.spec.terminationGracePeriodSeconds}",
    ])?;
    // delay 10 s + 24h5m + 10 s margin.
    ensure!(
        grace.trim() == "86720",
        "terminationGracePeriodSeconds is {grace}, want 86720"
    );
    println!("PASS chart: drainTimeout unset, terminationGracePeriodSeconds {grace}");
    env.kubectl(&["rollout", "status", DEPLOY, "-n", NS, "--timeout=300s"])?;
    wait_pool_warm(env)?;

    let ca = env.kubectl(&[
        "get",
        "secret",
        "buildkit-controller-ca",
        "-n",
        NS,
        "-o",
        "jsonpath={.data.client-ca\\.crt}",
    ])?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(ca.trim())
        .context("decoding client-ca.crt")?;
    let path = work.join("api-ca.pem");
    std::fs::write(&path, decoded)?;
    until("API listener", Duration::from_mins(2), || {
        // A TLS-verified 401 shows the API listener is serving.
        let probe = Command::new("curl")
            .args(["-sS", "-o", "/dev/null", "-w", "%{http_code}", "--cacert"])
            .arg(&path)
            .arg(format!("{}/v1/pools", *API))
            .output()?;
        Ok(String::from_utf8_lossy(&probe.stdout)
            .starts_with("401")
            .then_some(()))
    })?;
    Ok(path)
}

fn wait_pool_warm(env: &Cluster) -> Result<()> {
    until(
        "pool Ready with one warm worker",
        Duration::from_mins(5),
        || {
            let s = env.kubectl(&[
                "get",
                "buildkitpool",
                POOL,
                "-n",
                POOL_NS,
                "-o",
                "jsonpath={.status.conditions[?(@.type==\"Ready\")].status} {.status.warm}",
            ])?;
            Ok((s.trim() == "True 1").then_some(()))
        },
    )
}

/// Controller pods by name: `(ready, terminating)`.
fn controller_pods(env: &Cluster) -> Result<BTreeMap<String, (bool, bool)>> {
    let out = env.kubectl(&[
        "get", "pods", "-n", NS, "-l", SELECTOR, "-o",
        "jsonpath={range .items[*]}{.metadata.name} {.status.containerStatuses[0].ready} {.metadata.deletionTimestamp}{\"\\n\"}{end}",
    ])?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let name = (*f.first()?).to_owned();
            Some((name, (f.get(1) == Some(&"true"), f.len() > 2)))
        })
        .collect())
}

/// Ready controller pods that are not terminating.
fn live_controllers(env: &Cluster) -> Result<Vec<String>> {
    Ok(controller_pods(env)?
        .into_iter()
        .filter(|(_, (ready, terminating))| *ready && !terminating)
        .map(|(n, _)| n)
        .collect())
}

/// Sum of the samples of metric `name` on controller pod `pod`.
fn metric(env: &Cluster, pod: &str, name: &str) -> Result<f64> {
    let text = env.kubectl(&[
        "get",
        "--raw",
        &format!("/api/v1/namespaces/{NS}/pods/{pod}:8080/proxy/metrics"),
    ])?;
    Ok(text
        .lines()
        .filter(|l| l.starts_with(name) && l[name.len()..].starts_with(['{', ' ']))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum())
}

fn lease_holder(env: &Cluster) -> Result<String> {
    Ok(env
        .kubectl(&[
            "get",
            "lease",
            LEASE,
            "-n",
            NS,
            "-o",
            "jsonpath={.spec.holderIdentity}",
        ])?
        .trim()
        .to_owned())
}

/// A build context whose RUN step prints `tick N` every 5 s for `secs`.
fn prepare(work: &Path, secs: u64) -> Result<PathBuf> {
    let ctx = work.join("ctx");
    let _ = std::fs::remove_dir_all(&ctx);
    std::fs::create_dir_all(&ctx)?;
    copy_busybox(&ctx)?;
    let ticks = secs / 5;
    std::fs::write(
        ctx.join("Dockerfile"),
        format!(
            "FROM scratch\nCOPY lib/ /lib/\nCOPY bin/ /bin/\nRUN [\"/bin/busybox\", \"sh\", \"-c\", \"i=0; while [ $i -lt {ticks} ]; do i=$((i+1)); /bin/busybox echo tick $i; /bin/busybox sleep 5; done\"]\n"
        ),
    )?;
    Ok(ctx)
}

struct Client<'a> {
    env: &'a Cluster,
    ca: PathBuf,
    token: String,
    work: PathBuf,
    /// Length of check a's RUN step.
    build_secs: u64,
    /// Whether check a must see the drain outlast [`OLD_BUDGET`].
    long: bool,
}

impl Client<'_> {
    /// `curl` against the API with the token in a header file, not argv.
    fn curl(&self) -> Result<Command> {
        let auth = self.work.join("auth");
        std::fs::write(&auth, format!("Authorization: Bearer {}\n", self.token))?;
        let mut c = Command::new("curl");
        c.args(["-sS", "--cacert"])
            .arg(&self.ca)
            .arg("-H")
            .arg(format!("@{}", auth.display()));
        Ok(c)
    }

    /// POST an allocation with the public half of a fresh client key pair.
    /// Returns the curl command writing headers, body and the status code into
    /// `<work>/<tag>.{hdr,body,code}`.
    fn allocate(&self, tag: &str, queue_secs: u64) -> Result<Command> {
        let key = rcgen::KeyPair::generate()?.public_key_pem();
        let body = serde_json::json!({
            "namespace": POOL_NS, "pool": POOL, "ttlSeconds": 900,
            "queueTimeoutSeconds": queue_secs, "publicKey": key,
        })
        .to_string();
        let mut c = self.curl()?;
        c.arg("-D")
            .arg(self.work.join(format!("{tag}.hdr")))
            .arg("-o")
            .arg(self.work.join(format!("{tag}.body")))
            .args(["-w", "%{http_code}", "-H", "Content-Type: application/json"])
            .args(["--data", &body])
            .arg(format!("{}/v1/allocations", *API))
            .stdout(File::create(self.work.join(format!("{tag}.code")))?);
        Ok(c)
    }

    fn read(&self, tag: &str, ext: &str) -> String {
        std::fs::read_to_string(self.work.join(format!("{tag}.{ext}"))).unwrap_or_default()
    }

    /// Allocate and return the id; fails unless the answer is 201.
    fn allocate_ok(&self, tag: &str, queue_secs: u64) -> Result<String> {
        status(&mut self.allocate(tag, queue_secs)?)?;
        let code = self.read(tag, "code");
        let body = self.read(tag, "body");
        ensure!(code == "201", "allocation {tag}: HTTP {code} {body}");
        let v: serde_json::Value = serde_json::from_str(&body)?;
        v["id"].as_str().map(str::to_owned).context("allocation id")
    }

    /// `GET /v1/allocations/{id}?wait=25s`: the status code and body.
    fn get(&self, id: &str) -> Result<(String, String)> {
        let out = self.work.join("get.body");
        let code = output(
            self.curl()?
                .arg("-o")
                .arg(&out)
                .args(["-w", "%{http_code}", "--max-time", "40"])
                .arg(format!("{}/v1/allocations/{id}?wait=25s", *API)),
        )?;
        Ok((code, std::fs::read_to_string(&out).unwrap_or_default()))
    }

    fn release(&self, id: &str) -> Result<()> {
        let code = output(
            self.curl()?
                .args(["-o", "/dev/null", "-w", "%{http_code}", "-X", "DELETE"])
                .arg(format!("{}/v1/allocations/{id}", *API)),
        )?;
        ensure!(code == "204", "release {id}: HTTP {code}");
        Ok(())
    }
}

/// a. A long build through the edge survives `kubectl rollout restart`.
fn build_survives_rollout(c: &Client<'_>, ctx: &Path) -> Result<()> {
    let env = c.env;
    let log_path = c.work.join("build.log");
    let log = File::create(&log_path)?;
    let mut build = Command::new("sh");
    build
        .arg(root().join("docs/ci/buildkit-allocate.sh"))
        .args([
            "sh",
            "-c",
            "docker buildx build --builder \"$BUILDER\" --progress=plain --no-cache -o type=cacheonly .",
        ])
        .current_dir(ctx)
        .env("API", API.as_str())
        .env("API_CA", &c.ca)
        .env("POOL", format!("{POOL_NS}/{POOL}"))
        .env("TOKEN", &c.token)
        .env("TTL", "1800")
        .env("QUEUE_TIMEOUT", "120")
        .stdout(log.try_clone()?)
        .stderr(log);
    let mut build = Spawned::spawn(&mut build)?;
    let result = watch_build(c, &mut build, &log_path);
    if result.is_err() {
        build.kill();
        eprintln!(
            "--- build log\n{}",
            std::fs::read_to_string(&log_path).unwrap_or_default()
        );
    }
    result?;
    env.kubectl(&["rollout", "status", DEPLOY, "-n", NS, "--timeout=300s"])?;
    wait_pool_warm(env)
}

fn has_tick(log: &Path, n: u64) -> bool {
    let want = format!(" tick {n}");
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .any(|l| l.trim_end().ends_with(&want))
}

fn watch_build(c: &Client<'_>, build: &mut Spawned, log: &Path) -> Result<()> {
    let env = c.env;
    let last_tick = c.build_secs / 5;
    until(
        "the build's RUN step to start",
        Duration::from_mins(5),
        || {
            if let Some(s) = build.try_wait()? {
                bail!("the build exited early: {s}");
            }
            Ok(has_tick(log, 2).then_some(()))
        },
    )?;
    let carriers: Vec<String> = until(
        "the replica carrying the build",
        Duration::from_mins(1),
        || {
            let mut v = Vec::new();
            for p in live_controllers(env)? {
                if metric(env, &p, "buildkit_edge_active_connections")? > 0.0 {
                    v.push(p);
                }
            }
            Ok((!v.is_empty()).then_some(v))
        },
    )?;
    println!("build running through the edge on {carriers:?}");
    env.kubectl(&["rollout", "restart", DEPLOY, "-n", NS])?;
    let restarted = Instant::now();

    let mut stopped: BTreeMap<String, Instant> = BTreeMap::new();
    // The drain budget each carrier logged, seen while the build still ran.
    let mut budgets: BTreeMap<String, u64> = BTreeMap::new();
    let exit = loop {
        if let Some(s) = build.try_wait()? {
            break s;
        }
        ensure!(
            restarted.elapsed() < Duration::from_secs(c.build_secs + 300),
            "the build did not finish"
        );
        let pods = controller_pods(env)?;
        for p in &carriers {
            match pods.get(p) {
                Some((_, true)) => {
                    stopped.entry(p.clone()).or_insert_with(Instant::now);
                    if !budgets.contains_key(p) && !has_tick(log, last_tick) {
                        let logs = env.kubectl(&["logs", "-n", NS, p]).unwrap_or_default();
                        if let Some(b) = drain_budget(&logs) {
                            budgets.insert(p.clone(), b);
                        }
                    }
                }
                Some((_, false)) => {}
                // Once buildx is done the connection closes and the carrier
                // may exit while the recipe is still releasing.
                None if has_tick(log, last_tick) => {}
                None => bail!("carrier {p} exited while the build was still running"),
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    let finished = Instant::now();
    ensure!(exit.success(), "the build failed: {exit}");
    ensure!(
        has_tick(log, last_tick),
        "the RUN step did not finish all its ticks"
    );
    for p in &carriers {
        let at = stopped
            .get(p)
            .with_context(|| format!("the rollout never stopped carrier {p}"))?;
        let drained = finished.duration_since(*at);
        // The carrier was draining, with its open connection, before the build ended.
        let budget = budgets.get(p).copied().with_context(|| {
            format!("carrier {p} logged no drain while the build was still running")
        })?;
        ensure!(
            budget > OLD_BUDGET.as_secs(),
            "carrier {p} drains for at most {budget}s; with drainTimeout unset it must outlast the old {}s budget",
            OLD_BUDGET.as_secs()
        );
        if c.long {
            ensure!(
                drained > OLD_BUDGET,
                "carrier {p} drained only {}s; the build must outlast the old {}s budget",
                drained.as_secs(),
                OLD_BUDGET.as_secs()
            );
        }
        until(
            &format!("carrier {p} to exit after the build"),
            Duration::from_mins(1),
            || Ok((!controller_pods(env)?.contains_key(p)).then_some(())),
        )?;
        println!(
            "PASS a: {}s build succeeded through {p} across rollout restart; {p} drained {}s after SIGTERM with a {budget}s budget (old budget {}s) and exited {}s after the build ended",
            c.build_secs,
            drained.as_secs(),
            OLD_BUDGET.as_secs(),
            finished.elapsed().as_secs()
        );
    }
    Ok(())
}

/// `drain_budget_s` of the controller's `draining` log line, if it has one.
/// Text logs may carry ANSI colour codes, which are dropped first.
fn drain_budget(logs: &str) -> Option<u64> {
    logs.lines().map(strip_ansi).find_map(|l| {
        let fields = l.split_once(" draining ")?.1;
        fields
            .split_whitespace()
            .find_map(|f| f.strip_prefix("drain_budget_s="))?
            .parse()
            .ok()
    })
}

/// Whether the controller's log shows its allocator stopped before the Lease
/// was released, so no claim or pod write of the old leader can follow.
fn allocator_stopped_first(logs: &str) -> bool {
    let lines: Vec<String> = logs.lines().map(strip_ansi).collect();
    let at = |msg: &str| lines.iter().position(|l| l.contains(msg));
    matches!(
        (at("allocator stopped"), at("leadership released")),
        (Some(a), Some(r)) if a < r
    )
}

/// `line` without ANSI escape sequences (`ESC [ ... <letter>`).
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// b. SIGTERM to the leader moves the Lease to a live replica within seconds.
fn lease_moves_on_sigterm(env: &Cluster) -> Result<()> {
    let holder = until("a lease holder", Duration::from_mins(1), || {
        let h = lease_holder(env)?;
        Ok((!h.is_empty()).then_some(h))
    })?;
    let leader = live_controllers(env)?
        .into_iter()
        .find(|p| holder.starts_with(p.as_str()))
        .with_context(|| format!("lease holder {holder} is not a live replica"))?;
    env.kubectl(&["delete", "pod", &leader, "-n", NS, "--wait=false"])?;
    let t0 = Instant::now();
    let (new, old_still_there) = loop {
        ensure!(
            t0.elapsed() < Duration::from_mins(1),
            "the Lease did not move"
        );
        let h = lease_holder(env)?;
        if !h.is_empty() && !h.starts_with(leader.as_str()) {
            let pods = controller_pods(env)?;
            if pods.iter().any(|(p, (ready, terminating))| {
                h.starts_with(p.as_str()) && *ready && !terminating
            }) {
                break (h, pods.contains_key(&leader));
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    let moved = t0.elapsed();
    let old_logs = env
        .kubectl(&["logs", "-n", NS, &leader])
        .unwrap_or_default();
    ensure!(
        moved <= LEASE_MOVE_MAX,
        "the Lease took {}ms to move",
        moved.as_millis()
    );
    ensure!(
        old_still_there,
        "the old leader had already exited, so the early release is not shown"
    );
    ensure!(
        allocator_stopped_first(&old_logs),
        "{leader} did not log `allocator stopped` before `leadership released`"
    );
    println!(
        "PASS b: Lease moved from {leader} to {new} {}ms after SIGTERM, while {leader} was still shutting down; its allocator stopped before it released the Lease",
        moved.as_millis()
    );
    until("two live replicas", Duration::from_mins(3), || {
        Ok((live_controllers(env)?.len() == 2).then_some(()))
    })?;
    Ok(())
}

/// c. A pending allocation is a claim, so it survives a rollout of every
/// replica, is counted once in the pool's `status.waiting` throughout, and
/// binds once the held worker is released.
fn pending_claim_survives_rollout(c: &Client<'_>) -> Result<()> {
    let env = c.env;
    wait_pool_warm(env)?;
    let held = c.allocate_ok("a", 0)?;
    println!("holding the only worker with allocation {held}");
    status(&mut c.allocate("b", 300)?)?;
    let (code, body) = (c.read("b", "code"), c.read("b", "body"));
    ensure!(code == "202", "request b: HTTP {code} {body}");
    let v: serde_json::Value = serde_json::from_str(&body)?;
    ensure!(
        v["state"] == "pending" && v["position"] == 1,
        "request b: {body}"
    );
    let id = v["id"].as_str().context("allocation id")?.to_owned();
    let claim = |field: &str| {
        env.kubectl(&[
            "get",
            "bkc",
            &id,
            "-n",
            NS,
            "-o",
            &format!("jsonpath={field}"),
        ])
    };
    let uid = claim("{.metadata.uid}")?;
    println!("PASS c: request b answered 202 pending at place 1 after the POST's wait ({id})");
    env.kubectl(&["rollout", "restart", DEPLOY, "-n", NS])?;
    let t0 = Instant::now();
    let (max_waiting, max_pods) = watch_for_surplus(env)?;
    env.kubectl(&["rollout", "status", DEPLOY, "-n", NS, "--timeout=300s"])?;
    let rolled = t0.elapsed();
    let now = claim("{.metadata.uid} {.status.phase} {.status.queuePosition}")?;
    ensure!(
        now == format!("{uid} Pending 1"),
        "the pending claim did not survive the rollout: {now}"
    );
    ensure!(
        max_waiting == 1,
        "status.waiting reached {max_waiting} with one pending claim"
    );
    ensure!(max_pods <= 1, "{max_pods} worker pods with scaling.max 1");
    println!(
        "PASS c: after a rollout of {}s the claim is still Pending at place 1 (uid unchanged); status.waiting stayed 1, worker pods max {max_pods}",
        rolled.as_secs()
    );
    c.release(&held)?;
    let started = Instant::now();
    loop {
        let (code, body) = c.get(&id)?;
        match code.as_str() {
            "200" => {
                let v: serde_json::Value = serde_json::from_str(&body)?;
                ensure!(v["id"] == id.as_str(), "bound: {body}");
                break;
            }
            "202" => {}
            _ => bail!("poll of {id}: HTTP {code} {body}"),
        }
        ensure!(
            started.elapsed() < Duration::from_mins(3),
            "the pending claim was not bound after the release"
        );
    }
    println!(
        "PASS c: the claim bound {}ms after the held worker was released",
        started.elapsed().as_millis()
    );
    c.release(&id)
}

/// Highest `status.waiting` and live worker pod count seen over 20 s.
fn watch_for_surplus(env: &Cluster) -> Result<(i32, usize)> {
    let mut max_waiting = 0;
    let mut max_pods = 0;
    let watch = Instant::now();
    while watch.elapsed() < Duration::from_secs(20) {
        let waiting = env.kubectl(&[
            "get",
            "buildkitpool",
            POOL,
            "-n",
            POOL_NS,
            "-o",
            "jsonpath={.status.waiting}",
        ])?;
        max_waiting = max_waiting.max(waiting.trim().parse::<i32>().unwrap_or(0));
        let pods = env.kubectl(&[
            "get", "pods", "-n", POOL_NS, "-o",
            "jsonpath={range .items[*]}{.metadata.name} {.metadata.deletionTimestamp}{\"\\n\"}{end}",
        ])?;
        let live = pods
            .lines()
            .filter(|l| l.split_whitespace().count() == 1)
            .count();
        max_pods = max_pods.max(live);
        std::thread::sleep(Duration::from_secs(2));
    }
    Ok((max_waiting, max_pods))
}

#[cfg(test)]
mod tests {
    use super::{allocator_stopped_first, drain_budget};

    #[test]
    fn the_allocator_must_stop_before_the_lease_is_released() {
        let first = "\
2026-10-29T10:00:00.000Z  INFO buildkit_controller::alloc::leader: allocator stopped component=\"allocator\"
2026-10-29T10:00:00.001Z  INFO buildkit_controller::runtime::stop: leadership released
";
        assert!(allocator_stopped_first(first));
        let late = "\
2026-10-29T10:00:00.001Z  INFO buildkit_controller::runtime::stop: leadership released
2026-10-29T10:00:05.000Z  INFO buildkit_controller::alloc::leader: allocator stopped component=\"allocator\"
";
        assert!(!allocator_stopped_first(late));
        assert!(!allocator_stopped_first(
            "INFO buildkit_controller::runtime::stop: leadership released"
        ));
    }

    #[test]
    fn drain_budget_comes_from_the_draining_line_only() {
        let logs = "\
2026-10-29T10:00:00.000Z  INFO buildkit_controller::edge::listener: edge listener closed, draining open=1
2026-10-29T10:00:00.001Z  INFO buildkit_controller::runtime::stop: draining drain_budget_s=86700
";
        assert_eq!(drain_budget(logs), Some(86700));
        assert_eq!(
            drain_budget("INFO x: edge listener closed, draining open=1"),
            None
        );
        assert_eq!(drain_budget(""), None);
        let colour = "\u{1b}[32m INFO\u{1b}[0m stop: draining \u{1b}[3mdrain_budget_s\u{1b}[0m\u{1b}[2m=\u{1b}[0m86700";
        assert_eq!(drain_budget(colour), Some(86700));
    }
}
