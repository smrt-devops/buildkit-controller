//! `cargo xtask registry-verify` runs the cache registry role in a throwaway
//! kind cluster against two real rootless buildkitd pods that talk to it with
//! internal-CA client certs:
//!
//! a. `trusted` (a push grant) builds and exports `--cache-to
//!    type=registry,...,mode=max,ignore-error=true,oci-mediatypes=true`.
//! b. `reader` (a pull-only grant, a fresh daemon with no local cache) builds
//!    the same context with `--cache-from` that ref and shows the steps
//!    `CACHED`.
//! c. The same `reader` build also exports to another tag; the registry
//!    refuses the push, `ignore-error` keeps the build green, and no manifest
//!    lands.
//!
//! The registry's grants come from its static grants file, its certs from a
//! test CA made here. Every kind and kubectl call gets the private kubeconfig
//! under `target/registry-verify`; the task deletes the cluster on every exit
//! path.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};

use super::root;
use crate::image::WORKER_IMAGE;
use crate::kube::Cluster;
use crate::proc::status;

const NS: &str = "bk-registry";
/// The pool the worker certs name; repositories live under `<NS>/<POOL>/`.
const POOL: &str = "cache";
const NONCE: &str = "0123456789abcdef";
use crate::image::CONTROLLER_IMAGE;
const HOST: &str = "buildkit-cache.bk-registry.svc:5000";
const METRICS: &str = "http://buildkit-cache.bk-registry.svc:8080/metrics";
const SOCKET: &str = "unix:///run/user/1000/buildkit/buildkitd.sock";

fn repo() -> String {
    format!("{HOST}/{NS}/{POOL}/app")
}

