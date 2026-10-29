//! `cargo xtask kind-all` runs every kind suite in one go: the PR gate.
//!
//! It builds what the suites share once (the controller image, skipped when
//! its inputs are unchanged, the debug controller binary and the kind-backed
//! test binaries), creates two clusters, loads the images once, and runs each
//! suite as `cargo xtask <suite>` with `BK_KIND_CLUSTER` naming its cluster.
//!
//! - `bk-all`, the shared cluster, runs [`SHARED`] side by side. Each suite
//!   keeps to its own namespaces, release names, `NodePorts` and Gateway, and
//!   every controller on it watches only its suite's namespaces. Its kind
//!   config is the union of the suites' (`xtask/fixtures/kind-all/kind.yaml`),
//!   and Envoy Gateway is installed on it once.
//! - `bk-solo` runs [`SOLO`] one after the other, because they cannot share.
//!
//! Each suite's output goes to `target/kind-all/<suite>.log`. Soak and perf
//! measure a quiet cluster over long windows: they run only with `--nightly`
//! (or `--only`), each on its own cluster once the others are done and their
//! clusters deleted. The two shared clusters are deleted at the end unless
//! `--keep`; `--down` deletes kept ones. Cluster names get `-$BK_KIND_SUFFIX` when that is set.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::kube::{CLUSTER_VAR, Cluster};
use crate::proc::{Spawned, Timed, status};

/// Suites on the shared cluster, longest first by measured time, so the slow ones
/// start first when `--jobs` limits how many run at once.
pub(crate) const SHARED: &[&str] = &[
    "reconcile-verify",
    "alloc-verify",
    "e2e",
    "drain-verify",
    "proxy-verify",
    "edge-verify",
    "api-verify",
    "helm-verify",
    "registry-verify",
    "crd-verify",
    "render-verify",
];

/// Suites that cannot share a cluster, run in this order on the second one.
/// runtime-verify restarts and silences the API server and needs the CRDs
/// absent at its start. claim-verify runs a cluster-wide controller, which
/// would answer every other suite's claims and reconcile their pools.
pub(crate) const SOLO: &[&str] = &["runtime-verify", "claim-verify"];

/// The long runs, each on a cluster of its own, after the others.
pub(crate) const NIGHTLY: &[&str] = &["soak", "perf"];

/// What `kind-all` was asked to do.
pub(crate) struct Opts {
    /// Run only these suites; empty runs all.
    pub(crate) only: Vec<String>,
    /// Leave the clusters running.
    pub(crate) keep: bool,
    /// Delete the clusters a `--keep` run left, and run nothing.
    pub(crate) down: bool,
    /// How many shared-cluster suites run at once; 0 is all of them.
    pub(crate) jobs: usize,
    /// Also run [`NIGHTLY`].
    pub(crate) nightly: bool,
}

/// One suite's outcome.
struct Outcome {
    suite: &'static str,
    cluster: String,
    took: Duration,
    passed: bool,
    log: PathBuf,
}

