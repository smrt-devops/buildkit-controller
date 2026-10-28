//! `cargo xtask drain-verify` proves that stopping a controller replica never
//! cuts a live build. It installs the chart on a throwaway kind cluster with two
//! replicas, `shutdown.drainTimeout` unset, and one pool with a single worker,
//! then checks three things:
//!
//! a. A build whose RUN step takes 390 s runs through the edge while the
//!    controller is rolled (`kubectl rollout restart`). The replica carrying the
//!    connection drains for more than the old fixed 300 s budget, the build
//!    succeeds, and the replica exits once the connection ends. 390 s leaves room
//!    for the rollout to reach the carrier late.
//! b. The Lease moves to a live replica within 15 s of SIGTERM to the leader,
//!    while the old leader is still in its `shutdown.delay`.
//! c. With the one worker held, a second allocation queued on a replica that is
//!    then deleted gets HTTP 503 `shutting_down` with `Retry-After`, not a reset,
//!    and a retry after the first allocation is released succeeds.
//!
//! The client is the real curl recipe `docs/ci/buildkit-allocate.sh` and real
//! `docker buildx` on the host, through the API and edge that kind publishes on
//! 127.0.0.1:28443 and :28444. The cluster name is `bk-drain-verify`, plus
//! `-$BK_KIND_SUFFIX` when that is set. Every kind, kubectl and helm call uses
//! the private kubeconfig under `target/drain-verify`, and the cluster is
//! deleted on every exit path.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::helm_verify::{Env, until};
use crate::image::WORKER_IMAGE;
use crate::root;

fn cluster() -> String {
    super::kind_cluster("bk-drain-verify")
}
const NS: &str = "bk-drain";
const POOL_NS: &str = "drain-ci";
const POOL: &str = "drain";
const API: &str = "https://localhost:28443";
const CONTROLLER_IMAGE: &str = "buildkit-controller:dev";
const DEPLOY: &str = "deploy/buildkit-controller";
const LEASE: &str = "buildkit-controller-leader";
const SELECTOR: &str = "app.kubernetes.io/name=buildkit-controller";
const AUD: &str = "buildkit-controller";
/// Length of the build's RUN step, printed as one tick every 5 s.
const BUILD_SECS: u64 = 390;
/// The fixed drain budget the controller used to have.
const OLD_BUDGET: Duration = Duration::from_mins(5);
/// The longest a Lease may take to move after SIGTERM to the leader.
const LEASE_MOVE_MAX: Duration = Duration::from_secs(15);

struct Cleanup(Env);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self
            .0
            .tool("kind", &["delete", "cluster", "--name", cluster().as_str()]);
    }
}

pub(crate) fn drain_verify() -> Result<()> {
    let work = root().join("target/drain-verify");
    std::fs::create_dir_all(&work)?;
    let cleanup = Cleanup(Env {
        kubeconfig: work.join("kubeconfig"),
    });
    let env = &cleanup.0;
    let result = run(env, &work);
    if result.is_err() {
        diagnose(env);
    }
    result
}

