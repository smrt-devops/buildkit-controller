//! `cargo xtask proxy-verify` checks PROXY protocol behind Envoy Gateway on a
//! throwaway kind cluster `bk-proxy-verify`.
//!
//! The task installs the pinned Envoy Gateway (see `gateway.rs`), then the chart
//! with `gateway.envoy.proxyProtocol` on and both listeners trusting the Envoy
//! pod addresses (each a /32). Two client pods (socat) stand for CI jobs in the
//! cluster, and the host reaches the Gateway through `kubectl port-forward`. The
//! scenarios are:
//!
//! a. A build with the curl recipe and `docker buildx` through the edge `TLSRoute`
//!    succeeds, and the controller logs the edge connection as the
//!    port-forward's address (127.0.0.1) via the Envoy pod.
//! b. An API request from client pod 1 through the `HTTPRoute` is logged with
//!    that pod's address, via the Envoy pod. No log line names an Envoy pod
//!    as `peer`.
//! c. Client pod 2 sends a forged PROXY header straight to the controller's API
//!    and edge ports. It is not trusted, so the bytes reach TLS, the handshakes
//!    fail, the logs show pod 2's own address, and the forged address appears
//!    nowhere.
//! d. The header counters show the proxied connections and no refusals.
//! e. With `api.maxConnectionsPerIp: 1`, pod 1 holds an API connection through
//!    a passthrough `TLSRoute`. Its second connection is refused while pod 2's
//!    goes through: the cap counts clients, not the Envoy pod.
//!
//! Every kind, kubectl and helm call gets the private kubeconfig under
//! `target/proxy-verify`. The task deletes the cluster on every exit path.

use std::fmt::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};

use buildkit_controller::net::ProxyError;

use crate::gateway::{PROXY as GATEWAY, SOCAT};
use crate::image::WORKER_IMAGE;
use crate::kube::Cluster;
use crate::proc::{Spawned, host_platform, output, until, write};
use crate::root;

const NS: &str = "bk-proxy";
const POOL_NS: &str = "proxy-ci";
const CLIENT_NS: &str = "bk-clients";
const SOCAT_IMAGE: &str = SOCAT.0;
const SERVER_NAME: &str = "bk.proxy.test";
/// SNI of the extra passthrough `TLSRoute` to the API (scenario e).
const PASSTHROUGH_HOST: &str = "pt.proxy.test";
const AUD: &str = "buildkit-controller";
const FORGED: [u8; 4] = [203, 0, 113, 9];
const CONTROLLER: &str = "buildkit-controller.bk-proxy.svc";

pub(crate) fn proxy_verify() -> Result<()> {
    let work = root().join("target/proxy-verify");
    std::fs::create_dir_all(&work)?;
    let env = &Cluster::for_suite("bk-proxy-verify", &work)?;
    if std::env::var_os("PROXY_VERIFY_KEEP").is_some() {
        env.keep();
    }
    let result = run(env, &work);
    if result.is_err() {
        for args in [
            vec!["get", "pods", "-A", "-o", "wide"],
            vec!["logs", "-n", NS, "deploy/buildkit-controller", "--tail=80"],
        ] {
            if let Ok(o) = env.kubectl(&args) {
                eprintln!("--- kubectl {args:?}\n{o}");
            }
        }
        crate::gateway::dump(env);
    }
    result
}

/// `bytes` as a `printf` format of octal escapes, which busybox `printf` accepts.
fn printf_octal(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "\\{b:03o}");
        s
    })
}

/// A PROXY v2 header over TCP/IPv4 claiming `src`.
fn forged_header(src: [u8; 4]) -> Vec<u8> {
    let mut h = b"\r\n\r\n\0\r\nQUIT\n".to_vec();
    h.extend_from_slice(&[0x21, 0x11, 0, 12]);
    h.extend_from_slice(&src);
    h.extend_from_slice(&[10, 0, 0, 1, 0, 1, 0x20, 0xfb]);
    h
}

