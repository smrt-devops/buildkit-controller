//! `cargo xtask perf` measures the plan's efficiency targets on a throwaway
//! kind cluster (`bk-perf`). The method and the recorded runs are in
//! `docs/performance.md`. The task measures:
//!
//! 1. Allocation latency with a warm pod: `curl` in a client pod times an
//!    HTTP POST to `/v1/allocations` up to the 201 that carries the client
//!    cert. Every batch waits until enough free pods are Ready first.
//! 2. Edge connect overhead: time to first response byte of one HTTP/2
//!    request over a fresh mTLS connection, straight to the worker pod and
//!    through the edge, at the same concurrency.
//! 3. Edge throughput: a 512 MiB `COPY` with `buildctl`, through the edge and
//!    direct, as in `edge-verify`.
//! 4. Idle cost: each replica's RSS from `/proc` on the node, and every API
//!    request of the controller's `ServiceAccount` over an idle window, from
//!    the API server audit log.
//!
//! Every kind and kubectl call gets the private kubeconfig under `target/perf`.
//! The task never reads or writes `~/.kube/config`, and it deletes the cluster
//! on every exit path.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use buildkit_controller::pki::CaBundle;
use buildkit_controller::render::BUILDKIT_PORT;
use serde_json::Value;
use time::OffsetDateTime;

use super::edge_verify::{chart_rbac, controller_fixture, secret_from_files};
use super::reconcile_verify::KIND_CONFIG;
use super::root;
use crate::image::WORKER_IMAGE;
use crate::kube::Cluster;
use crate::proc::{status, stdout as run_out};

const SYSTEM_NS: &str = "buildkit-system";
const NS: &str = "bk-perf";
const POOL: &str = "perf";
const CURL_IMAGE: &str = "curlimages/curl:8.11.1";
const SERVER_NAME: &str = "bk.example.com";
const API: &str = "https://buildkit-controller.buildkit-system.svc:8443";
/// Warm pods the pool keeps (`scaling.min`).
const WARM: usize = 5;
/// Allocations per batch. Each one finds a warm pod.
pub(crate) const BATCH: usize = 4;
/// Connects per concurrency level and path.
const CONNECTS: usize = 1000;
/// Size of the throughput file (as in `edge-verify`).
const BIG: u64 = 512 * 1024 * 1024;
const LABEL_STATE: &str = "buildkit.smrt-devops.net/state";

