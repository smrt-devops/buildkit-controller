//! Controller-side re-check of the template rules of `spec.template`
//! (defence against schema drift or an older stored object).

use buildkit_controller_api::v1alpha1::{
    BuildKitPoolSpec, CONTAINER_NAME, RESERVED_ENV_PREFIXES, RESERVED_KEY_PREFIX,
    RESERVED_LABEL_KEYS, RESERVED_MOUNT_PATHS, RESERVED_VOLUMES,
};
use serde_json::Value;

use super::{RenderError, TemplateConflictField};

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
