//! `cargo xtask runtime-verify` runs two controller replicas against a
//! throwaway kind cluster. It shows `/readyz` turning true only after
//! `InitDone` (the CRD is applied late on purpose), `/readyz` staying true
//! while the API server restarts and the watches relist, `/readyz` failing
//! within its bound when the API server goes silent without an error, exactly
//! one leader,
//! failover within the spec bound after SIGKILL of the leader, and Lease
//! release on SIGTERM.
//!
//! The cluster uses a private kubeconfig under `target/runtime-verify`. The
//! task never reads or writes the user's `~/.kube/config`, and it deletes the
//! cluster on every exit path.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::kube::Cluster;
use crate::proc::Spawned;

const NAMESPACE: &str = "bk-runtime";
/// leaseDuration (15 s) plus slack for polling and the contender's timer.
const FAILOVER_BOUND: Duration = Duration::from_secs(20);
/// How long `/readyz` is sampled after the API server answers again: the
/// longest watch restart backoff (30 s) plus time for the relist.
const RELIST_SETTLE: Duration = Duration::from_secs(35);

fn lease_holder(cluster: &Cluster) -> Result<String> {
    cluster.kubectl(&[
        "-n",
        NAMESPACE,
        "get",
        "lease",
        "buildkit-controller-leader",
        "-o",
        "jsonpath={.spec.holderIdentity}",
    ])
}

/// One replica and its listener ports. Both replicas run on the host, so no
/// port may be shared: the second replica would fail to bind and exit.
struct Replica {
    name: &'static str,
    ops: u16,
    api: u16,
    edge: u16,
}

/// The two replicas, on free loopback ports, so parallel runs from other
/// checkouts never probe each other's replicas.
fn replicas() -> Result<[Replica; 2]> {
    let [ops1, api1, edge1, ops2, api2, edge2] = crate::proc::free_ports()?;
    Ok([
        Replica {
            name: "r1",
            ops: ops1,
            api: api1,
            edge: edge1,
        },
        Replica {
            name: "r2",
            ops: ops2,
            api: api2,
            edge: edge2,
        },
    ])
}

fn http(port: u16, path: &str) -> Option<(u16, String)> {
    let out = Command::new("curl")
        .args(["-s", "-m", "2", "-w", "\n%{http_code}"])
        .arg(format!("http://127.0.0.1:{port}{path}"))
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let (body, code) = text.rsplit_once('\n')?;
    Some((code.trim().parse().ok()?, body.to_owned()))
}

fn is_leader(port: u16) -> Option<bool> {
    let (_, body) = http(port, "/metrics")?;
    body.lines()
        .find_map(|l| l.strip_prefix("buildkit_leader "))
        .map(|v| v.trim() == "1")
}

/// Polls `f` every 250 ms; returns how long it took to hold.
fn wait_for(what: &str, limit: Duration, mut f: impl FnMut() -> bool) -> Result<Duration> {
    let start = Instant::now();
    crate::proc::poll(what, limit, Duration::from_millis(250), || {
        Ok(f().then(|| start.elapsed()))
    })
}

