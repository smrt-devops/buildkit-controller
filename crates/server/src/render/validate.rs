//! Controller-side re-check of the CEL rules the rendered pod relies on
//! (defence against schema drift, or an older stored object that CRD
//! validation ratcheting lets keep a value a newer rule refuses), plus the
//! check only the controller can make: that it can edit `buildkitdConfig`.

use buildkit_controller_api::v1alpha1::{
    BuildKitPoolSpec, CONTAINER_NAME, RESERVED_ENV_PREFIXES, RESERVED_KEY_PREFIX,
    RESERVED_LABEL_KEYS, RESERVED_MOUNT_PATHS, RESERVED_VOLUMES,
};
use serde_json::Value;

use super::config::with_registry;
use super::{RenderError, TemplateConflictField, WorkerNetwork};

/// Re-exported name of a single offending field.
pub type Conflict = TemplateConflictField;

/// Volume sources a template may use.
const VOLUME_KEYS: [&str; 5] = ["name", "configMap", "secret", "emptyDir", "projected"];
/// Sources a `projected` volume may combine (never `serviceAccountToken`).
const PROJECTED_KEYS: [&str; 3] = ["configMap", "secret", "downwardAPI"];
/// Sources an env `valueFrom` may use (never `resourceFieldRef` or `fileKeyRef`).
const ENV_VALUE_FROM_KEYS: [&str; 3] = ["configMapKeyRef", "secretKeyRef", "fieldRef"];

fn conflict(out: &mut Vec<Conflict>, field: String, reason: &str) {
    out.push(Conflict {
        field,
        reason: reason.to_owned(),
    });
}

/// True when `p` is absolute with no empty, `.` or `..` segment (the CEL rule).
fn is_normalised(p: &str) -> bool {
    p.strip_prefix('/').is_some_and(|rest| {
        rest.split('/')
            .all(|s| !s.is_empty() && s != "." && s != "..")
    })
}

/// A C0 control character or DEL (the CEL rule on `volumeMounts`).
fn has_control(s: &str) -> bool {
    s.bytes().any(|b| b < 0x20 || b == 0x7f)
}

/// The architecture of a single declared platform (`linux/arm64/v8` gives `arm64`).
pub(super) fn single_arch(platforms: &[String]) -> Option<&str> {
    match platforms {
        [one] => one.split('/').nth(1),
        _ => None,
    }
}

/// Every template conflict, in a fixed order. Empty means the template is valid.
#[must_use]
pub fn validate_template(spec: &BuildKitPoolSpec) -> Vec<Conflict> {
    let mut out = Vec::new();
    let t = &spec.template;
    for k in t.metadata.labels.keys() {
        if RESERVED_LABEL_KEYS.contains(&k.as_str()) || k.starts_with(RESERVED_KEY_PREFIX) {
            conflict(
                &mut out,
                format!("spec.template.metadata.labels[{k}]"),
                "label key is reserved by the controller",
            );
        }
    }
    for k in t.metadata.annotations.keys() {
        if k.starts_with(RESERVED_KEY_PREFIX) {
            conflict(
                &mut out,
                format!("spec.template.metadata.annotations[{k}]"),
                "annotation key prefix buildkit.smrt-devops.net/ is reserved",
            );
        }
    }
    if let (Some(want), Some(have)) = (
        single_arch(&spec.platforms),
        t.spec.node_selector.get("kubernetes.io/arch"),
    ) && want != have
    {
        conflict(
            &mut out,
            "spec.template.spec.nodeSelector[kubernetes.io/arch]".into(),
            "disagrees with the declared platform",
        );
    }
    if t.spec
        .priority_class_name
        .as_deref()
        .is_some_and(|n| n.starts_with("system-"))
    {
        conflict(
            &mut out,
            "spec.template.spec.priorityClassName".into(),
            "must not start with system-",
        );
    }
    volumes(spec, &mut out);
    containers(spec, &mut out);
    registry_entry(spec, &mut out);
    out
}

