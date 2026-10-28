//! The only place that builds `kind`, `kubectl` and `helm` commands.
//!
//! Every command runs without the inherited variables that can pick a cluster
//! (`KUBECONFIG`, `KUBERNETES_SERVICE_*`, `HELM_KUBE*`, `HELM_NAMESPACE`, ...),
//! with `KUBECONFIG` set to a private file under `target/`. kubectl and helm
//! also get `--kubeconfig` on the command line, and [`Cluster`] adds the kind
//! context, so a missing file is an error rather than a fallback to
//! `~/.kube/config`, in-cluster credentials or `HELM_KUBEAPISERVER`.

use std::cell::Cell;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::proc::Spawned;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

/// Variables that select a cluster or credentials for kubectl, helm or kind.
/// Every variable whose name starts with `KUBE` or `HELM_KUBE` is removed too.
const CLUSTER_VARS: &[&str] = &[
    "KUBECONFIG",
    "KUBERNETES_SERVICE_HOST",
    "KUBERNETES_SERVICE_PORT",
    "HELM_KUBEAPISERVER",
    "HELM_KUBETOKEN",
    "HELM_KUBECAFILE",
    "HELM_KUBECONTEXT",
    "HELM_KUBEASUSER",
    "HELM_KUBEASGROUPS",
    "HELM_KUBEINSECURE_SKIP_TLS_VERIFY",
    "HELM_KUBETLS_SERVER_NAME",
    "HELM_NAMESPACE",
    "KIND_CLUSTER_NAME",
];

fn is_cluster_var(name: &str) -> bool {
    CLUSTER_VARS.contains(&name) || name.starts_with("KUBE") || name.starts_with("HELM_KUBE")
}

/// Removes every inherited variable that could point `c` at a cluster.
/// Use it on any process that talks to the API server (`cargo test`, the
/// controller binary) as well as on the tools.
pub(crate) fn scrub(c: &mut Command) -> &mut Command {
    for name in CLUSTER_VARS {
        c.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(is_cluster_var) {
            c.env_remove(&name);
        }
    }
    c
}

/// Binds `c`, a process that finds its cluster through `KUBECONFIG` (the
/// controller binary), to `kubeconfig` alone.
pub(crate) fn with_kubeconfig<'a>(c: &'a mut Command, kubeconfig: &Path) -> &'a mut Command {
    scrub(c).env("KUBECONFIG", kubeconfig)
}

/// The variable the kind-backed tests in `crates/server/tests` read their
/// kubeconfig from. They never fall back to `KUBECONFIG` or `~/.kube/config`.
pub(crate) const TEST_KUBECONFIG: &str = "BK_TEST_KUBECONFIG";

fn tool(program: &str, kubeconfig: &Path) -> Command {
    let mut c = Command::new(program);
    with_kubeconfig(&mut c, kubeconfig);
    // kind takes --kubeconfig only on some subcommands; its callers pass it.
    if program != "kind" {
        c.arg("--kubeconfig").arg(kubeconfig);
    }
    c
}

/// `helm` bound to `kubeconfig`, for commands that need no cluster (`helm
/// template`, `helm pull`) with [`sandbox`]. Cluster commands use [`Cluster::helm`].
pub(crate) fn helm(kubeconfig: &Path) -> Command {
    tool("helm", kubeconfig)
}

/// A kubeconfig path that is never created, for commands that need no cluster
/// (`kind load`, `helm template`). Pointing them here keeps them off `~/.kube/config`.
pub(crate) fn sandbox() -> PathBuf {
    crate::root().join("target/kube-sandbox/kubeconfig")
}

/// `kind load docker-image <image> --name <cluster>`.
pub(crate) fn kind_load(image: &str, cluster: &str) -> Command {
    let mut c = tool("kind", &sandbox());
    c.args(["load", "docker-image", image, "--name", cluster]);
    c
}

/// FNV-1a of `bytes`. Unlike `DefaultHasher`, it is the same in every Rust release.
pub(crate) fn fnv1a(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5_u32, |h, b| {
        (h ^ u32::from(*b)).wrapping_mul(0x0100_0193)
    })
}

