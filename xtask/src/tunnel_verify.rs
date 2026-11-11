//! `cargo xtask tunnel-verify` proves the tunnel transport (edge.md §10) end to
//! end on a kind cluster `bk-tunnel-verify`, behind a gateway that terminates TLS.
//!
//! The Gateway ([`crate::gateway::TUNNEL`]) has one listener, HTTPS with
//! `tls.mode: Terminate`, and none that passes TLS through. The chart is
//! installed with `edge.endpoint` empty, so allocations answer with the tunnel
//! path `/v1/tunnel` and only the API `HTTPRoute` and its `BackendTLSPolicy`
//! render. The clients are real `docker buildx` on the host and the Action's own
//! forwarder, `action/dist/tunnel.js`, which
//! `xtask/fixtures/tunnel-verify/client.js` starts through `action/dist/lib.js`
//! as the Action does. kind publishes the listener's `NodePort` on one loopback
//! host port (see [`PORTS`]). The scenarios are:
//!
//! a. A build through the tunnel uploads a 100 MiB random file in its context,
//!    hashes it in a `RUN` step and exports it back to the host, where both
//!    hashes match. The edge byte counters with `transport="tunnel"` carry at
//!    least 100 MiB each way.
//! b. A TLS connection through the forwarder that reaches the worker and then
//!    sends nothing stays open for `--idle-secs` (75 s by default: past the
//!    `HTTPRoute`'s 60 s timeouts and three of the server's 20 s pings) and
//!    still answers an HTTP/2 PING. The next build on the same builder works.
//! c. Releasing the allocation closes that connection within [`RELEASE_BOUND`].
//! d. A builder with one node per pool, each its own forwarder port, builds for
//!    both pools' platforms without `edge.allocationDomain`.
//! e. A client certificate from another CA is refused through the tunnel.
//!    Then the Action's real post step, `action/dist/post.js`, removes that
//!    builder, stops its forwarder and releases both allocations through the
//!    Gateway with the CA from its state (its OIDC token from a loopback stub).
//! f. Every edge connection of the run is labelled `transport="tunnel"`; the
//!    TCP listener saw none.
//!
//! Every kind, kubectl and helm call gets the private kubeconfig under
//! `target/tunnel-verify` (`target/tunnel-verify-<suffix>` with
//! `BK_KIND_SUFFIX`). The task deletes the cluster on every exit path.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use ring::rand::SecureRandom as _;

use crate::gateway::{TUNNEL as GATEWAY, hex};
use crate::image::{WORKER_IMAGE, copy_busybox};
use crate::kube::{Cluster, HostPorts};
use crate::proc::{Spawned, host_platform, output, poll, status, until, write};
use crate::root;

const NS: &str = "bk-tunnel";
const POOL_NS: &str = "tunnel-ci";
const SERVER_NAME: &str = "bk.tunnel.test";
const AUD: &str = "buildkit-controller";
/// The size of the file scenario a moves each way.
const BIG: u64 = 100 * 1024 * 1024;
/// How soon a release must close an open tunnel connection.
const RELEASE_BOUND: Duration = Duration::from_secs(5);
/// The controller's metrics, through the API server's Service proxy.
const METRICS: &str =
    "/api/v1/namespaces/bk-tunnel/services/http:buildkit-controller-metrics:metrics/proxy/metrics";

/// The loopback host port kind publishes the Gateway's HTTPS listener on: 38443
/// without `BK_KIND_SUFFIX`, else one in 24000-25999. Only `api` is published.
pub(crate) static PORTS: std::sync::LazyLock<HostPorts> = std::sync::LazyLock::new(|| {
    HostPorts::get(
        HostPorts {
            api: 38443,
            edge: 38444,
        },
        24000,
    )
});
static API: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| format!("https://localhost:{}", PORTS.api));

pub(crate) fn tunnel_verify(idle_secs: u64) -> Result<()> {
    let work = root()
        .join("target")
        .join(crate::kind_cluster("tunnel-verify"));
    std::fs::create_dir_all(&work)?;
    let env = &Cluster::for_suite("bk-tunnel-verify", &work)?;
    if std::env::var_os("TUNNEL_VERIFY_KEEP").is_some() {
        env.keep();
    }
    let result = run(env, &work, Duration::from_secs(idle_secs));
    if result.is_err() {
        for args in [
            vec!["get", "pods", "-A", "-o", "wide"],
            vec!["logs", "-n", NS, "deploy/buildkit-controller", "--tail=80"],
            vec!["get", "buildkitpool", "-n", POOL_NS, "-o", "yaml"],
        ] {
            if let Ok(o) = env.kubectl(&args) {
                eprintln!("--- kubectl {args:?}\n{o}");
            }
        }
        crate::gateway::dump(env);
    }
    result
}