fn values(trusted: &[String], edge_port: u16, host: &str) -> String {
    let cidrs = trusted
        .iter()
        .map(|ip| format!("{ip}/32"))
        .collect::<Vec<_>>()
        .join(", ");
    let gw = GATEWAY.name;
    let gw_ns = GATEWAY.ns;
    format!(
        "\
fullnameOverride: buildkit-controller
image: {{repository: buildkit-controller, tag: dev, pullPolicy: Never}}
replicaCount: 1
podDisruptionBudget: {{enabled: false}}
watchNamespaces: [{POOL_NS}]
logLevel: info,buildkit_controller::http::listener=debug,buildkit_controller::edge::listener=debug
shutdown: {{delay: 0s, drainTimeout: 30s}}
edge:
  endpoint: tcp://localhost:{edge_port}
  serverName: {SERVER_NAME}
  extraNames: [localhost]
  proxyProtocol: {{enabled: true, trustedCIDRs: [{cidrs}]}}
api:
  proxyProtocol: {{enabled: true, trustedCIDRs: [{cidrs}]}}
gateway:
  api:
    parentRefs: [{{name: {gw}, namespace: {gw_ns}, sectionName: api}}]
    hostnames: [localhost]
  edge:
    parentRefs: [{{name: {gw}, namespace: {gw_ns}, sectionName: edge}}]
  envoy: {{enabled: true, proxyProtocol: true}}
  # A second, passthrough route to the API for scenario e: one downstream
  # connection is one controller connection. It has its own Service, because
  # the BackendTLSPolicy on the chart's Service would make Envoy re-encrypt.
  extraManifests:
    - apiVersion: v1
      kind: Service
      metadata: {{name: api-passthrough, namespace: {NS}}}
      spec:
        selector: {{app.kubernetes.io/name: buildkit-controller, app.kubernetes.io/instance: bk}}
        ports: [{{name: api, port: 8443, targetPort: api}}]
    - apiVersion: gateway.networking.k8s.io/v1
      kind: TLSRoute
      metadata: {{name: api-passthrough, namespace: {NS}}}
      spec:
        parentRefs: [{{name: {gw}, namespace: {gw_ns}, sectionName: api-passthrough}}]
        hostnames: [{PASSTHROUGH_HOST}]
        rules: [{{backendRefs: [{{name: api-passthrough, port: 8443}}]}}]
    - apiVersion: gateway.envoyproxy.io/v1alpha1
      kind: BackendTrafficPolicy
      metadata: {{name: api-passthrough, namespace: {NS}}}
      spec:
        targetRefs: [{{group: gateway.networking.k8s.io, kind: TLSRoute, name: api-passthrough}}]
        proxyProtocol: {{version: V2}}
pools:
  - name: proxy
    namespace: {POOL_NS}
    spec:
      platforms: [linux/{host}]
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
}

/// A client pod: socat (and busybox) for raw TCP and TLS.
fn client_pod(name: &str) -> String {
    format!(
        "\
apiVersion: v1
kind: Pod
metadata: {{name: {name}, namespace: {CLIENT_NS}}}
spec:
  containers:
    - name: c
      image: {SOCAT_IMAGE}
      imagePullPolicy: Never
      command: [sleep, infinity]
"
    )
}

/// One JSON log line of the controller.
struct Event(serde_json::Map<String, serde_json::Value>);

impl Event {
    fn get(&self, key: &str) -> &str {
        self.0.get(key).and_then(|v| v.as_str()).unwrap_or("")
    }

    fn is(&self, message: &str) -> bool {
        self.get("message") == message
    }

    /// The IP of an `ip:port` field.
    fn ip(&self, key: &str) -> &str {
        self.get(key).rsplit_once(':').map_or("", |(ip, _)| ip)
    }
}

impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} peer={} via={}",
            self.get("message"),
            self.get("peer"),
            self.get("via")
        )
    }
}

