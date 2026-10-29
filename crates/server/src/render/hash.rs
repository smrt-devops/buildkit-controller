//! Stable hashing with canonical JSON (sorted keys) and SHA-256.

use buildkit_controller_api::v1alpha1::BuildKitPool;
use ring::digest;
use serde_json::{Value, json};

use super::{RenderError, pod::pod_spec};

/// Lowercase hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Lowercase hex SHA-256 of `data`.
pub(super) fn sha256_hex(data: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, data).as_ref())
}

/// Append `v` as JSON with every object's keys sorted, so the output does not depend on map
/// insertion order or on any `serde_json` feature flag.
fn canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                if let Some(child) = m.get(k) {
                    canonical(child, out);
                }
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, child) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(child, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// First 16 hex chars of SHA-256 of `buildkitdConfig`, an input of [`template_hash`].
#[must_use]
pub fn config_hash(buildkitd_config: &str) -> String {
    sha256_hex(buildkitd_config.as_bytes())[..16].to_owned()
}

/// `P template-hash`: first 10 hex chars of SHA-256 over the
/// merged pod spec, the user's pod labels and annotations, and the
/// `buildkitdConfig` hash. The CA fingerprint, the slot and per-pod state are
/// not inputs, so a CA rotation does not retire free pods by this hash. Nor is the
/// cache registry entry: a worker made before `cache.host` changed exports to the
/// old host, which fails without failing the build, until it is replaced.
pub fn template_hash(pool: &BuildKitPool) -> Result<String, RenderError> {
    let spec = pod_spec(pool)?;
    let doc = json!({
        "spec": serde_json::to_value(&spec).map_err(|e| RenderError::Serialize(e.to_string()))?,
        "labels": pool.spec.template.metadata.labels,
        "annotations": pool.spec.template.metadata.annotations,
        "configHash": config_hash(&pool.spec.workers.buildkitd_config),
    });
    let mut s = String::new();
    canonical(&doc, &mut s);
    Ok(sha256_hex(s.as_bytes())[..10].to_owned())
}