/// The chart's values: an empty `edge.endpoint`, the API route on the Gateway's
/// HTTPS listener, and two pools. Pool `a` runs the host's platform; pool `b`
/// declares two others, so its worker is not pinned to an arch the node lacks.
fn values(host: &str) -> String {
    let foreign = if host == "arm64" { "amd64" } else { "arm64" };
    let pool = |name: &str, platforms: &str| {
        format!(
            "  - name: {name}
    namespace: {POOL_NS}
    spec:
      platforms: [{platforms}]
      scaling: {{min: 1, max: 1}}
      access:
        rules:
          - serviceAccount: {{name: builder}}
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
    };
    format!(
        "\
fullnameOverride: buildkit-controller
image: {{repository: buildkit-controller, tag: dev, pullPolicy: Never}}
replicaCount: 1
podDisruptionBudget: {{enabled: false}}
watchNamespaces: [{POOL_NS}]
logFormat: text
shutdown: {{delay: 0s, drainTimeout: 30s}}
edge:
  endpoint: \"\"
  serverName: {SERVER_NAME}
gateway:
  api:
    parentRefs: [{{name: {gw}, namespace: {gw_ns}, sectionName: api}}]
    hostnames: [localhost]
pools:
{a}{b}",
        gw = GATEWAY.name,
        gw_ns = GATEWAY.ns,
        a = pool("a", &format!("linux/{host}")),
        b = pool("b", &format!("linux/{foreign}, linux/riscv64")),
    )
}

/// Creates the cluster, installs the Gateway and the chart, and checks the
/// routes. Returns the Gateway's CA file, which API clients trust.
fn install(env: &Cluster, work: &Path) -> Result<PathBuf> {
    let ports = *PORTS;
    if !env.attached() {
        env.delete();
        ports.check_free(env.name())?;
    }
    let kind_cfg = crate::fixture(
        "tunnel-verify/kind.yaml",
        work,
        &[("API_PORT", ports.api.to_string())],
    )?;
    env.create(Some(&kind_cfg), "120s")?;
    crate::image::load_controller(env)?;
    crate::image::ensure_worker()?;
    status(&mut crate::kube::kind_load(WORKER_IMAGE, env.name()))?;
    let ca = GATEWAY.install(env, work)?;

    for ns in [NS, POOL_NS] {
        env.kubectl(&["create", "namespace", ns])?;
    }
    env.kubectl(&["create", "serviceaccount", "builder", "-n", POOL_NS])?;
    let values_file = write(&work.join("values.yaml"), &values(host_platform()?))?;
    let chart = root().join("helm/buildkit-controller");
    env.helm(&[
        "upgrade",
        "--install",
        "bk",
        &chart.to_string_lossy(),
        "-n",
        NS,
        "-f",
        &values_file,
    ])?;
    env.kubectl(&[
        "rollout",
        "status",
        "deploy/buildkit-controller",
        "-n",
        NS,
        "--timeout=300s",
    ])?;
    for p in ["a", "b"] {
        env.wait_condition(
            &format!("buildkitpool/{p}"),
            Some(POOL_NS),
            "Ready",
            Duration::from_mins(5),
        )?;
    }
    routes(env, &ca)?;
    Ok(ca)
}

/// The Gateway has only its terminating listener, no `TLSRoute` renders, and the
/// API route (with `/v1/tunnel` unbounded) and its `BackendTLSPolicy` work.
fn routes(env: &Cluster, ca: &Path) -> Result<()> {
    let listeners = env.kubectl(&[
        "get",
        "gateway",
        GATEWAY.name,
        "-n",
        GATEWAY.ns,
        "-o",
        "jsonpath={range .spec.listeners[*]}{.name}={.protocol}/{.tls.mode} {end}",
    ])?;
    ensure!(
        listeners.trim() == "api=HTTPS/Terminate",
        "the Gateway has other listeners: {listeners}"
    );
    let manifest = env.helm(&["get", "manifest", "bk", "-n", NS])?;
    ensure!(
        !manifest.contains("kind: TLSRoute"),
        "the chart rendered a TLSRoute:\n{manifest}"
    );
    let tlsroutes = env.kubectl(&["get", "tlsroute", "-n", NS, "-o", "name"])?;
    ensure!(tlsroutes.trim().is_empty(), "TLSRoutes exist: {tlsroutes}");
    for kind in ["httproute", "backendtlspolicy"] {
        until(
            &format!("{kind}/buildkit-controller-api Accepted"),
            Duration::from_mins(2),
            || {
                Ok(
                    crate::gateway::accepted(env, kind, "buildkit-controller-api", NS)?
                        .then_some(()),
                )
            },
        )?;
    }
    let rule = env.kubectl(&[
        "get",
        "httproute",
        "buildkit-controller-api",
        "-n",
        NS,
        "-o",
        "jsonpath={.spec.rules[0].matches[0].path.value} {.spec.rules[0].timeouts.request}",
    ])?;
    ensure!(
        rule.trim() == "/v1/tunnel 0s",
        "first HTTPRoute rule: {rule}"
    );
    // A TLS-verified 401 shows the route and the re-encryption work, and a 426
    // for a GET without Upgrade shows /v1/tunnel reaches the controller.
    for (path, want) in [("/v1/pools", "401"), ("/v1/tunnel", "426")] {
        until(
            &format!("{path} answers {want}"),
            Duration::from_mins(2),
            || {
                let probe = Command::new("curl")
                    .args(["-sS", "-o", "/dev/null", "-w", "%{http_code}", "--cacert"])
                    .arg(ca)
                    .arg(format!("{}{path}", *API))
                    .output()?;
                Ok(String::from_utf8_lossy(&probe.stdout)
                    .starts_with(want)
                    .then_some(()))
            },
        )?;
    }
    println!(
        "PASS routes: the Gateway's only listener is HTTPS/Terminate; no TLSRoute; HTTPRoute (with /v1/tunnel at timeout 0s) and BackendTLSPolicy Accepted; /v1/pools 401 and /v1/tunnel without Upgrade 426 through the Gateway"
    );
    Ok(())
}

/// One allocation, as files in its own directory.
struct Alloc {
    id: String,
    endpoint: String,
    dir: PathBuf,
}

impl Alloc {
    /// The allocation response.
    fn json(&self) -> PathBuf {
        self.dir.join("alloc.json")
    }

    /// The private key the allocation was made for.
    fn key(&self) -> PathBuf {
        self.dir.join("key.pem")
    }

    /// Where `client.js` writes the buildx material (`writeMaterial`).
    fn material(&self) -> PathBuf {
        self.dir.join("material")
    }
}

/// The public API through the Gateway, as a `ServiceAccount`.
struct Api {
    ca: PathBuf,
    /// The `Authorization` header in a file, so the token stays off curl's argv.
    auth: PathBuf,
    /// The same token, for the Action's post step (scenario d).
    token: String,
    work: PathBuf,
    made: AtomicUsize,
}

impl Api {
    fn new(env: &Cluster, ca: PathBuf, work: &Path) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let token = env.kubectl(&[
            "create",
            "token",
            "builder",
            "-n",
            POOL_NS,
            "--audience",
            AUD,
            "--duration",
            "60m",
        ])?;
        let auth = work.join("auth");
        let _ = std::fs::remove_file(&auth);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&auth)
            .with_context(|| auth.display().to_string())?;
        writeln!(f, "Authorization: Bearer {}", token.trim())?;
        Ok(Self {
            ca,
            auth,
            token: token.trim().to_owned(),
            work: work.to_path_buf(),
            made: AtomicUsize::new(0),
        })
    }

    /// `curl` against the API with the token and the Gateway's CA; returns the
    /// HTTP status, with the body in `out`.
    fn curl(&self, out: &Path, args: &[&str]) -> Result<String> {
        let mut c = Command::new("curl");
        c.args(["-sS", "--proto", "=https", "--max-time", "60", "--cacert"])
            .arg(&self.ca)
            .arg("-H")
            .arg(format!("@{}", self.auth.display()))
            .arg("-o")
            .arg(out)
            .args(["-w", "%{http_code}"])
            .args(args);
        Ok(crate::proc::stdout(&mut c)?.trim().to_owned())
    }

    /// Allocates a worker of `pool` with a fresh key, following a 202 until it binds.
    fn allocate(&self, pool: &str) -> Result<Alloc> {
        let n = self.made.fetch_add(1, Ordering::Relaxed);
        let dir = self.work.join(format!("alloc-{n}-{pool}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let key = KeyPair::generate()?;
        write(&dir.join("key.pem"), &key.serialize_pem())?;
        let body = serde_json::json!({
            "namespace": POOL_NS,
            "pool": pool,
            "publicKey": key.public_key_pem(),
            "ttlSeconds": 3600,
            "queueTimeoutSeconds": 240,
        });
        let req = write(&dir.join("req.json"), &body.to_string())?;
        let resp = dir.join("alloc.json");
        let code = self.curl(
            &resp,
            &[
                "-H",
                "Content-Type: application/json",
                "--data-binary",
                &format!("@{req}"),
                &format!("{}/v1/allocations", *API),
            ],
        )?;
        let id = field(&resp, "id")?;
        match code.as_str() {
            "201" => {}
            "202" => {
                let url = format!("{}/v1/allocations/{id}?wait=25s", *API);
                until(
                    &format!("allocation {id} bound"),
                    Duration::from_mins(2),
                    || Ok((self.curl(&resp, &[&url])? == "200").then_some(())),
                )?;
            }
            _ => bail!(
                "allocate {pool}: HTTP {code}: {}",
                std::fs::read_to_string(&resp).unwrap_or_default()
            ),
        }
        Ok(Alloc {
            endpoint: field(&resp, "endpoint")?,
            id,
            dir,
        })
    }

    fn release(&self, a: &Alloc) -> Result<()> {
        let code = self.curl(
            &a.dir.join("release.json"),
            &["-X", "DELETE", &format!("{}/v1/allocations/{}", *API, a.id)],
        )?;
        ensure!(code == "204", "release {}: HTTP {code}", a.id);
        Ok(())
    }
}

/// String field `key` of the JSON object in `path`.
fn field(path: &Path, key: &str) -> Result<String> {
    let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("not JSON: {text}"))?;
    v[key]
        .as_str()
        .map(str::to_owned)
        .with_context(|| format!("no {key} in {text}"))
}

