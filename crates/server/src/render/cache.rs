//! Cache volumes: one PVC per cached slot, mounted at the
//! buildkitd state dir in place of the `emptyDir`.

use std::collections::BTreeMap;

use buildkit_controller_api::v1alpha1::{BuildKitPool, LocalCache};
use k8s_openapi::api::core::v1::{
    PersistentVolumeClaim, PersistentVolumeClaimSpec, PersistentVolumeClaimVolumeSource, Pod,
    VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use serde_json::json;

use super::hash::sha256_hex;
use super::{
    CONTROLLER, LABEL_MANAGED_BY, LABEL_NAME, LABEL_POOL, RenderError, owner_ref, pool_identity,
};

/// Value of [`LABEL_NAME`] on cache volumes.
pub const CACHE_VOLUME_NAME: &str = "buildkit-cache";
/// `PVC volume-hash`: SHA-256 over `size` and `storageClass`.
pub const LABEL_VOLUME_HASH: &str = "buildkit.smrt-devops.net/volume-hash";
/// `PVC cache-keys` and `P cache-keys` (annotations): the recent keys the volume served, newest first.
pub const KEY_CACHE_KEYS: &str = "buildkit.smrt-devops.net/cache-keys";
/// Name of the pod volume that holds the buildkitd state dir.
pub(super) const STATE_VOLUME: &str = "state";

/// `min(cache.local.maxVolumes, scaling.max)`, or 0 when `cache.local` is unset.
#[must_use]
pub fn cache_limit(pool: &BuildKitPool) -> u32 {
    pool.spec.workers.cache.local.as_ref().map_or(0, |lc| {
        u32::try_from(lc.max_volumes.min(pool.spec.scaling.max)).unwrap_or(0)
    })
}

/// Whether `slot` has a volume.
#[must_use]
pub fn is_cached_slot(pool: &BuildKitPool, slot: u32) -> bool {
    slot < cache_limit(pool)
}

/// `<pool>-cache-<slot>`.
#[must_use]
pub fn cache_volume_name(pool: &str, slot: u32) -> String {
    format!("{pool}-cache-{slot}")
}

/// The slot of a volume named `<pool>-cache-<n>`, or `None` for any other name.
#[must_use]
pub fn cache_volume_slot(pool: &str, volume: &str) -> Option<u32> {
    let n = volume.strip_prefix(pool)?.strip_prefix("-cache-")?;
    if n.is_empty() || (n.len() > 1 && n.starts_with('0')) {
        return None;
    }
    n.parse().ok()
}

/// `PVC volume-hash`: first 10 hex chars of SHA-256 over `size` and `storageClass`.
#[must_use]
pub fn volume_hash(lc: &LocalCache) -> String {
    let doc = json!({"size": lc.size.0, "storageClass": lc.storage_class});
    sha256_hex(doc.to_string().as_bytes())[..10].to_owned()
}

/// The PVC of `slot`, or `None` when the slot has no volume.
///
/// # Errors
/// [`RenderError::MissingMetadata`] or [`RenderError::NameTooLong`] for an incomplete pool.
pub fn cache_volume(
    pool: &BuildKitPool,
    slot: u32,
) -> Result<Option<PersistentVolumeClaim>, RenderError> {
    let (name, ns, uid) = pool_identity(pool)?;
    let Some(lc) = pool.spec.workers.cache.local.as_ref() else {
        return Ok(None);
    };
    if !is_cached_slot(pool, slot) {
        return Ok(None);
    }
    let labels = BTreeMap::from([
        (LABEL_MANAGED_BY.to_owned(), CONTROLLER.to_owned()),
        (LABEL_NAME.to_owned(), CACHE_VOLUME_NAME.to_owned()),
        (LABEL_POOL.to_owned(), name.to_owned()),
        (LABEL_VOLUME_HASH.to_owned(), volume_hash(lc)),
    ]);
    Ok(Some(PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(cache_volume_name(name, slot)),
            namespace: Some(ns.to_owned()),
            labels: Some(labels),
            owner_references: Some(vec![owner_ref(name, uid)]),
            ..ObjectMeta::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_owned()]),
            volume_mode: Some("Filesystem".to_owned()),
            storage_class_name: lc.storage_class.clone(),
            resources: Some(VolumeResourceRequirements {
                requests: Some(BTreeMap::from([("storage".to_owned(), lc.size.clone())])),
                limits: None,
            }),
            ..PersistentVolumeClaimSpec::default()
        }),
        status: None,
    }))
}

/// Point the `state` volume of a cached slot's pod at the slot's PVC.
pub(super) fn mount_cache_volume(pod: &mut Pod, pool: &str, slot: u32) {
    let volumes = pod.spec.as_mut().and_then(|s| s.volumes.as_mut());
    if let Some(v) = volumes.and_then(|vs| vs.iter_mut().find(|v| v.name == STATE_VOLUME)) {
        v.empty_dir = None;
        v.persistent_volume_claim = Some(PersistentVolumeClaimVolumeSource {
            claim_name: cache_volume_name(pool, slot),
            read_only: None,
        });
    }
}

