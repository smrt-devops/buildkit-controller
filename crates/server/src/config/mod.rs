//! Typed configuration from the YAML file, flags and environment.
//!
//! [`Config::from_yaml`] parses and validates; every validation problem is
//! collected into one [`ConfigError::Invalid`] so the operator sees them all
//! (exit code 2). [`Settings`] bundles the file with the flag/env values.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use buildkit_controller_api::v1alpha1::Duration as CrdDuration;
use clap::Parser;
use serde::Deserialize;

/// Config errors.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("reading config {path}: {source}")]
    Read {
        /// File path.
        path: String,
        /// Cause.
        source: std::io::Error,
    },
    /// The file is not valid YAML for the schema (unknown keys included).
    #[error("parsing config: {0}")]
    Parse(String),
    /// One or more validation rules failed.
    #[error("invalid configuration:\n  - {}", .0.join("\n  - "))]
    Invalid(Vec<String>),
}

/// Command line.
#[derive(Clone, Debug, Parser)]
#[command(name = "buildkit-controller", version, about)]
pub struct Cli {
    /// Config file.
    #[arg(
        long,
        env = "BUILDKIT_CONTROLLER_CONFIG",
        default_value = "/etc/buildkit-controller/config.yaml"
    )]
    pub config: PathBuf,
    /// Log filter, e.g. `info` or `buildkit_controller=debug`.
    #[arg(long, env = "LOG_LEVEL", default_value = "info")]
    pub log_level: String,
    /// Log encoding.
    #[arg(long, env = "LOG_FORMAT", value_enum, default_value = "json")]
    pub log_format: LogFormat,
    /// Lease identity and log field `replica`.
    #[arg(skip)]
    pub pod_name: String,
    /// Controller namespace.
    #[arg(skip)]
    pub pod_namespace: String,
}

/// Log encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum LogFormat {
    /// One JSON object per line.
    Json,
    /// Human readable.
    Text,
}

/// Everything the process was configured with.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Flags and environment.
    pub cli: Cli,
    /// The validated file.
    pub config: Config,
}

impl Settings {
    /// Read `POD_NAME` / `POD_NAMESPACE`, load and validate the file.
    pub fn load(mut cli: Cli, env: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut problems = Vec::new();
        for (var, slot) in [
            ("POD_NAME", &mut cli.pod_name),
            ("POD_NAMESPACE", &mut cli.pod_namespace),
        ] {
            match env(var).filter(|v| !v.is_empty()) {
                Some(v) => *slot = v,
                None => problems.push(format!("environment variable {var} is required")),
            }
        }
        let text = std::fs::read_to_string(&cli.config).map_err(|source| ConfigError::Read {
            path: cli.config.display().to_string(),
            source,
        })?;
        let config = match Config::from_yaml(&text) {
            Ok(c) => {
                problems.extend(c.own_namespace_problems(&cli.pod_namespace));
                c
            }
            Err(ConfigError::Invalid(mut more)) => {
                problems.append(&mut more);
                return Err(ConfigError::Invalid(problems));
            }
            Err(e) => return Err(e),
        };
        if problems.is_empty() {
            Ok(Self { cli, config })
        } else {
            Err(ConfigError::Invalid(problems))
        }
    }
}

/// The YAML file.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Config {
    /// Namespaces whose pools, pods and claims the controller watches; `[]` =
    /// all namespaces. Read through [`Config::watched_namespaces`].
    pub watch_namespaces: Vec<String>,
    /// Listen addresses.
    pub listen: Listen,
    /// API listener limits.
    pub api: Api,
    /// Kubernetes client settings.
    pub kube: Kube,
    /// Leader election.
    pub leader_election: LeaderElection,
    /// PKI.
    pub pki: Pki,
    /// Edge.
    pub edge: Edge,
    /// Authentication.
    pub auth: Auth,
    /// Worker `NetworkPolicy` inputs.
    pub worker_network: WorkerNetwork,
    /// Shutdown timing.
    pub shutdown: Shutdown,
    /// The `BuildKitClaim` controller.
    pub claims: Claims,
    /// The built-in cache registry.
    pub registry: Registry,
}

/// `listen`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Listen {
    /// HTTPS API.
    pub api: SocketAddr,
    /// mTLS edge.
    pub edge: SocketAddr,
    /// Plaintext ops port.
    pub ops: SocketAddr,
}

impl Default for Listen {
    fn default() -> Self {
        Self {
            api: SocketAddr::from(([0, 0, 0, 0], 8443)),
            edge: SocketAddr::from(([0, 0, 0, 0], 8444)),
            ops: SocketAddr::from(([0, 0, 0, 0], 8080)),
        }
    }
}

/// `api`: limits of the `listen.api` listener.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Api {
    /// Open API connections per replica; more are closed on accept.
    pub max_connections: u32,
    /// Open API connections from one client address; more are closed on accept. 0: no cap.
    pub max_connections_per_ip: u32,
    /// PROXY protocol v2 from a gateway in front of the listener.
    pub proxy_protocol: ProxyProtocol,
}

