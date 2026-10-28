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
use toml_edit::{DocumentMut, InlineTable, Item, Table, TomlError, Value};

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

/// `PVC cache-scope`: `<scope>@<generation>`, the cache scope that owns the
/// volume's contents as of the generation the volume holds. `P cache-scope`
/// (on a pod in a cached slot): the scope the pod serves, empty for a pod
/// that may serve any scope because its volume was emptied when it started.
pub const KEY_CACHE_SCOPE: &str = "buildkit.smrt-devops.net/cache-scope";
/// `P cache-generation`: the generation the pod's volume must hold for the
/// init container to keep it. Absent, the volume is emptied.
pub const KEY_CACHE_GENERATION: &str = "buildkit.smrt-devops.net/cache-generation";
/// The file in the state dir that names the volume's generation. An init
/// container that empties the volume writes the generation its pod was bound
/// to, or its pod uid when there was none.
pub const GENERATION_FILE: &str = ".buildkit-controller-generation";

/// Who owns a volume's contents: the scope, and the generation they belong to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// An opaque scope id (see `alloc::cache::scope`).
    pub scope: String,
    /// The id the volume's generation file holds (the uid of the pod that began it).
    pub generation: String,
}

impl Binding {
    /// Parse a `PVC cache-scope` value. `None` for anything malformed.
    #[must_use]
    pub fn parse(v: &str) -> Option<Self> {
        let (scope, generation) = v.split_once('@')?;
        let ok = |s: &str| !s.is_empty() && !s.contains('@');
        (ok(scope) && ok(generation)).then(|| Self {
            scope: scope.to_owned(),
            generation: generation.to_owned(),
        })
    }

    /// The `PVC cache-scope` value.
    #[must_use]
    pub fn value(&self) -> String {
        format!("{}@{}", self.scope, self.generation)
    }
}

/// The scope id of a scope name: the first 32 hex chars of its SHA-256, so
/// no caller identity is written to the cluster in clear.
#[must_use]
pub fn scope_id(name: &str) -> String {
    sha256_hex(name.as_bytes())[..32].to_owned()
}

/// What a new pod in a cached slot does with the slot's volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// The volume belongs to the pod's scope: the init container keeps it if
    /// it still holds the bound generation.
    Kept,
    /// The volume belonged to another scope (or to no known one): the init container empties it.
    Wiped,
    /// The volume was never bound: the init container empties it, which costs nothing.
    Fresh,
}

/// Bind a pod about to be created in a cached slot. `binding` is the volume's
/// `PVC cache-scope`; `scope` is the caller's scope, or `None` for a warm pod
/// that takes the volume's own scope (and serves any scope when it has none).
/// Sets `P cache-scope`, and `P cache-generation` only when the volume is kept.
pub fn bind_pod(pod: &mut Pod, binding: Option<&Binding>, scope: Option<&str>) -> Bound {
    let (target, bound) = match (binding, scope) {
        (Some(b), Some(s)) if b.scope == s => (s.to_owned(), Bound::Kept),
        (Some(_), Some(s)) => (s.to_owned(), Bound::Wiped),
        (Some(b), None) => (b.scope.clone(), Bound::Kept),
        (None, s) => (s.unwrap_or_default().to_owned(), Bound::Fresh),
    };
    let ann = pod.metadata.annotations.get_or_insert_with(BTreeMap::new);
    ann.insert(KEY_CACHE_SCOPE.to_owned(), target);
    match (bound, binding) {
        (Bound::Kept, Some(b)) => {
            ann.insert(KEY_CACHE_GENERATION.to_owned(), b.generation.clone());
        }
        _ => {
            ann.remove(KEY_CACHE_GENERATION);
        }
    }
    bound
}

fn pod_ann<'a>(pod: &'a Pod, key: &str) -> Option<&'a str> {
    pod.metadata
        .annotations
        .as_ref()?
        .get(key)
        .map(String::as_str)
}

/// Whether a pod may serve `scope`. A pod outside a cached slot always may; a
/// pod in one only when it serves `scope` or started with an emptied volume
/// and serves any scope. A pod with no `P cache-scope` (not rendered by this
/// version) never may.
#[must_use]
pub fn pod_serves(pod: &Pod, cached: bool, scope: &str) -> bool {
    !cached || pod_ann(pod, KEY_CACHE_SCOPE).is_some_and(|s| s.is_empty() || s == scope)
}

/// The `PVC cache-scope` to write after `scope` won `pod`, or `None` when the
/// volume's binding already says so. The generation is the one the pod was
/// told to keep, else the pod's own uid (its init container emptied the volume).
#[must_use]
pub fn won_binding(pod: &Pod, scope: &str) -> Option<Binding> {
    let generation = pod_ann(pod, KEY_CACHE_GENERATION);
    if generation.is_some() && pod_ann(pod, KEY_CACHE_SCOPE) == Some(scope) {
        return None;
    }
    let generation = generation.or(pod.metadata.uid.as_deref())?;
    Some(Binding {
        scope: scope.to_owned(),
        generation: generation.to_owned(),
    })
}