/// Logs every request of the controller's `ServiceAccount` and nothing else.
const AUDIT_POLICY: &str = "apiVersion: audit.k8s.io/v1
kind: Policy
omitStages: [RequestReceived]
rules:
- level: Metadata
  users: [\"system:serviceaccount:buildkit-system:buildkit-controller\"]
- level: None
";

/// Shell helper: string field `$1` of the JSON in file `$2` (from the soak client).
const FIELD: &str = r#"field() { v=$(tr -d '\n' < "$2" | sed -n "s/.*\"$1\": *\"\([^\"]*\)\".*/\1/p"); printf '%b' "$v"; }
"#;

fn controller_manifest() -> Result<String> {
    let mut m = controller_fixture()?;
    for (from, to) in [
        (
            "    edge:\n      endpoint: tcp://bk.example.com:8444\n      serverName: bk.example.com\n",
            "    edge:\n      endpoint: tcp://buildkit-controller.buildkit-system.svc:8444\n      serverName: bk.example.com\n    auth:\n      serviceAccount: {enabled: true, audiences: [buildkit-controller]}\n",
        ),
        ("  replicas: 1\n", "  replicas: 2\n"),
        (
            "            - {name: edge, containerPort: 8444}\n",
            "            - {name: edge, containerPort: 8444}\n            - {name: api, containerPort: 8443}\n",
        ),
        (
            "    - {name: edge, port: 8444, targetPort: edge}\n",
            "    - {name: edge, port: 8444, targetPort: edge}\n    - {name: api, port: 8443, targetPort: api}\n",
        ),
    ] {
        ensure!(
            m.contains(from),
            "controller fixture changed: {from:?} not found"
        );
        m = m.replacen(from, to, 1);
    }
    Ok(m)
}

fn pool_manifest() -> String {
    format!(
        "apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitPool
metadata: {{name: {POOL}, namespace: {NS}}}
spec:
  scaling: {{min: {WARM}, max: 10}}
  allocation: {{defaultTTL: 2m, maxTTL: 30m, queueTimeout: 1m}}
  access:
    rules:
      - serviceAccount: {{name: perf-client}}
"
    )
}

/// A client pod: curl, plus `buildctl` and a 512 MiB random build context
/// from an init container. `extra` adds pod-level fields and `mounts` and
/// `volumes` add the credentials.
fn client_pod(ns: &str, labels: &str, extra: &str, mounts: &str, volumes: &str) -> String {
    format!(
        "apiVersion: v1
kind: Pod
metadata: {{name: client, namespace: {ns}, labels: {{{labels}}}}}
spec:
{extra}  securityContext:
    runAsNonRoot: true
    runAsUser: 1000
    seccompProfile: {{type: RuntimeDefault}}
  initContainers:
    - name: tools
      image: {WORKER_IMAGE}
      imagePullPolicy: Never
      command: [sh, -c]
      args:
        - |
          set -e
          cp /usr/bin/buildctl /tools/
          mkdir -p /work/big
          head -c {BIG} /dev/urandom > /work/big/big
          printf 'FROM scratch\\nCOPY big /big\\n' > /work/big/Dockerfile
          # The client key pair: only the public half is sent, as JSON string
          # content (newlines escaped).
          openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out /work/key.pem
          openssl pkey -in /work/key.pem -pubout -out /work/pub.pem
          sed 's/$/\\\\n/' /work/pub.pem | tr -d '\\n' > /work/pub.json
      securityContext: &sc
        allowPrivilegeEscalation: false
        capabilities: {{drop: [ALL]}}
      volumeMounts: [{{name: tools, mountPath: /tools}}, {{name: work, mountPath: /work}}]
  containers:
    - name: client
      image: {CURL_IMAGE}
      imagePullPolicy: Never
      command: [sleep, infinity]
      securityContext: *sc
      volumeMounts:
        - {{name: tools, mountPath: /tools}}
        - {{name: work, mountPath: /work}}
{mounts}  volumes:
    - {{name: tools, emptyDir: {{}}}}
    - {{name: work, emptyDir: {{}}}}
{volumes}"
    )
}

fn pool_client() -> String {
    let sa = "apiVersion: v1\nkind: ServiceAccount\nmetadata: {name: perf-client, namespace: bk-perf}\n---\n";
    sa.to_owned()
        + &client_pod(
            NS,
            "app: perf-client",
            "  serviceAccountName: perf-client\n",
            "        - {name: trust, mountPath: /trust}\n        - {name: bk-token, mountPath: /var/run/secrets/bk}\n",
            "    - {name: trust, configMap: {name: api-trust}}\n    - name: bk-token\n      projected:\n        sources:\n          - serviceAccountToken: {audience: buildkit-controller, path: token, expirationSeconds: 3600}\n",
        )
}

/// The direct client carries the controller's name label, which the worker
/// `NetworkPolicy` admits.
fn direct_client() -> String {
    client_pod(
        SYSTEM_NS,
        "app.kubernetes.io/name: buildkit-controller",
        "",
        "        - {name: certs, mountPath: /certs}\n",
        "    - {name: certs, secret: {secretName: direct-certs}}\n",
    )
}

/// Nearest-rank percentile of sorted samples.
fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn sorted(mut v: Vec<f64>) -> Vec<f64> {
    v.sort_by(f64::total_cmp);
    v
}

/// API request counts of an audit log slice, keyed `verb resource[/sub]`.
/// A watch counts when it starts, every other request when it completes.
fn requests(lines: &[&str]) -> BTreeMap<String, u64> {
    let mut m = BTreeMap::new();
    for e in lines
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
    {
        let verb = e["verb"].as_str().unwrap_or_default();
        let stage = e["stage"].as_str().unwrap_or_default();
        let counted = if verb == "watch" {
            stage == "ResponseStarted"
        } else {
            stage == "ResponseComplete"
        };
        if !counted {
            continue;
        }
        let o = &e["objectRef"];
        let mut res = o["resource"].as_str().unwrap_or("-").to_owned();
        if let Some(sub) = o["subresource"].as_str() {
            res = format!("{res}/{sub}");
        }
        *m.entry(format!("{verb} {res}")).or_insert(0) += 1;
    }
    m
}

/// Reads and list calls outside leader election: the polling the plan forbids.
fn polling(reqs: &BTreeMap<String, u64>) -> u64 {
    reqs.iter()
        .filter(|(k, _)| {
            (k.starts_with("get ") || k.starts_with("list ")) && !k.ends_with(" leases")
        })
        .map(|(_, n)| n)
        .sum()
}

struct Perf {
    env: Cluster,
    node: String,
}

impl Perf {
    fn k(&self, args: &[&str]) -> Result<String> {
        self.env.kubectl(args)
    }

    fn sh(&self, ns: &str, script: &str) -> Result<String> {
        self.k(&[
            "-n", ns, "exec", "client", "-c", "client", "--", "sh", "-c", script,
        ])
    }

    fn pods(&self, selector: &str) -> Result<Vec<Value>> {
        let out = self.k(&["-n", NS, "get", "pods", "-l", selector, "-o", "json"])?;
        let v: Value = serde_json::from_str(&out)?;
        Ok(v["items"].as_array().cloned().unwrap_or_default())
    }

    /// Free pods that are Ready and not terminating.
    fn warm(&self) -> Result<usize> {
        Ok(self
            .pods(&format!("{LABEL_STATE}=free"))?
            .iter()
            .filter(|p| p["metadata"]["deletionTimestamp"].is_null())
            .filter(|p| {
                p["status"]["conditions"].as_array().is_some_and(|c| {
                    c.iter()
                        .any(|c| c["type"] == "Ready" && c["status"] == "True")
                })
            })
            .count())
    }

    fn wait_warm(&self, n: usize) -> Result<()> {
        let start = Instant::now();
        while self.warm()? < n {
            ensure!(
                start.elapsed() < Duration::from_mins(4),
                "fewer than {n} warm pods after 4 minutes"
            );
            std::thread::sleep(Duration::from_millis(500));
        }
        Ok(())
    }

    fn post(ttl: u32, out: &str, fmt: &str) -> String {
        format!(
            "curl -s -o {out} -w '{fmt}' --cacert /trust/ca.crt \
             -H \"Authorization: Bearer $(cat /var/run/secrets/bk/token)\" \
             -H 'Content-Type: application/json' \
             -d '{{\"namespace\":\"{NS}\",\"pool\":\"{POOL}\",\"ttlSeconds\":{ttl},\"publicKey\":\"'\"$(cat /work/pub.json)\"'\"}}' {API}/v1/allocations"
        )
    }

    fn release(ids: &str) -> String {
        format!(
            "{FIELD}for f in {ids}; do id=$(field id $f); curl -s -o /dev/null --cacert /trust/ca.crt -X DELETE \
             -H \"Authorization: Bearer $(cat /var/run/secrets/bk/token)\" {API}/v1/allocations/$id; done"
        )
    }

    /// Allocation latency in ms, `batches` x `BATCH` samples.
    fn allocations(&self, batches: usize) -> Result<Vec<f64>> {
        let mut ms = Vec::new();
        for b in 0..batches {
            self.wait_warm(BATCH)?;
            let files: Vec<String> = (0..BATCH).map(|i| format!("/work/a.{i}.json")).collect();
            let script: String = files
                .iter()
                .map(|f| {
                    Self::post(120, f, "%{http_code} %{time_total}")
                        + &format!("; if grep -q clientCert {f}; then echo ' cert'; else echo ' nocert'; fi\n")
                })
                .collect();
            let out = self.sh(NS, &script)?;
            let mut got = 0;
            for l in out.lines() {
                let mut w = l.split_whitespace();
                let (Some("201"), Some(t), Some("cert")) = (w.next(), w.next(), w.next()) else {
                    bail!("batch {b}: allocation failed: {out}");
                };
                ms.push(t.parse::<f64>()? * 1e3);
                got += 1;
            }
            ensure!(
                got == BATCH,
                "batch {b}: {got} of {BATCH} allocations: {out}"
            );
            self.sh(NS, &Self::release(&files.join(" ")))?;
        }
        Ok(ms)
    }

    /// `CONNECTS` fresh mTLS connects split over `c` loops, each sending one
    /// HTTP/2 request. Returns (handshake ms, first byte ms) per connect.
    fn connects(&self, ns: &str, conc: usize, curl_args: &str) -> Result<(Vec<f64>, Vec<f64>)> {
        let per = CONNECTS / conc;
        let script = format!(
            "probe() {{ i=0; while [ $i -lt {per} ]; do i=$((i+1)); curl -s -o /dev/null --http2 \
             -w '%{{http_code}} %{{time_appconnect}} %{{time_starttransfer}}\\n' {curl_args}; done; }}\n\
             rm -f /work/p.*; for c in $(seq 1 {conc}); do probe > /work/p.$c & done; wait; cat /work/p.*"
        );
        let out = self.sh(ns, &script)?;
        let (mut hs, mut ttfb, mut codes) = (Vec::new(), Vec::new(), BTreeMap::new());
        for l in out.lines() {
            let w: Vec<&str> = l.split_whitespace().collect();
            let [code, a, t] = w[..] else { continue };
            let (a, t) = (a.parse::<f64>()?, t.parse::<f64>()?);
            ensure!(code != "000" && t > 0.0, "{ns}: connect failed: {l}");
            *codes.entry(code.to_owned()).or_insert(0_usize) += 1;
            hs.push(a * 1e3);
            ttfb.push(t * 1e3);
        }
        ensure!(
            ttfb.len() == per * conc,
            "{ns}: {} of {} connects",
            ttfb.len(),
            per * conc
        );
        println!("perf: {ns} c={conc} HTTP status counts {codes:?}");
        Ok((hs, ttfb))
    }

    /// RSS in MiB of each controller container, read from `/proc` on the node.
    fn rss(&self) -> Result<Vec<f64>> {
        let script = "for id in $(crictl ps -q --name '^controller$' --label io.kubernetes.pod.namespace=buildkit-system); do \
             pid=$(crictl inspect -o go-template --template '{{.info.pid}}' $id); \
             awk '/VmRSS/ {print $2}' /proc/$pid/status; done";
        let out = run_out(Command::new("docker").args(["exec", &self.node, "sh", "-c", script]))?;
        let kib: Vec<f64> = out
            .lines()
            .map(|l| l.trim().parse::<f64>().map(|k| k / 1024.0))
            .collect::<Result<_, _>>()?;
        ensure!(kib.len() == 2, "expected two controller containers: {out}");
        Ok(kib)
    }

    fn audit_lines(&self) -> Result<String> {
        run_out(Command::new("docker").args([
            "exec",
            &self.node,
            "cat",
            "/var/log/kubernetes/audit.log",
        ]))
    }
}

#[allow(clippy::too_many_lines, reason = "one linear setup script")]
fn setup(p: &Perf, work: &std::path::Path) -> Result<CaBundle> {
    crate::image::load_controller(&p.env)?;
    crate::image::ensure_worker()?;
    crate::image::ensure(CURL_IMAGE)?;
    for img in [WORKER_IMAGE, CURL_IMAGE] {
        status(&mut crate::kube::kind_load(img, p.env.name()))?;
    }
    p.env.apply_crds()?;
    p.k(&["create", "namespace", SYSTEM_NS])?;
    p.k(&[
        "label",
        "namespace",
        SYSTEM_NS,
        "pod-security.kubernetes.io/enforce=restricted",
    ])?;
    p.k(&["create", "namespace", NS])?;

    // The task supplies the CA so it can issue the direct client cert.
    let now = OffsetDateTime::now_utc().replace_nanosecond(0)?;
    let ca = CaBundle::generate(now)?;
    secret_from_files(
        &p.env,
        SYSTEM_NS,
        "buildkit-controller-ca",
        &work.join("ca"),
        ca.to_secret_data(),
    )?;
    let edge_client = ca.issue_edge_client(now)?;
    let mut files = BTreeMap::new();
    files.insert(
        "internal-ca.crt".to_owned(),
        ca.internal_ca().cert_pem().as_bytes().to_vec(),
    );
    files.insert(
        "direct.crt".to_owned(),
        edge_client.cert_pem().as_bytes().to_vec(),
    );
    files.insert(
        "direct.key".to_owned(),
        edge_client
            .private_key_pem()
            .context("generated key missing")?
            .into_bytes(),
    );
    secret_from_files(
        &p.env,
        SYSTEM_NS,
        "direct-certs",
        &work.join("direct"),
        files,
    )?;

    p.env
        .apply(&(chart_rbac("", &[])? + &controller_manifest()?))?;
    p.k(&[
        "-n",
        SYSTEM_NS,
        "rollout",
        "status",
        "deploy/buildkit-controller",
        "--timeout=180s",
    ])?;
    p.env.apply(&pool_manifest())?;
    let start = Instant::now();
    let bundle = loop {
        let b = p.env.kubectl_raw(
            &[
                "-n",
                SYSTEM_NS,
                "get",
                "configmap",
                "buildkit-controller-ca-bundle",
                "-o",
                "jsonpath={.data.ca\\.crt}",
            ],
            None,
        )?;
        if b.status.success() && !b.stdout.is_empty() {
            break b.stdout;
        }
        ensure!(
            start.elapsed() < Duration::from_mins(2),
            "no CA bundle ConfigMap"
        );
        std::thread::sleep(Duration::from_secs(1));
    };
    let trust = work.join("trust.crt");
    std::fs::write(&trust, bundle)?;
    p.k(&[
        "-n",
        NS,
        "create",
        "configmap",
        "api-trust",
        &format!("--from-file=ca.crt={}", trust.display()),
    ])?;
    p.env.apply(&pool_client())?;
    p.env.apply(&direct_client())?;
    for ns in [NS, SYSTEM_NS] {
        p.k(&[
            "-n",
            ns,
            "wait",
            "--for=condition=Ready",
            "pod/client",
            "--timeout=300s",
        ])?;
    }
    Ok(ca)
}

fn verdict(pass: bool) -> String {
    if pass { "PASS" } else { "MISS" }.to_owned()
}

struct Row {
    target: &'static str,
    measured: String,
    verdict: String,
}

/// Runs the loopback benchmark, `crates/server/benches/loopback.rs`, in release mode.
pub(crate) fn perf_loopback() -> Result<()> {
    let status = Command::new("cargo")
        .args([
            "bench",
            "--locked",
            "-p",
            "buildkit-controller",
            "--bench",
            "loopback",
        ])
        .current_dir(root())
        .status()
        .context("running cargo bench")?;
    ensure!(status.success(), "loopback benchmark failed");
    Ok(())
}

#[allow(clippy::too_many_lines, reason = "one linear measurement script")]
pub(crate) fn perf(batches: usize, idle_secs: u64) -> Result<()> {
    let work = root().join("target/perf");
    let policies = work.join("policies");
    std::fs::create_dir_all(&policies)?;
    std::fs::write(policies.join("audit.yaml"), AUDIT_POLICY)?;
    let kind_config = work.join("kind.yaml");
    std::fs::write(
        &kind_config,
        KIND_CONFIG.replace("POLICY_DIR", &policies.display().to_string()),
    )?;
    let name = super::kind_cluster("bk-perf");
    let p = &Perf {
        env: Cluster::new(&name, work.join("kubeconfig")),
        node: format!("{name}-control-plane"),
    };
    p.env.create(Some(&kind_config), "180s")?;
    setup(p, &work)?;
    println!("perf: cluster ready");

    // 1. Allocation latency with a warm pod.
    let alloc = sorted(p.allocations(batches)?);
    println!("perf: {} allocations measured", alloc.len());

    // 2 and 3 share one allocation and its worker.
    p.wait_warm(1)?;
    let out = p.sh(
        NS,
        &format!(
            "set -e\n{FIELD}{} >/dev/null\nfield clientCert /work/e.json > /work/e.crt\n\
             cp /work/key.pem /work/e.key\n{{ field caCert /work/e.json; cat /trust/ca.crt; }} > /work/e.ca\n\
             field id /work/e.json",
            Perf::post(1800, "/work/e.json", "%{http_code}")
        ),
    )?;
    let id = out.trim().to_owned();
    ensure!(!id.is_empty(), "edge allocation failed: {out}");
    let worker = p
        .pods("app.kubernetes.io/managed-by=buildkit-controller")?
        .into_iter()
        .find(|w| w["metadata"]["annotations"]["buildkit.smrt-devops.net/alloc"] == id.as_str())
        .context("no worker pod for the allocation")?;
    let pod_ip = worker["status"]["podIP"]
        .as_str()
        .context("worker pod IP")?
        .to_owned();
    let edge_ip = p
        .k(&[
            "-n",
            SYSTEM_NS,
            "get",
            "service",
            "buildkit-controller",
            "-o",
            "jsonpath={.spec.clusterIP}",
        ])?
        .trim()
        .to_owned();
    let nonce = worker["metadata"]["annotations"]["buildkit.smrt-devops.net/worker-nonce"]
        .as_str()
        .context("worker pod nonce")?;
    let pod = worker["metadata"]["name"]
        .as_str()
        .context("worker pod name")?;
    let worker_name = format!("{nonce}.{pod}.{POOL}.{NS}.worker.buildkit.internal");
    let direct_curl = format!(
        "--cacert /certs/internal-ca.crt --cert /certs/direct.crt --key /certs/direct.key \
         --connect-to {worker_name}:{BUILDKIT_PORT}:{pod_ip}:{BUILDKIT_PORT} https://{worker_name}:{BUILDKIT_PORT}/"
    );
    let edge_curl = format!(
        "--cacert /work/e.ca --cert /work/e.crt --key /work/e.key \
         --connect-to {SERVER_NAME}:8444:{edge_ip}:8444 https://{SERVER_NAME}:8444/"
    );
    let mut connect = Vec::new();
    for c in [1, 8] {
        let (dhs, d) = p.connects(SYSTEM_NS, c, &direct_curl)?;
        let (ehs, e) = p.connects(NS, c, &edge_curl)?;
        connect.push((c, sorted(dhs), sorted(d), sorted(ehs), sorted(e)));
    }
    println!("perf: connects measured");

    let mut rates = Vec::new();
    for (ns, fl) in [
        (
            NS,
            format!(
                "--addr tcp://{edge_ip}:8444 --tlsservername {SERVER_NAME} --tlscacert /work/e.ca \
                 --tlscert /work/e.crt --tlskey /work/e.key"
            ),
        ),
        (
            SYSTEM_NS,
            format!(
                "--addr tcp://{pod_ip}:{BUILDKIT_PORT} --tlsservername {worker_name} \
                 --tlscacert /certs/internal-ca.crt --tlscert /certs/direct.crt --tlskey /certs/direct.key"
            ),
        ),
    ] {
        let start = Instant::now();
        p.sh(
            ns,
            &format!(
                "/tools/buildctl {fl} build --progress=plain --no-cache --frontend dockerfile.v0 \
                 --local context=/work/big --local dockerfile=/work/big"
            ),
        )?;
        #[allow(clippy::cast_precision_loss)]
        rates.push(BIG as f64 / 1e6 / start.elapsed().as_secs_f64());
    }
    p.sh(NS, &Perf::release("/work/e.json"))?;
    println!("perf: throughput measured");

    // 4. Idle: no allocations, the pool back at its warm minimum.
    p.wait_warm(WARM)?;
    std::thread::sleep(Duration::from_mins(1));
    let states = p.k(&["-n", NS, "get", "pods", "-L", LABEL_STATE, "--no-headers"])?;
    println!("perf: worker pods at the start of the idle window:\n{states}");
    let before = p.audit_lines()?.lines().count();
    println!("perf: idle window of {idle_secs} s");
    std::thread::sleep(Duration::from_secs(idle_secs));
    let log = p.audit_lines()?;
    let lines: Vec<&str> = log.lines().skip(before).collect();
    let reqs = requests(&lines);
    let rss = p.rss()?;

    let mut rows = Vec::new();
    rows.push(Row {
        target: "allocation p50 < 150 ms (warm pod)",
        measured: format!(
            "p50 {:.1} / p95 {:.1} / p99 {:.1} ms (n={})",
            pct(&alloc, 50.0),
            pct(&alloc, 95.0),
            pct(&alloc, 99.0),
            alloc.len()
        ),
        verdict: verdict(pct(&alloc, 50.0) < 150.0),
    });
    for (c, dhs, d, ehs, e) in &connect {
        let over = pct(e, 99.0) - pct(d, 99.0);
        rows.push(Row {
            target: if *c == 1 {
                "edge connect overhead p99 < 5 ms (c=1)"
            } else {
                "edge connect overhead p99 < 5 ms (c=8)"
            },
            measured: format!(
                "{over:+.1} ms (p50 {:+.1}); first byte p50/p99 direct {:.1}/{:.1}, edge {:.1}/{:.1}; \
                 handshake p99 direct {:.1}, edge {:.1}",
                pct(e, 50.0) - pct(d, 50.0),
                pct(d, 50.0),
                pct(d, 99.0),
                pct(e, 50.0),
                pct(e, 99.0),
                pct(dhs, 99.0),
                pct(ehs, 99.0)
            ),
            verdict: if over < 0.0 {
                format!("inconclusive (p99 diff negative; p50 {:+.1} ms)", pct(e, 50.0) - pct(d, 50.0))
            } else {
                verdict(over < 5.0)
            },
        });
    }
    let (edge, direct) = (rates[0], rates[1]);
    rows.push(Row {
        target: "edge throughput >= 1 GiB/s (loopback-class)",
        measured: format!(
            "edge {edge:.0} MB/s, direct {direct:.0} MB/s ({:.0}% of direct)",
            edge / direct * 100.0
        ),
        verdict: verdict(edge >= 1_073.7),
    });
    let max_rss = rss.iter().copied().fold(0.0, f64::max);
    rows.push(Row {
        target: "idle replica RSS < 40 MiB",
        measured: format!(
            "{} MiB",
            rss.iter()
                .map(|r| format!("{r:.1}"))
                .collect::<Vec<_>>()
                .join(" / ")
        ),
        verdict: verdict(max_rss < 40.0),
    });
    let total: u64 = reqs.values().sum();
    rows.push(Row {
        target: "no API polling while idle",
        measured: format!(
            "{total} requests in {idle_secs} s, {} get/list outside leases: {}",
            polling(&reqs),
            reqs.iter()
                .map(|(k, n)| format!("{k}={n}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        verdict: verdict(polling(&reqs) == 0),
    });

    let mut table = String::from("| target | measured | verdict |\n|---|---|---|\n");
    for r in &rows {
        writeln!(table, "| {} | {} | {} |", r.target, r.measured, r.verdict)?;
    }
    print!("{table}");
    let stamp = OffsetDateTime::now_utc().unix_timestamp();
    std::fs::write(work.join(format!("result-{stamp}.md")), &table)?;
    std::fs::write(
        work.join(format!("alloc-{stamp}.txt")),
        alloc
            .iter()
            .map(|v| format!("{v:.2}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The client makes its own key and sends only the public half, as JSON
    /// string content. These are the exact shell lines checked in the worker image.
    #[test]
    fn client_sends_its_own_public_key() {
        let pod = pool_client();
        assert!(pod.contains("openssl pkey -in /work/key.pem -pubout -out /work/pub.pem\n"));
        assert!(pod.contains("sed 's/$/\\\\n/' /work/pub.pem | tr -d '\\n' > /work/pub.json\n"));
        let post = Perf::post(120, "/work/a.json", "%{http_code}");
        assert!(
            post.contains(r#""ttlSeconds":120,"publicKey":"'"$(cat /work/pub.json)"'"}'"#),
            "{post}"
        );
    }

    #[test]
    fn percentile_is_nearest_rank() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert!((pct(&v, 50.0) - 50.0).abs() < f64::EPSILON);
        assert!((pct(&v, 99.0) - 99.0).abs() < f64::EPSILON);
        assert!((pct(&[7.0], 99.0) - 7.0).abs() < f64::EPSILON);
        assert!(pct(&[], 50.0).is_nan());
    }

    #[test]
    fn audit_counts_watch_starts_and_completed_requests() {
        let lines = [
            r#"{"verb":"watch","stage":"ResponseStarted","objectRef":{"resource":"pods"}}"#,
            r#"{"verb":"watch","stage":"ResponseComplete","objectRef":{"resource":"pods"}}"#,
            r#"{"verb":"update","stage":"ResponseComplete","objectRef":{"resource":"leases"}}"#,
            r#"{"verb":"get","stage":"ResponseComplete","objectRef":{"resource":"leases"}}"#,
            r#"{"verb":"list","stage":"ResponseComplete","objectRef":{"resource":"pods"}}"#,
            r#"{"verb":"patch","stage":"ResponseComplete","objectRef":{"resource":"buildkitpools","subresource":"status"}}"#,
        ];
        let r = requests(&lines);
        assert_eq!(r["watch pods"], 1);
        assert_eq!(r["update leases"], 1);
        assert_eq!(r["patch buildkitpools/status"], 1);
        assert_eq!(polling(&r), 1, "{r:?}");
    }

    #[test]
    fn controller_manifest_adds_api_and_auth() {
        let m = controller_manifest().unwrap();
        assert!(m.contains("replicas: 2") && m.contains("containerPort: 8443"));
        assert!(m.contains("serviceAccount: {enabled: true"));
    }
}