fn diagnose(env: &Env) {
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

fn run(env: &Env, work: &Path) -> Result<()> {
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
    let ctx = prepare(work)?;
    let c = Client {
        env,
        ca,
        token,
        work: work.to_path_buf(),
    };
    build_survives_rollout(&c, &ctx)?;
    lease_moves_on_sigterm(env)?;
    queued_request_gets_503(&c)?;
    println!("drain-verify: all checks passed");
    Ok(())
}

fn install(env: &Env, work: &Path) -> Result<PathBuf> {
    let root = root();
    let _ = env.tool("kind", &["delete", "cluster", "--name", cluster().as_str()]);
    let kind_cfg = root.join("xtask/fixtures/drain-verify/kind.yaml");
    env.tool(
        "kind",
        &[
            "create",
            "cluster",
            "--name",
            cluster().as_str(),
            "--config",
            &kind_cfg.to_string_lossy(),
            "--wait",
            "120s",
        ],
    )?;
    crate::image::image(CONTROLLER_IMAGE, Some(cluster().as_str()))?;
    crate::image::ensure_worker()?;
    status(&mut crate::kube::kind_load(
        WORKER_IMAGE,
        cluster().as_str(),
    ))?;

    for ns in [NS, POOL_NS] {
        env.kubectl(&["create", "namespace", ns])?;
    }
    env.kubectl(&["create", "serviceaccount", "builder", "-n", POOL_NS])?;
    let chart = root.join("helm/buildkit-controller");
    let values = root.join("xtask/fixtures/drain-verify/values.yaml");
    env.tool(
        "helm",
        &[
            "install",
            "bk",
            &chart.to_string_lossy(),
            "-n",
            NS,
            "-f",
            &values.to_string_lossy(),
            "--timeout",
            "5m",
        ],
    )?;
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
    let decoded =
        output(Command::new("sh").args(["-c", &format!("printf %s '{ca}' | base64 -d")]))?;
    let path = work.join("api-ca.pem");
    std::fs::write(&path, decoded)?;
    until("API listener", Duration::from_mins(2), || {
        // A TLS-verified 401 shows the API listener is serving.
        let probe = Command::new("curl")
            .args(["-sS", "-o", "/dev/null", "-w", "%{http_code}", "--cacert"])
            .arg(&path)
            .arg(format!("{API}/v1/pools"))
            .output()?;
        Ok(String::from_utf8_lossy(&probe.stdout)
            .starts_with("401")
            .then_some(()))
    })?;
    Ok(path)
}

fn wait_pool_warm(env: &Env) -> Result<()> {
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

fn status(c: &mut Command) -> Result<()> {
    ensure!(
        c.status().with_context(|| format!("{c:?}"))?.success(),
        "{c:?} failed"
    );
    Ok(())
}

fn output(c: &mut Command) -> Result<String> {
    let o = c.output().with_context(|| format!("{c:?}"))?;
    ensure!(
        o.status.success(),
        "{c:?} failed:\n{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Controller pods by name: `(ready, terminating)`.
fn controller_pods(env: &Env) -> Result<BTreeMap<String, (bool, bool)>> {
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
fn live_controllers(env: &Env) -> Result<Vec<String>> {
    Ok(controller_pods(env)?
        .into_iter()
        .filter(|(_, (ready, terminating))| *ready && !terminating)
        .map(|(n, _)| n)
        .collect())
}

/// Sum of the samples of metric `name` on controller pod `pod`.
fn metric(env: &Env, pod: &str, name: &str) -> Result<f64> {
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

fn lease_holder(env: &Env) -> Result<String> {
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

/// Copies busybox and the musl loader out of the worker image into `dir`, as a
/// tar stream (the rootless image cannot write to a host bind mount).
fn copy_busybox(dir: &Path) -> Result<()> {
    let mut docker = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--entrypoint",
            "sh",
            WORKER_IMAGE,
            "-c",
            "cd / && tar -chf - bin/busybox lib/ld-musl-*.so.1 lib/libc.musl-*.so.1",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .context("starting docker run for busybox")?;
    let stream = docker.stdout.take().context("docker stdout")?;
    let untar = Command::new("tar")
        .args(["-xf", "-", "-C"])
        .arg(dir)
        .stdin(stream)
        .status()
        .context("running tar")?;
    ensure!(
        docker.wait()?.success() && untar.success(),
        "copying busybox failed"
    );
    Ok(())
}

/// A build context whose RUN step prints `tick N` every 5 s for [`BUILD_SECS`].
fn prepare(work: &Path) -> Result<PathBuf> {
    let ctx = work.join("ctx");
    let _ = std::fs::remove_dir_all(&ctx);
    std::fs::create_dir_all(&ctx)?;
    copy_busybox(&ctx)?;
    let ticks = BUILD_SECS / 5;
    std::fs::write(
        ctx.join("Dockerfile"),
        format!(
            "FROM scratch\nCOPY lib/ /lib/\nCOPY bin/ /bin/\nRUN [\"/bin/busybox\", \"sh\", \"-c\", \"i=0; while [ $i -lt {ticks} ]; do i=$((i+1)); /bin/busybox echo tick $i; /bin/busybox sleep 5; done\"]\n"
        ),
    )?;
    Ok(ctx)
}

struct Client<'a> {
    env: &'a Env,
    ca: PathBuf,
    token: String,
    work: PathBuf,
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
            .arg(format!("{API}/v1/allocations"))
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

    fn release(&self, id: &str) -> Result<()> {
        let code = output(
            self.curl()?
                .args(["-o", "/dev/null", "-w", "%{http_code}", "-X", "DELETE"])
                .arg(format!("{API}/v1/allocations/{id}")),
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
    let mut build = Command::new("sh")
        .arg(root().join("docs/ci/buildkit-allocate.sh"))
        .args([
            "sh",
            "-c",
            "docker buildx build --builder \"$BUILDER\" --progress=plain --no-cache -o type=cacheonly .",
        ])
        .current_dir(ctx)
        .env("API", API)
        .env("API_CA", &c.ca)
        .env("POOL", format!("{POOL_NS}/{POOL}"))
        .env("TOKEN", &c.token)
        .env("TTL", "1800")
        .env("QUEUE_TIMEOUT", "120")
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()?;
    let result = watch_build(c, &mut build, &log_path);
    if result.is_err() {
        let _ = build.kill();
        let _ = build.wait();
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

fn watch_build(c: &Client<'_>, build: &mut Child, log: &Path) -> Result<()> {
    let env = c.env;
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
    let exit = loop {
        if let Some(s) = build.try_wait()? {
            break s;
        }
        ensure!(
            restarted.elapsed() < Duration::from_secs(BUILD_SECS + 300),
            "the build did not finish"
        );
        let pods = controller_pods(env)?;
        for p in &carriers {
            match pods.get(p) {
                Some((_, true)) => {
                    stopped.entry(p.clone()).or_insert_with(Instant::now);
                }
                Some((_, false)) => {}
                // Once buildx is done the connection closes and the carrier
                // may exit while the recipe is still releasing.
                None if has_tick(log, BUILD_SECS / 5) => {}
                None => bail!("carrier {p} exited while the build was still running"),
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    };
    let finished = Instant::now();
    ensure!(exit.success(), "the build failed: {exit}");
    ensure!(
        has_tick(log, BUILD_SECS / 5),
        "the RUN step did not finish all its ticks"
    );
    for p in &carriers {
        let at = stopped
            .get(p)
            .with_context(|| format!("the rollout never stopped carrier {p}"))?;
        let drained = finished.duration_since(*at);
        ensure!(
            drained > OLD_BUDGET,
            "carrier {p} drained only {}s; the build must outlast the old {}s budget",
            drained.as_secs(),
            OLD_BUDGET.as_secs()
        );
        until(
            &format!("carrier {p} to exit after the build"),
            Duration::from_mins(1),
            || Ok((!controller_pods(env)?.contains_key(p)).then_some(())),
        )?;
        println!(
            "PASS a: {BUILD_SECS}s build succeeded through {p} across rollout restart; {p} drained {}s after SIGTERM (old budget {}s) and exited {}s after the build ended",
            drained.as_secs(),
            OLD_BUDGET.as_secs(),
            finished.elapsed().as_secs()
        );
    }
    Ok(())
}

/// b. SIGTERM to the leader moves the Lease to a live replica within seconds.
fn lease_moves_on_sigterm(env: &Env) -> Result<()> {
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
    ensure!(
        moved <= LEASE_MOVE_MAX,
        "the Lease took {}ms to move",
        moved.as_millis()
    );
    ensure!(
        old_still_there,
        "the old leader had already exited, so the early release is not shown"
    );
    println!(
        "PASS b: Lease moved from {leader} to {new} {}ms after SIGTERM, while {leader} was still shutting down",
        moved.as_millis()
    );
    until("two live replicas", Duration::from_mins(3), || {
        Ok((live_controllers(env)?.len() == 2).then_some(()))
    })?;
    Ok(())
}

/// c. A request queued on a stopping replica gets 503 `shutting_down`.
fn queued_request_gets_503(c: &Client<'_>) -> Result<()> {
    let env = c.env;
    wait_pool_warm(env)?;
    let held = c.allocate_ok("a", 0)?;
    println!("holding the only worker with allocation {held}");
    let mut queued = c.allocate("b", 300)?.spawn()?;
    let replica = until(
        "the second request to queue",
        Duration::from_mins(1),
        || {
            if let Some(s) = queued.try_wait()? {
                bail!(
                    "request b ended before it queued: {s} {}",
                    c.read("b", "code")
                );
            }
            let w = env.kubectl(&[
                "get",
                "buildkitpool",
                POOL,
                "-n",
                POOL_NS,
                "-o",
                "jsonpath={range .status.waiters[*]}{.replica} {.count}{\"\\n\"}{end}",
            ])?;
            Ok(w.lines().find_map(|l| {
                let (r, n) = l.split_once(' ')?;
                (n.trim().parse::<i32>().ok()? > 0).then(|| r.to_owned())
            }))
        },
    )?;
    env.kubectl(&["delete", "pod", &replica, "-n", NS, "--wait=false"])?;
    let t0 = Instant::now();
    let exit = loop {
        if let Some(s) = queued.try_wait()? {
            break s;
        }
        if t0.elapsed() > Duration::from_mins(1) {
            let _ = queued.kill();
            bail!("the queued request got no answer within 60s of SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let answered = t0.elapsed();
    let (code, hdr, body) = (c.read("b", "code"), c.read("b", "hdr"), c.read("b", "body"));
    ensure!(
        exit.success(),
        "curl failed ({exit}): the connection was cut, not answered"
    );
    ensure!(code == "503", "queued request got HTTP {code}: {body}");
    let retry_after = hdr
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("retry-after")
                .then(|| v.trim().to_owned())
        })
        .context("no Retry-After header")?;
    ensure!(body.contains("\"shutting_down\""), "body: {body}");
    println!(
        "PASS c: request queued on {replica} answered 503 shutting_down, Retry-After {retry_after}, {}ms after SIGTERM",
        answered.as_millis()
    );
    retry_is_counted_once(c, &replica, t0, &held)
}

/// c, continued: the retry of the refused request queues on a live replica and is
/// counted once, and it gets 201 once the held worker is released.
fn retry_is_counted_once(c: &Client<'_>, replica: &str, t0: Instant, held: &str) -> Result<()> {
    let env = c.env;
    // The client retries at once, while the worker is still held, so it queues
    // on a live replica. The stopping replica's entry must already read 0, or
    // the leader counts the request twice and plans surplus workers.
    let mut retry = c.allocate("b2", 120)?.spawn()?;
    let cleared = until(
        "the stopping replica's waiters entry to clear and the retry to queue",
        Duration::from_secs(30),
        || {
            if let Some(s) = retry.try_wait()? {
                bail!(
                    "the retry ended before it queued: {s} {}",
                    c.read("b2", "code")
                );
            }
            let w = waiters(env)?;
            let old = w.get(replica).copied().unwrap_or(0);
            let queued_elsewhere = w.iter().any(|(r, n)| r != replica && *n > 0);
            Ok((old == 0 && queued_elsewhere).then_some(w))
        },
    )?;
    let cleared_after = t0.elapsed();
    let (max_waiting, max_pods) = watch_for_surplus(env)?;
    ensure!(
        max_waiting <= 1,
        "status.waiting reached {max_waiting} with one queued request"
    );
    ensure!(max_pods <= 1, "{max_pods} worker pods with scaling.max 1");
    println!(
        "PASS c: {replica}'s status.waiters entry read 0 {}ms after SIGTERM ({cleared:?}); over 20s with the retry queued, status.waiting max {max_waiting}, worker pods max {max_pods}",
        cleared_after.as_millis()
    );
    c.release(held)?;
    let started = Instant::now();
    loop {
        if let Some(s) = retry.try_wait()? {
            ensure!(s.success(), "the retry failed: {s}");
            break;
        }
        ensure!(
            started.elapsed() < Duration::from_mins(3),
            "the retry was not served after the release"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    let (code, body) = (c.read("b2", "code"), c.read("b2", "body"));
    ensure!(code == "201", "retry: HTTP {code} {body}");
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let retried = v["id"].as_str().context("allocation id")?.to_owned();
    c.release(&retried)?;
    println!("PASS c: the queued retry got 201 after the release ({retried})");
    Ok(())
}

/// `status.waiters` of the pool, by replica.
fn waiters(env: &Env) -> Result<BTreeMap<String, i32>> {
    let w = env.kubectl(&[
        "get",
        "buildkitpool",
        POOL,
        "-n",
        POOL_NS,
        "-o",
        "jsonpath={range .status.waiters[*]}{.replica} {.count}{\"\\n\"}{end}",
    ])?;
    Ok(w.lines()
        .filter_map(|l| {
            let (r, n) = l.split_once(' ')?;
            Some((r.to_owned(), n.trim().parse().ok()?))
        })
        .collect())
}

/// Highest `status.waiting` and live worker pod count seen over 20 s.
fn watch_for_surplus(env: &Env) -> Result<(i32, usize)> {
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