/// A buildx builder with one remote node per allocation, each dialling its own
/// port of one forwarder process. `client.js` holds the forwarder; closing its
/// stdin stops it, so a killed task leaves no forwarder behind.
struct Builder {
    name: String,
    ports: Vec<u16>,
    /// The forwarder's pid, as the Action saves it in its state.
    forwarder: u64,
    stdin: Option<ChildStdin>,
    client: Spawned,
}

impl Builder {
    /// `nodes` pairs each allocation with the platforms its node declares.
    fn start(api: &Api, name: &str, nodes: &[(&Alloc, Vec<String>)]) -> Result<Self> {
        let cfg = serde_json::json!({
            "api": API.as_str(),
            "caFile": api.ca,
            "builder": name,
            "nodes": nodes.iter().map(|(a, platforms)| serde_json::json!({
                "alloc": a.json(),
                "key": a.key(),
                "dir": a.material(),
                "platforms": platforms,
            })).collect::<Vec<_>>(),
        });
        let cfg_file = write(&api.work.join(format!("{name}.json")), &cfg.to_string())?;
        let mut client = Spawned::spawn(
            Command::new("node")
                .arg(root().join("xtask/fixtures/tunnel-verify/client.js"))
                .arg(root().join("action/dist"))
                .arg(&cfg_file)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped()),
        )?;
        let stdin = client.take_stdin();
        let mut reader = BufReader::new(client.take_stdout().context("client.js stdout")?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        ensure!(
            !line.is_empty(),
            "client.js exited before the builder was up"
        );
        let v: serde_json::Value =
            serde_json::from_str(&line).with_context(|| format!("client.js printed {line:?}"))?;
        let ports = v["endpoints"]
            .as_array()
            .context("no endpoints")?
            .iter()
            .map(|e| local_port(e.as_str().unwrap_or_default()))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            ports.len() == nodes.len(),
            "{} endpoints for {} nodes",
            ports.len(),
            nodes.len()
        );
        let forwarder = v["pid"].as_u64().context("no forwarder pid")?;
        Ok(Self {
            name: name.to_owned(),
            ports,
            forwarder,
            stdin,
            client,
        })
    }

    /// `docker buildx build` on this builder.
    fn build(&self) -> Command {
        let mut c = Command::new("docker");
        c.args([
            "buildx",
            "build",
            "--builder",
            &self.name,
            "--progress=plain",
        ]);
        c
    }
}