/// `buildkitdConfig` must take the cache registry entry.
fn registry_entry(spec: &BuildKitPoolSpec, out: &mut Vec<Conflict>) {
    if let Err(why) = with_registry(&spec.workers.buildkitd_config, "cache.invalid:5000") {
        conflict(out, "spec.workers.buildkitdConfig".into(), &why);
    }
}

fn volumes(spec: &BuildKitPoolSpec, out: &mut Vec<Conflict>) {
    for (i, v) in spec.template.spec.volumes.iter().enumerate() {
        let at = format!("spec.template.spec.volumes[{i}]");
        if RESERVED_VOLUMES.contains(&v.name.as_str()) {
            conflict(
                out,
                format!("{at}.name"),
                "volume name is reserved by the controller",
            );
        }
        let Ok(Value::Object(m)) = serde_json::to_value(v) else {
            continue;
        };
        for k in m.keys().filter(|k| !VOLUME_KEYS.contains(&k.as_str())) {
            conflict(
                out,
                format!("{at}.{k}"),
                "volume source is not allowed in a template",
            );
        }
        let sources = m
            .get("projected")
            .and_then(|p| p.get("sources"))
            .and_then(Value::as_array);
        for (j, s) in sources.into_iter().flatten().enumerate() {
            if s.as_object().is_none_or(|o| o.len() != 1) {
                conflict(
                    out,
                    format!("{at}.projected.sources[{j}]"),
                    "must set exactly one of configMap, secret or downwardAPI",
                );
            }
            for k in s
                .as_object()
                .into_iter()
                .flat_map(serde_json::Map::keys)
                .filter(|k| !PROJECTED_KEYS.contains(&k.as_str()))
            {
                conflict(
                    out,
                    format!("{at}.projected.sources[{j}].{k}"),
                    "projected source is not allowed in a template",
                );
            }
        }
    }
}

fn containers(spec: &BuildKitPoolSpec, out: &mut Vec<Conflict>) {
    let cs = &spec.template.spec.containers;
    let base = "spec.template.spec.containers";
    if cs.len() != 1 || cs.first().is_none_or(|c| c.name != CONTAINER_NAME) {
        conflict(
            out,
            base.into(),
            "must be exactly one container named buildkitd",
        );
    }
    let Some(c) = cs.first() else {
        return;
    };
    let at = format!("{base}[0]");
    let limits = c.resources.limits.as_ref();
    for key in ["memory", "ephemeral-storage"] {
        if !limits.is_some_and(|l| l.contains_key(key)) {
            conflict(
                out,
                format!("{at}.resources.limits[{key}]"),
                "limit is required",
            );
        }
    }
    for (i, e) in c.env.iter().enumerate() {
        if RESERVED_ENV_PREFIXES.iter().any(|p| e.name.starts_with(p)) {
            conflict(
                out,
                format!("{at}.env[{i}].name"),
                "env name prefix is reserved by the controller",
            );
        }
        let Ok(Value::Object(m)) = serde_json::to_value(e) else {
            continue;
        };
        let value_from = m.get("valueFrom").and_then(Value::as_object);
        if value_from.is_some_and(|v| v.len() != 1) {
            conflict(
                out,
                format!("{at}.env[{i}].valueFrom"),
                "must set exactly one of configMapKeyRef, secretKeyRef or fieldRef",
            );
        }
        for k in value_from
            .into_iter()
            .flat_map(serde_json::Map::keys)
            .filter(|k| !ENV_VALUE_FROM_KEYS.contains(&k.as_str()))
        {
            conflict(
                out,
                format!("{at}.env[{i}].valueFrom.{k}"),
                "env source is not allowed in a template",
            );
        }
    }
    mounts(spec, &at, out);
}

