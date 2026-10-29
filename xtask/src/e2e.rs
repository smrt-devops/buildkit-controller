//! `cargo xtask e2e` runs the end-to-end suite on a kind cluster `bk-e2e`.
//!
//! The task installs the controller from the Helm chart. A mock OIDC issuer
//! runs in this process (trusted through the issuer's `caBundleConfigMap`) and
//! a `registry:2` runs in the cluster beside the chart's built-in cache registry. The clients are the real recipe
//! `docs/ci/buildkit-allocate.sh` and real `docker buildx` on the host. They go
//! through the real API and mTLS edge behind Envoy Gateway (see `gateway.rs`),
//! installed with the chart's default Gateway API exposure: an `HTTPRoute` with a
//! `BackendTLSPolicy` for the API and a `TLSRoute` for the edge. kind publishes the
//! Gateway's listeners (`NodePorts`) on two loopback host ports, 18443 and 18444
//! by default (see [`PORTS`]). `*.localhost` resolves to loopback, which makes
//! per-allocation endpoints (`<id>.localhost:<edge port>`) reachable.
//!
//! Every kind, kubectl and helm call gets the private kubeconfig under
//! `target/e2e` (`target/e2e-<suffix>` with `BK_KIND_SUFFIX`). The task never
//! touches `~/.kube/config`, and `e2e` deletes the cluster on every exit path.
//! `e2e-up` installs the same cluster trusting GitHub's OIDC issuer and leaves
//! it running, for the Action job in CI. It writes the API URL to `api-url` in
//! the work directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};

use crate::e2e_issuer::Issuer;
use crate::image::{WORKER_IMAGE, copy_busybox};
use crate::kube::{Cluster, HostPorts};
use crate::proc::{Spawned, host_platform, output, status, until, write};
use crate::root;

const NS: &str = "bk-system";
const POOL_NS: &str = "ci";
const OIDC_HOST: &str = "mock-oidc.bk-oidc.svc";
/// The API URL clients use, through the Gateway.
/// The loopback host ports kind publishes the Gateway's API and edge listeners
/// on: 18443 and 18444 without `BK_KIND_SUFFIX` (CI), else a pair in 20000-21999.
pub(crate) static PORTS: std::sync::LazyLock<HostPorts> = std::sync::LazyLock::new(|| {
    HostPorts::get(
        HostPorts {
            api: 18443,
            edge: 18444,
        },
        20000,
    )
});
static API: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| format!("https://localhost:{}", PORTS.api));
pub(crate) const REGISTRY_IMAGE: &str = "registry:2";
const REG: &str = "registry.e2e-reg.svc.cluster.local:5000";
/// The chart's cache registry, as workers reach it.
const CACHE_HOST: &str = "buildkit-controller-registry.bk-system.svc:5000";
/// A cache volume as v0.1.0 labelled it, created before install; the leader
/// deletes it. v0.1.0 upgrade shim: remove in the release after 0.2.0.
const OLD_VOLUME: &str = "v010-volume";
const GITHUB_ISSUER: &str = "https://token.actions.githubusercontent.com";
const AUD: &str = "buildkit-controller";