impl Drop for Builder {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["buildx", "rm", "--force", &self.name])
            .output();
        drop(self.stdin.take());
        let _ = poll(
            "client.js to stop the forwarder",
            Duration::from_secs(5),
            Duration::from_millis(50),
            || Ok(self.client.try_wait()?.map(drop)),
        );
    }
}

/// A stand-in for the runner's OIDC token endpoint on loopback: every request
/// that names `bearer` gets `{"value": token}`, others a 401. It stops when dropped.
struct OidcStub {
    url: String,
    stop: Arc<std::sync::atomic::AtomicBool>,
    served: Arc<AtomicUsize>,
}

impl OidcStub {
    fn start(token: &str, bearer: &str) -> Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("http://{}/token", listener.local_addr()?);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let served = Arc::new(AtomicUsize::new(0));
        let body = serde_json::json!({ "value": token }).to_string();
        let want = format!("authorization: bearer {bearer}");
        let (stop_t, served_t) = (Arc::clone(&stop), Arc::clone(&served));
        std::thread::spawn(move || {
            while !stop_t.load(Ordering::Relaxed) {
                let Ok((mut s, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                };
                let _ = s.set_nonblocking(false);
                let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf) {
                        Ok(n) if n > 0 => req.extend_from_slice(&buf[..n]),
                        _ => break,
                    }
                }
                let head = String::from_utf8_lossy(&req).to_lowercase();
                let reply = if head.lines().any(|l| l.trim() == want) {
                    served_t.fetch_add(1, Ordering::Relaxed);
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                } else {
                    "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_owned()
                };
                let _ = s.write_all(reply.as_bytes());
            }
        });
        Ok(Self { url, stop, served })
    }
}

impl Drop for OidcStub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Runs the Action's real post step, `action/dist/post.js`, with the state the
/// main step saves for `builder`: it removes the builder, stops the forwarder
/// and releases `allocs` through the Gateway with the saved CA. Its OIDC token
/// comes from [`OidcStub`], so the release authenticates as the `ServiceAccount`.
/// Returns the step's output.
fn post_step(api: &Api, builder: &Builder, allocs: &[&Alloc]) -> Result<String> {
    let mut nonce = [0u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| anyhow::anyhow!("random bytes"))?;
    let bearer = hex(&nonce);
    let stub = OidcStub::start(&api.token, &bearer)?;
    let ca = std::fs::read_to_string(&api.ca)?;
    let join =
        |f: &dyn Fn(&Alloc) -> String| allocs.iter().map(|a| f(a)).collect::<Vec<_>>().join(",");
    let out = output(
        Command::new("node")
            .arg(root().join("action/dist/post.js"))
            .env("STATE_api", API.as_str())
            .env("STATE_ca", ca)
            .env("STATE_ids", join(&|a| a.id.clone()))
            .env("STATE_dirs", join(&|a| a.material().display().to_string()))
            .env("STATE_builder", &builder.name)
            .env("STATE_forwarder", builder.forwarder.to_string())
            .env("STATE_audience", AUD)
            .env("ACTIONS_ID_TOKEN_REQUEST_URL", &stub.url)
            .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", &bearer)
            .stdin(Stdio::null()),
    )?;
    ensure!(
        stub.served.load(Ordering::Relaxed) >= 1,
        "the post step never fetched its token:\n{out}"
    );
    ensure!(!out.contains("::warning::"), "the post step warned:\n{out}");
    Ok(out)
}