fn mounts(spec: &BuildKitPoolSpec, at: &str, out: &mut Vec<Conflict>) {
    let Some(c) = spec.template.spec.containers.first() else {
        return;
    };
    for (i, m) in c.volume_mounts.iter().enumerate() {
        let path = &m.mount_path;
        if has_control(path) {
            conflict(
                out,
                format!("{at}.volumeMounts[{i}].mountPath"),
                "must not contain control characters",
            );
        }
        if m.sub_path.as_deref().is_some_and(has_control) {
            conflict(
                out,
                format!("{at}.volumeMounts[{i}].subPath"),
                "must not contain control characters",
            );
        }
        if m.mount_propagation.as_deref() == Some("Bidirectional") {
            conflict(
                out,
                format!("{at}.volumeMounts[{i}].mountPropagation"),
                "Bidirectional needs a privileged container",
            );
        }
        if !is_normalised(path) {
            conflict(
                out,
                format!("{at}.volumeMounts[{i}].mountPath"),
                "must be absolute with no empty, '.' or '..' segment",
            );
        }
        if RESERVED_MOUNT_PATHS
            .iter()
            .any(|p| path == p || path.strip_prefix(p).is_some_and(|r| r.starts_with('/')))
        {
            conflict(
                out,
                format!("{at}.volumeMounts[{i}].mountPath"),
                "is under a controller-owned path",
            );
        }
        if !spec.template.spec.volumes.iter().any(|v| v.name == m.name) {
            conflict(
                out,
                format!("{at}.volumeMounts[{i}].name"),
                "must name a template volume",
            );
        }
    }
}

/// `Ok` when the template has no conflict.
pub(super) fn check(spec: &BuildKitPoolSpec) -> Result<(), RenderError> {
    let c = validate_template(spec);
    if c.is_empty() {
        Ok(())
    } else {
        Err(RenderError::TemplateConflict(c))
    }
}

/// Ranges no `allowCIDRs` entry may overlap unless the operator permits it:
/// link-local and loopback, the cloud metadata endpoints outside link-local,
/// and the NAT64 local-use prefix.
pub const PROTECTED_CIDRS: [&str; 12] = [
    "169.254.0.0/16",
    "fe80::/10",
    "127.0.0.0/8",
    "::1/128",
    // Azure wire server.
    "168.63.129.16/32",
    // Alibaba Cloud metadata.
    "100.100.100.200/32",
    // AWS IMDS over IPv6.
    "fd00:ec2::254/128",
    // GCE metadata over IPv6.
    "fd20:ce::254/128",
    // Akamai (Linode) metadata over IPv6.
    "fd00:a9fe:a9fe::1/128",
    // Oracle Cloud metadata over IPv6.
    "fd00:c1::a9fe:a9fe/128",
    // Scaleway metadata over IPv6.
    "fd00:42::42/128",
    // RFC 8215 local-use NAT64: the operator picks where the IPv4 address
    // sits inside it, so its entries cannot be checked as IPv4.
    "64:ff9b:1::/48",
];

/// One address range, IPv4 or IPv6, with host bits cleared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Net {
    V4(u32, u32),
    V6(u128, u32),
}

/// IPv6 /96 blocks whose last 32 bits are an IPv4 address that the block
/// reaches: `::ffff:0:0/96` (IPv4-mapped) and `64:ff9b::/96` (the RFC 6052
/// NAT64 well-known prefix).
const EMBEDS_V4: [u128; 2] = [0xffff_0000_0000, 0x0064_ff9b << 96];
/// Prefix length of the [`EMBEDS_V4`] blocks.
const EMBED_PREFIX: u32 = 96;

fn mask32(a: u32, p: u32) -> u32 {
    a.checked_shr(32 - p)
        .map_or(0, |v| v.checked_shl(32 - p).unwrap_or(0))
}

fn mask128(a: u128, p: u32) -> u128 {
    a.checked_shr(128 - p)
        .map_or(0, |v| v.checked_shl(128 - p).unwrap_or(0))
}