/// The init container's script. It keeps the state dir only when it holds the
/// expected generation. Otherwise it first deletes the generation file, so an
/// interrupted wipe is never kept, then everything else, and records the
/// expected generation when there is one (the pod serves the scope bound to it),
/// else the pod uid. Build history is removed either way. It
/// runs under rootlesskit, as buildkitd does, so it can delete files that
/// builds created with other user ids.
pub(super) fn init_script(dir: &str) -> String {
    format!(
        r#"set -eu
d={dir}
m="$d/{GENERATION_FILE}"
if [ -n "$EXPECT_GENERATION" ] && [ -f "$m" ] && [ "$(cat "$m")" = "$EXPECT_GENERATION" ]; then
  echo "keeping generation $EXPECT_GENERATION"
else
  rm -f -- "$m"
  rm -rf -- "$d"/* "$d"/.[!.]* "$d"/..?*
  g="${{EXPECT_GENERATION:-$POD_UID}}"
  printf %s "$g" > "$m"
  echo "emptied, generation $g"
fi
rm -f -- "$d/history.db"
"#
    )
}

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

/// The pool's `buildkitd.toml`. With `cache.local` it gains
/// `[worker.oci] maxUsedSpace` and `gc = true` (unless the pool's config
/// already sets `maxUsedSpace`, or the bound's quantity is not one
/// [`quantity_bytes`] reads), and `[history] maxEntries = 0`, which turns
/// build history off: a volume outlives the allocation, and build records
/// (logs, build arguments) must not reach a later one. The rest of the config
/// keeps its formatting.
///
/// A config that [`with_cache_settings`] refuses is returned unchanged:
/// `validate_template` reports it, so no pod is rendered from it.
#[must_use]
pub fn buildkitd_config(pool: &BuildKitPool) -> String {
    let cfg = &pool.spec.workers.buildkitd_config;
    pool.spec
        .workers
        .cache
        .local
        .as_ref()
        .and_then(|lc| with_cache_settings(cfg, gc_keep_bytes(lc)).ok())
        .unwrap_or_else(|| cfg.clone())
}

/// `cfg` with the `cache.local` settings of [`buildkitd_config`], the GC
/// bound only when `bytes` is known, or why `cfg` cannot take them: it is not TOML, or
/// `worker`, `worker.oci` or `history` is not a table.
pub(super) fn with_cache_settings(cfg: &str, bytes: Option<u64>) -> Result<String, String> {
    let mut doc: DocumentMut = cfg
        .parse()
        .map_err(|e: TomlError| format!("is not valid TOML: {}", e.message()))?;
    let not_table = |key: &str| format!("sets `{key}` to something other than a table");
    let worker = child(doc.as_item_mut(), "worker").ok_or_else(|| not_table("worker"))?;
    let oci = child(worker, "oci").ok_or_else(|| not_table("worker.oci"))?;
    if let Some(bytes) = bytes
        && oci.get("maxUsedSpace").is_none()
    {
        set(oci, "maxUsedSpace", bytes.to_string().into());
        set(oci, "gc", true.into());
    }
    let history = child(doc.as_item_mut(), "history").ok_or_else(|| not_table("history"))?;
    set(history, "maxEntries", 0_i64.into());
    Ok(doc.to_string())
}

/// The table `key` of table `parent`, created (as a table of the same kind)
/// when missing. `None` when either is not a table.
fn child<'a>(parent: &'a mut Item, key: &str) -> Option<&'a mut Item> {
    match parent {
        Item::Table(t) => {
            let item = t.entry(key).or_insert_with(|| {
                let mut new = Table::new();
                new.set_implicit(true);
                Item::Table(new)
            });
            item.is_table_like().then_some(item)
        }
        Item::Value(Value::InlineTable(t)) => {
            if !t.contains_key(key) {
                t.insert(key, Value::InlineTable(InlineTable::new()));
            }
            let (_, item) = t.get_key_value_mut(key)?;
            item.is_table_like().then_some(item)
        }
        _ => None,
    }
}

/// Set `key` of the table `item` (a table [`child`] returned) to `v`, keeping
/// the formatting of a value it replaces.
fn set(item: &mut Item, key: &str, mut v: Value) {
    let Some(t) = item.as_table_like_mut() else {
        return;
    };
    if let Some(old) = t.get_mut(key).and_then(Item::as_value_mut) {
        *v.decor_mut() = old.decor().clone();
        *old = v;
    } else {
        t.insert(key, Item::Value(v));
    }
}