/// Two loopback host ports a kind cluster publishes, for an API and an edge listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HostPorts {
    pub(crate) api: u16,
    pub(crate) edge: u16,
}

impl HostPorts {
    /// Suffixed runs get a port pair in `[first, first + 2 * PAIRS)`.
    const PAIRS: u32 = 1000;

    /// `default` without `BK_KIND_SUFFIX`, else the suffix's pair above `first`.
    pub(crate) fn get(default: Self, first: u16) -> Self {
        Self::for_suffix(
            default,
            first,
            std::env::var("BK_KIND_SUFFIX").ok().as_deref(),
        )
    }

    /// A stable pair per suffix, so two checkouts with different suffixes run at
    /// once and a task gives the same URL every time.
    fn for_suffix(default: Self, first: u16, suffix: Option<&str>) -> Self {
        let Some(s) = suffix.filter(|s| !s.is_empty()) else {
            return default;
        };
        // hash % PAIRS < 1000, so the pair stays below first + 2000.
        let pair = u16::try_from(fnv1a(s.as_bytes()) % Self::PAIRS).unwrap_or(0);
        let api = first + 2 * pair;
        Self { api, edge: api + 1 }
    }

    /// Fails when another process holds one of the ports, naming it, instead of
    /// letting kind fail later with "port is already allocated".
    pub(crate) fn check_free(self, cluster: &str) -> Result<()> {
        for port in [self.api, self.edge] {
            std::net::TcpListener::bind(("127.0.0.1", port)).with_context(|| {
                format!(
                    "host port {port} for {cluster} is taken; delete that cluster or pick another BK_KIND_SUFFIX"
                )
            })?;
        }
        Ok(())
    }
}

/// Clusters to delete if the process is interrupted: (name, kubeconfig).
static LIVE: Mutex<Vec<(String, PathBuf)>> = Mutex::new(Vec::new());

fn delete_cluster(name: &str, kubeconfig: &Path) {
    let _ = tool("kind", kubeconfig)
        .args(["delete", "cluster", "--name", name, "--kubeconfig"])
        .arg(kubeconfig)
        .status();
}

/// A throwaway kind cluster with a private kubeconfig. Dropping it deletes the
/// cluster, and so does SIGINT or SIGTERM (see [`crate::proc::on_signal`]).
pub(crate) struct Cluster {
    name: String,
    kubeconfig: PathBuf,
    keep: Cell<bool>,
}