pub(crate) fn kind_all(opts: &Opts) -> Result<()> {
    let start = Instant::now();
    let work = crate::root()
        .join("target")
        .join(crate::kind_cluster("kind-all"));
    std::fs::create_dir_all(&work)?;
    let main = Cluster::new(&crate::kind_cluster("bk-all"), work.join("main/kubeconfig"));
    let solo = Cluster::new(
        &crate::kind_cluster("bk-solo"),
        work.join("solo/kubeconfig"),
    );
    if opts.down {
        return Ok(());
    }
    if opts.keep {
        main.keep();
        solo.keep();
    }
    for name in &opts.only {
        if ![SHARED, SOLO, NIGHTLY]
            .iter()
            .any(|l| l.contains(&name.as_str()))
        {
            bail!("no kind suite {name}; the suites are {SHARED:?}, {SOLO:?} and {NIGHTLY:?}");
        }
    }
    let pick = |list: &[&'static str], by_default: bool| -> Vec<&'static str> {
        list.iter()
            .copied()
            .filter(|s| (opts.only.is_empty() && by_default) || opts.only.iter().any(|o| o == s))
            .collect()
    };
    let (shared, alone) = (pick(SHARED, true), pick(SOLO, true));
    let nightly = pick(NIGHTLY, opts.nightly);

    setup(&main, &solo, &work, !shared.is_empty(), !alone.is_empty())?;
    println!(
        "kind-all: clusters ready after {}s",
        start.elapsed().as_secs()
    );

    let outcomes = Mutex::new(Vec::new());
    let queue = Mutex::new(shared.iter().copied().collect::<VecDeque<_>>());
    let jobs = if opts.jobs == 0 {
        shared.len()
    } else {
        opts.jobs
    };
    std::thread::scope(|s| {
        s.spawn(|| {
            for suite in &alone {
                let o = run_suite(suite, Some(&solo), &work);
                lock(&outcomes).push(o);
            }
        });
        for _ in 0..jobs {
            s.spawn(|| {
                loop {
                    let Some(suite) = lock(&queue).pop_front() else {
                        break;
                    };
                    let o = run_suite(suite, Some(&main), &work);
                    lock(&outcomes).push(o);
                }
            });
        }
    });
    // The long runs get the host to themselves.
    drop(main);
    drop(solo);
    std::thread::scope(|s| {
        for suite in &nightly {
            s.spawn(|| {
                let o = run_suite(suite, None, &work);
                lock(&outcomes).push(o);
            });
        }
    });

    let outcomes = outcomes
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    report(&outcomes, start.elapsed())
}

/// Builds what the suites share and creates and prepares the clusters that
/// will be used, all at once where they do not depend on each other.
fn setup(
    main: &Cluster,
    solo: &Cluster,
    work: &Path,
    use_main: bool,
    use_solo: bool,
) -> Result<()> {
    std::thread::scope(|s| {
        let built = s.spawn(prebuild);
        let main_up = use_main.then(|| s.spawn(|| create_main(main, work)));
        let solo_up = use_solo.then(|| s.spawn(|| solo.create(None, "120s")));
        join(built)?;
        if let Some(t) = main_up {
            join(t)?;
        }
        if let Some(t) = solo_up {
            join(t)?;
        }
        Ok::<_, anyhow::Error>(())
    })?;
    std::thread::scope(|s| {
        let main_ready = use_main.then(|| s.spawn(|| prepare_main(main, work)));
        let solo_ready = use_solo.then(|| s.spawn(|| prepare_solo(solo)));
        if let Some(t) = main_ready {
            join(t)?;
        }
        if let Some(t) = solo_ready {
            join(t)?;
        }
        Ok(())
    })
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn join<T>(t: std::thread::ScopedJoinHandle<'_, Result<T>>) -> Result<T> {
    t.join()
        .map_err(|_| anyhow::anyhow!("a kind-all setup thread panicked"))?
}

/// Builds everything the suites would otherwise each build: the controller
/// image, the pinned images, the kind-backed test binaries and the debug
/// controller the host-run suites start.
fn prebuild() -> Result<()> {
    let _t = Timed::named("prebuild");
    crate::image::controller()?;
    crate::image::ensure_worker()?;
    crate::image::ensure(crate::e2e::REGISTRY_IMAGE)?;
    crate::gateway::ensure_image(crate::gateway::SOCAT.0, crate::gateway::SOCAT.1)?;
    let mut c = Command::new("cargo");
    crate::kube::scrub(&mut c)
        .current_dir(crate::root())
        .args(["test", "--locked", "-p", "buildkit-controller", "--no-run"])
        .args(["--test", "alloc_verify", "--test", "api_verify"])
        .args(["--test", "reconcile_verify"]);
    status(&mut c)?;
    crate::build_controller().map(drop)
}

/// Creates the shared cluster with the union kind config.
fn create_main(main: &Cluster, work: &Path) -> Result<()> {
    let policies = work.join("policies");
    std::fs::create_dir_all(&policies)?;
    std::fs::write(
        policies.join("audit.yaml"),
        crate::reconcile_verify::AUDIT_POLICY,
    )?;
    let (e2e, drain) = (*crate::e2e::PORTS, *crate::drain_verify::PORTS);
    main.delete();
    e2e.check_free(main.name())?;
    drain.check_free(main.name())?;
    let config = crate::fixture(
        "kind-all/kind.yaml",
        work,
        &[
            ("E2E_API", e2e.api.to_string()),
            ("E2E_EDGE", e2e.edge.to_string()),
            ("DRAIN_API", drain.api.to_string()),
            ("DRAIN_EDGE", drain.edge.to_string()),
            ("POLICY_DIR", policies.display().to_string()),
        ],
    )?;
    main.create(Some(&config), "180s")
}

/// Loads the images every shared suite uses, applies the CRDs and installs
/// Envoy Gateway, so no suite does any of it twice.
fn prepare_main(main: &Cluster, work: &Path) -> Result<()> {
    crate::image::load_controller(main)?;
    for image in [
        crate::image::WORKER_IMAGE,
        crate::e2e::REGISTRY_IMAGE,
        crate::gateway::SOCAT.0,
    ] {
        status(&mut crate::kube::kind_load(image, main.name()))?;
    }
    main.apply_crds()?;
    crate::gateway::install_envoy_gateway(main, work)
}

/// Loads the images of the solo suites. The CRDs stay off: runtime-verify
/// applies them itself, late on purpose.
fn prepare_solo(solo: &Cluster) -> Result<()> {
    crate::image::load_controller(solo)?;
    status(&mut crate::kube::kind_load(
        crate::image::WORKER_IMAGE,
        solo.name(),
    ))
}

/// Runs `cargo xtask <suite>` on `cluster`, or on a cluster of its own when
/// `None`, its output in `<work>/<suite>.log`.
fn run_suite(suite: &'static str, cluster: Option<&Cluster>, work: &Path) -> Outcome {
    let log = work.join(format!("{suite}.log"));
    let started = Instant::now();
    let name = cluster.map_or_else(
        || crate::kind_cluster(&format!("bk-{suite}")),
        |c| c.name().to_owned(),
    );
    println!("kind-all: {suite} started on {name}");
    let passed = spawn_suite(suite, cluster, &log).unwrap_or_else(|e| {
        eprintln!("kind-all: {suite}: {e:#}");
        false
    });
    let took = started.elapsed();
    println!(
        "kind-all: {suite} {} in {}s",
        if passed { "passed" } else { "FAILED" },
        took.as_secs()
    );
    Outcome {
        suite,
        cluster: name,
        took,
        passed,
        log,
    }
}

fn spawn_suite(suite: &str, cluster: Option<&Cluster>, log: &Path) -> Result<bool> {
    let exe = std::env::current_exe().context("finding the xtask binary")?;
    let out = std::fs::File::create(log).with_context(|| log.display().to_string())?;
    let mut c = Command::new(exe);
    c.arg(suite).stdout(out.try_clone()?).stderr(out);
    match cluster {
        Some(cl) => c.env(CLUSTER_VAR, cl.name()),
        None => c.env_remove(CLUSTER_VAR),
    };
    Ok(Spawned::spawn(&mut c)?.wait()?.success())
}

/// Prints one line per suite and the tail of each failed suite's log.
fn report(outcomes: &[Outcome], total: Duration) -> Result<()> {
    println!("\nkind-all results:");
    let mut sorted: Vec<&Outcome> = outcomes.iter().collect();
    sorted.sort_by_key(|o| std::cmp::Reverse(o.took));
    for o in &sorted {
        println!(
            "  {:<18} {:<6} {:>5}s  {}",
            o.suite,
            if o.passed { "pass" } else { "FAIL" },
            o.took.as_secs(),
            o.cluster
        );
    }
    println!("kind-all: {}s in total", total.as_secs());
    let failed: Vec<&Outcome> = sorted.into_iter().filter(|o| !o.passed).collect();
    for o in &failed {
        let text = std::fs::read_to_string(&o.log).unwrap_or_default();
        let tail: Vec<&str> = text.lines().rev().take(40).collect();
        eprintln!("\n--- {} (last 40 lines of {})", o.suite, o.log.display());
        for l in tail.iter().rev() {
            eprintln!("{l}");
        }
    }
    if !failed.is_empty() {
        let names: Vec<&str> = failed.iter().map(|o| o.suite).collect();
        bail!("kind-all: {} failed: {names:?}", failed.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{NIGHTLY, SHARED, SOLO};
    use clap::CommandFactory as _;

    /// A new kind suite cannot be left out of the PR gate by accident: every
    /// task that names a kind suite is in kind-all, or does not run a suite.
    #[test]
    fn every_kind_suite_is_in_kind_all() {
        let not_suites = [
            "root",
            "crdgen",
            "image",
            "helm-test",
            "docs",
            "e2e-up",
            "kind-all",
            "help",
        ];
        for task in crate::Cli::command().get_subcommands() {
            let name = task.get_name();
            assert!(
                not_suites.contains(&name)
                    || [SHARED, SOLO, NIGHTLY].iter().any(|l| l.contains(&name)),
                "{name} is not in kind-all"
            );
        }
        for s in SHARED.iter().chain(SOLO).chain(NIGHTLY) {
            assert!(
                crate::Cli::command().find_subcommand(s).is_some(),
                "{s} is not a task"
            );
        }
    }
}
