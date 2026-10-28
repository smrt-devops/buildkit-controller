//! Envoy Gateway on a kind cluster: the reference Gateway API implementation
//! that `cargo xtask e2e` exposes the controller through.
//!
//! Everything is pinned. The Gateway API CRDs are the upstream standard-channel
//! bundle, checked against a SHA-256, so the chart is tested without the
//! experimental channel. The Envoy Gateway chart is a pinned version whose
//! archive is checked the same way, and its own CRDs come from that archive.
//! Both images are pulled on the host by digest, tagged, and loaded with
//! `kind load`, so the cluster pulls nothing. The Gateway and its listeners
//! are in `xtask/fixtures/gateway/gateway.yaml`.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use crate::kube::Cluster;
use crate::proc::stdout as run;
use crate::root;

/// Namespace of the Gateway the controller's routes attach to.
pub(crate) const NS: &str = "bk-gateway";
/// The Gateway's name.
pub(crate) const NAME: &str = "bk";
const EG_NS: &str = "envoy-gateway-system";
/// Gateway API v1.6.1, standard channel: `HTTPRoute`, `BackendTLSPolicy` and `TLSRoute` at v1.
const GATEWAY_API: (&str, &str) = (
    "https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.6.1/standard-install.yaml",
    "24d931f22abd8e40c973264319ead7cfa09d0fb7716b7ab1ee2ff174cb063a73",
);
/// Envoy Gateway v1.9.3, built against Gateway API v1.6.1. The hash is of the
/// chart archive `helm pull` writes.
const EG_CHART: &str = "oci://docker.io/envoyproxy/gateway-helm";
const EG_VERSION: &str = "v1.9.3";
const EG_CHART_SHA256: &str = "1a152513adbcdb0527d7bbe3ea9b090abeb87e4ec9f34b2c18ac8c37f3177541";
/// `(tag the cluster uses, index digest)`. The envoy tag is the default data
/// plane of Envoy Gateway v1.9.3. `envoy-gateway-values.yaml` and `gateway.yaml`
/// under `xtask/fixtures/gateway` name these tags.
const IMAGES: [(&str, &str); 2] = [
    (
        "docker.io/envoyproxy/gateway:v1.9.3",
        "sha256:192fc9bdadb6d2462e8cf7bd702ce3dd9648f3310c6038787448a1f509b7cd9f",
    ),
    (
        "docker.io/envoyproxy/envoy:distroless-v1.39.5",
        "sha256:9d84a586119a27b50437dbb01eccd4722832741539b1ed53c8e9d6da6067a6e0",
    ),
];
/// The TLS Secret of the Gateway's HTTPS listener.
const API_SECRET: &str = "bk-gateway-api";
/// socat 1.8.1.3 as `(tag, index digest)`: the mock OIDC issuer's server in e2e
/// and the client pods in proxy-verify. Pulled by digest and tagged locally.
pub(crate) const SOCAT: (&str, &str) = (
    "docker.io/alpine/socat:1.8.1.3",
    "sha256:603174b77eebaba25ba8f8ebb15e1234d6600409554e95d6fb203fd106150287",
);

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Whether `path` exists and hashes to `sha256`.
fn verified(path: &Path, sha256: &str) -> bool {
    std::fs::read(path).is_ok_and(|b| sha256_hex(&b) == sha256)
}

/// Fails unless `path` hashes to `sha256`.
fn verify(path: &Path, sha256: &str) -> Result<()> {
    let got = sha256_hex(&std::fs::read(path).with_context(|| path.display().to_string())?);
    if got != sha256 {
        bail!(
            "{} has SHA-256 {got}, want {sha256}; delete it and retry, or update the pin",
            path.display()
        );
    }
    Ok(())
}

/// The pinned image as `tag`, pulled by digest unless the local `tag` already is that digest.
pub(crate) fn ensure_image(tag: &str, digest: &str) -> Result<()> {
    let repo = tag.rsplit_once(':').map_or(tag, |(r, _)| r);
    let by_digest = format!("{repo}@{digest}");
    let local = Command::new("docker")
        .args(["image", "inspect", "--format", "{{json .RepoDigests}}", tag])
        .output()
        .context("docker image inspect")?;
    let short = repo.trim_start_matches("docker.io/");
    if local.status.success()
        && String::from_utf8_lossy(&local.stdout).contains(&format!("{short}@{digest}"))
    {
        return Ok(());
    }
    run(Command::new("docker").args(["pull", &by_digest]))?;
    run(Command::new("docker").args(["tag", &by_digest, tag]))?;
    Ok(())
}