/// Set `P cache-keys` on a pod about to be created in a slot whose volume carries `keys`.
pub fn set_pod_cache_keys(pod: &mut Pod, keys: &str) {
    pod.metadata
        .annotations
        .get_or_insert_with(BTreeMap::new)
        .insert(KEY_CACHE_KEYS.to_owned(), keys.to_owned());
}

/// `P cache-keys` of a pod: the comma-separated recent keys of its slot's volume.
#[must_use]
pub fn pod_cache_keys(pod: &Pod) -> Option<&str> {
    pod.metadata
        .annotations
        .as_ref()?
        .get(KEY_CACHE_KEYS)
        .map(String::as_str)
}

/// Whether a `cache-keys` list contains `key`.
#[must_use]
pub fn keys_contain(keys: Option<&str>, key: &str) -> bool {
    keys.is_some_and(|l| l.split(',').any(|k| k == key))
}

/// `keys` with `key` moved to the front, at most `cap` long.
#[must_use]
pub fn push_key(keys: Option<&str>, key: &str, cap: usize) -> String {
    let rest = keys
        .unwrap_or_default()
        .split(',')
        .filter(|k| !k.is_empty() && *k != key);
    std::iter::once(key)
        .chain(rest)
        .take(cap.max(1))
        .collect::<Vec<_>>()
        .join(",")
}

/// Bytes of a Kubernetes quantity with an optional binary or decimal suffix.
/// `None` for forms a `size` never takes (exponents, milli-units).
#[must_use]
pub fn quantity_bytes(q: &str) -> Option<u64> {
    const UNITS: [(&str, u64); 12] = [
        ("Ki", 1 << 10),
        ("Mi", 1 << 20),
        ("Gi", 1 << 30),
        ("Ti", 1 << 40),
        ("Pi", 1 << 50),
        ("Ei", 1 << 60),
        ("k", 1_000),
        ("M", 1_000_000),
        ("G", 1_000_000_000),
        ("T", 1_000_000_000_000),
        ("P", 1_000_000_000_000_000),
        ("E", 1_000_000_000_000_000_000),
    ];
    let (num, mult) = UNITS
        .iter()
        .find_map(|(s, m)| q.strip_suffix(s).map(|n| (n, *m)))
        .unwrap_or((q, 1));
    let (int, frac) = num.split_once('.').unwrap_or((num, ""));
    if int.is_empty()
        || !int.bytes().all(|c| c.is_ascii_digit())
        || !frac.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let whole = int.parse::<u64>().ok()?.checked_mul(mult)?;
    let scale = 10u64.checked_pow(u32::try_from(frac.len()).ok()?)?;
    let part = if frac.is_empty() {
        0
    } else {
        frac.parse::<u64>().ok()?.checked_mul(mult)? / scale
    };
    whole.checked_add(part)
}

/// GC bound of a pool with `cache.local`: `gcKeepStorage`, else 80% of `size`.
#[must_use]
pub fn gc_keep_bytes(lc: &LocalCache) -> Option<u64> {
    match &lc.gc_keep_storage {
        Some(q) => quantity_bytes(&q.0),
        None => quantity_bytes(&lc.size.0).map(|b| b / 5 * 4),
    }
}

/// The pool's `buildkitd.toml`. With
/// `cache.local` it gains `[worker.oci] maxUsedSpace` and `gc = true`, unless
/// the pool's config already sets `maxUsedSpace`. Text-level, so the rest of
/// the config stays byte for byte.
#[must_use]
pub fn buildkitd_config(pool: &BuildKitPool) -> String {
    let cfg = &pool.spec.workers.buildkitd_config;
    let Some(bytes) = pool
        .spec
        .workers
        .cache
        .local
        .as_ref()
        .and_then(gc_keep_bytes)
    else {
        return cfg.clone();
    };
    if cfg.contains("maxUsedSpace") {
        return cfg.clone();
    }
    let lines = format!("  maxUsedSpace = \"{bytes}\"\n  gc = true\n");
    let is_header = |l: &str| l.split_whitespace().collect::<String>() == "[worker.oci]";
    if cfg.lines().any(is_header) {
        // TOML forbids a duplicate key, so a `gc` the section already has is dropped.
        let is_gc = |l: &str| l.split_once('=').is_some_and(|(k, _)| k.trim() == "gc");
        let mut out = String::with_capacity(cfg.len() + lines.len());
        let mut in_oci = false;
        for l in cfg.lines() {
            if l.trim_start().starts_with('[') {
                in_oci = is_header(l);
            } else if in_oci && is_gc(l) {
                continue;
            }
            out.push_str(l);
            out.push('\n');
            if is_header(l) {
                out.push_str(&lines);
            }
        }
        return out;
    }
    let sep = if cfg.is_empty() || cfg.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    format!("{cfg}{sep}[worker.oci]\n{lines}")
}