/// The port of a `tcp://127.0.0.1:<port>` endpoint, the only kind the forwarder hands out.
fn local_port(endpoint: &str) -> Result<u16> {
    endpoint
        .strip_prefix("tcp://127.0.0.1:")
        .and_then(|p| p.parse().ok())
        .with_context(|| format!("not a local forwarder endpoint: {endpoint:?}"))
}

/// The HTTP/2 client preface and an empty SETTINGS frame: what a gRPC client
/// sends first, so buildkitd keeps the connection.
const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\0\0\0\x04\0\0\0\0\0";
/// An HTTP/2 PING with an all-zero payload; the peer answers with a PING ACK.
const H2_PING: &[u8] = &[0, 0, 8, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// `openssl s_client` arguments for a TLS connection to the edge through a
/// forwarder `port`, offering `h2` as buildx does.
fn s_client(port: u16, ca: &Path, cert: &Path, key: &Path) -> Command {
    let mut c = Command::new("openssl");
    c.args(["s_client", "-quiet", "-nocommands", "-verify_return_error"])
        .args(["-connect", &format!("127.0.0.1:{port}")])
        .args(["-servername", SERVER_NAME, "-alpn", "h2", "-CAfile"])
        .arg(ca)
        .arg("-cert")
        .arg(cert)
        .arg("-key")
        .arg(key);
    c
}

/// An HTTP/2 connection to the worker through the tunnel that sends nothing
/// after its preface unless asked. `got` counts the bytes the worker sent.
struct Held {
    child: Spawned,
    stdin: ChildStdin,
    got: Arc<AtomicUsize>,
}

impl Held {
    fn open(port: u16, a: &Alloc) -> Result<Self> {
        let m = a.material();
        let mut child = Spawned::spawn(
            s_client(
                port,
                &m.join("ca.pem"),
                &m.join("cert.pem"),
                &m.join("key.pem"),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
        )?;
        let mut stdin = child.take_stdin().context("s_client stdin")?;
        let mut stdout = child.take_stdout().context("s_client stdout")?;
        let got = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&got);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                counter.fetch_add(n, Ordering::Relaxed);
            }
        });
        stdin.write_all(H2_PREFACE)?;
        stdin.flush()?;
        let mut held = Self { child, stdin, got };
        poll(
            "the worker's HTTP/2 SETTINGS through the tunnel",
            Duration::from_secs(10),
            Duration::from_millis(100),
            || {
                ensure!(held.alive()?, "the held connection closed at once");
                Ok((held.received() > 0).then_some(()))
            },
        )?;
        Ok(held)
    }

    fn alive(&mut self) -> Result<bool> {
        Ok(self.child.try_wait()?.is_none())
    }

    fn received(&self) -> usize {
        self.got.load(Ordering::Relaxed)
    }

    /// Sends a PING and waits for the worker's answer.
    fn ping(&mut self) -> Result<()> {
        let before = self.received();
        self.stdin.write_all(H2_PING)?;
        self.stdin.flush()?;
        poll(
            "the worker's PING ACK",
            Duration::from_secs(10),
            Duration::from_millis(100),
            || Ok((self.received() >= before + H2_PING.len()).then_some(())),
        )
    }
}

/// The sum of every sample of `name` whose labels include all of `labels`.
fn sample_sum(text: &str, name: &str, labels: &[(&str, &str)]) -> f64 {
    text.lines()
        .filter_map(|l| {
            let (series, value) = l.rsplit_once(' ')?;
            let (n, set) = match series.split_once('{') {
                Some((n, rest)) => (n, rest.strip_suffix('}')?),
                None => (series, ""),
            };
            let have: Vec<&str> = set.split(',').collect();
            (n == name
                && labels
                    .iter()
                    .all(|(k, v)| have.contains(&format!("{k}=\"{v}\"").as_str())))
            .then(|| value.parse::<f64>().ok())?
        })
        .sum::<f64>()
        // An empty sum is -0.0, which prints as "-0".
        + 0.0
}

fn metric(env: &Cluster, name: &str, labels: &[(&str, &str)]) -> Result<f64> {
    Ok(sample_sum(
        &env.kubectl(&["get", "--raw", METRICS])?,
        name,
        labels,
    ))
}

/// `buildkit_edge_bytes_total` for one direction and transport.
fn edge_bytes(env: &Cluster, direction: &str, transport: &str) -> Result<f64> {
    metric(
        env,
        "buildkit_edge_bytes_total",
        &[("direction", direction), ("transport", transport)],
    )
}