/// Which issuer the controller trusts.
enum Mode<'a> {
    /// The in-process mock issuer.
    Mock(&'a Issuer),
    /// GitHub's, for the repository `owner/name`.
    Github(&'a str),
}

/// Kind's pod and service ranges, set in `xtask/fixtures/e2e/kind.yaml`. The
/// controller lists them as cluster ranges, and the pools open them (the
/// in-cluster registry) through `workerNetwork.permittedCIDRs`.
const KIND_CIDRS: &str = "10.244.0.0/16, 10.96.0.0/16";

/// The work directory: `target/e2e`, or `target/e2e-<suffix>`, so two runs from
/// one checkout keep their kubeconfigs and files apart.
fn work_dir() -> PathBuf {
    let name = super::kind_cluster("e2e");
    root().join("target").join(name)
}

/// The e2e cluster: `bk-e2e`, plus `-$BK_KIND_SUFFIX` when that is set, or the
/// cluster named by `BK_KIND_CLUSTER`.
fn cluster(work: &Path) -> Result<Cluster> {
    Cluster::for_suite("bk-e2e", work)
}

fn pool_yaml(name: &str, platforms: &str, rules: &str) -> String {
    format!(
        "  - name: {name}
    namespace: {POOL_NS}
    spec:
      platforms: [{platforms}]
      scaling: {{min: 1, max: 1}}
      network:
        egress: {{allowCIDRs: [{KIND_CIDRS}]}}
      workers:
        buildkitdConfig: |
          [registry.\"{REG}\"]
            http = true
            insecure = true
      access:
        rules:
{rules}
      template:
        spec:
          containers:
            - name: buildkitd
              image: {WORKER_IMAGE}
              imagePullPolicy: Never
              resources:
                requests: {{cpu: 100m, memory: 256Mi, ephemeral-storage: 256Mi}}
                limits: {{memory: 3Gi, ephemeral-storage: 10Gi}}
"
    )
}

#[allow(clippy::too_many_lines, reason = "one linear install script")]
/// Create the cluster and install the chart with its pools. Returns the API CA
/// file (the client CA, which signs the API serving certificate).
fn install(env: &Cluster, work: &Path, mode: &Mode) -> Result<PathBuf> {
    let root = root();
    // A leftover cluster of this name holds the ports; delete it before checking them.
    let ports = *PORTS;
    if !env.attached() {
        env.delete();
        ports.check_free(env.name())?;
    }
    let kind_cfg = crate::fixture(
        "e2e/kind.yaml",
        work,
        &[
            ("API_PORT", ports.api.to_string()),
            ("EDGE_PORT", ports.edge.to_string()),
        ],
    )?;
    env.create(Some(&kind_cfg), "120s")?;

    crate::image::load_controller(env)?;
    crate::image::ensure_worker()?;
    let mut images = vec![WORKER_IMAGE];
    if matches!(mode, Mode::Mock(_)) {
        crate::image::ensure(REGISTRY_IMAGE)?;
        images.push(REGISTRY_IMAGE);
        crate::gateway::ensure_image(crate::gateway::SOCAT.0, crate::gateway::SOCAT.1)?;
        images.push(crate::gateway::SOCAT.0);
    }
    for image in images {
        status(&mut crate::kube::kind_load(image, env.name()))?;
    }
    let gateway_ca = crate::gateway::E2E.install(env, work)?;

    for ns in [NS, POOL_NS] {
        env.kubectl(&["create", "namespace", ns])?;
    }
    env.kubectl(&["create", "serviceaccount", "builder", "-n", POOL_NS])?;
    env.apply(&format!(
        "apiVersion: v1\nkind: PersistentVolumeClaim\nmetadata:\n  name: {OLD_VOLUME}\n  namespace: {POOL_NS}\n  labels: {{app.kubernetes.io/managed-by: buildkit-controller, app.kubernetes.io/name: buildkit-cache}}\nspec:\n  accessModes: [ReadWriteOnce]\n  resources: {{requests: {{storage: 1Mi}}}}\n"
    ))?;

    let (issuer_url, oidc_ca, rules) = match mode {
        Mode::Mock(i) => {
            let ca = write(&work.join("oidc-ca.crt"), &i.ca_pem)?;
            env.kubectl(&["create", "namespace", "bk-oidc"])?;
            env.kubectl(&[
                "create",
                "configmap",
                "mock-oidc-ca",
                "-n",
                NS,
                &format!("--from-file=ca.crt={ca}"),
            ])?;
            let key = write(&work.join("oidc-key.pem"), &i.key_pem)?;
            let discovery = write(&work.join("discovery.json"), &i.discovery)?;
            let jwks = write(&work.join("jwks.json"), &i.jwks)?;
            env.kubectl(&[
                "create",
                "secret",
                "generic",
                "mock-oidc",
                "-n",
                "bk-oidc",
                &format!("--from-file=tls.crt={ca}"),
                &format!("--from-file=tls.key={key}"),
                &format!("--from-file=discovery.json={discovery}"),
                &format!("--from-file=jwks.json={jwks}"),
            ])?;
            env.kubectl(&[
                "apply",
                "-f",
                &crate::fixture("e2e/mock-oidc.yaml", work, &[])?.to_string_lossy(),
            ])?;
            env.kubectl(&[
                "apply",
                "-f",
                &root
                    .join("xtask/fixtures/e2e/registry.yaml")
                    .to_string_lossy(),
            ])?;
            (
                i.url.clone(),
                "      caBundleConfigMap: {name: mock-oidc-ca, key: ca.crt}\n".to_owned(),
                format!(
                    "          - oidc: {{issuer: \"{}\", claims: {{team: e2e}}}}\n          - serviceAccount: {{name: builder}}",
                    i.url
                ),
            )
        }
        Mode::Github(repo) => (
            GITHUB_ISSUER.to_owned(),
            String::new(),
            format!(
                "          - oidc: {{issuer: \"{GITHUB_ISSUER}\", claims: {{repository: \"{repo}\"}}}}"
            ),
        ),
    };

    let values = format!(
        "\
fullnameOverride: buildkit-controller
image: {{repository: buildkit-controller, tag: dev, pullPolicy: Never}}
replicaCount: 1
podDisruptionBudget: {{enabled: false}}
watchNamespaces: [{POOL_NS}]
logFormat: text
gateway:
  api:
    parentRefs: [{{name: {gw}, namespace: {gw_ns}, sectionName: api}}]
    hostnames: [localhost]
  edge:
    parentRefs: [{{name: {gw}, namespace: {gw_ns}, sectionName: edge}}]
shutdown: {{delay: 0s, drainTimeout: 30s}}
edge:
  endpoint: tcp://localhost:{edge_port}
  serverName: bk.e2e.test
  allocationDomain: localhost
  extraNames: [localhost, 127.0.0.1]
workerNetwork:
  clusterCIDRs: [{KIND_CIDRS}]
  permittedCIDRs: [{KIND_CIDRS}]
auth:
  oidc:
    - url: \"{issuer_url}\"
      audiences: [{AUD}]
{oidc_ca}",
        edge_port = ports.edge,
        gw = crate::gateway::E2E.name,
        gw_ns = crate::gateway::E2E.ns,
    );
    let values_file = write(&work.join("values.yaml"), &values)?;
    let chart = root
        .join("helm/buildkit-controller")
        .to_string_lossy()
        .into_owned();
    // A pool that declares one platform is pinned to that arch, which a single-arch
    // kind node cannot satisfy for the foreign one. Two declared platforms mean
    // emulation and no pin.
    let host = host_platform()?;
    let foreign = if host == "arm64" { "amd64" } else { "arm64" };
    let platforms: Vec<(String, String)> = match mode {
        Mode::Mock(_) => vec![
            (host.to_owned(), format!("linux/{host}")),
            (
                foreign.to_owned(),
                format!("linux/{foreign}, linux/riscv64"),
            ),
        ],
        Mode::Github(_) => vec![(host.to_owned(), format!("linux/{host}"))],
    };
    let mut pools = String::from("pools:\n");
    for (p, plats) in &platforms {
        pools.push_str(&pool_yaml(p, plats, &rules));
    }
    let ready: Vec<String> = platforms.iter().map(|(p, _)| p.clone()).collect();
    let pools_file = write(&work.join("pools.yaml"), &pools)?;
    env.helm(&[
        "upgrade",
        "--install",
        "bk",
        &chart,
        "-n",
        NS,
        "-f",
        &values_file,
        "-f",
        &pools_file,
    ])?;
    env.kubectl(&[
        "rollout",
        "status",
        "deploy/buildkit-controller",
        "-n",
        NS,
        "--timeout=300s",
    ])?;
    env.kubectl(&[
        "rollout",
        "status",
        "statefulset/buildkit-controller-registry",
        "-n",
        NS,
        "--timeout=300s",
    ])?;
    for p in &ready {
        env.wait_condition(
            &format!("buildkitpool/{p}"),
            Some(POOL_NS),
            "Ready",
            Duration::from_mins(5),
        )?;
    }
    println!("pools Ready: {ready:?}");

    // API clients trust the Gateway's certificate, not the controller's.
    let path = work.join("api-ca.pem");
    std::fs::copy(&gateway_ca, &path)?;
    until("API listener", Duration::from_mins(2), || {
        // A TLS-verified 401 through the Gateway shows the HTTPRoute and the
        // BackendTLSPolicy work: the gateway answers 503 when it cannot re-encrypt.
        let probe = Command::new("curl")
            .args(["-sS", "-o", "/dev/null", "-w", "%{http_code}", "--cacert"])
            .arg(&path)
            .arg(format!("{}/v1/pools", *API))
            .output()?;
        Ok(String::from_utf8_lossy(&probe.stdout)
            .starts_with("401")
            .then_some(()))
    })?;
    for (kind, name) in [
        ("httproute", "buildkit-controller-api"),
        ("backendtlspolicy", "buildkit-controller-api"),
        ("tlsroute", "buildkit-controller-edge"),
    ] {
        ensure!(
            crate::gateway::accepted(env, kind, name, NS)?,
            "{kind}/{name} is not Accepted"
        );
    }
    println!("PASS routes Accepted: HTTPRoute and BackendTLSPolicy (API), TLSRoute (edge)");
    Ok(path)
}

/// Install a cluster for the GitHub Action job and leave it running.
pub(crate) fn e2e_up(repo: &str) -> Result<()> {
    let work = work_dir();
    std::fs::create_dir_all(&work)?;
    let env = cluster(&work)?;
    // Left running for the Action job, whether or not the install succeeds.
    env.keep();
    let ca = install(&env, &work, &Mode::Github(repo))?;
    // The CI job reads the URL from this file, not from a fixed port.
    write(&work.join("api-url"), &format!("{}\n", *API))?;
    println!("e2e-up: API {}, CA {}", *API, ca.display());
    Ok(())
}

pub(crate) fn e2e() -> Result<()> {
    let work = work_dir();
    std::fs::create_dir_all(&work)?;
    let env = &cluster(&work)?;
    if std::env::var_os("E2E_KEEP").is_some() {
        env.keep();
    }
    let result = run(env, &work);
    if result.is_err() {
        for args in [
            vec!["get", "pods", "-A", "-o", "wide"],
            vec![
                "logs",
                "-n",
                NS,
                "-l",
                "app.kubernetes.io/name=buildkit-controller",
                "--tail=60",
            ],
            vec!["get", "buildkitpool", "-A", "-o", "yaml"],
        ] {
            if let Ok(o) = env.kubectl(&args) {
                eprintln!("--- kubectl {args:?}\n{o}");
            }
        }
        crate::gateway::dump(env);
    }
    result
}

/// A client of the API, implementing the curl recipe from `docs/ci`.
struct Client<'a> {
    env: &'a Cluster,
    ca: PathBuf,
    ctx: PathBuf,
    work: PathBuf,
}

impl Client<'_> {
    fn recipe(&self, pool: &str, token: &str, script: &str) -> Command {
        let mut c = Command::new("sh");
        c.arg(root().join("docs/ci/buildkit-allocate.sh"))
            .args(["sh", "-c", script])
            .current_dir(&self.ctx)
            .env("API", API.as_str())
            .env("API_CA", &self.ca)
            .env("POOL", pool)
            .env("TOKEN", token)
            .env("QUEUE_TIMEOUT", "240")
            .env("OUTD", &self.work);
        c
    }

    fn build(&self, pool: &str, token: &str, extra: &str) -> Result<String> {
        let script = format!(
            "docker buildx build --builder \"$BUILDER\" --progress=plain -o type=cacheonly {extra} ."
        );
        output(&mut self.recipe(pool, token, &script))
    }

    /// The curl recipe with `CACHE_KEY` (and `CACHE` unless empty), building with `$CACHE_ARGS`.
    fn cached_build(&self, pool: &str, token: &str, key: &str, cache: &str) -> Result<String> {
        let script = "docker buildx build --builder \"$BUILDER\" --progress=plain -o type=cacheonly $CACHE_ARGS .";
        let mut c = self.recipe(pool, token, script);
        c.env("CACHE_KEY", key);
        if !cache.is_empty() {
            c.env("CACHE", cache);
        }
        output(&mut c)
    }

    fn sa_token(&self) -> Result<String> {
        Ok(self
            .env
            .kubectl(&[
                "create",
                "token",
                "builder",
                "-n",
                POOL_NS,
                "--audience",
                AUD,
                "--duration",
                "30m",
            ])?
            .trim()
            .to_owned())
    }
}

