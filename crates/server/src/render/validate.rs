//! Controller-side re-check of the template rules of `spec.template`
//! (defence against schema drift or an older stored object).

use buildkit_controller_api::v1alpha1::{
    BuildKitPoolSpec, CONTAINER_NAME, RESERVED_ENV_PREFIXES, RESERVED_KEY_PREFIX,
    RESERVED_LABEL_KEYS, RESERVED_MOUNT_PATHS, RESERVED_VOLUMES,
};
use serde_json::Value;

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
    volumes(spec, &mut out);
    containers(spec, &mut out);
    out
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
        for k in m
            .get("valueFrom")
            .and_then(Value::as_object)
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
    for (i, m) in c.volume_mounts.iter().enumerate() {
        let path = &m.mount_path;
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
/// link-local (with the cloud metadata endpoints), the Azure wire server and loopback.
pub const PROTECTED_CIDRS: [&str; 5] = [
    "169.254.0.0/16",
    "168.63.129.16/32",
    "fe80::/10",
    "127.0.0.0/8",
    "::1/128",
];

/// One address range, IPv4 or IPv6, with host bits cleared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Net {
    V4(u32, u32),
    V6(u128, u32),
}

/// `::ffff:0:0/96`, the IPv4-mapped IPv6 block.
const MAPPED: (u128, u32) = (0xffff_0000_0000, 96);

fn mask32(a: u32, p: u32) -> u32 {
    a.checked_shr(32 - p)
        .map_or(0, |v| v.checked_shl(32 - p).unwrap_or(0))
}

fn mask128(a: u128, p: u32) -> u128 {
    a.checked_shr(128 - p)
        .map_or(0, |v| v.checked_shl(128 - p).unwrap_or(0))
}

impl Net {
    /// Parses `addr/prefix`. An IPv4-mapped IPv6 range of prefix 96 or more is
    /// read as the IPv4 range it maps, since it names the same hosts.
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
                if p >= MAPPED.1 && mask128(bits, MAPPED.1) == MAPPED.0 {
                    // The low 32 bits are the IPv4 address.
                    #[allow(clippy::cast_possible_truncation, reason = "low 32 bits")]
                    let v4 = bits as u32;
                    return Some(Self::V4(v4, p - MAPPED.1));
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
            // A short IPv6 prefix can hold the whole IPv4-mapped block.
            (Self::V6(a, p), Self::V4(..)) => p <= MAPPED.1 && mask128(MAPPED.0, p) == a,
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
        .map(|c| ("the link-local, metadata or loopback range", c));
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
