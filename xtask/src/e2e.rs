//! `cargo xtask e2e`: the end-to-end suite on a kind cluster `bk-e2e`.
//! `cargo xtask e2e` runs the end-to-end suite on a kind cluster `bk-e2e`.
//!
//! The task installs the controller from the Helm chart. A mock OIDC issuer
//! runs in this process (trusted through the issuer's `caBundleConfigMap`) and
//! a `registry:2` runs in the cluster. The clients are the real recipe
//! `docs/ci/buildkit-allocate.sh` and real `docker buildx` on the host. They go
//! through the real API and mTLS edge, which kind publishes on 127.0.0.1:18443
//! and :18444 (`NodePorts`). `*.localhost` resolves to loopback, which makes
//! per-allocation endpoints (`<id>.localhost:18444`) reachable.
//!
//! Every kind, kubectl and helm call gets the private kubeconfig under
//! `target/e2e`. The task never touches `~/.kube/config`, and `e2e` deletes the
//! cluster on every exit path. `e2e-up` installs the same cluster trusting
//! GitHub's OIDC issuer and leaves it running, for the Action job in CI.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use crate::e2e_issuer::Issuer;
use crate::helm_verify::{Env, until};
use crate::root;

static CLUSTER: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| super::kind_cluster("bk-e2e"));
const NS: &str = "bk-system";
const POOL_NS: &str = "ci";
const OIDC_HOST: &str = "mock-oidc.bk-oidc.svc";
const SOCAT_IMAGE: &str = "alpine/socat:latest";
const API: &str = "https://localhost:18443";
const WORKER_IMAGE: &str = "moby/buildkit:v0.34.0-rootless";
const LOCAL_BUILDKIT: &str = "moby/buildkit:rootless";
const CONTROLLER_IMAGE: &str = "buildkit-controller:dev";
const REGISTRY_IMAGE: &str = "registry:2";
const REG: &str = "registry.e2e-reg.svc.cluster.local:5000";
const GITHUB_ISSUER: &str = "https://token.actions.githubusercontent.com";
const AUD: &str = "buildkit-controller";