/// The registry: Deployment, Service, grants and buildkitd config.
fn manifests() -> String {
    format!(
        "\
apiVersion: v1
kind: ConfigMap
metadata: {{name: cache-grants, namespace: {NS}}}
data:
  grants.yaml: |
    - {{namespace: {NS}, pod: trusted, repository: {NS}/{POOL}/app, push: true}}
    - {{namespace: {NS}, pod: reader, repository: {NS}/{POOL}/app, push: false}}
---
apiVersion: v1
kind: ConfigMap
metadata: {{name: buildkitd-config, namespace: {NS}}}
data:
  buildkitd.toml: |
    [registry.\"{HOST}\"]
      ca = [\"/certs/ca.crt\"]
      [[registry.\"{HOST}\".keypair]]
        key = \"/certs/client.key\"
        cert = \"/certs/client.crt\"
---
apiVersion: apps/v1
kind: Deployment
metadata: {{name: buildkit-cache, namespace: {NS}}}
spec:
  replicas: 1
  selector: {{matchLabels: {{app: buildkit-cache}}}}
  template:
    metadata: {{labels: {{app: buildkit-cache}}}}
    spec:
      automountServiceAccountToken: false
      securityContext:
        runAsNonRoot: true
        runAsUser: 65532
        runAsGroup: 65532
        seccompProfile: {{type: RuntimeDefault}}
      containers:
        - name: cache
          image: {CONTROLLER_IMAGE}
          imagePullPolicy: Never
          args: [cache, --log-format=text, --tls-dir=/etc/tls, --data-dir=/data,
                 --grants-file=/etc/grants/grants.yaml]
          env:
            - name: POD_NAME
              valueFrom: {{fieldRef: {{fieldPath: metadata.name}}}}
          securityContext:
            allowPrivilegeEscalation: false
            readOnlyRootFilesystem: true
            capabilities: {{drop: [ALL]}}
          ports:
            - {{name: registry, containerPort: 5000}}
            - {{name: ops, containerPort: 8080}}
          readinessProbe:
            httpGet: {{path: /readyz, port: ops}}
            periodSeconds: 2
          volumeMounts:
            - {{name: tls, mountPath: /etc/tls, readOnly: true}}
            - {{name: grants, mountPath: /etc/grants, readOnly: true}}
            - {{name: data, mountPath: /data}}
      volumes:
        - {{name: tls, secret: {{secretName: cache-tls}}}}
        - {{name: grants, configMap: {{name: cache-grants}}}}
        - {{name: data, emptyDir: {{}}}}
---
apiVersion: v1
kind: Service
metadata: {{name: buildkit-cache, namespace: {NS}}}
spec:
  selector: {{app: buildkit-cache}}
  ports:
    - {{name: registry, port: 5000, targetPort: registry}}
    - {{name: ops, port: 8080, targetPort: ops}}
"
    )
}

/// A rootless buildkitd pod named `name` with its client cert under /certs.
fn buildkitd(name: &str) -> String {
    format!(
        "\
apiVersion: v1
kind: Pod
metadata: {{name: {name}, namespace: {NS}}}
spec:
  automountServiceAccountToken: false
  securityContext:
    runAsUser: 1000
    runAsGroup: 1000
    runAsNonRoot: true
    fsGroup: 1000
    seccompProfile: {{type: Unconfined}}
    appArmorProfile: {{type: Unconfined}}
  containers:
    - name: buildkitd
      image: {WORKER_IMAGE}
      imagePullPolicy: Never
      args: [--addr={SOCKET}, --config=/etc/buildkit/buildkitd.toml, --oci-worker-no-process-sandbox]
      readinessProbe:
        exec: {{command: [buildctl, --addr, {SOCKET}, debug, workers]}}
        periodSeconds: 1
      volumeMounts:
        - {{name: certs, mountPath: /certs, readOnly: true}}
        - {{name: config, mountPath: /etc/buildkit, readOnly: true}}
        - {{name: state, mountPath: /home/user/.local/share/buildkit}}
        - {{name: work, mountPath: /work}}
  volumes:
    - {{name: certs, secret: {{secretName: {name}-certs}}}}
    - {{name: config, configMap: {{name: buildkitd-config}}}}
    - {{name: state, emptyDir: {{}}}}
    - {{name: work, emptyDir: {{}}}}
"
    )
}

/// The same context in every pod: busybox and its loader, two COPY steps and
/// a RUN, so a cache hit shows as `CACHED` lines.
const PREPARE: &str = r#"set -e
mkdir -p /work/ctx/bin /work/ctx/lib
cp /bin/busybox /work/ctx/bin/
cp -L /lib/ld-musl-*.so.1 /lib/libc.musl-*.so.1 /work/ctx/lib/
printf 'FROM scratch\nCOPY lib/ /lib/\nCOPY bin/ /bin/\nRUN ["/bin/busybox", "sh", "-c", "/bin/busybox echo registry-cache-step > /step"]\n' > /work/ctx/Dockerfile
"#;

/// A test CA and the leaves the suite needs, as PEM.
struct Pki(CertifiedIssuer<'static, KeyPair>);

impl Pki {
    fn new() -> Result<Self> {
        let mut p = CertificateParams::new(Vec::<String>::new())?;
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        Ok(Self(CertifiedIssuer::self_signed(p, KeyPair::generate()?)?))
    }

    /// `(cert, key)` for `names` with `eku`.
    fn leaf(&self, names: &[String], eku: ExtendedKeyUsagePurpose) -> Result<(String, String)> {
        let key = KeyPair::generate()?;
        let mut p = CertificateParams::new(names.to_vec())?;
        p.extended_key_usages = vec![eku];
        Ok((p.signed_by(&key, &self.0)?.pem(), key.serialize_pem()))
    }
}

/// Creates Secret `name` with `files` (key, content) written under `dir`.
fn secret(env: &Cluster, dir: &Path, name: &str, files: &[(&str, &str)]) -> Result<()> {
    std::fs::create_dir_all(dir.join(name))?;
    let mut args = vec![
        "-n".to_owned(),
        NS.to_owned(),
        "create".into(),
        "secret".into(),
        "generic".into(),
        name.into(),
    ];
    for (k, v) in files {
        let path = dir.join(name).join(k);
        std::fs::write(&path, v)?;
        args.push(format!("--from-file={k}={}", path.display()));
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    env.kubectl(&args).map(drop)
}

/// Runs `script` in pod `pod`; returns (success, stdout and stderr).
fn exec(env: &Cluster, pod: &str, script: &str) -> Result<(bool, String)> {
    let out = env.kubectl_raw(&["-n", NS, "exec", pod, "--", "sh", "-c", script], None)?;
    Ok((
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    ))
}

/// The sum of the `name` samples whose labels contain every one of `labels`.
fn metric(env: &Cluster, name: &str, labels: &[&str]) -> Result<f64> {
    let (ok, text) = exec(env, "trusted", &format!("wget -qO- {METRICS}"))?;
    ensure!(ok, "metrics fetch failed: {text}");
    Ok(sum(&text, name, labels))
}

fn sum(text: &str, name: &str, labels: &[&str]) -> f64 {
    text.lines()
        .filter(|l| l.starts_with(&format!("{name}{{")) && labels.iter().all(|x| l.contains(x)))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .fold(0.0, |a, b| a + b)
}

fn build(cache_flags: &str) -> String {
    format!(
        "buildctl --addr {SOCKET} build --progress=plain --frontend dockerfile.v0 \
         --local context=/work/ctx --local dockerfile=/work/ctx {cache_flags}"
    )
}

fn export_to(tag: &str) -> String {
    format!(
        "--export-cache type=registry,ref={}:{tag},mode=max,ignore-error=true,oci-mediatypes=true",
        repo()
    )
}

/// Dumps state on a failed run; the cluster it holds is deleted after that.
struct Cleanup(Cluster, std::cell::Cell<bool>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        if self.1.get() {
            return;
        }
        for args in [
            &["get", "pods", "-A", "-o", "wide"][..],
            &["-n", NS, "describe", "pods"][..],
            &["-n", NS, "logs", "deploy/buildkit-cache", "--tail=80"][..],
        ] {
            if let Ok(o) = self.0.kubectl_raw(args, None) {
                println!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );
            }
        }
    }
}

struct Report {
    passed: usize,
    failed: usize,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool, detail: &str) {
        println!("{} {name}: {detail}", if ok { "PASS" } else { "FAIL" });
        if ok {
            self.passed += 1;
        } else {
            self.failed += 1;
        }
    }
}

/// The `CACHED` step lines of a plain-progress build log.
fn cached_steps(log: &str) -> usize {
    log.lines()
        .filter(|l| l.starts_with('#') && l.trim_end().ends_with(" CACHED"))
        .count()
}

pub(crate) fn registry_verify() -> Result<()> {
    let work = root().join("target/registry-verify");
    std::fs::create_dir_all(&work)?;
    let cleanup = Cleanup(
        Cluster::for_suite("bk-registry-verify", &work)?,
        std::cell::Cell::new(false),
    );
    let env = &cleanup.0;
    env.create(None, "120s")?;
    crate::image::load_controller(env)?;
    crate::image::ensure_worker()?;
    status(&mut crate::kube::kind_load(WORKER_IMAGE, env.name()))?;
    deploy(env, &work)?;

    let mut r = Report {
        passed: 0,
        failed: 0,
    };
    let pushed = |env: &Cluster| {
        metric(
            env,
            "buildkit_registry_requests_total",
            &["route=\"manifest\"", "method=\"PUT\"", "status=\"201\""],
        )
    };
    let denied_push =
        |env: &Cluster| metric(env, "buildkit_registry_denied_total", &["action=\"push\""]);

    // a.
    let (ok, log) = exec(env, "trusted", &build(&export_to("main")))?;
    let manifests = pushed(env)?;
    r.check(
        "a/trusted build exports the cache",
        ok && manifests >= 1.0 && denied_push(env)? == 0.0,
        &format!("build ok={ok}, manifest pushes {manifests}"),
    );
    if !ok {
        println!("{log}");
    }

    // b and c.
    let pulls_before = metric(
        env,
        "buildkit_registry_requests_total",
        &["route=\"manifest\"", "status=\"200\""],
    )?;
    let flags = format!(
        "--import-cache type=registry,ref={}:main {}",
        repo(),
        export_to("pr")
    );
    let (ok, log) = exec(env, "reader", &build(&flags))?;
    let cached = cached_steps(&log);
    let pulls = metric(
        env,
        "buildkit_registry_requests_total",
        &["route=\"manifest\"", "status=\"200\""],
    )? - pulls_before;
    r.check(
        "b/fresh reader imports the cache and hits",
        ok && cached >= 3 && pulls >= 1.0,
        &format!("build ok={ok}, {cached} CACHED steps, {pulls} manifest pulls"),
    );
    let denied = denied_push(env)?;
    let after = pushed(env)?;
    r.check(
        "c/push with a read-only grant is refused",
        ok && denied >= 1.0 && (after - manifests).abs() < f64::EPSILON,
        &format!("push denials {denied}, manifest pushes {manifests}->{after}"),
    );
    if r.failed > 0 {
        println!("{log}");
    }
    let refused = metric(env, "buildkit_registry_connections_refused_total", &[])?;
    r.check(
        "d/no connection refused at TLS",
        refused == 0.0,
        &format!("{refused} refused connections"),
    );

    println!("registry-verify: {} passed, {} failed", r.passed, r.failed);
    if r.failed > 0 {
        bail!("{} registry-verify scenarios failed", r.failed);
    }
    cleanup.1.set(true);
    Ok(())
}

/// Certs, the registry, and the two buildkitd pods with their contexts.
fn deploy(env: &Cluster, work: &Path) -> Result<()> {
    env.kubectl(&["create", "namespace", NS])?;
    env.kubectl(&[
        "label",
        "namespace",
        NS,
        "pod-security.kubernetes.io/enforce=privileged",
    ])?;
    let pki = Pki::new()?;
    let ca = pki.0.pem();
    let host = HOST.split(':').next().context("registry host")?;
    let (crt, key) = pki.leaf(
        &[host.to_owned(), format!("{host}.cluster.local")],
        ExtendedKeyUsagePurpose::ServerAuth,
    )?;
    let certs = work.join("certs");
    secret(
        env,
        &certs,
        "cache-tls",
        &[("tls.crt", &crt), ("tls.key", &key), ("ca.crt", &ca)],
    )?;
    for pod in ["trusted", "reader"] {
        let name = format!("{NONCE}.{pod}.{POOL}.{NS}.worker.buildkit.internal");
        let (crt, key) = pki.leaf(&[name], ExtendedKeyUsagePurpose::ClientAuth)?;
        secret(
            env,
            &certs,
            &format!("{pod}-certs"),
            &[("client.crt", &crt), ("client.key", &key), ("ca.crt", &ca)],
        )?;
    }
    env.apply(&manifests())?;
    env.apply(&buildkitd("trusted"))?;
    env.apply(&buildkitd("reader"))?;
    env.kubectl(&[
        "-n",
        NS,
        "rollout",
        "status",
        "deploy/buildkit-cache",
        "--timeout=120s",
    ])?;
    for pod in ["trusted", "reader"] {
        env.kubectl(&[
            "-n",
            NS,
            "wait",
            "--for=condition=Ready",
            &format!("pod/{pod}"),
            "--timeout=180s",
        ])?;
        let (ok, out) = exec(env, pod, PREPARE)?;
        ensure!(ok, "preparing the build context in {pod}: {out}");
    }
    // The registry answers metrics before the first build.
    crate::proc::poll(
        "the cache registry's metrics",
        Duration::from_mins(1),
        Duration::from_secs(1),
        || Ok(metric(env, "buildkit_registry_denied_total", &[]).ok()),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_sums_matching_samples() {
        let text = "\
buildkit_registry_requests_total{route=\"manifest\",method=\"PUT\",status=\"201\"} 2
buildkit_registry_requests_total{route=\"manifest\",method=\"GET\",status=\"200\"} 3
buildkit_registry_requests_total{route=\"blob\",method=\"PUT\",status=\"201\"} 7
";
        let n = sum(
            text,
            "buildkit_registry_requests_total",
            &["route=\"manifest\"", "method=\"PUT\""],
        );
        assert!((n - 2.0).abs() < f64::EPSILON);
        assert!((sum(text, "buildkit_registry_requests_total", &[]) - 12.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cached_lines_are_counted() {
        let log =
            "#5 [2/3] COPY lib/ /lib/\n#5 CACHED\n\n#6 [3/3] RUN x\n#6 CACHED \n#7 exporting\n";
        assert_eq!(cached_steps(log), 2);
    }
}