impl Net {
    /// Parses `addr/prefix`. A range inside an [`EMBEDS_V4`] block (prefix 96
    /// or more) is read as the IPv4 range it embeds, since it reaches the same hosts.
    fn parse(s: &str) -> Option<Self> {
        let (addr, prefix) = s.split_once('/')?;
        if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let p: u32 = prefix.parse().ok()?;
        match addr.parse::<std::net::IpAddr>().ok()? {
            std::net::IpAddr::V4(a) => (p <= 32).then(|| Self::V4(mask32(u32::from(a), p), p)),
            std::net::IpAddr::V6(a) => {
                if p > 128 {
                    return None;
                }
                let bits = mask128(u128::from(a), p);
                if p >= EMBED_PREFIX && EMBEDS_V4.contains(&mask128(bits, EMBED_PREFIX)) {
                    // The low 32 bits are the IPv4 address.
                    #[allow(clippy::cast_possible_truncation, reason = "low 32 bits")]
                    let v4 = bits as u32;
                    return Some(Self::V4(v4, p - EMBED_PREFIX));
                }
                Some(Self::V6(bits, p))
            }
        }
    }

    fn prefix(self) -> u32 {
        match self {
            Self::V4(_, p) | Self::V6(_, p) => p,
        }
    }

    /// `self` holds every address of `other`.
    fn contains(self, other: Self) -> bool {
        match (self, other) {
            (Self::V4(a, p), Self::V4(b, q)) => p <= q && mask32(b, p) == a,
            (Self::V6(a, p), Self::V6(b, q)) => p <= q && mask128(b, p) == a,
            // A short IPv6 prefix can hold a whole block that embeds IPv4.
            (Self::V6(a, p), Self::V4(..)) => {
                p <= EMBED_PREFIX && EMBEDS_V4.iter().any(|b| mask128(*b, p) == a)
            }
            (Self::V4(..), Self::V6(..)) => false,
        }
    }

    /// The two ranges share an address. CIDR ranges either nest or are disjoint.
    fn overlaps(self, other: Self) -> bool {
        self.contains(other) || other.contains(self)
    }
}

/// The first range of `list` that overlaps `e`.
fn first_overlap<'s>(list: impl IntoIterator<Item = &'s str>, e: Net) -> Option<&'s str> {
    list.into_iter()
        .find(|c| Net::parse(c).is_some_and(|c| c.overlaps(e)))
}

/// Why an `allowCIDRs` entry is refused, or `None` when it may be rendered.
///
/// An entry is refused when it is not a CIDR range, when it spans a whole
/// address family, or when it overlaps (holds or lies inside) a
/// [`PROTECTED_CIDRS`] range, one of `net.cluster_cidrs` or one of
/// `net.extra_denied_cidrs` (the operator's explicit deny), unless a
/// `net.permitted_cidrs` range holds the whole entry. The whole-family check
/// has no exception.
#[must_use]
pub fn refused_allow_cidr(entry: &str, net: &WorkerNetwork) -> Option<String> {
    let Some(e) = Net::parse(entry) else {
        return Some("is not a CIDR range".into());
    };
    if e.prefix() == 0 {
        return Some("opens a whole address family".into());
    }
    if net
        .permitted_cidrs
        .iter()
        .filter_map(|p| Net::parse(p))
        .any(|p| p.prefix() > 0 && p.contains(e))
    {
        return None;
    }
    let protected = first_overlap(PROTECTED_CIDRS, e)
        .map(|c| ("the link-local, metadata, loopback or NAT64 range", c));
    let (what, c) = protected
        .or_else(|| {
            first_overlap(net.cluster_cidrs.iter().map(String::as_str), e)
                .map(|c| ("the cluster range", c))
        })
        .or_else(|| {
            first_overlap(net.extra_denied_cidrs.iter().map(String::as_str), e)
                .map(|c| ("the operator-denied range", c))
        })?;
    Some(format!(
        "overlaps {what} {c}; an operator can permit it in workerNetwork.permittedCIDRs"
    ))
}

/// Every refused `spec.network.egress.allowCIDRs` entry, in spec order.
/// Empty means every entry may be rendered.
#[must_use]
pub fn refused_allow_cidrs(spec: &BuildKitPoolSpec, net: &WorkerNetwork) -> Vec<Conflict> {
    spec.network
        .egress
        .allow_cidrs
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            refused_allow_cidr(c, net).map(|why| Conflict {
                field: format!("spec.network.egress.allowCIDRs[{i}]"),
                reason: format!("`{c}` {why}"),
            })
        })
        .collect()
}