impl Default for Api {
    fn default() -> Self {
        Self {
            max_connections: 4096,
            max_connections_per_ip: 128,
            proxy_protocol: ProxyProtocol::default(),
        }
    }
}

/// `api.proxyProtocol` and `edge.proxyProtocol`: PROXY protocol v2, read only
/// from `trustedCIDRs` (see [`crate::net::ProxyProtocol`]).
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct ProxyProtocol {
    /// Off by default: every connection counts as its socket peer.
    pub enabled: bool,
    /// Sources that must send a header (the gateway's addresses). Required when enabled.
    #[serde(rename = "trustedCIDRs")]
    pub trusted_cidrs: Vec<String>,
    /// Longest wait for the header after accept.
    pub header_timeout: CrdDuration,
}

impl Default for ProxyProtocol {
    fn default() -> Self {
        Self {
            enabled: false,
            trusted_cidrs: Vec::new(),
            header_timeout: CrdDuration::new("5s"),
        }
    }
}

impl ProxyProtocol {
    fn validate(&self, field: &str, p: &mut Vec<String>) {
        if secs(&self.header_timeout, &format!("{field}.headerTimeout"), p).is_zero() {
            p.push(format!("{field}.headerTimeout must be positive"));
        }
        if self.enabled && self.trusted_cidrs.is_empty() {
            p.push(format!(
                "{field}.trustedCIDRs must list the gateway's addresses when enabled"
            ));
        }
        for c in &self.trusted_cidrs {
            match c.parse::<crate::net::Cidr>() {
                Err(e) => p.push(format!("{field}.trustedCIDRs: {e}")),
                // Trusting a huge range lets most clients choose their own
                // address, for the per-address caps and the logs.
                Ok(n) if n.prefix() < min_trusted_prefix(c) => p.push(format!(
                    "{field}.trustedCIDRs: `{c}` is too broad for a gateway range (the shortest allowed prefix is /{MIN_TRUSTED_V4} for IPv4, /{MIN_TRUSTED_V6} for IPv6)"
                )),
                Ok(_) => {}
            }
        }
    }
}

/// `kube`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Kube {
    /// Timeout of every non-watch API call. Must be below the client read
    /// timeout (90 s, [`READ_TIMEOUT`](crate::runtime::READ_TIMEOUT)), which
    /// would otherwise cut a silent call first.
    pub request_timeout: CrdDuration,
}

impl Default for Kube {
    fn default() -> Self {
        Self {
            request_timeout: CrdDuration::new("5s"),
        }
    }
}

/// `leaderElection`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct LeaderElection {
    /// Lease object name.
    pub lease_name: String,
    /// Lease duration.
    pub lease_duration: CrdDuration,
    /// Renew and retry cadence.
    pub renew_interval: CrdDuration,
    /// Self-fence margin.
    pub fence_margin: CrdDuration,
}

impl Default for LeaderElection {
    fn default() -> Self {
        Self {
            lease_name: "buildkit-controller-leader".into(),
            lease_duration: CrdDuration::new("15s"),
            renew_interval: CrdDuration::new("2s"),
            fence_margin: CrdDuration::new("5s"),
        }
    }
}

/// `pki`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Pki {
    /// CA Secret name.
    pub ca_secret_name: String,
    /// TEST ONLY (`cargo xtask soak`). Short leaf lifetimes. Unset in
    /// production, where leaves follow the normal lifetimes.
    pub test_leaf_lifetimes: Option<TestLeafLifetimes>,
}

/// `pki.testLeafLifetimes` (TEST ONLY): makes leaf renewals happen within
/// minutes. CA lifetimes are never affected.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestLeafLifetimes {
    /// Worker serving cert validity (production 30 days).
    pub worker_validity: CrdDuration,
    /// The worker Secret is reissued with less than this left (production 10 days).
    pub worker_renew_before: CrdDuration,
    /// A free worker pod is retired with less than this left (production 25 h).
    pub worker_min_left: CrdDuration,
    /// Edge/API serving and edge client cert validity (production 30 days / 24 h).
    pub edge_validity: CrdDuration,
}

impl Default for Pki {
    fn default() -> Self {
        Self {
            ca_secret_name: "buildkit-controller-ca".into(),
            test_leaf_lifetimes: None,
        }
    }
}

/// `edge`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Edge {
    /// `tcp://host:port` returned to clients (required).
    pub endpoint: String,
    /// DNS name returned as serverName (required).
    pub server_name: String,
    /// Wildcard allocation domain, or empty.
    pub allocation_domain: String,
    /// Extra DNS names / IPs in the serving cert.
    pub extra_names: Vec<String>,
    /// Service name for SANs.
    pub service_name: String,
    /// Serving TLS.
    pub tls: EdgeTls,
    /// Client handshake timeout.
    pub handshake_timeout: CrdDuration,
    /// Upstream connect timeout.
    pub upstream_connect_timeout: CrdDuration,
    /// Established connection cap ([`EDGE_MAX_CONNECTIONS`] by default).
    pub max_connections: u32,
    /// In-handshake connection cap.
    pub max_handshakes: u32,
    /// In-handshake connections from one client address. 0: no cap.
    pub max_handshakes_per_ip: u32,
    /// PROXY protocol v2 from a gateway in front of the listener.
    pub proxy_protocol: ProxyProtocol,
}