fn now() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs())
}

fn read_secs(p: &Path) -> Result<u64> {
    std::fs::read_to_string(p)
        .with_context(|| p.display().to_string())?
        .trim()
        .parse()
        .context("epoch seconds")
}

/// Build contexts. One is RUN-based (busybox copied out of the worker image,
/// because nothing can be pulled inside kind) and one is COPY-only.
fn prepare(work: &Path) -> Result<(PathBuf, PathBuf)> {
    let ctx = work.join("ctx");
    let mp = work.join("ctx-mp");
    for d in [&ctx, &mp] {
        let _ = std::fs::remove_dir_all(d);
        std::fs::create_dir_all(d)?;
    }
    copy_busybox(&ctx)?;
    write(
        &ctx.join("Dockerfile"),
        "FROM scratch\nCOPY lib/ /lib/\nCOPY bin/ /bin/\nRUN [\"/bin/busybox\", \"sh\", \"-c\", \"/bin/busybox echo built-through-edge > /ok; /bin/busybox cat /ok\"]\n",
    )?;
    write(&mp.join("note"), "multi-platform\n")?;
    write(&mp.join("Dockerfile"), "FROM scratch\nCOPY note /note\n")?;
    Ok((ctx, mp))
}

#[allow(clippy::too_many_lines, reason = "one linear scenario script")]
fn run(env: &Cluster, work: &Path) -> Result<()> {
    let issuer = Issuer::new(OIDC_HOST)?;
    let ca = install(env, work, &Mode::Mock(&issuer))?;
    let (ctx, mp) = prepare(work)?;
    let host = host_platform()?;
    let pool = format!("{POOL_NS}/{host}");
    let c = Client {
        env,
        ca: ca.clone(),
        ctx,
        work: work.to_path_buf(),
    };
    let oidc = issuer.token(AUD, "e2e")?;
    let foreign = if host == "arm64" { "amd64" } else { "arm64" };

    // l. A long wait through the Gateway, beside scenarios a to h. X holds the only
    // worker of the foreign pool for 60 s, and Y queues behind it through two or
    // more polls. No request stays open over 25 s: Y's claim waits on the server
    // while the client polls.
    let long = LongPoll::start(&c, &format!("{POOL_NS}/{foreign}"), &issuer)?;

    // a. A build with a RUN step succeeds through the edge (OIDC token).
    let out = c.build(&pool, &oidc, "")?;
    ensure!(
        out.contains("built-through-edge"),
        "RUN output missing:\n{out}"
    );
    println!("PASS a: docker buildx build with RUN through the edge ({pool})");

    // b. Mock-issuer OIDC (above) and ServiceAccount TokenReview both allocate,
    // and a token from nobody is refused. Scenario h builds with the account.
    let sa = c.sa_token()?;
    output(&mut c.recipe(&pool, &sa, "true")).context("SA allocation")?;
    let bad = c.recipe(&pool, "not-a-token", "true").output()?;
    let why = String::from_utf8_lossy(&bad.stderr);
    ensure!(
        !bad.status.success() && why.contains("allocation failed: HTTP 401"),
        "bogus token not refused with 401:\n{why}"
    );
    println!(
        "PASS b: mock OIDC and ServiceAccount TokenReview allocate; a bogus token is refused with 401"
    );

    // c. Registry cache. The second build, on a fresh worker, hits it.
    let cache = format!("type=registry,ref={REG}/cache:e2e");
    let first = c.build(&pool, &oidc, &format!("--cache-to {cache},mode=max"))?;
    ensure!(
        !first.contains("CACHED"),
        "first build was cached:\n{first}"
    );
    let second = c.build(&pool, &oidc, &format!("--cache-from {cache}"))?;
    let hits = second.matches("CACHED").count();
    ensure!(
        hits >= 3,
        "expected the COPY and RUN steps CACHED, got {hits}:\n{second}"
    );
    println!("PASS c: registry cache (second build on a fresh worker: {hits} CACHED steps)");

    // d. Queueing. With max=1 and two concurrent allocations, B waits for A's release.
    for stale in ["a.start", "a.end", "b.start"] {
        let _ = std::fs::remove_file(work.join(stale));
    }
    let a_script = r#"date +%s > "$OUTD/a.start"; sleep 20; date +%s > "$OUTD/a.end""#;
    let b_script = r#"date +%s > "$OUTD/b.start""#;
    let mut a = Spawned::spawn(
        c.recipe(&pool, &oidc, a_script)
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )?;
    until("A holds the worker", Duration::from_mins(3), || {
        Ok(work.join("a.start").exists().then_some(()))
    })?;
    let b_launched = now()?;
    let mut b = Spawned::spawn(
        c.recipe(&pool, &oidc, b_script)
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )?;
    // B waits as a claim in the controller's namespace.
    let pending = until("B pending as a claim", Duration::from_mins(1), || {
        let s = c.env.kubectl(&[
            "get", "bkc", "-n", NS, "-o",
            "jsonpath={range .items[?(@.status.phase==\"Pending\")]}{.metadata.name} {.spec.poolRef.name}{\"\\n\"}{end}",
        ])?;
        let head = format!(" {host}");
        Ok(s.lines().find(|l| l.ends_with(&head)).map(str::to_owned))
    })?;
    ensure!(a.wait()?.success(), "allocation A failed");
    ensure!(b.wait()?.success(), "allocation B failed");
    let (a_end, b_start) = (
        read_secs(&work.join("a.end"))?,
        read_secs(&work.join("b.start"))?,
    );
    ensure!(
        b_start >= a_end,
        "B started at {b_start}, before A ended at {a_end}"
    );
    ensure!(
        b_start >= b_launched + 10,
        "B did not wait (launched {b_launched}, started {b_start})"
    );
    println!(
        "PASS d: second allocation queued {}s as claim {pending} and ran after the first was released",
        b_start - b_launched
    );

    // f. The built-in cache registry. Two builds of one key on fresh workers, each
    // with the flags from its allocation response (a push event: trusted). The
    // second imports what the first exported.
    let repo = format!("{CACHE_HOST}/{POOL_NS}/{host}/");
    let first = c.cached_build(&pool, &oidc, "e2e", "")?;
    let flags = first
        .lines()
        .find_map(|l| l.strip_prefix("cache flags: "))
        .with_context(|| format!("no cache flags:\n{first}"))?
        .to_owned();
    ensure!(
        flags.contains(&format!("--cache-from=type=registry,ref={repo}"))
            && flags.contains(&format!("--cache-to=type=registry,ref={repo}"))
            && flags.contains("mode=max,ignore-error=true,oci-mediatypes=true"),
        "built-in registry flags missing: {flags}"
    );
    ensure!(
        !first.contains("CACHED"),
        "first build was cached:\n{first}"
    );
    let second = c.cached_build(&pool, &oidc, "e2e", "")?;
    let hits = second.matches("CACHED").count();
    ensure!(
        hits >= 3,
        "expected CACHED steps from the built-in registry, got {hits}:\n{second}"
    );
    println!(
        "PASS f: built-in cache registry (second build on a fresh worker: {hits} CACHED steps)"
    );

    // g. A pull_request build of the same caller reads the cache and gets no
    // --cache-to. A push it adds itself is refused by the registry.
    let pr = issuer.token_for_event(AUD, "e2e", "pull_request")?;
    let read = c.cached_build(&pool, &pr, "e2e", "")?;
    let pr_flags = read
        .lines()
        .find_map(|l| l.strip_prefix("cache flags: "))
        .with_context(|| format!("no cache flags:\n{read}"))?;
    ensure!(
        pr_flags.contains("--cache-from=") && !pr_flags.contains("--cache-to="),
        "an untrusted build got: {pr_flags}"
    );
    let pr_hits = read.matches("CACHED").count();
    ensure!(
        pr_hits >= 3,
        "the untrusted build did not read the cache ({pr_hits} CACHED):\n{read}"
    );
    let denied_before = registry_metric(env, "buildkit_registry_denied_total{action=\"push\"}")?;
    let target = flags
        .split_whitespace()
        .find_map(|f| f.strip_prefix("--cache-from="))
        .context("no cache ref")?
        .to_owned();
    let push = c
        .recipe(
            &pool,
            &pr,
            &format!(
                "docker buildx build --builder \"$BUILDER\" --progress=plain -o type=cacheonly --no-cache --cache-to {target},mode=max ."
            ),
        )
        .output()?;
    let denied = registry_metric(env, "buildkit_registry_denied_total{action=\"push\"}")?;
    ensure!(
        !push.status.success() && denied > denied_before,
        "the untrusted push was not refused (exit {:?}, push denials {denied_before} -> {denied}):\n{}",
        push.status.code(),
        String::from_utf8_lossy(&push.stderr)
    );
    println!(
        "PASS g: an untrusted (pull_request) build reads the cache ({pr_hits} CACHED) and its own push is refused ({denied} push denials)"
    );

    // h. Another identity has another scope: the ServiceAccount does not read the
    // OIDC caller's cache.
    let sa = c.sa_token()?;
    let other = c.cached_build(&pool, &sa, "e2e", "")?;
    ensure!(
        other.matches("CACHED").count() == 0,
        "the ServiceAccount read another identity's cache:\n{other}"
    );
    println!("PASS h: another identity starts with an empty cache");

    // m. Upgrade: the cache volume of v0.1.0 created before install is gone.
    // v0.1.0 upgrade shim: remove in the release after 0.2.0.
    until(
        "the v0.1.0 cache volume is deleted",
        Duration::from_mins(2),
        || {
            let left = env.kubectl(&["get", "pvc", "-n", POOL_NS, "-o", "name"])?;
            Ok((!left.contains(OLD_VOLUME)).then_some(()))
        },
    )?;
    println!("PASS m: the leader deleted the v0.1.0 cache volume {OLD_VOLUME}");

    long.finish()?;

    // e. Multi-platform. Two pools and a two-node builder, built the way the Action builds it.
    let out = output(
        Command::new("bash")
            .arg(root().join("xtask/fixtures/e2e/multiplatform.sh"))
            .env("API", API.as_str())
            .env("API_CA", &ca)
            .env("TOKEN", &oidc)
            .env(
                "POOLS",
                format!("{POOL_NS}/{host}=linux/{host} {POOL_NS}/{foreign}=linux/{foreign},linux/riscv64"),
            )
            .env("CTX", &mp),
    )?;
    for node in ["n0", "n1"] {
        ensure!(
            out.lines()
                .any(|l| l.trim_start().starts_with("Name:") && l.trim_end().ends_with(node)),
            "node {node} not attached:\n{out}"
        );
        // The Status line sits in the node's block, before the next Name line.
        let running = out
            .lines()
            .skip_while(|l| !(l.trim_start().starts_with("Name:") && l.trim_end().ends_with(node)))
            .skip(1)
            .take_while(|l| !l.trim_start().starts_with("Name:"))
            .any(|l| l.trim_start().starts_with("Status:") && l.trim_end().ends_with("running"));
        ensure!(running, "node {node} is not Status: running:\n{out}");
    }
    let path = out
        .lines()
        .find_map(|l| l.strip_prefix("multiplatform path: "))
        .context("multiplatform.sh did not print which path ran")?;
    ensure!(
        std::env::var("CI").as_deref() != Ok("true") || path.starts_with("RUN"),
        "CI requires the emulated RUN path, got: {path}"
    );
    for needle in ["[linux/arm64", "[linux/amd64", "linux/riscv64"] {
        ensure!(
            out.contains(needle),
            "multi-platform output lacks {needle:?}:\n{out}"
        );
    }
    println!(
        "PASS e: two nodes running (native pool + emulated-arch pool declaring two platforms); linux/arm64,linux/amd64 build and each platform build alone succeed; multiplatform path: {path}"
    );

    gateway_options(env, &c, &pool, &issuer)?;
    println!("e2e: all scenarios passed");
    Ok(())
}