struct Rig<'a> {
    env: &'a Cluster,
    /// In-cluster name of the Envoy Service.
    envoy: String,
}

impl Rig<'_> {
    /// `sh -c script` in client pod `pod`; `(exit ok, output)`.
    fn sh(&self, pod: &str, script: &str) -> Result<(bool, String)> {
        let o = self
            .env
            .kubectl_raw(
                &["exec", "-n", CLIENT_NS, pod, "--", "sh", "-c", script],
                None,
            )
            .context("kubectl exec")?;
        Ok((
            o.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
        ))
    }

    fn pod_ip(&self, pod: &str) -> Result<String> {
        Ok(self
            .env
            .kubectl(&[
                "get",
                "pod",
                "-n",
                CLIENT_NS,
                pod,
                "-o",
                "jsonpath={.status.podIP}",
            ])?
            .trim()
            .to_owned())
    }

    fn logs(&self) -> Result<String> {
        self.env
            .kubectl(&["logs", "-n", NS, "deploy/buildkit-controller", "--tail=-1"])
    }

    /// The controller's JSON log events (span fields merged in).
    fn events(&self) -> Result<Vec<Event>> {
        Ok(self
            .logs()?
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| match v {
                serde_json::Value::Object(m) => Some(Event(m)),
                _ => None,
            })
            .collect())
    }

    /// `openssl` client through socat to the passthrough API route, stdin from `input`.
    fn passthrough(&self, input: &str) -> String {
        format!(
            "{input} | socat -t2 - OPENSSL:{}:9443,verify=0,snihost={PASSTHROUGH_HOST}",
            self.envoy
        )
    }

    /// `name{result="<result>"}`, or `None` when the controller has not created
    /// that series. Fails when the metric family itself is missing.
    fn metric(&self, name: &str, result: &str) -> Result<Option<f64>> {
        let (ok, out) = self.sh(
            "p1",
            &format!(
                "(printf 'GET /metrics HTTP/1.0\\r\\n\\r\\n'; sleep 2) | socat -t5 - TCP:buildkit-controller-metrics.{NS}.svc:8080"
            ),
        )?;
        ensure!(ok, "metrics: {out}");
        let family = name.strip_suffix("_total").unwrap_or(name);
        ensure!(
            out.lines()
                .any(|l| l.starts_with(&format!("# TYPE {family} "))),
            "no {family} family in /metrics:\n{out}"
        );
        let want = format!("{name}{{result=\"{result}\"}} ");
        Ok(out
            .lines()
            .find_map(|l| l.strip_prefix(&want))
            .and_then(|v| v.trim().parse().ok()))
    }
}