impl Default for Edge {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            server_name: String::new(),
            allocation_domain: String::new(),
            extra_names: Vec::new(),
            service_name: "buildkit-controller".into(),
            tls: EdgeTls::default(),
            handshake_timeout: CrdDuration::new("10s"),
            upstream_connect_timeout: CrdDuration::new("5s"),
            max_connections: EDGE_MAX_CONNECTIONS,
            max_handshakes: 1024,
            max_handshakes_per_ip: 32,
            proxy_protocol: ProxyProtocol::default(),
        }
    }
}

/// Default `edge.maxConnections`, the same as the chart. Sized to the chart's
/// 512Mi memory limit: an established edge connection holds about 200 KiB and
/// an idle replica about 14 MiB, so 2000 connections stay near 400 MiB.
/// Raise it together with the memory limit.
pub const EDGE_MAX_CONNECTIONS: u32 = 2000;

/// `edge.tls`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct EdgeTls {
    /// `""` = self-issued.
    pub secret_name: String,
}

/// `auth`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Auth {
    /// OIDC issuers.
    pub oidc: Vec<Oidc>,
    /// Service account tokens.
    pub service_account: ServiceAccountAuth,
}

/// One OIDC issuer.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Oidc {
    /// Exact `iss`, https only.
    pub url: String,
    /// Accepted audiences.
    #[serde(default)]
    pub audiences: Vec<String>,
    /// `""` = discovery.
    #[serde(default)]
    pub jwks_url: String,
    /// Extra trusted CA certificates (inline PEM) for this issuer's HTTPS
    /// fetches. Exclusive with `ca_bundle_file`.
    #[serde(default)]
    pub ca_bundle: String,
    /// Path to a PEM file of extra trusted CAs. Exclusive with `ca_bundle`.
    #[serde(default)]
    pub ca_bundle_file: String,
}

impl Oidc {
    /// The issuer's extra CA certificates. Empty when neither option is set.
    ///
    /// # Errors
    /// Both options set, an unreadable file, or PEM without a certificate.
    pub fn ca_certs(&self) -> Result<Vec<rustls_pki_types::CertificateDer<'static>>, String> {
        let (what, pem) = match (self.ca_bundle.is_empty(), self.ca_bundle_file.is_empty()) {
            (true, true) => return Ok(Vec::new()),
            (false, false) => return Err("caBundle and caBundleFile are mutually exclusive".into()),
            (false, true) => ("caBundle".to_owned(), self.ca_bundle.clone()),
            (true, false) => (
                format!("caBundleFile {}", self.ca_bundle_file),
                std::fs::read_to_string(&self.ca_bundle_file)
                    .map_err(|e| format!("caBundleFile {}: {e}", self.ca_bundle_file))?,
            ),
        };
        let certs = pem::parse_many(pem.as_bytes())
            .map_err(|e| format!("{what} is not valid PEM: {e}"))?
            .into_iter()
            .filter(|p| p.tag() == "CERTIFICATE")
            .map(|p| rustls_pki_types::CertificateDer::from(p.into_contents()))
            .collect::<Vec<_>>();
        if certs.is_empty() {
            return Err(format!("{what} contains no CERTIFICATE block"));
        }
        let mut probe = rustls::RootCertStore::empty();
        for c in &certs {
            probe
                .add(c.clone())
                .map_err(|e| format!("{what} has an invalid certificate: {e}"))?;
        }
        Ok(certs)
    }
}

/// `auth.serviceAccount`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct ServiceAccountAuth {
    /// Enabled.
    pub enabled: bool,
    /// Accepted audiences.
    pub audiences: Vec<String>,
    /// `TokenReview` calls in flight per replica; more callers get 503.
    pub max_concurrent_reviews: u32,
    /// `TokenReview` calls in flight for one client address; more get 503. 0: no cap.
    pub max_reviews_per_ip: u32,
}

impl Default for ServiceAccountAuth {
    fn default() -> Self {
        Self {
            enabled: true,
            audiences: vec!["buildkit-controller".into()],
            max_concurrent_reviews: 32,
            max_reviews_per_ip: 4,
        }
    }
}

/// `workerNetwork`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct WorkerNetwork {
    /// Denied CIDRs.
    #[serde(rename = "deniedCIDRs")]
    pub denied_cidrs: Vec<String>,
    /// Extra denied CIDRs.
    #[serde(rename = "extraDeniedCIDRs")]
    pub extra_denied_cidrs: Vec<String>,
    /// The cluster's pod, service and node CIDRs.
    #[serde(rename = "clusterCIDRs")]
    pub cluster_cidrs: Vec<String>,
    /// Ranges a pool's `allowCIDRs` may open although they overlap the link-local,
    /// metadata, loopback, cluster or extra denied ranges.
    #[serde(rename = "permittedCIDRs")]
    pub permitted_cidrs: Vec<String>,
    /// DNS allowance.
    pub dns: Dns,
}

