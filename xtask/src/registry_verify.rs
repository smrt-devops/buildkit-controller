//! `cargo xtask registry-verify` runs the cache registry role in a throwaway
//! kind cluster against five real rootless buildkitd pods that talk to it with
//! internal-CA client certs:
//!
//! a. `trusted` (a push grant) builds and exports `--cache-to
//!    type=registry,...,mode=max,ignore-error=true,oci-mediatypes=true`.
//! b. `reader` (a pull-only grant, a fresh daemon with no local cache) builds
//!    the same context with `--cache-from` that ref, and its RUN step does
//!    not run.
//! c. The same `reader` build also exports to another tag; the registry
//!    refuses the push, `ignore-error` keeps the build green, and no manifest
//!    lands.
//! e to h. Branch caches: an untrusted worker reads the trusted cache through
//!    its branch scope and exports to it, a fresh worker of the same branch hits
//!    it, another branch and a trusted worker are refused it (see
//!    [`branch_scenarios`]).
//!
//! A hit is a step that did not run: every RUN step prints `ran-1-<name>`,
//! and its command spells that as `ran-$((1))-<name>`, so only the step's
//! output carries the marker.
//!
//! `--store minio|gcs|azure` runs the same scenarios with the registry's store
//! in `MinIO`, fake-gcs-server or Azurite, deployed in the cluster as `store`
//! and reached through `BUILDKIT_CACHE_STORE_URL` and
//! `BUILDKIT_CACHE_STORE_OPT_*` (the Secret `cache-store` holds credentials,
//! passed through `envFrom` with that prefix). Two things only a store can
//! show are added: after start-up the store has seen a listing (see
//! [`Store::listed`]), and scenario `i` deletes the registry pod and expects a
//! daemon with no local cache to still hit the cache the dead pod served.
//!
//! Expected wall time: volume 2 min 54 s measured with a cold controller image
//! build (137 s of it; about 45 s with the image cached). The stores add the
//! store image load and start-up, about 30 s for `MinIO` and fake-gcs-server and
//! about 60 s for Azurite, whose image is the largest; these are estimates
//! until the first run that has the bucket binary. Every wait is at most 90 s,
//! except the kind cluster create (120 s), the buildkitd pods' readiness (180 s)
//! and the registry rollout (120 s), which the volume run already needs.
//!
//! The registry's grants come from its static grants file, its certs from a
//! test CA made here. Every kind and kubectl call gets the private kubeconfig
//! under `target/registry-verify`; the task deletes the cluster on every exit
//! path.

use std::fmt::Write as _;
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
/// The RUN step every Dockerfile has, and the one only the branch build has.
const SHARED: &str = "registry-cache-step";
const STEP: &str = "branch-only-step";
use crate::image::CONTROLLER_IMAGE;
const HOST: &str = "buildkit-cache.bk-registry.svc:5000";
const METRICS: &str = "http://buildkit-cache.bk-registry.svc:8080/metrics";
const SOCKET: &str = "unix:///run/user/1000/buildkit/buildkitd.sock";

fn repo() -> String {
    format!("{HOST}/{NS}/{POOL}/app")
}

/// Branch scopes of two branches. The controller writes hash ids; the static
/// grants take any repository name.
const BRANCH_A: &str = "branch-a";
const BRANCH_B: &str = "branch-b";
/// The workers: one trusted, one read-only, two of branch a, one of branch b.
const PODS: [&str; 5] = ["trusted", "reader", "branch-a", "branch-a2", "branch-b"];

fn branch_repo(scope: &str) -> String {
    format!("{HOST}/{NS}/{POOL}/{scope}")
}