/// SHA-256 of the file at `path`, as hex.
fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).with_context(|| path.display().to_string())?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(hex(ctx.finish().as_ref()))
}

/// A fresh directory `work/name`.
fn fresh(work: &Path, name: &str) -> Result<PathBuf> {
    let d = work.join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

/// Scenario a's context: busybox (nothing can be pulled inside kind) and
/// [`BIG`] random bytes, which compress to nothing less. Returns the context
/// and the file's SHA-256.
fn big_context(work: &Path) -> Result<(PathBuf, String)> {
    let ctx = fresh(work, "ctx-big")?;
    copy_busybox(&ctx)?;
    let rng = ring::rand::SystemRandom::new();
    let mut f = std::fs::File::create(ctx.join("big"))?;
    let mut buf = vec![0u8; 1 << 20];
    for _ in 0..BIG / (1 << 20) {
        rng.fill(&mut buf)
            .map_err(|_| anyhow::anyhow!("random bytes"))?;
        f.write_all(&buf)?;
    }
    drop(f);
    write(
        &ctx.join("Dockerfile"),
        "FROM scratch\nCOPY lib/ /lib/\nCOPY bin/ /bin/\nCOPY big /big\nRUN [\"/bin/busybox\", \"sh\", \"-c\", \"/bin/busybox sha256sum /big > /big.sha256\"]\n",
    )?;
    let sum = sha256_file(&ctx.join("big"))?;
    Ok((ctx, sum))
}

/// A COPY-only context: it builds for any platform without emulation. `note`
/// holds `text`.
fn note_context(work: &Path, name: &str, text: &str) -> Result<PathBuf> {
    let ctx = fresh(work, name)?;
    write(&ctx.join("note"), text)?;
    write(&ctx.join("Dockerfile"), "FROM scratch\nCOPY note /note\n")?;
    Ok(ctx)
}

/// A client certificate and key from a CA the controller has never seen.
fn wrong_ca_cert(work: &Path) -> Result<(PathBuf, PathBuf)> {
    let mut p = CertificateParams::new(Vec::<String>::new())?;
    p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    let issuer = CertifiedIssuer::self_signed(p, KeyPair::generate()?)?;
    let key = KeyPair::generate()?;
    let mut leaf = CertificateParams::new(Vec::<String>::new())?;
    leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let cert = leaf.signed_by(&key, &issuer)?;
    let (cert_file, key_file) = (work.join("wrong-ca.crt"), work.join("wrong-ca.key"));
    write(&cert_file, &cert.pem())?;
    write(&key_file, &key.serialize_pem())?;
    Ok((cert_file, key_file))
}

#[allow(clippy::too_many_lines, reason = "one linear scenario script")]
fn run(env: &Cluster, work: &Path, idle: Duration) -> Result<()> {
    let ca = install(env, work)?;
    let api = Api::new(env, ca, work)?;
    let host = host_platform()?;
    let foreign = if host == "arm64" { "amd64" } else { "arm64" };
    let (big, big_sum) = big_context(work)?;

    // a. 100 MiB each way through the tunnel.
    let a = api.allocate("a")?;
    ensure!(
        a.endpoint == "/v1/tunnel",
        "allocation endpoint {:?}, not the tunnel path",
        a.endpoint
    );
    let one = Builder::start(
        &api,
        "bk-tunnel-one",
        &[(&a, vec![format!("linux/{host}")])],
    )?;
    let before = (
        edge_bytes(env, "client_to_worker", "tunnel")?,
        edge_bytes(env, "worker_to_client", "tunnel")?,
    );
    let out = fresh(work, "out-big")?;
    let start = Instant::now();
    let log = output(
        one.build()
            .arg("-o")
            .arg(format!("type=local,dest={}", out.display()))
            .arg(&big),
    )?;
    let secs = start.elapsed().as_secs_f64();
    let up = edge_bytes(env, "client_to_worker", "tunnel")? - before.0;
    let down = edge_bytes(env, "worker_to_client", "tunnel")? - before.1;
    let exported = sha256_file(&out.join("big"))?;
    let hashed = std::fs::read_to_string(out.join("big.sha256"))?;
    ensure!(
        exported == big_sum && hashed.starts_with(&big_sum),
        "sent {big_sum}, worker hashed {hashed:?}, got back {exported}:\n{log}"
    );
    #[allow(clippy::cast_precision_loss)]
    let big_f = BIG as f64;
    ensure!(
        up >= big_f && down >= big_f,
        "tunnel bytes: {up} up, {down} down; want {BIG} each way"
    );
    println!(
        "PASS a: {} MiB up and back through the tunnel in {secs:.1}s ({:.1} MB/s each way); the worker's hash and the exported file match; edge bytes transport=tunnel: {up:.0} up, {down:.0} down",
        BIG >> 20,
        big_f / 1e6 / secs
    );

    // b. An idle tunnel connection stays open, then the builder builds again.
    let mut held = Held::open(one.ports[0], &a)?;
    let active = metric(
        env,
        "buildkit_edge_active_connections",
        &[("transport", "tunnel")],
    )?;
    ensure!(active >= 1.0, "no active tunnel connection: {active}");
    let start = Instant::now();
    while start.elapsed() < idle {
        ensure!(
            held.alive()?,
            "the idle connection closed after {}s",
            start.elapsed().as_secs()
        );
        std::thread::sleep(Duration::from_secs(5).min(idle.saturating_sub(start.elapsed())));
    }
    ensure!(held.alive()?, "the idle connection closed");
    held.ping()?;
    let again = note_context(work, "ctx-again", "after the idle window\n")?;
    let log = output(one.build().args(["-o", "type=cacheonly"]).arg(&again))?;
    ensure!(log.contains("COPY note /note"), "build after idle:\n{log}");
    println!(
        "PASS b: a tunnel connection idle for {}s stayed open and answered a PING; the next build on the same builder worked",
        idle.as_secs()
    );

    // c. Releasing the allocation closes the open connection.
    let released = &[("reason", "released"), ("transport", "tunnel")];
    let rev_before = metric(env, "buildkit_edge_revocations_total", released)?;
    let start = Instant::now();
    api.release(&a)?;
    let cut = poll(
        "the released connection to close",
        Duration::from_secs(10),
        Duration::from_millis(50),
        || Ok((!held.alive()?).then(|| start.elapsed())),
    )?;
    let rev_after = metric(env, "buildkit_edge_revocations_total", released)?;
    ensure!(
        cut <= RELEASE_BOUND && rev_after > rev_before,
        "closed {:.2}s after the release (bound {RELEASE_BOUND:?}); revocations released {rev_before} -> {rev_after}",
        cut.as_secs_f64()
    );
    println!(
        "PASS c: releasing the allocation closed the open tunnel connection in {:.2}s; revocations{{reason=released,transport=tunnel}} {rev_before} -> {rev_after}",
        cut.as_secs_f64()
    );
    drop(held);
    drop(one);

    // d. Two pools, two forwarder ports, one builder, no allocationDomain.
    let (pa, pb) = (api.allocate("a")?, api.allocate("b")?);
    let multi = Builder::start(
        &api,
        "bk-tunnel-multi",
        &[
            (&pa, vec![format!("linux/{host}")]),
            (
                &pb,
                vec![format!("linux/{foreign}"), "linux/riscv64".to_owned()],
            ),
        ],
    )?;
    ensure!(
        multi.ports[0] != multi.ports[1],
        "one port for two nodes: {:?}",
        multi.ports
    );
    let ok = |pool: &'static str| {
        metric(
            env,
            "buildkit_edge_connections_total",
            &[("pool", pool), ("result", "ok"), ("transport", "tunnel")],
        )
    };
    let ok_before = (ok("a")?, ok("b")?);
    let mut nonce = [0u8; 8];
    ring::rand::SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| anyhow::anyhow!("random bytes"))?;
    let text = format!("multi-pool {}\n", hex(&nonce));
    let ctx = note_context(work, "ctx-multi", &text)?;
    let out = fresh(work, "out-multi")?;
    let log = output(
        multi
            .build()
            .args(["--platform", &format!("linux/{host},linux/{foreign}")])
            .arg("-o")
            .arg(format!("type=local,dest={}", out.display()))
            .arg(&ctx),
    )?;
    for arch in [host, foreign] {
        let got = std::fs::read_to_string(out.join(format!("linux_{arch}/note")))
            .with_context(|| format!("no linux/{arch} output:\n{log}"))?;
        ensure!(got == text, "linux/{arch} note: {got:?}");
    }
    let ok_after = (ok("a")?, ok("b")?);
    ensure!(
        ok_after.0 > ok_before.0 && ok_after.1 > ok_before.1,
        "tunnel connections per pool: a {} -> {}, b {} -> {}",
        ok_before.0,
        ok_after.0,
        ok_before.1,
        ok_after.1
    );
    println!(
        "PASS d: a two-node builder (pools a and b, forwarder ports {:?}) built linux/{host} and linux/{foreign} without edge.allocationDomain; tunnel connections a {} -> {}, b {} -> {}",
        multi.ports, ok_before.0, ok_after.0, ok_before.1, ok_after.1
    );

    // e. A certificate from another CA is refused through the tunnel.
    let tls_error = &[("result", "tls_error"), ("transport", "tunnel")];
    let refused_before = metric(env, "buildkit_edge_connections_total", tls_error)?;
    let (crt, key) = wrong_ca_cert(work)?;
    let mut probe = Spawned::spawn(
        s_client(multi.ports[0], &pa.material().join("ca.pem"), &crt, &key)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )?;
    let exited = poll(
        "the wrong-CA client to be refused",
        Duration::from_secs(10),
        Duration::from_millis(100),
        || probe.try_wait(),
    );
    if exited.is_err() {
        probe.kill();
    }
    let said = probe.wait_with_output()?;
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&said.stdout),
        String::from_utf8_lossy(&said.stderr)
    );
    let refused_after = metric(env, "buildkit_edge_connections_total", tls_error)?;
    ensure!(
        exited.is_ok_and(|s| !s.success())
            && said.contains("alert unknown ca")
            && refused_after > refused_before,
        "wrong-CA client not refused (tls_error {refused_before} -> {refused_after}):\n{said}"
    );
    let alert = said
        .lines()
        .find(|l| l.contains("alert unknown ca"))
        .unwrap_or_default()
        .trim();
    println!(
        "PASS e: a client certificate from another CA is refused through the tunnel: {alert}; connections{{result=tls_error,transport=tunnel}} {refused_before} -> {refused_after}"
    );

    // The Action's post step cleans up: builder, forwarder, and both releases
    // through the Gateway with the CA from its state.
    let said = post_step(&api, &multi, &[&pa, &pb])?;
    let forwarder_gone = i32::try_from(multi.forwarder)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .is_none_or(|p| rustix::process::test_kill_process(p).is_err());
    let builder_gone = !Command::new("docker")
        .args(["buildx", "inspect", &multi.name])
        .output()?
        .status
        .success();
    let mut gone = Vec::new();
    for a in [&pa, &pb] {
        let code = api.curl(
            &a.dir.join("release-again.json"),
            &["-X", "DELETE", &format!("{}/v1/allocations/{}", *API, a.id)],
        )?;
        gone.push(code);
    }
    ensure!(
        forwarder_gone && builder_gone && gone.iter().all(|c| c == "404"),
        "after post.js: forwarder gone {forwarder_gone}, builder gone {builder_gone}, release again {gone:?}:\n{said}"
    );
    println!(
        "PASS post: action/dist/post.js removed the builder, stopped the forwarder and released both allocations through the Gateway with the saved CA (a second DELETE answers {gone:?})"
    );
    drop(multi);

    // f. The transport label: tunnel for every connection above, none over TCP.
    let tunnel_ok = metric(
        env,
        "buildkit_edge_connections_total",
        &[("result", "ok"), ("transport", "tunnel")],
    )?;
    let tcp = metric(
        env,
        "buildkit_edge_connections_total",
        &[("transport", "tcp")],
    )?;
    let tcp_bytes =
        edge_bytes(env, "client_to_worker", "tcp")? + edge_bytes(env, "worker_to_client", "tcp")?;
    ensure!(
        tunnel_ok >= 3.0 && tcp == 0.0 && tcp_bytes == 0.0,
        "connections: {tunnel_ok} ok over the tunnel, {tcp} over TCP with {tcp_bytes} bytes"
    );
    println!(
        "PASS f: edge metrics: {tunnel_ok} connections ok with transport=tunnel; transport=tcp: {tcp} connections, {tcp_bytes} bytes"
    );
    println!("tunnel-verify: all scenarios passed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(
        clippy::float_cmp,
        reason = "the sums are small integers, exact in f64"
    )]
    fn sample_sum_matches_every_requested_label_in_any_order() {
        let text = "\
# TYPE buildkit_edge_bytes counter
buildkit_edge_bytes_total{direction=\"client_to_worker\",transport=\"tunnel\"} 10
buildkit_edge_bytes_total{direction=\"worker_to_client\",transport=\"tunnel\"} 7
buildkit_edge_bytes_total{direction=\"client_to_worker\",transport=\"tcp\"} 5
buildkit_edge_bytes_total_created{direction=\"client_to_worker\",transport=\"tunnel\"} 99
buildkit_leader 1
";
        let n = "buildkit_edge_bytes_total";
        assert_eq!(
            sample_sum(
                text,
                n,
                &[("transport", "tunnel"), ("direction", "client_to_worker")]
            ),
            10.0
        );
        assert_eq!(sample_sum(text, n, &[("transport", "tunnel")]), 17.0);
        assert_eq!(sample_sum(text, n, &[]), 22.0);
        assert_eq!(sample_sum(text, n, &[("transport", "quic")]), 0.0);
        assert_eq!(sample_sum(text, "buildkit_leader", &[]), 1.0);
    }

    #[test]
    fn local_port_takes_only_forwarder_endpoints() {
        assert_eq!(local_port("tcp://127.0.0.1:41234").unwrap(), 41234);
        assert!(local_port("/v1/tunnel").is_err());
        assert!(local_port("tcp://localhost:41234").is_err());
    }

    #[test]
    fn values_leave_the_edge_endpoint_empty_and_route_only_the_api() {
        let v = values("arm64");
        assert!(v.contains("  endpoint: \"\"\n"), "{v}");
        assert!(!v.contains("allocationDomain"), "{v}");
        assert!(v.contains("sectionName: api}]"), "{v}");
        assert!(!v.contains("edge:\n    parentRefs"), "{v}");
        assert!(v.contains("platforms: [linux/arm64]"), "{v}");
        assert!(v.contains("platforms: [linux/amd64, linux/riscv64]"), "{v}");
    }

    #[test]
    fn h2_frames_have_the_lengths_their_headers_name() {
        let settings = &H2_PREFACE[24..];
        assert_eq!(&H2_PREFACE[..24], b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
        assert_eq!((settings.len(), settings[3]), (9, 4));
        assert_eq!(
            (H2_PING.len(), usize::from(H2_PING[2]), H2_PING[3]),
            (17, 8, 6)
        );
    }
}