/// Restart the API server in the kind node and sample `/readyz` on every
/// replica until the API answers again and the watches have had time to
/// relist. Every sample must be 200: a watch error alone must not fail
/// readiness (see `STALE_AFTER` in `runtime/stores.rs`).
fn api_server_blip(env: &Cluster, replicas: &[Replica]) -> Result<()> {
    let node = format!("{}-control-plane", env.name());
    let before: Vec<u64> = replicas.iter().map(|r| pod_watch_errors(r.ops)).collect();
    let stopped = Command::new("docker")
        .args(["exec", &node, "sh", "-c"])
        .arg("crictl stop $(crictl ps -q --name '^kube-apiserver$')")
        .output()
        .context("docker exec crictl stop")?;
    ensure!(
        stopped.status.success(),
        "stopping kube-apiserver: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let start = Instant::now();
    let (mut samples, mut down_seen, mut back_at) = (0_usize, false, None::<Instant>);
    loop {
        for r in replicas {
            let got = http(r.ops, "/readyz");
            ensure!(
                got.as_ref().is_some_and(|(c, _)| *c == 200),
                "{}: /readyz {got:?} {:.1?} into the API server restart",
                r.name,
                start.elapsed()
            );
            samples += 1;
        }
        let up = env
            .kubectl(&["get", "--raw", "/readyz", "--request-timeout=2s"])
            .is_ok();
        down_seen |= !up;
        if down_seen && up && back_at.is_none() {
            back_at = Some(Instant::now());
        }
        let hit = replicas
            .iter()
            .zip(&before)
            .all(|(r, b)| pod_watch_errors(r.ops) > *b);
        if let Some(t) = back_at
            && hit
            && t.elapsed() >= RELIST_SETTLE
        {
            break;
        }
        ensure!(
            start.elapsed() < Duration::from_mins(3),
            "API server restart did not finish (down seen {down_seen}, back {back_at:?}, watch errors seen {hit})"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    let outage = back_at.map(|t| t - start).unwrap_or_default();
    println!(
        "PASS API server restart: down {outage:.1?}, pod watch errors on every replica, /readyz 200 in all {samples} samples through the relist"
    );
    Ok(())
}

/// Earliest `/readyz` may fail once the API server goes silent: the 60 s
/// `STALE_AFTER` of `runtime/stores.rs`, counted from a watch error, which
/// comes no sooner than the stall.
const STALL_STALE_MIN: Duration = Duration::from_mins(1);
/// Latest: the 90 s client read timeout (`READ_TIMEOUT` in `runtime/mod.rs`)
/// plus `STALE_AFTER`, plus slack for polling.
const STALL_STALE_MAX: Duration = Duration::from_secs(150 + 15);
/// After traffic flows again: a connect attempt in flight (30 s), the longest
/// restart backoff (30 s) and the relist.
const STALL_RECOVER: Duration = Duration::from_secs(75);

/// Drop the API server's traffic to and from outside the kind node, so every
/// connection the replicas hold stalls with no error and no reset. Kubelet and
/// the node's own components reach it over loopback and keep working, so the
/// API server is not restarted. Without the client read timeout the watches
/// would hang and `/readyz` would stay 200 for good.
fn api_server_stall(cluster: &Cluster, replicas: &[Replica]) -> Result<()> {
    let node = format!("{}-control-plane", cluster.name());
    let iptables = |op: &str| -> Result<()> {
        let rules = format!(
            "iptables -w {op} INPUT -i eth0 -p tcp --dport 6443 -j DROP && \
             iptables -w {op} OUTPUT -o eth0 -p tcp --sport 6443 -j DROP"
        );
        let out = Command::new("docker")
            .args(["exec", &node, "sh", "-c", &rules])
            .output()
            .context("docker exec iptables")?;
        ensure!(
            out.status.success(),
            "iptables {op}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(())
    };
    let readyz = || -> Vec<Option<(u16, String)>> {
        replicas.iter().map(|r| http(r.ops, "/readyz")).collect()
    };
    ensure!(
        readyz()
            .iter()
            .all(|g| g.as_ref().is_some_and(|(c, _)| *c == 200)),
        "/readyz not 200 on both before the stall"
    );
    iptables("-I")?;
    let start = Instant::now();
    let mut early = None;
    let stale = wait_for(
        "/readyz 503 on both after the stall",
        STALL_STALE_MAX,
        || {
            let got = readyz();
            if got.iter().flatten().any(|(c, _)| *c != 200) && start.elapsed() < STALL_STALE_MIN {
                early.get_or_insert((start.elapsed(), got.clone()));
            }
            got.iter().all(|g| {
                g.as_ref().is_some_and(|(c, body)| {
                    *c == 503 && (body.contains("pod_store") || body.contains("pool_store"))
                })
            })
        },
    );
    let unblocked = iptables("-D");
    let stale = stale?;
    unblocked?;
    if let Some((at, got)) = early {
        bail!("/readyz failed {at:.1?} into the stall, before STALE_AFTER: {got:?}");
    }
    let back = wait_for("/readyz 200 on both after the stall", STALL_RECOVER, || {
        readyz()
            .iter()
            .all(|g| g.as_ref().is_some_and(|(c, _)| *c == 200))
    })?;
    println!(
        "PASS API server stall: /readyz 503 naming a stale store on both replicas {stale:.1?} after the API server went silent (bound {STALL_STALE_MAX:?}), 200 again {back:.1?} after traffic resumed"
    );
    Ok(())
}

/// `buildkit_watch_errors_total{kind="pod"}`, 0 when absent.
fn pod_watch_errors(port: u16) -> u64 {
    http(port, "/metrics")
        .and_then(|(_, body)| {
            body.lines().find_map(|l| {
                l.strip_prefix("buildkit_watch_errors_total{kind=\"pod\"} ")
                    .and_then(|v| v.trim().parse().ok())
            })
        })
        .unwrap_or(0)
}

fn write_config(dir: &Path, r: &Replica) -> Result<PathBuf> {
    let path = dir.join(format!("{}.yaml", r.name));
    std::fs::write(
        &path,
        format!(
            "listen:\n  ops: 127.0.0.1:{}\n  api: 127.0.0.1:{}\n  edge: 127.0.0.1:{}\nedge:\n  endpoint: tcp://bk.example.com:8444\n  serverName: bk.example.com\nshutdown:\n  delay: 0s\n  drainTimeout: 10s\n",
            r.ops, r.api, r.edge
        ),
    )?;
    Ok(path)
}

#[allow(clippy::too_many_lines, reason = "one linear scenario script")]
pub fn runtime_verify() -> Result<()> {
    let root = crate::root();
    let work = root.join("target/runtime-verify");
    std::fs::create_dir_all(&work)?;
    let bin = crate::build_controller()?;
    let cluster = Cluster::new(
        &super::kind_cluster("bk-runtime-verify"),
        work.join("kubeconfig"),
    );
    // Declared after the cluster, so the replicas are killed before it is deleted.
    let mut children: Vec<Spawned> = Vec::new();
    cluster.create(None, "120s")?;
    cluster.kubectl(&["create", "namespace", NAMESPACE])?;

    let replicas = &replicas()?;
    for r in replicas {
        let cfg = write_config(&work, r)?;
        let log = std::fs::File::create(work.join(format!("{}.log", r.name)))?;
        let mut child = Command::new(&bin);
        crate::kube::with_kubeconfig(&mut child, cluster.kubeconfig())
            .args(["--config"])
            .arg(&cfg)
            .env("POD_NAME", r.name)
            .env("POD_NAMESPACE", NAMESPACE)
            .env("LOG_LEVEL", "info")
            .stderr(Stdio::from(log));
        children.push(Spawned::spawn(&mut child).context("starting replica")?);
    }

    // 1. No CRD yet. The Pool store cannot reach InitDone, so /readyz is 503 and names it.
    wait_for("both ops ports up", Duration::from_secs(30), || {
        replicas
            .iter()
            .all(|r| http(r.ops, "/healthz").is_some_and(|(c, _)| c == 200))
    })?;
    std::thread::sleep(Duration::from_secs(3));
    for r in replicas {
        let (code, body) = http(r.ops, "/readyz").context("readyz")?;
        ensure!(
            code == 503 && body.contains("pool_store"),
            "{}: expected 503 naming pool_store before InitDone, got {code} {body:?}",
            r.name
        );
        println!(
            "PASS {} /readyz before InitDone -> {code} {}",
            r.name,
            body.trim()
        );
    }

    // 2. Apply the CRDs. The stores sync (watch backoff <= 30 s) and /readyz turns 200.
    cluster.apply_crds()?;
    let t = wait_for("/readyz 200 on both", Duration::from_secs(90), || {
        replicas
            .iter()
            .all(|r| http(r.ops, "/readyz").is_some_and(|(c, _)| c == 200))
    })?;
    println!("PASS /readyz 200 on both replicas {t:.1?} after the CRD appeared (InitDone)");

    // 3. Restart the API server. The watches fail and relist, and /readyz stays 200.
    api_server_blip(&cluster, replicas)?;

    // 3b. Silence the API server without an error. The client read timeout
    // turns the stall into watch errors, and /readyz fails within its bound.
    api_server_stall(&cluster, replicas)?;

    // 4. Exactly one leader, and it is the Lease holder.
    // The API server restart may have cost the Lease; it is re-acquired within
    // leaseDuration.
    wait_for("a leader", Duration::from_mins(1), || {
        replicas.iter().any(|r| is_leader(r.ops) == Some(true))
    })?;
    std::thread::sleep(Duration::from_secs(5));
    let leaders: Vec<&Replica> = replicas
        .iter()
        .filter(|r| is_leader(r.ops) == Some(true))
        .collect();
    ensure!(
        leaders.len() == 1,
        "expected exactly one leader, got {}",
        leaders.len()
    );
    let holder = lease_holder(&cluster)?;
    ensure!(
        holder == leaders[0].name,
        "lease holder {holder} != leader {}",
        leaders[0].name
    );
    println!("PASS exactly one leader: {holder} (Lease holderIdentity agrees)");

    // 5. Failover. SIGKILL the leader.
    let li = replicas
        .iter()
        .position(|r| r.name == holder)
        .context("leader index")?;
    let survivor = &replicas[1 - li];
    children[li].kill_leader()?;
    let killed = Instant::now();
    let took = wait_for("failover", FAILOVER_BOUND, || {
        is_leader(survivor.ops) == Some(true)
    })?;
    ensure!(killed.elapsed() <= FAILOVER_BOUND);
    let new_holder = lease_holder(&cluster)?;
    ensure!(
        new_holder == survivor.name,
        "lease holder after failover: {new_holder}"
    );
    println!(
        "PASS failover {took:.1?} after SIGKILL of {holder} (bound {FAILOVER_BOUND:?}; spec leaseDuration 15s)"
    );

    // 6. SIGTERM the survivor. It releases the Lease and exits 0.
    let pid = children[1 - li].id().to_string();
    ensure!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()?
            .success()
    );
    let status = {
        let start = Instant::now();
        loop {
            if let Some(s) = children[1 - li].try_wait()? {
                break s;
            }
            ensure!(
                start.elapsed() < Duration::from_secs(30),
                "survivor did not exit after SIGTERM"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    ensure!(status.success(), "survivor exit: {status}");
    let released = lease_holder(&cluster)?;
    ensure!(
        released.is_empty(),
        "Lease still held by {released:?} after SIGTERM"
    );
    println!("PASS SIGTERM: exit 0 and Lease holderIdentity cleared");
    println!("runtime-verify: all checks passed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `listen:` address in a replica config.
    fn listen_addrs(yaml: &str) -> Vec<String> {
        yaml.lines()
            .skip_while(|l| *l != "listen:")
            .skip(1)
            .take_while(|l| l.starts_with("  "))
            .filter_map(|l| l.split_once(": ").map(|(_, v)| v.to_owned()))
            .collect()
    }

    /// Both replicas run on the host, so they must not share a listener:
    /// the second one would fail to bind and exit.
    #[test]
    fn replicas_bind_distinct_listeners() {
        let dir = std::env::temp_dir().join(format!("rv-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut all = Vec::new();
        for r in &replicas().unwrap() {
            let yaml = std::fs::read_to_string(write_config(&dir, r).unwrap()).unwrap();
            let addrs = listen_addrs(&yaml);
            for key in ["ops", "api", "edge"] {
                assert!(
                    yaml.contains(&format!("\n  {key}: 127.0.0.1:")),
                    "{} sets no {key} listener",
                    r.name
                );
            }
            all.extend(addrs);
        }
        let _ = std::fs::remove_dir_all(&dir);
        let mut uniq = all.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), all.len(), "shared listener in {all:?}");
    }
}