/// Scenario l: an allocation that stays queued for over five minutes behind the
/// Gateway, which the client follows with polls of at most 25 s each.
struct LongPoll {
    holder: Spawned,
    waiter: Spawned,
    launched: u64,
    outd: PathBuf,
}

/// How long X holds the worker: Y waits through at least two 25 s polls.
const HOLD_SECS: u64 = 60;
/// The shortest wait of Y that shows what the scenario tests.
const MIN_WAIT_SECS: u64 = 50;

impl LongPoll {
    fn start(c: &Client, pool: &str, issuer: &Issuer) -> Result<Self> {
        let outd = c.work.clone();
        for stale in ["l.x", "l.y"] {
            let _ = std::fs::remove_file(outd.join(stale));
        }
        let holder = Spawned::spawn(
            c.recipe(
                pool,
                &issuer.token(AUD, "e2e")?,
                &format!(r#"date +%s > "$OUTD/l.x"; sleep {HOLD_SECS}"#),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
        )?;
        until("X holds the foreign worker", Duration::from_mins(3), || {
            Ok(outd.join("l.x").exists().then_some(()))
        })?;
        let launched = now()?;
        let waiter = Spawned::spawn(
            c.recipe(
                pool,
                &issuer.token(AUD, "e2e")?,
                r#"date +%s > "$OUTD/l.y""#,
            )
            .env("QUEUE_TIMEOUT", "600")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
        )?;
        Ok(Self {
            holder,
            waiter,
            launched,
            outd,
        })
    }

    fn finish(mut self) -> Result<()> {
        ensure!(self.holder.wait()?.success(), "long-poll holder X failed");
        let y = self.waiter.wait_with_output()?;
        ensure!(
            y.status.success(),
            "long-poll Y failed:\n{}{}",
            String::from_utf8_lossy(&y.stdout),
            String::from_utf8_lossy(&y.stderr)
        );
        let started = read_secs(&self.outd.join("l.y"))?;
        let waited = started.saturating_sub(self.launched);
        ensure!(
            waited >= MIN_WAIT_SECS,
            "Y waited {waited}s, less than the {MIN_WAIT_SECS}s the scenario tests"
        );
        println!(
            "PASS l: an allocation queued {waited}s through the Gateway, polled with requests of at most 25s, and got the worker"
        );
        Ok(())
    }
}

/// `helm upgrade --reuse-values` of the e2e release with `sets`.
fn upgrade(env: &Cluster, sets: &[&str]) -> Result<()> {
    let chart = root().join("helm/buildkit-controller");
    let chart = chart.to_string_lossy();
    let mut args = vec!["upgrade", "bk", &chart, "-n", NS, "--reuse-values"];
    for s in sets {
        args.extend(["--set", s]);
    }
    env.helm(&args)?;
    Ok(())
}

/// Scenarios i and j: the chart's other Gateway options on the same Gateway.
fn gateway_options(env: &Cluster, c: &Client, pool: &str, issuer: &Issuer) -> Result<()> {
    // i. Envoy Gateway extras. Both BackendTrafficPolicies are Accepted and a build
    // with a RUN step still goes through the API route and the edge.
    upgrade(env, &["gateway.envoy.enabled=true"])?;
    for route in ["api", "edge"] {
        let name = format!("buildkit-controller-{route}");
        until(
            &format!("backendtrafficpolicy/{name} Accepted"),
            Duration::from_mins(2),
            || Ok(crate::gateway::accepted(env, "backendtrafficpolicy", &name, NS)?.then_some(())),
        )?;
    }
    let out = c.build(pool, &issuer.token(AUD, "e2e")?, "")?;
    ensure!(
        out.contains("built-through-edge"),
        "build with the Envoy extras:\n{out}"
    );
    println!(
        "PASS i: gateway.envoy.enabled: both BackendTrafficPolicies Accepted; a build succeeds"
    );

    // j. The API by TLS passthrough. The TLSRoute on the Gateway's passthrough
    // listener reaches the controller, which serves its own certificate.
    upgrade(
        env,
        &[
            "gateway.api.mode=passthrough",
            &format!(
                "gateway.api.parentRefs[0].sectionName=api-passthrough,gateway.api.parentRefs[0].name={},gateway.api.parentRefs[0].namespace={}",
                crate::gateway::E2E.name,
                crate::gateway::E2E.ns
            ),
        ],
    )?;
    until(
        "tlsroute/buildkit-controller-api Accepted",
        Duration::from_mins(2),
        || {
            Ok(
                crate::gateway::accepted(env, "tlsroute", "buildkit-controller-api", NS)?
                    .then_some(()),
            )
        },
    )?;
    let ca = env.kubectl(&[
        "get",
        "configmap",
        "buildkit-controller-ca-bundle",
        "-n",
        NS,
        "-o",
        "jsonpath={.data.ca\\.crt}",
    ])?;
    let ca_file = write(&c.work.join("controller-ca.pem"), &ca)?;
    // kubectl port-forward exits when the pod refuses a connection, which it does
    // until Envoy has the listener, so each attempt opens its own.
    let code = until("API by passthrough", Duration::from_mins(2), || {
        let port = crate::gateway::PortForward::envoy(env, &crate::gateway::E2E, 9443)?;
        let url = format!("https://localhost:{}/v1/pools", port.local);
        let probe = Command::new("curl")
            .args(["-sS", "-o", "/dev/null", "-w", "%{http_code}", "--cacert"])
            .arg(&ca_file)
            .arg(&url)
            .output()?;
        let code = String::from_utf8_lossy(&probe.stdout).into_owned();
        ensure!(
            code.starts_with("401"),
            "{url}: HTTP {code} {}",
            String::from_utf8_lossy(&probe.stderr).trim()
        );
        Ok(Some(code))
    })?;
    println!(
        "PASS j: gateway.api.mode=passthrough: TLSRoute Accepted; the API answers {code} with the controller's own certificate"
    );
    Ok(())
}

/// The value of `sample` (name and labels, as exposed) on the cache registry's
/// `/metrics`, read through the API server's Service proxy; 0 when absent.
fn registry_metric(env: &Cluster, sample: &str) -> Result<f64> {
    let text = env.kubectl(&[
        "get",
        "--raw",
        &format!("/api/v1/namespaces/{NS}/services/http:buildkit-controller-registry:metrics/proxy/metrics"),
    ])?;
    Ok(text
        .lines()
        .find_map(|l| l.strip_prefix(sample)?.trim().parse().ok())
        .unwrap_or(0.0))
}