impl Default for WorkerNetwork {
    fn default() -> Self {
        let d = crate::render::WorkerNetwork::default();
        Self {
            denied_cidrs: d.denied_cidrs,
            extra_denied_cidrs: d.extra_denied_cidrs,
            cluster_cidrs: d.cluster_cidrs,
            permitted_cidrs: d.permitted_cidrs,
            dns: Dns {
                namespace: d.dns.namespace,
                pod_labels: d.dns.pod_labels,
                cidrs: d.dns.cidrs,
            },
        }
    }
}

/// `workerNetwork.dns`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Dns {
    /// DNS pods namespace.
    pub namespace: String,
    /// DNS pod labels.
    pub pod_labels: BTreeMap<String, String>,
    /// DNS CIDRs.
    pub cidrs: Vec<String>,
}

impl Default for Dns {
    fn default() -> Self {
        WorkerNetwork::default().dns
    }
}

/// The longest an edge connection can live: the CRD caps `maxTTL` at 24h, and
/// the allocation cert, whose `notAfter` closes the connection, outlives the
/// allocation by [`CERT_MARGIN`](crate::alloc::pods::CERT_MARGIN) (5 minutes).
/// The edge allows no skew past `notAfter`. With `shutdown.drainTimeout` unset
/// this bounds the drain, and the chart sizes the termination grace period from it.
pub const MAX_CONNECTION_LIFETIME: Duration = Duration::from_mins(24 * 60 + 5);

/// `shutdown`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Shutdown {
    /// Pause between going unready and closing listeners.
    pub delay: CrdDuration,
    /// Hard cap on the drain. Unset: the drain waits for every edge connection
    /// to end on its own, which takes at most [`MAX_CONNECTION_LIFETIME`].
    pub drain_timeout: Option<CrdDuration>,
}

impl Default for Shutdown {
    fn default() -> Self {
        Self {
            delay: CrdDuration::new("5s"),
            drain_timeout: None,
        }
    }
}

/// `claims`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Claims {
    /// Tenant namespaces whose in-cluster claims are served, besides the
    /// controller's own. The chart grants Secret access in these only; a claim
    /// elsewhere fails with `NamespaceNotServed`. Empty = no in-cluster claims.
    pub namespaces: Vec<String>,
}

/// `registry`: where workers reach the built-in cache registry. Unset, allocations
/// carry no cache flags and workers get no registry entry.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Registry {
    /// `<host>:<port>` of the registry Service, for example
    /// `bk-registry.buildkit-system.svc:5000`. `""` = no cache.
    pub host: String,
    /// Secret in the controller's namespace where the leader keeps the registry's
    /// serving certificate (`tls.crt`, `tls.key`) and the internal CA (`ca.crt`).
    pub tls_secret_name: String,
}

impl Registry {
    /// The host without its port, the name in the serving certificate.
    #[must_use]
    pub fn server_name(&self) -> &str {
        self.host
            .rsplit_once(':')
            .map_or(self.host.as_str(), |(h, _)| h)
    }
}

/// Leader election timing, validated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElectionTiming {
    /// `leaseDuration`.
    pub lease_duration: Duration,
    /// `renewInterval`.
    pub renew_interval: Duration,
    /// `fenceMargin`.
    pub fence_margin: Duration,
    /// `kube.requestTimeout`.
    pub request_timeout: Duration,
}

impl ElectionTiming {
    /// `leaseDuration - fenceMargin`. The leader stops leading this long after it last sent a renewal.
    #[must_use]
    pub fn renew_deadline(&self) -> Duration {
        self.lease_duration.saturating_sub(self.fence_margin)
    }
}

fn secs(d: &CrdDuration, field: &str, problems: &mut Vec<String>) -> Duration {
    capped(d, field, MAX_DURATION, problems)
}

/// The longest any duration in the file may be. It keeps the instant
/// arithmetic on every duration far from overflow.
const MAX_DURATION: Duration = Duration::from_hours(366 * 24);
/// The longest `leaderElection.leaseDuration`: a replica waits this long for
/// a Lease whose holder vanished.
pub const MAX_LEASE_DURATION: Duration = Duration::from_mins(10);
/// The longest `shutdown.delay`.
const MAX_SHUTDOWN_DELAY: Duration = Duration::from_hours(1);
/// The longest `shutdown.drainTimeout`.
const MAX_DRAIN_TIMEOUT: Duration = Duration::from_hours(48);
/// Shortest `trustedCIDRs` prefix for IPv4 and for IPv6.
const MIN_TRUSTED_V4: u8 = 8;
const MIN_TRUSTED_V6: u8 = 32;

fn min_trusted_prefix(c: &str) -> u8 {
    if c.contains(':') {
        MIN_TRUSTED_V6
    } else {
        MIN_TRUSTED_V4
    }
}