#[allow(clippy::too_many_lines, reason = "one linear scenario script")]
fn run(env: &Cluster, work: &Path) -> Result<()> {
    env.create(None, "120s")?;
    crate::image::load_controller(env)?;
    crate::image::ensure_worker()?;
    crate::gateway::ensure_image(SOCAT.0, SOCAT.1)?;
    for image in [WORKER_IMAGE, SOCAT_IMAGE] {
        output(&mut crate::kube::kind_load(image, env.name()))?;
    }
    let gateway_ca = GATEWAY.install(env, work)?;
    let envoy_ips = GATEWAY.envoy_pod_ips(env)?;
    println!("Envoy pod addresses (trusted): {envoy_ips:?}");
    let rig = Rig {
        env,
        envoy: GATEWAY.envoy_host(env)?,
    };

    for ns in [NS, POOL_NS, CLIENT_NS] {
        env.kubectl(&["create", "namespace", ns])?;
    }
    env.kubectl(&["create", "serviceaccount", "builder", "-n", POOL_NS])?;
    for p in ["p1", "p2"] {
        let file = write(&work.join(format!("{p}.yaml")), &client_pod(p))?;
        env.kubectl(&["apply", "-f", &file])?;
    }

    // The host reaches the Gateway's API and edge listeners through port-forwards,
    // so edge.endpoint can name the local edge port.
    let api_pf = crate::gateway::PortForward::envoy(env, &GATEWAY, 8443)?;
    let edge_pf = crate::gateway::PortForward::envoy(env, &GATEWAY, 8444)?;
    let values_file = write(
        &work.join("values.yaml"),
        &values(&envoy_ips, edge_pf.local, host_platform()?),
    )?;
    let chart = root().join("helm/buildkit-controller");
    let chart = chart.to_string_lossy();
    env.helm(&[
        "upgrade",
        "--install",
        "bk",
        &chart,
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
    env.wait_condition(
        "buildkitpool/proxy",
        Some(POOL_NS),
        "Ready",
        Duration::from_mins(5),
    )?;
    for p in ["p1", "p2"] {
        env.kubectl(&[
            "wait",
            "--for=condition=Ready",
            &format!("pod/{p}"),
            "-n",
            CLIENT_NS,
            "--timeout=120s",
        ])?;
    }
    for (kind, name) in [
        ("backendtrafficpolicy", "buildkit-controller-api"),
        ("backendtrafficpolicy", "buildkit-controller-edge"),
        ("backendtrafficpolicy", "api-passthrough"),
        ("tlsroute", "buildkit-controller-edge"),
        ("tlsroute", "api-passthrough"),
        ("httproute", "buildkit-controller-api"),
    ] {
        until(
            &format!("{kind}/{name} Accepted"),
            Duration::from_mins(2),
            || Ok(crate::gateway::accepted(env, kind, name, NS)?.then_some(())),
        )?;
    }
    let pp = env.kubectl(&[
        "get",
        "backendtrafficpolicy",
        "-n",
        NS,
        "-o",
        "jsonpath={range .items[*]}{.metadata.name}={.spec.proxyProtocol.version} {end}",
    ])?;
    println!(
        "PASS setup: Envoy BackendTrafficPolicies with proxyProtocol: {}",
        pp.trim()
    );

    let api = format!("https://localhost:{}", api_pf.local);
    until("API through the Gateway", Duration::from_mins(2), || {
        let probe = Command::new("curl")
            .args(["-sS", "-o", "/dev/null", "-w", "%{http_code}", "--cacert"])
            .arg(&gateway_ca)
            .arg(format!("{api}/v1/pools"))
            .output()?;
        Ok(String::from_utf8_lossy(&probe.stdout)
            .starts_with("401")
            .then_some(()))
    })?;

    // a. A build through the edge TLSRoute, PROXY protocol on.
    let ctx = work.join("ctx");
    let _ = std::fs::remove_dir_all(&ctx);
    std::fs::create_dir_all(&ctx)?;
    write(&ctx.join("note"), "through the gateway\n")?;
    write(&ctx.join("Dockerfile"), "FROM scratch\nCOPY note /note\n")?;
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
            "30m",
        ])?
        .trim()
        .to_owned();
    let build = output(
        Command::new("sh")
            .arg(root().join("docs/ci/buildkit-allocate.sh"))
            .args([
                "sh",
                "-c",
                "docker buildx build --builder \"$BUILDER\" --progress=plain -o type=cacheonly .",
            ])
            .current_dir(&ctx)
            .env("API", &api)
            .env("API_CA", &gateway_ca)
            .env("POOL", format!("{POOL_NS}/proxy"))
            .env("TOKEN", &token)
            .env("QUEUE_TIMEOUT", "240")
            .stdin(Stdio::null()),
    )?;
    ensure!(build.contains("COPY note /note"), "build output:\n{build}");
    let is_envoy = |ip: &str| envoy_ips.iter().any(|e| e == ip);
    let events = rig.events()?;
    let edge: Vec<&Event> = events
        .iter()
        .filter(|e| e.is("edge connection proxied"))
        .collect();
    ensure!(!edge.is_empty(), "no edge connection in the logs");
    for e in &edge {
        ensure!(
            e.ip("peer") == "127.0.0.1" && is_envoy(e.ip("via")),
            "edge connection not logged as the client via Envoy: {e}"
        );
    }
    println!(
        "PASS a: build through the edge TLSRoute; {} edge connections logged as the port-forward client via the Envoy pod: {}",
        edge.len(),
        edge[0]
    );

    // b. An API request from a client pod is logged with the pod's address.
    let p1 = rig.pod_ip("p1")?;
    let (_, out) = rig.sh(
        "p1",
        &format!(
            "(printf 'GET /v1/pools HTTP/1.1\\r\\nHost: localhost\\r\\nConnection: close\\r\\n\\r\\n'; sleep 3) | socat -t5 - OPENSSL:{}:8443,verify=0,snihost=localhost",
            rig.envoy
        ),
    )?;
    ensure!(
        out.contains("401"),
        "API through the Gateway from p1: {out}"
    );
    let line = until(
        "p1's API connection in the logs",
        Duration::from_secs(30),
        || {
            Ok(rig
                .events()?
                .into_iter()
                .find(|e| e.is("API PROXY header accepted") && e.ip("peer") == p1)
                .map(|e| e.to_string()))
        },
    )?;
    let events = rig.events()?;
    let from_p1: Vec<&Event> = events.iter().filter(|e| e.ip("peer") == p1).collect();
    ensure!(
        from_p1
            .iter()
            .all(|e| e.get("via").is_empty() || is_envoy(e.ip("via"))),
        "p1 not via an Envoy pod: {}",
        from_p1
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    );
    let as_peer: Vec<String> = events
        .iter()
        .filter(|e| is_envoy(e.ip("peer")))
        .map(ToString::to_string)
        .collect();
    ensure!(
        as_peer.is_empty(),
        "an Envoy pod logged as peer: {as_peer:?}"
    );
    println!(
        "PASS b: API request from p1 ({p1}) through the HTTPRoute logged as peer={p1} via the Envoy pod; no event names an Envoy pod as peer: {line}"
    );

    // c. A forged header from an untrusted pod, straight to the controller.
    let p2 = rig.pod_ip("p2")?;
    let forged = printf_octal(&forged_header(FORGED));
    for port in [8443, 8444] {
        let _ = rig.sh(
            "p2",
            &format!("printf '{forged}' | socat -t3 - TCP:{CONTROLLER}:{port}"),
        )?;
    }
    let (api_line, edge_line) = until(
        "p2's failed handshakes in the logs",
        Duration::from_secs(30),
        || {
            let events = rig.events()?;
            let find = |m: &str| {
                events
                    .iter()
                    .find(|e| e.is(m) && e.ip("peer") == p2)
                    .map(|e| format!("{e} error={}", e.get("error")))
            };
            Ok(find("TLS handshake").zip(find("edge handshake failed")))
        },
    )?;
    let forged_ip = "203.0.113.9";
    ensure!(
        !rig.logs()?.contains(forged_ip),
        "the forged address {forged_ip} reached the logs"
    );
    println!(
        "PASS c: forged PROXY header from untrusted p2 ({p2}) fails TLS on both ports and is logged as p2; {forged_ip} appears nowhere:\n  {api_line}\n  {edge_line}"
    );

    // d. Header counters: proxied connections on both listeners, no refusals.
    let proxied = |family: &str| -> Result<f64> {
        rig.metric(family, "proxied")?
            .with_context(|| format!("no {family}{{result=\"proxied\"}} sample"))
    };
    let api_proxied = proxied("buildkit_api_proxy_protocol_headers_total")?;
    let edge_proxied = proxied("buildkit_edge_proxy_protocol_headers_total")?;
    ensure!(
        api_proxied >= 1.0 && edge_proxied >= 1.0,
        "proxied: api {api_proxied}, edge {edge_proxied}"
    );
    for family in [
        "buildkit_api_proxy_protocol_headers_total",
        "buildkit_edge_proxy_protocol_headers_total",
    ] {
        // The labels come from the controller, so a rename cannot leave this
        // reading series that no longer exist.
        for err in [
            ProxyError::Signature,
            ProxyError::Timeout(Duration::ZERO),
            ProxyError::Truncated,
        ] {
            let result = err.label();
            let n = rig.metric(family, result)?.unwrap_or(0.0);
            ensure!(n == 0.0, "{family}{{result={result}}} = {n}");
        }
    }
    println!(
        "PASS d: headers proxied: api {api_proxied}, edge {edge_proxied}; no refused headers (p2's forgery is not counted: untrusted)"
    );

    // e. The per-address cap counts clients behind Envoy.
    env.helm(&[
        "upgrade",
        "bk",
        &chart,
        "-n",
        NS,
        "-f",
        &values_file,
        "--set",
        "api.maxConnectionsPerIp=1",
    ])?;
    env.kubectl(&[
        "rollout",
        "status",
        "deploy/buildkit-controller",
        "-n",
        NS,
        "--timeout=300s",
    ])?;
    until(
        "passthrough route serves the new replica",
        Duration::from_mins(2),
        || Ok(rig.sh("p2", &rig.passthrough("true"))?.0.then_some(())),
    )?;
    // p2's probe connection ends when socat closes; let it go.
    std::thread::sleep(Duration::from_secs(3));
    // p1 holds one connection; the controller closes it after 10 s idle.
    let accepted_from_p1 = || -> Result<usize> {
        Ok(rig
            .events()?
            .iter()
            .filter(|e| e.is("API PROXY header accepted") && e.ip("peer") == p1)
            .count())
    };
    let before = accepted_from_p1()?;
    let mut held = Spawned::spawn(
        env.kubectl_cmd()
            .args(["exec", "-n", CLIENT_NS, "p1", "--", "sh", "-c"])
            .arg(rig.passthrough("sleep 7"))
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )
    .context("holding p1's connection")?;
    crate::proc::poll(
        "the controller to accept p1's held connection",
        Duration::from_secs(5),
        Duration::from_millis(200),
        || Ok((accepted_from_p1()? > before).then_some(())),
    )?;
    let (p1_second, p1_out) = rig.sh("p1", &rig.passthrough("true"))?;
    let (p2_first, p2_out) = rig.sh("p2", &rig.passthrough("true"))?;
    let _ = held.wait();
    ensure!(
        !p1_second,
        "p1's second connection got through a cap of 1: {p1_out}"
    );
    ensure!(
        p2_first,
        "p2 was refused while only p1 held a connection: {p2_out}"
    );
    let capped = rig
        .events()?
        .into_iter()
        .find(|e| e.is("API per-address connection limit reached; closing") && e.ip("peer") == p1)
        .context("no per-address refusal for p1 in the logs")?;
    println!(
        "PASS e: api.maxConnectionsPerIp=1 behind Envoy: p1's second connection refused, p2 served at the same time: {capped}"
    );
    println!("proxy-verify: all scenarios passed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forged_header_is_a_v2_header_for_the_address() {
        let h = forged_header(FORGED);
        assert_eq!(h.len(), 28);
        assert_eq!(&h[12..16], &[0x21, 0x11, 0, 12]);
        assert_eq!(&h[16..20], &FORGED);
        assert!(printf_octal(&h).starts_with("\\015\\012\\015\\012\\000"));
    }

    #[test]
    fn values_trust_each_envoy_pod() {
        let v = values(&["10.244.0.7".into(), "10.244.0.8".into()], 41000, "arm64");
        assert!(v.contains("trustedCIDRs: [10.244.0.7/32, 10.244.0.8/32]"));
        assert!(v.contains("endpoint: tcp://localhost:41000"));
        assert!(v.contains("envoy: {enabled: true, proxyProtocol: true}"));
    }
}