impl Cluster {
    /// A handle on the kind cluster `name` whose kubeconfig is `kubeconfig`.
    /// Nothing is created until [`Cluster::create`].
    pub(crate) fn new(name: &str, kubeconfig: PathBuf) -> Self {
        crate::proc::on_signal();
        if let Ok(mut live) = LIVE.lock() {
            live.push((name.to_owned(), kubeconfig.clone()));
        }
        Self {
            name: name.to_owned(),
            kubeconfig,
            keep: Cell::new(false),
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn kubeconfig(&self) -> &Path {
        &self.kubeconfig
    }

    /// The kubeconfig context kind writes for this cluster.
    fn context(&self) -> String {
        format!("kind-{}", self.name)
    }

    /// Deletes the cluster if it exists.
    pub(crate) fn delete(&self) {
        delete_cluster(&self.name, &self.kubeconfig);
    }

    /// Deletes any leftover cluster of this name, then creates it, with the
    /// kind config file `config` if given, waiting up to `wait` for the node.
    pub(crate) fn create(&self, config: Option<&Path>, wait: &str) -> Result<()> {
        self.delete();
        let mut c = tool("kind", &self.kubeconfig);
        c.args(["create", "cluster", "--name", &self.name, "--wait", wait]);
        if let Some(cfg) = config {
            c.arg("--config").arg(cfg);
        }
        // In its own group, so an interrupt stops it before the cluster is deleted.
        let out = crate::proc::run(c.arg("--kubeconfig").arg(&self.kubeconfig))
            .context("running kind create cluster")?;
        ensure!(
            out.status.success(),
            "kind create cluster {}: {}",
            self.name,
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(())
    }

    /// Leave the cluster running when this handle is dropped.
    pub(crate) fn keep(&self) {
        self.keep.set(true);
        self.forget();
    }

    fn forget(&self) {
        if let Ok(mut live) = LIVE.lock() {
            live.retain(|(n, _)| *n != self.name);
        }
    }

    /// `kubectl` with this cluster's kubeconfig and context.
    pub(crate) fn kubectl_cmd(&self) -> Command {
        let mut c = tool("kubectl", &self.kubeconfig);
        c.arg("--context").arg(self.context());
        c
    }

    /// `helm` with this cluster's kubeconfig and context.
    pub(crate) fn helm_cmd(&self) -> Command {
        let mut c = tool("helm", &self.kubeconfig);
        c.arg("--kube-context").arg(self.context());
        c
    }

    /// Runs kubectl with `args` and `stdin` (empty when `None`), whatever its exit status.
    pub(crate) fn kubectl_raw(&self, args: &[&str], stdin: Option<&str>) -> Result<Output> {
        let mut child = Spawned::spawn(
            self.kubectl_cmd()
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )
        .context("spawning kubectl")?;
        let mut pipe = child.take_stdin().context("kubectl stdin")?;
        pipe.write_all(stdin.unwrap_or_default().as_bytes())?;
        drop(pipe);
        child.wait_with_output().context("waiting for kubectl")
    }

    /// kubectl's stdout; fails with its output when it exits non-zero.
    pub(crate) fn kubectl(&self, args: &[&str]) -> Result<String> {
        self.kubectl_in(args, None)
    }

    /// [`Cluster::kubectl`] with `stdin`.
    pub(crate) fn kubectl_in(&self, args: &[&str], stdin: Option<&str>) -> Result<String> {
        checked("kubectl", args, &self.kubectl_raw(args, stdin)?)
    }

    /// `kubectl apply -f -` of `yaml`.
    pub(crate) fn apply(&self, yaml: &str) -> Result<()> {
        self.kubectl_in(&["apply", "-f", "-"], Some(yaml)).map(drop)
    }

    /// helm's stdout; fails with its output when it exits non-zero.
    pub(crate) fn helm(&self, args: &[&str]) -> Result<String> {
        let out = crate::proc::run(self.helm_cmd().args(args)).context("running helm")?;
        checked("helm", args, &out)
    }

    /// `cargo test` of the ignored kind-backed test `name` in `crates/server/tests`,
    /// pointed at this cluster through [`TEST_KUBECONFIG`] only.
    pub(crate) fn server_test(&self, name: &str) -> Command {
        let mut c = Command::new("cargo");
        scrub(&mut c)
            .current_dir(crate::root())
            .env(TEST_KUBECONFIG, &self.kubeconfig)
            .args([
                "test",
                "--locked",
                "-p",
                "buildkit-controller",
                "--test",
                name,
            ])
            .args(["--", "--ignored", "--nocapture"]);
        c
    }

    /// Polls until `resource` (e.g. `crd/x`, `buildkitpool/y`) reports condition `kind`=True.
    ///
    /// Use this instead of `kubectl wait --for=condition=...` on an object that was just
    /// applied: before the API server writes any status, kubectl exits with
    /// `.status.conditions accessor error` rather than waiting.
    pub(crate) fn wait_condition(
        &self,
        resource: &str,
        namespace: Option<&str>,
        kind: &str,
        timeout: Duration,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut args = vec!["get", resource, "-o", "json"];
        if let Some(ns) = namespace {
            args.extend(["-n", ns]);
        }
        loop {
            let out = self.kubectl_raw(&args, None)?;
            let last = if out.status.success() {
                let obs = observe(&String::from_utf8_lossy(&out.stdout), kind)
                    .with_context(|| format!("parsing `kubectl get {resource} -o json`"))?;
                if obs.is_met() {
                    return Ok(());
                }
                obs.to_string()
            } else {
                format!(
                    "kubectl get failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )
            };
            if Instant::now() >= deadline {
                bail!("{resource} not {kind} after {timeout:?}; last observed: {last}");
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Waits for the project's CRD to be Established.
    pub(crate) fn wait_crd_established(&self) -> Result<()> {
        self.wait_condition(
            "crd/buildkitpools.buildkit.smrt-devops.net",
            None,
            "Established",
            Duration::from_mins(1),
        )
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        if self.keep.get() {
            return;
        }
        delete_cluster(&self.name, &self.kubeconfig);
        self.forget();
    }
}

/// Deletes every cluster still alive. Called once, on SIGINT or SIGTERM.
pub(crate) fn delete_live_clusters() {
    let live = LIVE.lock().map(|mut l| std::mem::take(&mut *l));
    for (name, kubeconfig) in live.unwrap_or_default() {
        eprintln!("deleting kind cluster {name}");
        delete_cluster(&name, &kubeconfig);
    }
}

/// The stdout of a finished `program` run; fails with stdout and stderr when it exited non-zero.
fn checked(program: &str, args: &[&str], out: &Output) -> Result<String> {
    ensure!(
        out.status.success(),
        "{program} {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether controller logs contain a Kubernetes API authorization denial.
/// Matches the kube error text, not a bare "403", which also appears in
/// microsecond timestamps, UIDs and resourceVersions.
pub(crate) fn shows_rbac_denial(logs: &str) -> bool {
    rbac_denials(logs).next().is_some()
}

/// The lines of `logs` that show a Kubernetes API authorization denial.
pub(crate) fn rbac_denials(logs: &str) -> impl Iterator<Item = &str> {
    logs.lines().filter(|l| {
        ["Forbidden", "is forbidden", "\"code\":403", "code: 403"]
            .iter()
            .any(|needle| l.contains(needle))
    })
}

/// What one `kubectl get -o json` says about a single condition.
#[derive(Debug, PartialEq, Eq)]
enum Observed {
    /// The object has no `status` (or no `status.conditions`) yet.
    NoConditions,
    /// `status.conditions` exists but has no entry of the wanted type; holds the types seen.
    Missing(Vec<String>),
    /// The condition's `status` field.
    Status(String),
}

impl Observed {
    fn is_met(&self) -> bool {
        matches!(self, Observed::Status(s) if s == "True")
    }
}

impl std::fmt::Display for Observed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Observed::NoConditions => write!(f, "no status.conditions yet"),
            Observed::Missing(seen) => write!(f, "condition absent (have: {})", seen.join(", ")),
            Observed::Status(s) => write!(f, "status={s}"),
        }
    }
}

/// Reads condition `kind` out of an object's JSON. Missing status or conditions is
/// "not yet", never an error: `kubectl wait` fails hard on that window instead of waiting.
fn observe(json: &str, kind: &str) -> Result<Observed> {
    let v: serde_json::Value = serde_json::from_str(json).with_context(|| {
        let head: String = json.chars().take(200).collect();
        format!("not JSON: {head:?}")
    })?;
    let Some(conds) = v.pointer("/status/conditions").and_then(|c| c.as_array()) else {
        return Ok(Observed::NoConditions);
    };
    let found = conds.iter().find(|c| {
        c["type"]
            .as_str()
            .is_some_and(|t| t.eq_ignore_ascii_case(kind))
    });
    Ok(match found {
        Some(c) => Observed::Status(c["status"].as_str().unwrap_or("").to_owned()),
        None => Observed::Missing(
            conds
                .iter()
                .filter_map(|c| c["type"].as_str().map(ToOwned::to_owned))
                .collect(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a command does with one environment variable.
    #[derive(Debug, PartialEq)]
    enum Var {
        Inherited,
        Removed,
        Set(std::ffi::OsString),
    }

    fn env_of(c: &Command, key: &str) -> Var {
        match c.get_envs().find(|(k, _)| *k == key) {
            None => Var::Inherited,
            Some((_, None)) => Var::Removed,
            Some((_, Some(v))) => Var::Set(v.to_owned()),
        }
    }

    fn args_of(c: &Command) -> Vec<String> {
        c.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn assert_scrubbed(name: &str, c: &Command) {
        for var in CLUSTER_VARS.iter().filter(|v| **v != "KUBECONFIG") {
            assert_eq!(env_of(c, var), Var::Removed, "{name} keeps {var}");
        }
    }

    #[test]
    fn every_builder_scrubs_the_environment_and_names_the_kubeconfig() {
        let kc = Path::new("/tmp/private/kubeconfig");
        let cluster = Cluster {
            name: "bk-x".into(),
            kubeconfig: kc.to_path_buf(),
            keep: Cell::new(true),
        };
        for (name, c, context_flag) in [
            ("kubectl", cluster.kubectl_cmd(), "--context"),
            ("helm", cluster.helm_cmd(), "--kube-context"),
        ] {
            assert_scrubbed(name, &c);
            assert_eq!(env_of(&c, "KUBECONFIG"), Var::Set(kc.into()), "{name}");
            let args = args_of(&c);
            assert_eq!(
                args[..4],
                [
                    "--kubeconfig",
                    "/tmp/private/kubeconfig",
                    context_flag,
                    "kind-bk-x"
                ],
                "{name}"
            );
        }
        let sandboxed = helm(&sandbox());
        assert_scrubbed("helm sandbox", &sandboxed);
        assert_eq!(args_of(&sandboxed)[0], "--kubeconfig");
        let load = kind_load("img", "c");
        assert_scrubbed("kind load", &load);
        assert_eq!(
            env_of(&load, "KUBECONFIG"),
            Var::Set(sandbox().into_os_string())
        );
        assert!(sandbox().components().any(|c| c.as_os_str() == "target"));
    }

    #[test]
    fn scrub_removes_inherited_cluster_variables() {
        assert!(is_cluster_var("KUBECONFIG"));
        assert!(is_cluster_var("KUBERNETES_SERVICE_HOST"));
        assert!(is_cluster_var("KUBECTL_EXTERNAL_DIFF"));
        assert!(is_cluster_var("HELM_KUBETOKEN"));
        assert!(is_cluster_var("HELM_KUBESOMETHING_NEW"));
        assert!(is_cluster_var("HELM_NAMESPACE"));
        assert!(!is_cluster_var("HELM_CACHE_HOME"));
        assert!(!is_cluster_var("PATH"));
        let mut c = Command::new("true");
        scrub(&mut c);
        assert_scrubbed("scrub", &c);
        assert_eq!(env_of(&c, "KUBECONFIG"), Var::Removed);
    }

    /// Lines of every `.rs` file under `dir`, with the file name.
    fn sources(dir: &Path) -> Vec<(PathBuf, String)> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                out.extend(sources(&p));
            } else if p.extension().is_some_and(|e| e == "rs") {
                out.push((p.clone(), std::fs::read_to_string(&p).unwrap()));
            }
        }
        out
    }

    /// Cluster tools are built only here, and nothing else picks a kubeconfig.
    #[test]
    fn cluster_tools_are_built_only_in_kube_rs() {
        let xtask = Path::new(env!("CARGO_MANIFEST_DIR"));
        let tests = xtask.join("../crates/server/tests");
        for (p, text) in sources(&xtask.join("src"))
            .into_iter()
            .chain(sources(&tests))
        {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            if name == "kube.rs" || name == "kind.rs" {
                continue;
            }
            for tool in ["kind", "kubectl", "helm"] {
                let needle = format!("Command::new(\"{tool}\")");
                assert!(!text.contains(&needle), "{} builds {tool}", p.display());
            }
            for needle in ["\"--kubeconfig\"", "\"KUBECONFIG\"", "\"--context\""] {
                assert!(!text.contains(needle), "{} sets {needle}", p.display());
            }
        }
    }

    /// The kind-backed server tests take their cluster only from the harness.
    #[test]
    fn server_tests_never_use_the_default_kubeconfig() {
        let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/server/tests");
        for (p, text) in sources(&tests) {
            for needle in ["try_default", "Config::infer", "from_kubeconfig("] {
                assert!(!text.contains(needle), "{} uses {needle}", p.display());
            }
        }
    }

    #[test]
    fn host_ports_are_fixed_without_a_suffix_and_stable_per_suffix() {
        const D: HostPorts = HostPorts {
            api: 18443,
            edge: 18444,
        };
        assert_eq!(HostPorts::for_suffix(D, 20000, None), D);
        assert_eq!(HostPorts::for_suffix(D, 20000, Some("")), D);
        let a = HostPorts::for_suffix(D, 20000, Some("cidrs1"));
        let b = HostPorts::for_suffix(D, 20000, Some("cidrs2"));
        assert_eq!(a, HostPorts::for_suffix(D, 20000, Some("cidrs1")));
        assert_ne!(a, b);
        for p in [a, b] {
            assert!(
                p.api >= 20000 && p.edge == p.api + 1 && p.edge < 22000,
                "{p:?}"
            );
            assert!(p.api % 2 == 0, "pairs never overlap: {p:?}");
        }
        let c = HostPorts::for_suffix(D, 22000, Some("cidrs1"));
        assert_eq!(c.api, a.api + 2000, "ranges per task never overlap");
    }

    #[test]
    fn fnv1a_matches_the_reference_values() {
        assert_eq!(fnv1a(b""), 0x811c_9dc5);
        assert_eq!(fnv1a(b"a"), 0xe40c_292c);
    }

    #[test]
    fn rbac_denials_ignore_bare_403_digits() {
        let logs = "2026-10-20T10:52:21.403512Z  INFO reconcile pool=p uid=4031-aa rv=\"94037\"";
        assert!(!shows_rbac_denial(logs));
        assert!(shows_rbac_denial(
            r#"pods "x" is forbidden: User "system:serviceaccount:ns:sa" cannot list"#
        ));
        assert!(shows_rbac_denial(r#"{"kind":"Status","code":403}"#));
        assert!(shows_rbac_denial("ApiError: Forbidden (ErrorResponse)"));
        assert!(shows_rbac_denial("error: ErrorResponse { code: 403 }"));
        assert_eq!(rbac_denials("a\nb is forbidden\nc").count(), 1);
    }

    #[test]
    fn missing_status_is_not_yet() {
        let o = observe(r#"{"metadata":{"name":"x"}}"#, "Established").unwrap();
        assert_eq!(o, Observed::NoConditions);
        assert!(!o.is_met());
        let o = observe(r#"{"status":{}}"#, "Established").unwrap();
        assert_eq!(o, Observed::NoConditions);
    }

    #[test]
    fn absent_condition_lists_what_exists() {
        let j = r#"{"status":{"conditions":[{"type":"NamesAccepted","status":"True"}]}}"#;
        let o = observe(j, "Established").unwrap();
        assert_eq!(o, Observed::Missing(vec!["NamesAccepted".into()]));
        assert!(!o.is_met());
    }

    #[test]
    fn false_is_not_met_and_true_is() {
        let f = r#"{"status":{"conditions":[{"type":"Established","status":"False"}]}}"#;
        let o = observe(f, "Established").unwrap();
        assert_eq!(o, Observed::Status("False".into()));
        assert!(!o.is_met());
        let t = r#"{"status":{"conditions":[{"type":"NamesAccepted","status":"True"},{"type":"Established","status":"True"}]}}"#;
        assert!(observe(t, "established").unwrap().is_met());
    }

    #[test]
    fn bad_json_is_an_error_that_quotes_the_input() {
        let e = observe("not json", "Ready").unwrap_err();
        assert!(format!("{e:#}").contains("\"not json\""), "{e:#}");
    }
}