/// Downloads and checks the pinned Gateway API CRDs and Envoy Gateway chart
/// into `target/gateway`. Returns `(CRD bundle, chart archive)`.
fn fetch() -> Result<(PathBuf, PathBuf)> {
    let _lock = crate::image::host_lock()?;
    let cache = root().join("target/gateway");
    std::fs::create_dir_all(&cache)?;
    let crds = cache.join("gateway-api-standard-install.yaml");
    if !verified(&crds, GATEWAY_API.1) {
        run(Command::new("curl")
            .args(["-fsSL", "--retry", "3", "-o"])
            .arg(&crds)
            .arg(GATEWAY_API.0))?;
    }
    verify(&crds, GATEWAY_API.1)?;
    let chart = cache.join(format!("gateway-helm-{EG_VERSION}.tgz"));
    if !verified(&chart, EG_CHART_SHA256) {
        let _ = std::fs::remove_file(&chart);
        run(crate::kube::helm(&crate::kube::sandbox())
            .args(["pull", EG_CHART, "--version", EG_VERSION, "-d"])
            .arg(&cache))?;
    }
    verify(&chart, EG_CHART_SHA256)?;
    Ok((crds, chart))
}

/// Installs Envoy Gateway and the Gateway `bk-gateway/bk` in `cluster`. The
/// Gateway's HTTPS listener serves a self-signed certificate for `localhost`;
/// returns the PEM file of that certificate, which API clients trust.
pub(crate) fn install(env: &Cluster, work: &Path) -> Result<PathBuf> {
    for (tag, digest) in IMAGES {
        ensure_image(tag, digest)?;
        run(&mut crate::kube::kind_load(tag, env.name()))?;
    }
    let (crds, chart) = fetch()?;
    let unpacked = work.join("gateway-chart");
    let _ = std::fs::remove_dir_all(&unpacked);
    std::fs::create_dir_all(&unpacked)?;
    let eg_crds = "gateway-helm/charts/crds/crds/generated";
    run(Command::new("tar")
        .arg("-xzf")
        .arg(&chart)
        .arg("-C")
        .arg(&unpacked)
        .arg(eg_crds))?;
    for f in [crds, unpacked.join(eg_crds)] {
        env.kubectl(&["apply", "--server-side", "-f", &f.to_string_lossy()])?;
    }
    let fixtures = root().join("xtask/fixtures/gateway");
    env.helm(&[
        "install",
        "eg",
        &chart.to_string_lossy(),
        "-n",
        EG_NS,
        "--create-namespace",
        "--skip-crds",
        "-f",
        &fixtures.join("envoy-gateway-values.yaml").to_string_lossy(),
        "--wait",
        "--timeout",
        "5m",
    ])?;
    println!("installed Envoy Gateway {EG_VERSION} and the Gateway API standard channel");

    env.kubectl(&["create", "namespace", NS])?;
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
        .context("gateway certificate")?;
    let ca = work.join("gateway-ca.pem");
    let key = work.join("gateway-key.pem");
    std::fs::write(&ca, cert.cert.pem())?;
    std::fs::write(&key, cert.signing_key.serialize_pem())?;
    env.kubectl(&[
        "create",
        "secret",
        "tls",
        API_SECRET,
        "-n",
        NS,
        &format!("--cert={}", ca.display()),
        &format!("--key={}", key.display()),
    ])?;
    env.kubectl(&[
        "apply",
        "-f",
        &fixtures.join("gateway.yaml").to_string_lossy(),
    ])?;
    env.wait_condition(
        &format!("gateway/{NAME}"),
        Some(NS),
        "Programmed",
        std::time::Duration::from_mins(5),
    )?;
    println!("Gateway {NS}/{NAME} Programmed");
    Ok(ca)
}