/// [`secs`] with a field-specific upper bound `max`; zero past it.
fn capped(d: &CrdDuration, field: &str, max: Duration, problems: &mut Vec<String>) -> Duration {
    if let Ok(s) = d.seconds() {
        let d = Duration::from_secs(s);
        if d <= max {
            return d;
        }
        problems.push(format!("{field}: must be at most {max:?}"));
        return Duration::ZERO;
    }
    problems.push(format!(
        "{field}: `{}` is not a duration like 5s, 2m, 1h30m",
        d.as_str()
    ));
    Duration::ZERO
}

/// `deniedCIDRs` plus each [`ALWAYS_DENIED`](crate::render::ALWAYS_DENIED)
/// range that `deniedCIDRs`, `extraDeniedCIDRs` and `clusterCIDRs` do not
/// already cover. Helm replaces a list it is given, so an override that adds
/// one range must not reopen the metadata endpoints.
fn denied_with_protected(n: &WorkerNetwork) -> Vec<String> {
    let denied: Vec<crate::net::Cidr> = n
        .denied_cidrs
        .iter()
        .chain(&n.extra_denied_cidrs)
        .chain(&n.cluster_cidrs)
        .filter_map(|c| c.parse().ok())
        .collect();
    let missing = crate::render::ALWAYS_DENIED
        .into_iter()
        .filter(|r| !r.parse().is_ok_and(|r| denied.iter().any(|d| d.covers(&r))))
        .map(String::from);
    n.denied_cidrs.iter().cloned().chain(missing).collect()
}

/// A validated duration as `time::Duration` (zero if unparsable, which
/// validation has already rejected).
fn dur(d: &CrdDuration) -> time::Duration {
    time::Duration::seconds(
        d.seconds()
            .map_or(0, |s| i64::try_from(s).unwrap_or(i64::MAX)),
    )
}

/// Smallest `pki.testLeafLifetimes.edgeValidity` (see `validate_test_leaf`).
const MIN_TEST_EDGE_VALIDITY: Duration = Duration::from_mins(3);