/// The registry: Deployment, Service, grants and buildkitd config. `store` is
/// where it keeps its blobs: its volume when `None`.
fn manifests(store: Option<Store>) -> String {
    // A store registry starts at 0: it needs its bucket, which is made later.
    let replicas = u8::from(store.is_none());
    let (args, env, volumes, mounts) = store.map_or_else(volume_parts, Store::registry_parts);
    format!(
        "\
apiVersion: v1
kind: ConfigMap
metadata: {{name: cache-grants, namespace: {NS}}}
data:
  grants.yaml: |
    - {{namespace: {NS}, pod: trusted, repository: {NS}/{POOL}/app, push: true}}
    - {{namespace: {NS}, pod: reader, repository: {NS}/{POOL}/app, push: false}}
    - {{namespace: {NS}, pod: branch-a, repository: {NS}/{POOL}/app, branch: {NS}/{POOL}/{BRANCH_A}}}
    - {{namespace: {NS}, pod: branch-a2, repository: {NS}/{POOL}/app, branch: {NS}/{POOL}/{BRANCH_A}}}
    - {{namespace: {NS}, pod: branch-b, repository: {NS}/{POOL}/app, branch: {NS}/{POOL}/{BRANCH_B}}}
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
  replicas: {replicas}
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
          args: [registry, --log-format=text, --tls-dir=/etc/tls, {args}--grants-file=/etc/grants/grants.yaml]
          env:
            - name: POD_NAME
              valueFrom: {{fieldRef: {{fieldPath: metadata.name}}}}
{env}          securityContext:
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
{mounts}      volumes:
        - {{name: tls, secret: {{secretName: cache-tls}}}}
        - {{name: grants, configMap: {{name: cache-grants}}}}
{volumes}---
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

/// The registry's arguments, extra environment, volumes and mounts when it
/// keeps its blobs on its own volume. Each string is spliced into the
/// Deployment: the last three are whole lines or empty.
fn volume_parts() -> (String, String, String, String) {
    (
        "--store-url=file:///data, ".into(),
        String::new(),
        "        - {name: data, emptyDir: {}}\n".into(),
        "            - {name: data, mountPath: /data}\n".into(),
    )
}

/// The bucket or container the registry's store lives in.
const BUCKET: &str = "bk-cache";
/// The prefix of a store option's environment variable.
const OPT: &str = "BUILDKIT_CACHE_STORE_OPT_";
/// The Secret of the store's credentials, passed through `envFrom` with [`OPT`].
const STORE_SECRET: &str = "cache-store";
/// Emulator credentials: `MinIO`'s are made up here, and Azurite's account and
/// key are the well-known ones every Azurite accepts. None is a real secret.
const MINIO_USER: &str = "bkadmin";
const MINIO_PASSWORD: &str = "bkadmin-secret";
const AZURE_ACCOUNT: &str = "devstoreaccount1";
const AZURE_KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

/// Creates the container over the Azure Blob REST API, with a `SharedKey`
/// signature, because the Azurite image carries no storage client. Arguments:
/// host, port, account, key, container. A 409 means it exists.
const AZURE_MKCONTAINER: &str = r"const http = require('http'), crypto = require('crypto');
const [host, port, acct, key, container] = process.argv.slice(1);
const date = new Date().toUTCString(), ver = '2021-08-06';
const sts = `PUT\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:${date}\nx-ms-version:${ver}\n/${acct}/${acct}/${container}\nrestype:container`;
const sig = crypto.createHmac('sha256', Buffer.from(key, 'base64')).update(sts).digest('base64');
const req = http.request({host, port, method: 'PUT', path: `/${acct}/${container}?restype=container`,
  headers: {'x-ms-date': date, 'x-ms-version': ver, 'Content-Length': 0, Authorization: `SharedKey ${acct}:${sig}`}},
  r => { console.log(r.statusCode); process.exit(r.statusCode === 201 || r.statusCode === 409 ? 0 : 1); });
req.on('error', e => { console.log(e.message); process.exit(1); });
req.end();";

/// An object store the registry can keep its blobs in, run in the cluster.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Store {
    /// `MinIO`, through the `s3://` scheme.
    Minio,
    /// fake-gcs-server, through `gs://`.
    Gcs,
    /// Azurite, through `az://`.
    Azure,
}

impl Store {
    pub(crate) const ALL: [Self; 3] = [Self::Minio, Self::Gcs, Self::Azure];

    /// The image as `(tag, index digest)`, pulled by digest and tagged locally
    /// (see `gateway::ensure_image`). The `minio/minio` repositories serve no
    /// pullable pinned release any more, so `MinIO` comes from the Bitnami
    /// archive, which carries the server and `mc` at one fixed release.
    pub(crate) const fn image(self) -> (&'static str, &'static str) {
        match self {
            Self::Minio => (
                "docker.io/bitnamilegacy/minio:2025.7.23-debian-12-r3",
                "sha256:953d489a81cc4de7975f90e07202189c4325da39b0b92470b6a13c7ea99e36cd",
            ),
            Self::Gcs => (
                "docker.io/fsouza/fake-gcs-server:1.52.2",
                "sha256:d47b4cf8b87006cab8fbbecfa5f06a2a3c5722e464abddc0d107729663d40ec4",
            ),
            Self::Azure => (
                "mcr.microsoft.com/azure-storage/azurite:3.35.0",
                "sha256:647c63a91102a9d8e8000aab803436e1fc85fbb285e7ce830a82ee5d6661cf37",
            ),
        }
    }

    const fn port(self) -> u16 {
        match self {
            Self::Minio => 9000,
            Self::Gcs => 4443,
            Self::Azure => 10000,
        }
    }

    /// Where the store answers in the cluster.
    fn endpoint(self) -> String {
        format!("http://store.{NS}.svc:{}", self.port())
    }

    /// The registry's `BUILDKIT_CACHE_STORE_URL`.
    fn url(self) -> String {
        let scheme = match self {
            Self::Minio => "s3",
            Self::Gcs => "gs",
            Self::Azure => "az",
        };
        format!("{scheme}://{BUCKET}")
    }

    /// The registry's plain options, as `(key in the variable name, value)`.
    fn options(self) -> Vec<(&'static str, String)> {
        let http = ("ALLOW_HTTP", "true".to_owned());
        match self {
            Self::Minio => vec![
                ("ENDPOINT", self.endpoint()),
                http,
                ("REGION", "us-east-1".into()),
            ],
            Self::Gcs => vec![
                ("BASE_URL", self.endpoint()),
                http,
                ("SKIP_SIGNATURE", "true".into()),
            ],
            Self::Azure => vec![
                ("ENDPOINT", format!("{}/{AZURE_ACCOUNT}", self.endpoint())),
                http,
                ("ACCOUNT_NAME", AZURE_ACCOUNT.into()),
            ],
        }
    }

    /// The credentials the Secret [`STORE_SECRET`] holds, as `(key, value)`.
    fn credentials(self) -> Vec<(&'static str, &'static str)> {
        match self {
            Self::Minio => vec![
                ("ACCESS_KEY_ID", MINIO_USER),
                ("SECRET_ACCESS_KEY", MINIO_PASSWORD),
            ],
            Self::Gcs => vec![],
            Self::Azure => vec![("ACCESS_KEY", AZURE_KEY)],
        }
    }

    /// The registry's arguments, extra environment, volumes and mounts (see
    /// [`volume_parts`]): no volume, the URL and options as environment, the
    /// credentials through `envFrom`.
    fn registry_parts(self) -> (String, String, String, String) {
        let mut env = String::new();
        let url = self.url();
        let _ = writeln!(
            env,
            "            - {{name: BUILDKIT_CACHE_STORE_URL, value: \"{url}\"}}"
        );
        for (key, value) in self.options() {
            let _ = writeln!(
                env,
                "            - {{name: {OPT}{key}, value: \"{value}\"}}"
            );
        }
        if !self.credentials().is_empty() {
            let _ = write!(
                env,
                "          envFrom:\n            - prefix: {OPT}\n              secretRef: {{name: {STORE_SECRET}}}\n"
            );
        }
        (String::new(), env, String::new(), String::new())
    }

    /// The store's own Deployment and Service, and the credentials Secret.
    fn manifest(self) -> String {
        let (tag, _) = self.image();
        let port = self.port();
        let (command, args, env, ready, user) = match self {
            Self::Minio => (
                "command: [/opt/bitnami/minio/bin/minio]\n          ",
                "args: [server, /data, --address, \":9000\"]\n          ".to_owned(),
                format!(
                    "env:
            - {{name: MINIO_ROOT_USER, value: {MINIO_USER}}}
            - {{name: MINIO_ROOT_PASSWORD, value: {MINIO_PASSWORD}}}
            - {{name: MINIO_PROMETHEUS_AUTH_TYPE, value: public}}
          volumeMounts:
            - {{name: data, mountPath: /data}}
          "
                ),
                "httpGet: {path: /minio/health/ready, port: 9000}",
                "securityContext: {runAsUser: 1001, runAsGroup: 1001, fsGroup: 1001}",
            ),
            Self::Gcs => (
                "",
                format!(
                    "args: [-scheme, http, -port, \"{port}\", -backend, memory, -external-url, \"{}\"]\n          ",
                    self.endpoint()
                ),
                String::new(),
                "tcpSocket: {port: 4443}",
                "securityContext: {}",
            ),
            Self::Azure => (
                "command: [azurite-blob]\n          ",
                "args: [--blobHost, \"0.0.0.0\", --skipApiVersionCheck]\n          ".to_owned(),
                String::new(),
                "tcpSocket: {port: 10000}",
                "securityContext: {}",
            ),
        };
        let secret = if self.credentials().is_empty() {
            String::new()
        } else {
            let data = self
                .credentials()
                .iter()
                .fold(String::new(), |mut s, (k, v)| {
                    let _ = writeln!(s, "  {k}: \"{v}\"");
                    s
                });
            format!(
                "apiVersion: v1\nkind: Secret\nmetadata: {{name: {STORE_SECRET}, namespace: {NS}}}\nstringData:\n{data}---\n"
            )
        };
        format!(
            "\
{secret}apiVersion: apps/v1
kind: Deployment
metadata: {{name: store, namespace: {NS}}}
spec:
  replicas: 1
  selector: {{matchLabels: {{app: store}}}}
  template:
    metadata: {{labels: {{app: store}}}}
    spec:
      automountServiceAccountToken: false
      {user}
      containers:
        - name: store
          image: {tag}
          imagePullPolicy: Never
          {command}{args}{env}readinessProbe:
            {ready}
            periodSeconds: 2
          ports:
            - {{name: store, containerPort: {port}}}
      volumes:
        - {{name: data, emptyDir: {{}}}}
---
apiVersion: v1
kind: Service
metadata: {{name: store, namespace: {NS}}}
spec:
  selector: {{app: store}}
  ports:
    - {{name: store, port: {port}, targetPort: store}}
"
        )
    }

    /// How many listings of the bucket `text` shows: `MinIO`'s metrics, or an
    /// emulator's request log.
    fn count_listings(self, text: &str) -> f64 {
        match self {
            Self::Minio => sum(text, "minio_s3_requests_total", &["api=\"listobjectsv2\""]),
            Self::Gcs => text
                .lines()
                .filter(|l| {
                    // A listing is a GET of the bucket's `o` collection; a
                    // GET of one object has a name after it.
                    let Some((_, rest)) = l.split_once("GET ") else {
                        return false;
                    };
                    let path = rest.split(' ').next().unwrap_or_default();
                    path.strip_prefix(&format!("/storage/v1/b/{BUCKET}/o"))
                        .is_some_and(|tail| tail.is_empty() || tail.starts_with('?'))
                })
                .fold(0.0, |n, _| n + 1.0),
            Self::Azure => text
                .lines()
                .filter(|l| {
                    // A blob listing names the container and `comp=list`;
                    // the account-level listing has no `restype=container`.
                    l.contains(&format!("GET /{AZURE_ACCOUNT}/{BUCKET}?"))
                        && l.contains("comp=list")
                        && l.contains("restype=container")
                })
                .fold(0.0, |n, _| n + 1.0),
        }
    }

    /// The text [`Store::count_listings`] reads.
    fn evidence(self, env: &Cluster) -> Result<String> {
        match self {
            Self::Minio => {
                let url = format!("{}/minio/v2/metrics/cluster", self.endpoint());
                let (ok, text) = exec(env, "trusted", &format!("wget -qO- {url}"))?;
                ensure!(ok, "MinIO metrics fetch failed: {text}");
                Ok(text)
            }
            Self::Gcs | Self::Azure => env.kubectl(&["-n", NS, "logs", "deploy/store"]),
        }
    }

    /// Whether the store has seen a listing of the bucket.
    fn listed(self, env: &Cluster) -> Result<bool> {
        Ok(self.count_listings(&self.evidence(env)?) > 0.0)
    }

    /// Waits for the store, then makes the bucket or container.
    fn create_bucket(self, env: &Cluster) -> Result<()> {
        env.kubectl(&[
            "-n",
            NS,
            "rollout",
            "status",
            "deploy/store",
            "--timeout=90s",
        ])?;
        let in_store = |script: &str| {
            env.kubectl(&["-n", NS, "exec", "deploy/store", "--", "sh", "-c", script])
        };
        match self {
            Self::Minio => {
                let mc = "/opt/bitnami/minio-client/bin/mc";
                in_store(&format!(
                    "export MC_CONFIG_DIR=/tmp/mc; {mc} alias set l http://127.0.0.1:9000 {MINIO_USER} {MINIO_PASSWORD} \
                     && {mc} mb --ignore-existing l/{BUCKET}"
                ))
                .map(drop)
            }
            // The fake's image has no shell: a buildkitd pod's busybox makes
            // the request.
            Self::Gcs => {
                let (ok, out) = exec(
                    env,
                    "trusted",
                    &format!(
                        "wget -qO- --header 'Content-Type: application/json' \
                         --post-data '{{\"name\":\"{BUCKET}\"}}' {}/storage/v1/b",
                        self.endpoint()
                    ),
                )?;
                ensure!(ok, "creating the bucket in fake-gcs-server: {out}");
                Ok(())
            }
            Self::Azure => {
                let script = format!(
                    "node -e '{AZURE_MKCONTAINER}' 127.0.0.1 10000 {AZURE_ACCOUNT} {AZURE_KEY} {BUCKET}"
                );
                in_store(&script).map(drop)
            }
        }
    }
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
/// a RUN that prints its marker when it runs. `/work/br` holds the Dockerfile
/// of a branch build: the same steps and one more of its own.
const PREPARE: &str = r#"set -e
mkdir -p /work/ctx/bin /work/ctx/lib /work/br
cp /bin/busybox /work/ctx/bin/
cp -L /lib/ld-musl-*.so.1 /lib/libc.musl-*.so.1 /work/ctx/lib/
cat > /work/ctx/Dockerfile <<'DOCKERFILE'
FROM scratch
COPY lib/ /lib/
COPY bin/ /bin/
RUN ["/bin/busybox", "sh", "-c", "/bin/busybox echo ran-$((1))-registry-cache-step | /bin/busybox tee /step"]
DOCKERFILE
cp /work/ctx/Dockerfile /work/br/Dockerfile
cat >> /work/br/Dockerfile <<'DOCKERFILE'
RUN ["/bin/busybox", "sh", "-c", "/bin/busybox echo ran-$((1))-branch-only-step | /bin/busybox tee /branch"]
DOCKERFILE
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

/// Runs `script` in pod `pod`, for 90 s at most; returns (success, stdout and
/// stderr).
fn exec(env: &Cluster, pod: &str, script: &str) -> Result<(bool, String)> {
    let out = env.kubectl_raw(
        &[
            "-n", NS, "exec", pod, "--", "timeout", "90", "sh", "-c", script,
        ],
        None,
    )?;
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

/// A branch build of the `/work/br` Dockerfile with `cache_flags`.
fn build_branch(cache_flags: &str) -> String {
    format!(
        "buildctl --addr {SOCKET} build --progress=plain --frontend dockerfile.v0 \
         --local context=/work/ctx --local dockerfile=/work/br {cache_flags}"
    )
}

/// The flags the controller gives a build of branch `scope`: import and
/// export the branch cache only. The registry answers what it lacks from the
/// trusted cache.
fn branch_flags(scope: &str) -> String {
    format!(
        "--import-cache type=registry,ref={b}:main \
         --export-cache type=registry,ref={b}:main,mode=max,ignore-error=true,oci-mediatypes=true",
        b = branch_repo(scope)
    )
}

/// Whether the RUN step `name` ran in the build that logged `log`, by the
/// marker it prints.
fn ran(log: &str, name: &str) -> bool {
    log.contains(&format!("ran-1-{name}"))
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
            &["-n", NS, "logs", "deploy/store", "--tail=40"][..],
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

pub(crate) fn registry_verify(store: Option<Store>) -> Result<()> {
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
    if let Some(store) = store {
        status(&mut crate::kube::kind_load(store.image().0, env.name()))?;
    }
    deploy(env, &work, store)?;

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
    let built = ran(&log, SHARED);
    r.check(
        "a/trusted build exports the cache",
        ok && built && manifests >= 1.0 && denied_push(env)? == 0.0,
        &format!("build ok={ok}, RUN step ran={built}, manifest pushes {manifests}"),
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
    let hit = !ran(&log, SHARED);
    let pulls = metric(
        env,
        "buildkit_registry_requests_total",
        &["route=\"manifest\"", "status=\"200\""],
    )? - pulls_before;
    r.check(
        "b/fresh reader imports the cache and hits",
        ok && hit && pulls >= 1.0,
        &format!("build ok={ok}, RUN step hit={hit}, {pulls} manifest pulls"),
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
    branch_scenarios(env, &mut r, &pushed)?;
    let refused = metric(env, "buildkit_registry_connections_refused_total", &[])?;
    r.check(
        "d/no connection refused at TLS",
        refused == 0.0,
        &format!("{refused} refused connections"),
    );
    if let Some(store) = store {
        store_scenarios(env, &mut r, store)?;
    }

    println!("registry-verify: {} passed, {} failed", r.passed, r.failed);
    if r.failed > 0 {
        bail!("{} registry-verify scenarios failed", r.failed);
    }
    cleanup.1.set(true);
    Ok(())
}

/// Branch caches, with the flags the controller gives each tier:
///
/// e. `branch-a` (untrusted, branch scope a) builds the branch Dockerfile:
///    the shared RUN step hits the trusted cache through the branch scope,
///    and the export to the branch links the trusted layers it reuses.
/// f. `branch-a2`, a fresh worker of the same branch, hits the branch's own
///    step from the branch cache.
/// g. `branch-b` (branch scope b) also asks for branch a's cache and gets
///    refused: its branch step runs again.
/// h. `trusted` asks for both branch caches and gets refused.
fn branch_scenarios(
    env: &Cluster,
    r: &mut Report,
    pushed: &dyn Fn(&Cluster) -> Result<f64>,
) -> Result<()> {
    let denied = |env: &Cluster, action: &str| {
        metric(
            env,
            "buildkit_registry_denied_total",
            &[&format!("action=\"{action}\"")],
        )
    };
    // e.
    let (before, denied_push) = (pushed(env)?, denied(env, "push")?);
    let (ok, log) = exec(env, "branch-a", &build_branch(&branch_flags(BRANCH_A)))?;
    let (shared, own) = (!ran(&log, SHARED), !ran(&log, STEP));
    let after = pushed(env)?;
    let pass = ok
        && shared
        && !own
        && after - before >= 1.0
        && (denied(env, "push")? - denied_push).abs() < f64::EPSILON;
    r.check(
        "e/branch build reads the trusted cache and exports its own",
        pass,
        &format!("build ok={ok}, shared step hit={shared}, branch step hit={own}, manifest pushes {before}->{after}"),
    );
    if !pass {
        println!("{log}");
    }

    // f.
    let (ok, log) = exec(env, "branch-a2", &build_branch(&branch_flags(BRANCH_A)))?;
    let own = !ran(&log, STEP);
    r.check(
        "f/second build of the branch hits its own branch cache",
        ok && own,
        &format!("build ok={ok}, branch step hit={own}"),
    );
    if !(ok && own) {
        println!("{log}");
    }

    // g.
    let denied_pull = denied(env, "pull")?;
    let flags = format!(
        "--import-cache type=registry,ref={}:main {}",
        branch_repo(BRANCH_A),
        branch_flags(BRANCH_B)
    );
    let (ok, log) = exec(env, "branch-b", &build_branch(&flags))?;
    let (own, pulls) = (!ran(&log, STEP), denied(env, "pull")? - denied_pull);
    r.check(
        "g/another branch cannot read the first branch's cache",
        ok && !own && pulls >= 1.0,
        &format!("build ok={ok}, branch step hit={own}, pull denials +{pulls}"),
    );
    if !ok || own {
        println!("{log}");
    }

    // h.
    let denied_pull = denied(env, "pull")?;
    let flags = format!(
        "--import-cache type=registry,ref={}:main --import-cache type=registry,ref={}:main",
        branch_repo(BRANCH_A),
        branch_repo(BRANCH_B)
    );
    let (ok, log) = exec(env, "trusted", &build_branch(&flags))?;
    let (own, pulls) = (!ran(&log, STEP), denied(env, "pull")? - denied_pull);
    r.check(
        "h/a trusted build sees no branch cache",
        ok && !own && pulls >= 2.0,
        &format!("build ok={ok}, branch step hit={own}, pull denials +{pulls}"),
    );
    if !ok || own {
        println!("{log}");
    }
    Ok(())
}

/// Certs, the registry, and the buildkitd pods with their contexts.
fn deploy(env: &Cluster, work: &Path, store: Option<Store>) -> Result<()> {
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
    for pod in PODS {
        let name = format!("{NONCE}.{pod}.{POOL}.{NS}.worker.buildkit.internal");
        let (crt, key) = pki.leaf(&[name], ExtendedKeyUsagePurpose::ClientAuth)?;
        secret(
            env,
            &certs,
            &format!("{pod}-certs"),
            &[("client.crt", &crt), ("client.key", &key), ("ca.crt", &ca)],
        )?;
    }
    if let Some(store) = store {
        env.apply(&store.manifest())?;
    }
    env.apply(&manifests(store))?;
    for pod in PODS {
        env.apply(&buildkitd(pod))?;
    }
    for pod in PODS {
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
    if let Some(store) = store {
        // The registry opens its bucket at start-up, so the bucket comes first.
        store.create_bucket(env)?;
        env.kubectl(&["-n", NS, "scale", "deploy/buildkit-cache", "--replicas=1"])?;
    }
    env.kubectl(&[
        "-n",
        NS,
        "rollout",
        "status",
        "deploy/buildkit-cache",
        "--timeout=120s",
    ])?;
    // The registry answers metrics before the first build.
    crate::proc::poll(
        "the cache registry's metrics",
        Duration::from_mins(1),
        Duration::from_secs(1),
        || Ok(metric(env, "buildkit_registry_denied_total", &[]).ok()),
    )?;
    if let Some(store) = store {
        // Start-up lists the store to rebuild the index, so it has seen a listing.
        crate::proc::until("the store to see a listing", Duration::from_mins(1), || {
            Ok(store.listed(env).ok().filter(|seen| *seen))
        })?;
    }
    Ok(())
}

/// Scenario i, the one the volume cannot pass: the registry pod is deleted,
/// and a daemon with no local cache (the reader, after a full prune) still
/// hits the cache the new pod serves from the store.
fn store_scenarios(env: &Cluster, r: &mut Report, store: Store) -> Result<()> {
    let pods = |env: &Cluster| {
        env.kubectl(&[
            "-n",
            NS,
            "get",
            "pods",
            "-l",
            "app=buildkit-cache",
            "-o",
            "jsonpath={range .items[*]}{.metadata.name} {.status.containerStatuses[0].ready}{\"\\n\"}{end}",
        ])
    };
    let old = pods(env)?;
    let old: Vec<&str> = old.lines().filter_map(|l| l.split(' ').next()).collect();
    env.kubectl(&[
        "-n",
        NS,
        "delete",
        "pod",
        "-l",
        "app=buildkit-cache",
        "--wait=true",
    ])?;
    let up = crate::proc::until("a new registry pod", Duration::from_secs(90), || {
        Ok(ready_replacement(&pods(env)?, &old))
    })
    .is_ok();
    let (pruned, out) = exec(
        env,
        "reader",
        &format!("buildctl --addr {SOCKET} prune --all"),
    )?;
    ensure!(pruned, "pruning the reader's local cache: {out}");
    let flags = format!("--import-cache type=registry,ref={}:main", repo());
    let (ok, log) = exec(env, "reader", &build(&flags))?;
    let hit = !ran(&log, SHARED);
    let pass = up && ok && hit;
    r.check(
        "i/the cache survives deleting the registry pod",
        pass,
        &format!("new pod ready={up}, build ok={ok}, RUN step hit={hit}, store {store:?}"),
    );
    if !pass {
        println!("{log}");
    }
    Ok(())
}

/// The name of a ready pod in `listing` (lines of `name ready`) that is not
/// in `old`.
fn ready_replacement(listing: &str, old: &[&str]) -> Option<String> {
    listing.lines().find_map(|l| {
        let (name, ready) = l.split_once(' ')?;
        (ready.trim() == "true" && !old.contains(&name)).then(|| name.to_owned())
    })
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
    fn a_step_ran_only_when_its_output_has_the_marker() {
        let step = |n: &str| {
            format!("#7 [5/5] RUN [\"sh\", \"-c\", \"echo ran-$((1))-{n} | tee /branch\"]\n")
        };
        let hit = format!("{}#7 CACHED\n", step(STEP));
        let miss = format!("{}#7 0.104 ran-1-{STEP}\n#7 DONE 0.1s\n", step(STEP));
        assert!(!ran(&hit, STEP), "the command text is not the marker");
        assert!(ran(&miss, STEP));
        assert!(!ran(&miss, SHARED));
    }

    #[test]
    fn the_volume_manifest_keeps_the_data_directory() {
        let m = manifests(None);
        assert!(m.contains("--store-url=file:///data"));
        assert!(m.contains("replicas: 1"));
        assert!(m.contains("mountPath: /data"));
        assert!(!m.contains("BUILDKIT_CACHE_STORE"));
    }

    #[test]
    fn a_store_manifest_points_the_registry_at_the_bucket() {
        for store in Store::ALL {
            let m = manifests(Some(store));
            assert!(
                m.contains(&format!(
                    "name: BUILDKIT_CACHE_STORE_URL, value: \"{}\"",
                    store.url()
                )),
                "{store:?}"
            );
            assert!(
                !m.contains("--store-url"),
                "the URL comes from the environment"
            );
            assert!(!m.contains("mountPath: /data"), "no volume holds the blobs");
            assert!(
                m.contains("replicas: 0"),
                "the registry waits for its bucket"
            );
            for (key, _) in store.options() {
                assert!(m.contains(&format!("name: {OPT}{key},")), "{store:?} {key}");
            }
            // Credentials only ever come through the Secret.
            for (_, secret) in store.credentials() {
                assert!(!m.contains(secret), "{store:?} leaks a credential");
                assert!(store.manifest().contains(secret));
            }
            assert_eq!(
                m.contains("envFrom"),
                !store.credentials().is_empty(),
                "{store:?}"
            );
        }
        assert!(manifests(Some(Store::Minio)).contains("s3://bk-cache"));
        assert!(manifests(Some(Store::Gcs)).contains("gs://bk-cache"));
        assert!(manifests(Some(Store::Azure)).contains("az://bk-cache"));
    }

    #[test]
    fn option_keys_are_distinct_upper_case_names() {
        for store in Store::ALL {
            let keys: Vec<_> = store.options().into_iter().map(|(k, _)| k).collect();
            let mut sorted = keys.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), keys.len(), "{store:?}");
            for key in keys
                .iter()
                .chain(store.credentials().iter().map(|(k, _)| k))
            {
                assert!(
                    key.bytes().all(|b| b.is_ascii_uppercase() || b == b'_'),
                    "{key}"
                );
            }
            // Never a key the URL owns (the registry refuses it).
            assert!(
                !keys
                    .iter()
                    .any(|k| k.contains("BUCKET") || k.contains("CONTAINER"))
            );
        }
    }

    #[test]
    fn store_images_are_pinned_by_digest_and_used_by_tag() {
        for store in Store::ALL {
            let (tag, digest) = store.image();
            let hex = digest.strip_prefix("sha256:").expect("a sha256 digest");
            assert_eq!(hex.len(), 64, "{tag}");
            assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()), "{tag}");
            assert!(
                tag.rsplit_once(':').is_some_and(|(_, v)| !v.contains('/'))
                    && !tag.ends_with(":latest")
            );
            let m = store.manifest();
            assert!(m.contains(&format!("image: {tag}\n")));
            assert!(m.contains("imagePullPolicy: Never"));
        }
        let tags: Vec<_> = Store::ALL.iter().map(|s| s.image().0).collect();
        assert_eq!(tags.len(), 3);
        assert!(tags[0] != tags[1] && tags[1] != tags[2] && tags[0] != tags[2]);
    }

    #[test]
    fn minio_serves_unauthenticated_metrics() {
        let m = Store::Minio.manifest();
        assert!(m.contains("MINIO_PROMETHEUS_AUTH_TYPE, value: public"));
        assert!(!m.contains("kind: Secret\nmetadata: {name: cache-store, namespace: bk-registry}\nstringData:\n  GOOGLE"));
    }

    #[test]
    fn minio_listings_come_from_the_listobjectsv2_counter() {
        let none = "\
minio_s3_requests_total{api=\"getbucketlocation\",server=\"x:9000\"} 3
minio_s3_requests_total{api=\"listobjectsv2\",server=\"x:9000\"} 0
";
        let some = none.replace(
            "listobjectsv2\",server=\"x:9000\"} 0",
            "listobjectsv2\",server=\"x:9000\"} 2",
        );
        assert!(Store::Minio.count_listings(none) < 1.0);
        assert!((Store::Minio.count_listings(&some) - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn gcs_listings_are_gets_of_the_bucket_collection_only() {
        let log = "\
time=2026-11-12T14:47:19.699Z level=INFO msg=\"10.0.0.1 - - [12/Nov/2026:14:47:19 +0000] \\\"POST /storage/v1/b HTTP/1.1\\\" 200 337\\n\"
time=2026-11-12T14:47:19.708Z level=INFO msg=\"10.0.0.1 - - [12/Nov/2026:14:47:19 +0000] \\\"GET /storage/v1/b/bk-cache/o?prefix=data/&delimiter= HTTP/1.1\\\" 200 27\\n\"
time=2026-11-12T14:47:19.709Z level=INFO msg=\"10.0.0.1 - - [12/Nov/2026:14:47:19 +0000] \\\"GET /storage/v1/b/bk-cache/o HTTP/1.1\\\" 200 27\\n\"
time=2026-11-12T14:47:19.710Z level=INFO msg=\"10.0.0.1 - - [12/Nov/2026:14:47:19 +0000] \\\"GET /storage/v1/b/bk-cache/o/data%2Fabc HTTP/1.1\\\" 200 5\\n\"
time=2026-11-12T14:47:19.711Z level=INFO msg=\"10.0.0.1 - - [12/Nov/2026:14:47:19 +0000] \\\"GET /storage/v1/b/other/o?prefix=x HTTP/1.1\\\" 200 5\\n\"
";
        assert!((Store::Gcs.count_listings(log) - 2.0).abs() < f64::EPSILON);
        assert!(Store::Gcs.count_listings("") < 1.0);
    }

    #[test]
    fn azure_listings_are_container_blob_listings_only() {
        let log = "\
127.0.0.1 - - [12/Nov/2026:14:47:41 +0000] \"PUT /devstoreaccount1/bk-cache?restype=container HTTP/1.1\" 201 -
127.0.0.1 - - [12/Nov/2026:14:47:42 +0000] \"GET /devstoreaccount1/bk-cache?prefix=data%2F&restype=container&comp=list HTTP/1.1\" 200 -
127.0.0.1 - - [12/Nov/2026:14:47:43 +0000] \"GET /devstoreaccount1?comp=list HTTP/1.1\" 200 -
127.0.0.1 - - [12/Nov/2026:14:47:44 +0000] \"GET /devstoreaccount1/bk-cache/data/abc HTTP/1.1\" 200 -
";
        assert!((Store::Azure.count_listings(log) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_replacement_pod_is_a_ready_pod_with_a_new_name() {
        let old = ["cache-1"];
        assert_eq!(ready_replacement("cache-1 true\n", &old), None);
        assert_eq!(ready_replacement("cache-2 false\n", &old), None);
        assert_eq!(ready_replacement("", &old), None);
        assert_eq!(
            ready_replacement("cache-1 true\ncache-2 true\n", &old).as_deref(),
            Some("cache-2")
        );
    }
}