/// `status.ancestors[*].conditions[type=Accepted].status` of a route or policy.
pub(crate) fn accepted(env: &Cluster, kind: &str, name: &str, ns: &str) -> Result<bool> {
    let s = env.kubectl(&[
        "get",
        kind,
        name,
        "-n",
        ns,
        "-o",
        "jsonpath={range .status.parents[*]}{.conditions[?(@.type==\"Accepted\")].status} {end}{range .status.ancestors[*]}{.conditions[?(@.type==\"Accepted\")].status} {end}",
    ])?;
    let all: Vec<&str> = s.split_whitespace().collect();
    Ok(!all.is_empty() && all.iter().all(|v| *v == "True"))
}

/// Envoy Gateway's logs and the Gateway API objects, for a failed run.
pub(crate) fn dump(env: &Cluster) {
    for args in [
        vec![
            "get",
            "gateway,httproute,tlsroute,backendtlspolicy,backendtrafficpolicy",
            "-A",
            "-o",
            "yaml",
        ],
        vec!["logs", "-n", EG_NS, "deploy/envoy-gateway", "--tail=60"],
    ] {
        if let Ok(o) = env.kubectl(&args) {
            eprintln!("--- kubectl {args:?}\n{o}");
        }
    }
}

/// Label selector of the Gateway's Envoy Service and pods.
fn envoy_selector() -> String {
    format!(
        "gateway.envoyproxy.io/owning-gateway-name={NAME},gateway.envoyproxy.io/owning-gateway-namespace={NS}"
    )
}

/// The name of the Gateway's Envoy Service in `envoy-gateway-system`.
pub(crate) fn envoy_service(env: &Cluster) -> Result<String> {
    let svc = env.kubectl(&[
        "get",
        "svc",
        "-n",
        EG_NS,
        "-l",
        &envoy_selector(),
        "-o",
        "jsonpath={.items[0].metadata.name}",
    ])?;
    let svc = svc.trim();
    ensure!(!svc.is_empty(), "no Envoy Service for {NS}/{NAME}");
    Ok(svc.to_owned())
}

/// In-cluster DNS name of the Gateway's Envoy Service.
pub(crate) fn envoy_host(env: &Cluster) -> Result<String> {
    Ok(format!("{}.{EG_NS}.svc", envoy_service(env)?))
}

/// Pod IPs of the Gateway's Envoy pods: the source addresses of every
/// connection Envoy opens to a backend.
pub(crate) fn envoy_pod_ips(env: &Cluster) -> Result<Vec<String>> {
    let ips = env.kubectl(&[
        "get",
        "pods",
        "-n",
        EG_NS,
        "-l",
        &envoy_selector(),
        "--field-selector=status.phase=Running",
        "-o",
        "jsonpath={.items[*].status.podIP}",
    ])?;
    let ips: Vec<String> = ips.split_whitespace().map(str::to_owned).collect();
    ensure!(!ips.is_empty(), "no running Envoy pod for {NS}/{NAME}");
    Ok(ips)
}

/// `kubectl port-forward` to a port of the Gateway's Envoy Service, on a free
/// local port. Stopped when dropped.
pub(crate) struct PortForward {
    /// Held only to stop kubectl when dropped.
    _child: crate::proc::Spawned,
    pub(crate) local: u16,
}

impl PortForward {
    pub(crate) fn envoy(env: &Cluster, port: u16) -> Result<Self> {
        let svc = envoy_service(env)?;
        let mut child = crate::proc::Spawned::spawn(
            env.kubectl_cmd()
                .args(["port-forward", "-n", EG_NS])
                .arg(format!("svc/{svc}"))
                .arg(format!(":{port}"))
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
        )
        .context("kubectl port-forward")?;
        let stdout = child
            .child()?
            .stdout
            .take()
            .context("port-forward stdout")?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        // kubectl prints a line per connection and exits once its stdout is closed, so
        // keep reading until it ends.
        std::thread::spawn(move || reader.lines().for_each(drop));
        // "Forwarding from 127.0.0.1:54321 -> 9443"
        let local = line
            .split_whitespace()
            .nth(2)
            .and_then(|a| a.rsplit_once(':'))
            .and_then(|(_, p)| p.parse().ok());
        // Dropping the child on this return stops kubectl.
        let Some(local) = local else {
            bail!("unexpected port-forward output: {line:?}");
        };
        Ok(Self {
            _child: child,
            local,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::sha256_hex;

    #[test]
    fn sha256_is_lowercase_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