/// A DNS-1123 label, the form of a namespace name.
fn is_namespace_name(s: &str) -> bool {
    (1..=63).contains(&s.len())
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A lowercase DNS-1123 subdomain, the form of most object names.
fn is_object_name(s: &str) -> bool {
    s.len() <= 253 && s.split('.').all(is_namespace_name)
}

/// Each entry of `list` is a namespace name, listed once.
fn namespace_list(field: &str, list: &[String], p: &mut Vec<String>) {
    let mut seen = std::collections::BTreeSet::new();
    for ns in list {
        if !is_namespace_name(ns) {
            p.push(format!("{field}: `{ns}` is not a namespace name"));
        } else if !seen.insert(ns.as_str()) {
            p.push(format!("{field}: `{ns}` is listed twice"));
        }
    }
}

fn is_dns_name(s: &str) -> bool {
    let s = s.strip_suffix('.').unwrap_or(s);
    !s.is_empty()
        && s.len() <= 253
        && s.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

impl Config {
    /// Parse and validate a config file body.
    pub fn from_yaml(text: &str) -> Result<Self, ConfigError> {
        let cfg: Self =
            serde_saphyr::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        cfg.validate().map(|()| cfg)
    }

    /// Election timing; call only on a validated config.
    #[must_use]
    pub fn election_timing(&self) -> ElectionTiming {
        let mut ignore = Vec::new();
        ElectionTiming {
            lease_duration: secs(&self.leader_election.lease_duration, "", &mut ignore),
            renew_interval: secs(&self.leader_election.renew_interval, "", &mut ignore),
            fence_margin: secs(&self.leader_election.fence_margin, "", &mut ignore),
            request_timeout: self.request_timeout(),
        }
    }

    /// `kube.requestTimeout`; call only on a validated config.
    #[must_use]
    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.kube.request_timeout.seconds().unwrap_or(5))
    }

    /// `shutdown.delay`; call only on a validated config.
    #[must_use]
    pub fn shutdown_delay(&self) -> Duration {
        Duration::from_secs(self.shutdown.delay.seconds().unwrap_or(0))
    }

    /// `shutdown.drainTimeout`, `None` when unset; call only on a validated config.
    #[must_use]
    pub fn drain_timeout(&self) -> Option<Duration> {
        self.shutdown
            .drain_timeout
            .as_ref()
            .map(|d| Duration::from_secs(d.seconds().unwrap_or(0)))
    }

    /// The longest the drain may take: `shutdown.drainTimeout`, else
    /// [`MAX_CONNECTION_LIFETIME`].
    #[must_use]
    pub fn drain_budget(&self) -> Duration {
        self.drain_timeout().unwrap_or(MAX_CONNECTION_LIFETIME)
    }

    /// Check every config rule, returning all failures.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut p = Vec::new();
        let rt = secs(&self.kube.request_timeout, "kube.requestTimeout", &mut p);
        let ld = capped(
            &self.leader_election.lease_duration,
            "leaderElection.leaseDuration",
            MAX_LEASE_DURATION,
            &mut p,
        );
        let ri = secs(
            &self.leader_election.renew_interval,
            "leaderElection.renewInterval",
            &mut p,
        );
        let fm = secs(
            &self.leader_election.fence_margin,
            "leaderElection.fenceMargin",
            &mut p,
        );
        // A zero timeout fails every edge handshake or upstream dial at once.
        for (d, f) in [
            (&self.edge.handshake_timeout, "edge.handshakeTimeout"),
            (
                &self.edge.upstream_connect_timeout,
                "edge.upstreamConnectTimeout",
            ),
        ] {
            if secs(d, f, &mut p).is_zero() {
                p.push(format!("{f} must be positive"));
            }
        }
        capped(
            &self.shutdown.delay,
            "shutdown.delay",
            MAX_SHUTDOWN_DELAY,
            &mut p,
        );
        if let Some(d) = &self.shutdown.drain_timeout {
            capped(d, "shutdown.drainTimeout", MAX_DRAIN_TIMEOUT, &mut p);
        }
        if rt.is_zero() {
            p.push("kube.requestTimeout must be positive".into());
        }
        let read = crate::runtime::READ_TIMEOUT;
        if rt >= read {
            p.push(format!(
                "kube.requestTimeout ({rt:?}) must be below the client read timeout ({read:?}), which ends any API request that receives nothing for that long"
            ));
        }
        if ri.is_zero() {
            p.push("leaderElection.renewInterval must be positive".into());
        }
        if !is_object_name(&self.leader_election.lease_name) {
            p.push(format!(
                "leaderElection.leaseName must be a lowercase DNS-1123 subdomain (got `{}`)",
                self.leader_election.lease_name
            ));
        }
        if fm < rt {
            p.push(format!(
                "leaderElection.fenceMargin ({fm:?}) must be >= kube.requestTimeout ({rt:?})"
            ));
        }
        if ri.saturating_add(rt) >= ld.saturating_sub(fm) {
            p.push(format!(
                "renewInterval + requestTimeout must be < leaseDuration - fenceMargin ({ri:?} + {rt:?} >= {ld:?} - {fm:?})"
            ));
        }
        self.validate_watch(&mut p);
        self.validate_registry(&mut p);
        self.validate_edge(&mut p);
        self.validate_auth(&mut p);
        self.validate_network(&mut p);
        self.api
            .proxy_protocol
            .validate("api.proxyProtocol", &mut p);
        self.edge
            .proxy_protocol
            .validate("edge.proxyProtocol", &mut p);
        self.validate_test_leaf(&mut p);
        if p.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(p))
        }
    }

    /// The namespaces to watch, `[]` = all. Call only on a validated config.
    #[must_use]
    pub fn watched_namespaces(&self) -> &[String] {
        &self.watch_namespaces
    }

    /// The namespaces whose claims are watched, `[]` = all. The controller's own
    /// namespace `own` (claims of CI callers) always; with `claims.namespaces`, every
    /// namespace when pools are watched cluster-wide (claims outside
    /// `claims.namespaces` are refused), else `claims.namespaces` too.
    #[must_use]
    pub fn claim_namespaces(&self, own: &str) -> Vec<String> {
        if self.claims.namespaces.is_empty() {
            return vec![own.to_owned()];
        }
        if self.watched_namespaces().is_empty() {
            return Vec::new();
        }
        let mut v: Vec<String> = std::iter::once(own.to_owned())
            .chain(self.claims.namespaces.iter().cloned())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// The namespaces whose in-cluster claims are served: the controller's own
    /// and `claims.namespaces`.
    #[must_use]
    pub fn served_claim_namespaces(&self, own: &str) -> std::collections::HashSet<String> {
        std::iter::once(own.to_owned())
            .chain(self.claims.namespaces.iter().cloned())
            .collect()
    }

    fn validate_watch(&self, p: &mut Vec<String>) {
        namespace_list("watchNamespaces", &self.watch_namespaces, p);
        namespace_list("claims.namespaces", &self.claims.namespaces, p);
    }

    fn validate_registry(&self, p: &mut Vec<String>) {
        let c = &self.registry;
        if c.host.is_empty() {
            return;
        }
        let port = c.host.rsplit_once(':').map(|(_, port)| port.parse::<u16>());
        if !matches!(port, Some(Ok(n)) if n > 0) || !is_dns_name(c.server_name()) {
            p.push(format!(
                "registry.host `{}` must be <dns-name>:<port>",
                c.host
            ));
        }
        if c.tls_secret_name.is_empty() {
            p.push("registry.tlsSecretName is required when registry.host is set".into());
        }
    }

    fn validate_edge(&self, p: &mut Vec<String>) {
        let e = &self.edge;
        for (v, f) in [
            (e.max_connections, "edge.maxConnections"),
            (e.max_handshakes, "edge.maxHandshakes"),
            (self.api.max_connections, "api.maxConnections"),
        ] {
            if v == 0 {
                p.push(format!("{f} must be at least 1"));
            }
        }
        match e
            .endpoint
            .strip_prefix("tcp://")
            .and_then(|r| r.rsplit_once(':'))
        {
            Some((host, port)) if !host.is_empty() && port.parse::<u16>().is_ok() => {}
            _ => p.push(format!(
                "edge.endpoint must be tcp://<host>:<port> (got `{}`)",
                e.endpoint
            )),
        }
        if !is_dns_name(&e.server_name) {
            p.push(format!(
                "edge.serverName must be a DNS name (got `{}`)",
                e.server_name
            ));
        }
        if !e.allocation_domain.is_empty() && !is_dns_name(&e.allocation_domain) {
            p.push(format!(
                "edge.allocationDomain must be empty or a DNS name (got `{}`)",
                e.allocation_domain
            ));
        }
    }

    fn validate_auth(&self, p: &mut Vec<String>) {
        let mut seen = std::collections::BTreeSet::new();
        for (i, o) in self.auth.oidc.iter().enumerate() {
            if !o.url.starts_with("https://") {
                p.push(format!(
                    "auth.oidc[{i}].url must be https (got `{}`)",
                    o.url
                ));
            }
            // `iss` is compared as an exact string and discovery appends a path to it.
            if o.url.contains(['?', '#']) {
                p.push(format!(
                    "auth.oidc[{i}].url must not contain a query or fragment (got `{}`)",
                    o.url
                ));
            }
            if !seen.insert(o.url.as_str()) {
                p.push(format!("auth.oidc[{i}].url `{}` is duplicated", o.url));
            }
            if o.audiences.is_empty() {
                p.push(format!("auth.oidc[{i}].audiences must be non-empty"));
            }
            if !o.jwks_url.is_empty() && !o.jwks_url.starts_with("https://") {
                p.push(format!("auth.oidc[{i}].jwksUrl must be https"));
            }
            if let Err(e) = o.ca_certs() {
                p.push(format!("auth.oidc[{i}]: {e}"));
            }
        }
        if self.auth.service_account.enabled && self.auth.service_account.audiences.is_empty() {
            p.push("auth.serviceAccount.audiences must be non-empty when enabled".into());
        }
        for a in &self.auth.service_account.audiences {
            if a.starts_with("https://kubernetes.default.svc") {
                p.push(format!(
                    "auth.serviceAccount.audiences must not include the API server audience `{a}`"
                ));
            }
        }
        if self.auth.service_account.enabled
            && self.auth.service_account.max_concurrent_reviews == 0
        {
            p.push("auth.serviceAccount.maxConcurrentReviews must be at least 1".into());
        }
    }

    fn validate_network(&self, p: &mut Vec<String>) {
        const DENY: Option<&str> = Some("would deny a whole address family");
        // A pool can never open a whole family, so permitting one is a mistake.
        const PERMIT: Option<&str> = Some("would permit a whole address family");
        let n = &self.worker_network;
        // The NetworkPolicy API refuses host bits, IPv4-mapped IPv6 and an `except` entry
        // equal to its `0.0.0.0/0` or `::/0` block. One bad entry fails every pool's
        // NetworkPolicy apply, so refuse it at load.
        for (f, list, whole) in [
            ("workerNetwork.deniedCIDRs", &n.denied_cidrs, DENY),
            (
                "workerNetwork.extraDeniedCIDRs",
                &n.extra_denied_cidrs,
                DENY,
            ),
            ("workerNetwork.clusterCIDRs", &n.cluster_cidrs, DENY),
            ("workerNetwork.permittedCIDRs", &n.permitted_cidrs, PERMIT),
            ("workerNetwork.dns.cidrs", &n.dns.cidrs, None),
        ] {
            for c in list {
                if !buildkit_controller_api::v1alpha1::is_canonical_cidr(c) {
                    p.push(format!(
                        "{f}: `{c}` is not a canonical CIDR (no bits set beyond the prefix length, no IPv4-mapped IPv6)"
                    ));
                } else if let Some(why) = whole
                    && c.ends_with("/0")
                {
                    p.push(format!("{f}: `{c}` {why}"));
                }
            }
        }
        if n.dns.pod_labels.is_empty() && n.dns.cidrs.is_empty() {
            p.push("workerNetwork.dns needs non-empty podLabels or cidrs".into());
        }
        // Port 53 to these ranges is open for every pool, `internet: Deny` included.
        for c in &n.dns.cidrs {
            if let Ok(d) = c.parse::<crate::net::Cidr>()
                && let Some(r) = crate::render::ALWAYS_DENIED
                    .into_iter()
                    .find(|r| r.parse().is_ok_and(|r| d.covers(&r)))
            {
                p.push(format!(
                    "workerNetwork.dns.cidrs: `{c}` holds the whole of {r}; list the DNS servers' own addresses"
                ));
            }
        }
    }

    /// Problems with the namespace lists given the controller's own namespace
    /// `own`. It holds the CA Secret, so no pool runs there, and it is always
    /// served for API claims, whose `owner`, `cacheScope` and `cacheTier` the
    /// leader trusts, so only cluster admins may create claims there.
    #[must_use]
    pub fn own_namespace_problems(&self, own: &str) -> Vec<String> {
        let mut p = Vec::new();
        if self.watch_namespaces.iter().any(|n| n == own) {
            p.push(format!(
                "watchNamespaces must not include the controller's namespace {own}: it holds the CA Secret, which a pool's workers could mount"
            ));
        }
        if self.claims.namespaces.iter().any(|n| n == own) {
            p.push(format!(
                "claims.namespaces must not include the controller's namespace {own}: it is always served for API claims, and creating claims there must stay limited to cluster admins"
            ));
        }
        p
    }

    fn validate_test_leaf(&self, p: &mut Vec<String>) {
        let Some(lt) = &self.pki.test_leaf_lifetimes else {
            return;
        };
        let f = "pki.testLeafLifetimes";
        let wv = secs(&lt.worker_validity, &format!("{f}.workerValidity"), p);
        let rb = secs(
            &lt.worker_renew_before,
            &format!("{f}.workerRenewBefore"),
            p,
        );
        let ml = secs(&lt.worker_min_left, &format!("{f}.workerMinLeft"), p);
        let ev = secs(&lt.edge_validity, &format!("{f}.edgeValidity"), p);
        if ml.is_zero() || ml >= rb || rb >= wv {
            p.push(format!(
                "{f}: need 0 < workerMinLeft < workerRenewBefore < workerValidity ({ml:?}, {rb:?}, {wv:?})"
            ));
        }
        // Renewal is due (2V - BACKDATE) / 3 after issue (2/3 of V plus the 5 m
        // backdate); at or under 2m30s it is due at once and the loader would
        // reissue on every step. 3 m leaves 20 s between renewals.
        if ev < MIN_TEST_EDGE_VALIDITY {
            p.push(format!(
                "{f}.edgeValidity must be at least {MIN_TEST_EDGE_VALIDITY:?} (got {ev:?})"
            ));
        }
    }

    /// Logs a startup warning when the test-only `pki.testLeafLifetimes` is
    /// set; returns whether it did.
    pub fn warn_test_knobs(&self) -> bool {
        let Some(lt) = &self.pki.test_leaf_lifetimes else {
            return false;
        };
        tracing::warn!(
            worker_validity = lt.worker_validity.as_str(),
            worker_renew_before = lt.worker_renew_before.as_str(),
            worker_min_left = lt.worker_min_left.as_str(),
            edge_validity = lt.edge_validity.as_str(),
            "pki.testLeafLifetimes is set: TEST ONLY short leaf lifetimes, never for production"
        );
        true
    }

    /// Logs a startup warning when `workerNetwork.clusterCIDRs` is empty, so a
    /// pool's `allowCIDRs` cannot be checked against the cluster's ranges;
    /// returns whether it did.
    pub fn warn_unchecked_cluster_ranges(&self) -> bool {
        if !self.worker_network.cluster_cidrs.is_empty() {
            return false;
        }
        tracing::warn!(
            "workerNetwork.clusterCIDRs is empty: allowCIDRs entries cannot be checked against the cluster's pod, service and node ranges, so a pool can open worker egress into the cluster; set clusterCIDRs"
        );
        true
    }

    /// Leaf validities. They are production unless `pki.testLeafLifetimes` is set.
    #[must_use]
    pub fn leaf_validity(&self) -> crate::pki::LeafValidity {
        let Some(t) = &self.pki.test_leaf_lifetimes else {
            return crate::pki::LeafValidity::default();
        };
        let e = dur(&t.edge_validity);
        crate::pki::LeafValidity {
            worker_serving: dur(&t.worker_validity),
            edge_serving: e,
            edge_client: e,
        }
    }

    /// `(renew_before, min_left)` for worker certs when the test knob is set.
    #[must_use]
    pub fn test_worker_thresholds(&self) -> Option<(time::Duration, time::Duration)> {
        self.pki
            .test_leaf_lifetimes
            .as_ref()
            .map(|t| (dur(&t.worker_renew_before), dur(&t.worker_min_left)))
    }

    /// The settings `render` reads.
    #[must_use]
    pub fn render_config(&self, controller_namespace: &str) -> crate::render::RenderConfig {
        let n = &self.worker_network;
        crate::render::RenderConfig {
            controller_namespace: controller_namespace.to_owned(),
            worker_network: crate::render::WorkerNetwork {
                denied_cidrs: denied_with_protected(n),
                extra_denied_cidrs: n.extra_denied_cidrs.clone(),
                cluster_cidrs: n.cluster_cidrs.clone(),
                permitted_cidrs: n.permitted_cidrs.clone(),
                dns: crate::render::WorkerDns {
                    namespace: n.dns.namespace.clone(),
                    pod_labels: n.dns.pod_labels.clone(),
                    cidrs: n.dns.cidrs.clone(),
                },
            },
            registry_host: (!self.registry.host.is_empty()).then(|| self.registry.host.clone()),
        }
    }
}

#[cfg(test)]
mod tests;