/// Which issuer the controller trusts.
enum Mode<'a> {
    /// The in-process mock issuer.
    Mock(&'a Issuer),
    /// GitHub's, for the repository `owner/name`.
    Github(&'a str),
}

struct Cleanup(Env);

impl Drop for Cleanup {
    fn drop(&mut self) {
        if std::env::var_os("E2E_KEEP").is_some() {
            return;
        }
        let _ = self
            .0
            .tool("kind", &["delete", "cluster", "--name", CLUSTER.as_str()]);
    }
}

fn status(c: &mut Command) -> Result<()> {
    ensure!(
        c.status().with_context(|| format!("{c:?}"))?.success(),
        "{c:?} failed"
    );
    Ok(())
}

/// Returns stdout and stderr of a command, and fails with both when it exits non-zero.
fn output(c: &mut Command) -> Result<String> {
    let o = c.output().with_context(|| format!("{c:?}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    ensure!(o.status.success(), "{c:?} failed:\n{text}");
    Ok(text)
}

fn have_image(image: &str) -> bool {
    Command::new("docker")
        .args(["image", "inspect", image])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Make `image` present locally, pulling when it is not.
fn ensure_image(image: &str) -> Result<()> {
    if !have_image(image) {
        status(Command::new("docker").args(["pull", image]))?;
    }
    Ok(())
}

/// The pool platform of this host.
fn host_platform() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "aarch64" => Ok("arm64"),
        "x86_64" => Ok("amd64"),
        a => bail!("unsupported host architecture {a}"),
    }
}

fn write(path: &Path, text: &str) -> Result<String> {
    std::fs::write(path, text).with_context(|| path.display().to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

fn pool_yaml(name: &str, platforms: &str, rules: &str) -> String {
    format!(
        "  - name: {name}
    namespace: {POOL_NS}
    spec:
      platforms: [{platforms}]
      scaling: {{min: 1, max: 1}}
      network:
        egress: {{allowCIDRs: [10.0.0.0/8]}}
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

/// Cache pools of scenarios f to h: `(name, scaling, chart cache value)`. Their
/// `cache` goes through the chart's per-pool value.
const CACHE_POOLS: [(&str, &str, &str); 3] = [
    (
        "cache-reg",
        "{min: 1, max: 1}",
        "{registry: {repository: \"REG/bkcache\"}}",
    ),
    (
        "cache-local",
        "{min: 1, max: 1}",
        "{local: {size: 1Gi, maxVolumes: 1}}",
    ),
    (
        "cache-spill",
        "{min: 2, max: 2}",
        "{local: {size: 1Gi, maxVolumes: 2}}",
    ),
];

/// [`pool_yaml`] with other scaling and a chart `cache` value.
fn cache_pool_yaml(name: &str, platform: &str, rules: &str, scaling: &str, cache: &str) -> String {
    pool_yaml(name, platform, rules)
        .replacen(
            "scaling: {min: 1, max: 1}",
            &format!("scaling: {scaling}"),
            1,
        )
        .replacen(
            &format!("    namespace: {POOL_NS}\n"),
            &format!(
                "    namespace: {POOL_NS}\n    cache: {}\n",
                cache.replace("REG", REG)
            ),
            1,
        )
}

#[allow(clippy::too_many_lines, reason = "one linear install script")]
/// Create the cluster and install the chart with its pools. Returns the API CA
/// file (the client CA, which signs the API serving certificate).
fn install(env: &Env, work: &Path, mode: &Mode) -> Result<PathBuf> {
    let root = root();
    let _ = env.tool("kind", &["delete", "cluster", "--name", CLUSTER.as_str()]);
    let kind_cfg = root.join("xtask/fixtures/e2e/kind.yaml");
    env.tool(
        "kind",
        &[
            "create",
            "cluster",
            "--name",
            CLUSTER.as_str(),
            "--config",
            &kind_cfg.to_string_lossy(),
            "--wait",
            "120s",
        ],
    )?;

    crate::image::image(CONTROLLER_IMAGE, Some(CLUSTER.as_str()))?;
    if !have_image(WORKER_IMAGE) {
        if have_image(LOCAL_BUILDKIT) {
            status(Command::new("docker").args(["tag", LOCAL_BUILDKIT, WORKER_IMAGE]))?;
        } else {
            ensure_image(WORKER_IMAGE)?;
        }
    }
    let mut images = vec![WORKER_IMAGE];
    if matches!(mode, Mode::Mock(_)) {
        ensure_image(REGISTRY_IMAGE)?;
        images.push(REGISTRY_IMAGE);
        ensure_image(SOCAT_IMAGE)?;
        images.push(SOCAT_IMAGE);
    }
    for image in images {
        status(&mut crate::kube::kind_load(image, CLUSTER.as_str()))?;
    }

    for ns in [NS, POOL_NS] {
        env.kubectl(&["create", "namespace", ns])?;
    }
    env.kubectl(&["create", "serviceaccount", "builder", "-n", POOL_NS])?;

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
                &root
                    .join("xtask/fixtures/e2e/mock-oidc.yaml")
                    .to_string_lossy(),
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
logFormat: text
service:
  type: NodePort
  nodePorts: {{api: 30443, edge: 30444}}
shutdown: {{delay: 0s, drainTimeout: 30s}}
edge:
  endpoint: tcp://localhost:18444
  serverName: bk.e2e.test
  allocationDomain: localhost
  extraNames: [localhost, 127.0.0.1]
auth:
  oidc:
    - url: \"{issuer_url}\"
      audiences: [{AUD}]
{oidc_ca}"
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
    let mut ready: Vec<String> = platforms.iter().map(|(p, _)| p.clone()).collect();
    if matches!(mode, Mode::Mock(_)) {
        for (name, scaling, cache) in CACHE_POOLS {
            let plat = format!("linux/{host}");
            pools.push_str(&cache_pool_yaml(name, &plat, &rules, scaling, cache));
            ready.push(name.to_owned());
        }
    }
    let pools_file = write(&work.join("pools.yaml"), &pools)?;
    env.tool(
        "helm",
        &[
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
        ],
    )?;
    env.kubectl(&[
        "rollout",
        "status",
        "deploy/buildkit-controller",
        "-n",
        NS,
        "--timeout=300s",
    ])?;
    for p in &ready {
        env.kubectl(&[
            "wait",
            "--for=condition=Ready",
            &format!("buildkitpool/{p}"),
            "-n",
            POOL_NS,
            "--timeout=300s",
        ])?;
    }
    println!("pools Ready: {ready:?}");

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
    write(&path, &decoded)?;
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

/// Install a cluster for the GitHub Action job and leave it running.
pub(crate) fn e2e_up(repo: &str) -> Result<()> {
    let work = root().join("target/e2e");
    std::fs::create_dir_all(&work)?;
    let env = Env {
        kubeconfig: work.join("kubeconfig"),
    };
    let ca = install(&env, &work, &Mode::Github(repo))?;
    println!("e2e-up: API {API}, CA {}", ca.display());
    Ok(())
}

pub(crate) fn e2e() -> Result<()> {
    let work = root().join("target/e2e");
    std::fs::create_dir_all(&work)?;
    let cleanup = Cleanup(Env {
        kubeconfig: work.join("kubeconfig"),
    });
    let env = &cleanup.0;
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
    }
    result
}

/// A client of the API, implementing the curl recipe from `docs/ci`.
struct Client<'a> {
    env: &'a Env,
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
            .env("API", API)
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

    /// `kubectl exec -n ci <args[0]> -- <args[1..]>`. `--kubeconfig` goes before
    /// `exec`, because [`Env::kubectl`] appends it, which would land after `--`.
    fn exec(&self, args: &[&str]) -> Result<String> {
        let (pod, cmd) = args.split_first().context("exec needs a pod")?;
        output(
            crate::kube::kubectl(&self.env.kubeconfig)
                .arg("--kubeconfig")
                .arg(&self.env.kubeconfig)
                .args(["exec", "-n", POOL_NS, pod, "--"])
                .args(cmd),
        )
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

    /// `(alloc == free && Ready, cache-keys)` of a pool pod, or `None` when it does not exist.
    fn pod_state(&self, pod: &str) -> Option<(bool, String)> {
        let Ok(out) = self.env.kubectl(&["get", "pod", "-n", POOL_NS, pod, "-o",
            "jsonpath={.metadata.annotations.buildkit\\.smrt-devops\\.net/alloc} {.status.conditions[?(@.type==\"Ready\")].status} {.metadata.deletionTimestamp} |{.metadata.annotations.buildkit\\.smrt-devops\\.net/cache-keys}"])
        else {
            return None;
        };
        let (state, keys) = out.split_once('|').unwrap_or((&out, ""));
        let f: Vec<&str> = state.split_whitespace().collect();
        Some((f == ["free", "True"], keys.trim().to_owned()))
    }

    /// Waits until `pod` is a free Ready pod whose cache keys contain `key` (any keys when empty).
    fn wait_warm(&self, pod: &str, key: &str) -> Result<()> {
        until(
            &format!("{pod} warm with key {key:?}"),
            Duration::from_mins(3),
            || {
                Ok(self
                    .pod_state(pod)
                    .is_some_and(|(free, keys)| {
                        free && (key.is_empty() || keys.split(',').any(|k| k == key))
                    })
                    .then_some(()))
            },
        )
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

/// Copies busybox and the musl loader out of the worker image into `dir`.
/// The files come out as a tar stream on stdout, not through a bind mount. The
/// rootless image runs as uid 1000, which cannot write to a host directory the
/// runner owns on Linux. Extracted on the host, the files belong to the caller.
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
        .arg("-xf")
        .arg("-")
        .arg("-C")
        .arg(dir)
        .stdin(stream)
        .status()
        .context("running tar")?;
    let copied = docker.wait().context("waiting for docker run")?;
    ensure!(
        copied.success(),
        "copying busybox out of {WORKER_IMAGE} failed: {copied}"
    );
    ensure!(
        untar.success(),
        "extracting busybox into {} failed: {untar}",
        dir.display()
    );
    ensure!(
        dir.join("bin/busybox").is_file(),
        "busybox missing after the copy"
    );
    Ok(())
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
fn run(env: &Env, work: &Path) -> Result<()> {
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

    // a. A build with a RUN step succeeds through the edge (OIDC token).
    let out = c.build(&pool, &oidc, "")?;
    ensure!(
        out.contains("built-through-edge"),
        "RUN output missing:\n{out}"
    );
    println!("PASS a: docker buildx build with RUN through the edge ({pool})");

    // b. Mock-issuer OIDC (above) and ServiceAccount TokenReview both work,
    // and a token from nobody is refused.
    let sa = c.sa_token()?;
    let out = c.build(&pool, &sa, "")?;
    ensure!(out.contains("built-through-edge"), "SA build:\n{out}");
    let bad = c.recipe(&pool, "not-a-token", "true").output()?;
    ensure!(!bad.status.success(), "bogus token was accepted");
    println!(
        "PASS b: mock OIDC and ServiceAccount TokenReview allocate and build; a bogus token is refused"
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
    let mut a = c
        .recipe(&pool, &oidc, a_script)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    until("A holds the worker", Duration::from_mins(3), || {
        Ok(work.join("a.start").exists().then_some(()))
    })?;
    let b_launched = now()?;
    let mut b = c
        .recipe(&pool, &oidc, b_script)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
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
        "PASS d: second allocation queued {}s and ran after the first was released",
        b_start - b_launched
    );

    // f. Registry layer. Two builds of one cacheKey on fresh workers, each with the
    // flags from its allocation response; the second imports what the first exported.
    let reg_pool = format!("{POOL_NS}/cache-reg");
    let first = c.cached_build(&reg_pool, &oidc, "e2e-reg", "")?;
    let want = format!(
        "cache flags: --cache-from=type=registry,ref={REG}/bkcache:e2e-reg --cache-to=type=registry,ref={REG}/bkcache:e2e-reg,mode=max"
    );
    ensure!(first.contains(&want), "registry flags missing:\n{first}");
    ensure!(
        !first.contains("CACHED"),
        "first build was cached:\n{first}"
    );
    let second = c.cached_build(&reg_pool, &oidc, "e2e-reg", "")?;
    let hits = second.matches("CACHED").count();
    ensure!(
        hits >= 3,
        "expected CACHED steps from the registry ref, got {hits}:\n{second}"
    );
    println!(
        "PASS f: registry layer from the response flags (second build on a fresh worker: {hits} CACHED steps)"
    );

    // g. Local layer on a pool with no registry config. Slot 0 keeps its volume, so
    // the second build on a new pod in that slot reuses the first build's state.
    let local_pool = format!("{POOL_NS}/cache-local");
    let first = c.cached_build(&local_pool, &oidc, "e2e-local", "local")?;
    ensure!(
        !first.contains("cache flags:"),
        "a local-only pool returned registry flags:\n{first}"
    );
    ensure!(
        !first.contains("CACHED"),
        "first build was cached:\n{first}"
    );
    c.wait_warm("cache-local-0", "e2e-local")?;
    let second = c.cached_build(&local_pool, &oidc, "e2e-local", "local")?;
    let hits = second.matches("CACHED").count();
    ensure!(
        second.contains("cache: key e2e-local, local hit"),
        "expected local hit:\n{second}"
    );
    ensure!(
        hits >= 3,
        "expected CACHED steps from the volume, got {hits}:\n{second}"
    );
    let config = c.exec(&["cache-local-0", "cat", "/etc/buildkit/buildkitd.toml"])?;
    ensure!(
        config.contains("maxUsedSpace = \"858993456\""),
        "gcKeepStorage (1Gi / 5 * 4) not in buildkitd.toml:\n{config}"
    );
    let workers = c.exec(&[
        "cache-local-0",
        "buildctl",
        "--addr",
        "unix:///run/user/1000/buildkit/buildkitd.sock",
        "debug",
        "workers",
        "-v",
    ])?;
    let gc: Vec<&str> = workers
        .lines()
        .filter(|l| l.contains("Maximum used space") && l.contains("858.993456MB"))
        .collect();
    ensure!(
        !gc.is_empty(),
        "buildctl debug workers shows no 858.993456MB GC bound:\n{workers}"
    );
    println!(
        "PASS g: local layer (result hit, {hits} CACHED steps); buildkitd maxUsedSpace=858993456; buildctl: {}",
        gc.join(" | ").trim()
    );

    // h. Concurrency. Two overlapping builds of one key on a pool with two cached
    // slots, one of which holds the key: both succeed, the first is a hit, and the
    // second, on the other slot, reports spill.
    let spill_pool = format!("{POOL_NS}/cache-spill");
    c.cached_build(&spill_pool, &oidc, "e2e-spill", "")?;
    let holder = until(
        "a cache-spill pod holding e2e-spill",
        Duration::from_mins(3),
        || {
            for pod in ["cache-spill-0", "cache-spill-1"] {
                if c.pod_state(pod)
                    .is_some_and(|(free, keys)| free && keys.split(',').any(|k| k == "e2e-spill"))
                {
                    return Ok(Some(pod));
                }
            }
            Ok(None)
        },
    )?;
    let other = if holder == "cache-spill-0" {
        "cache-spill-1"
    } else {
        "cache-spill-0"
    };
    c.wait_warm(other, "")?;
    let _ = std::fs::remove_file(work.join("h.a"));
    let build = "docker buildx build --builder \"$BUILDER\" --progress=plain -o type=cacheonly $CACHE_ARGS .";
    let a = c
        .recipe(
            &spill_pool,
            &oidc,
            &format!("touch \"$OUTD/h.a\"; {build} && sleep 15"),
        )
        .env("CACHE_KEY", "e2e-spill")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    until("A holds the cached slot", Duration::from_mins(3), || {
        Ok(
            (work.join("h.a").exists() && c.pod_state(holder).is_some_and(|(free, _)| !free))
                .then_some(()),
        )
    })?;
    let b = c.cached_build(&spill_pool, &oidc, "e2e-spill", "")?;
    let a = a.wait_with_output()?;
    let a_out = format!(
        "{}{}",
        String::from_utf8_lossy(&a.stdout),
        String::from_utf8_lossy(&a.stderr)
    );
    ensure!(a.status.success(), "build A failed:\n{a_out}");
    ensure!(
        a_out.contains("cache: key e2e-spill, local hit"),
        "A was not a hit:\n{a_out}"
    );
    ensure!(
        b.contains("cache: key e2e-spill, local spill"),
        "B did not spill:\n{b}"
    );
    println!(
        "PASS h: two overlapping e2e-spill builds both succeed; A local hit on {holder}, B local spill on {other}"
    );

    // e. Multi-platform. Two pools and a two-node builder, built the way the Action builds it.
    let foreign = if host == "arm64" { "amd64" } else { "arm64" };
    let out = output(
        Command::new("bash")
            .arg(root().join("xtask/fixtures/e2e/multiplatform.sh"))
            .env("API", API)
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

    println!("e2e: all scenarios passed");
    Ok(())
}
