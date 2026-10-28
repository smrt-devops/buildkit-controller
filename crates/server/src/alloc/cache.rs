//! Build cache on the API replicas: which layers a request
//! uses, candidate order, ensure volume before a cold create, and the
//! `cache-keys` annotation after a win.

use std::sync::Arc;

use buildkit_controller_api::dto::{CacheChoice, CacheKey, CacheResponse, LocalCacheResult};
use buildkit_controller_api::v1alpha1::{BuildKitPool, CacheAffinity, CacheScope};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use kube::ResourceExt;
use serde_json::{Value, json};
use tracing::{error, warn};

use super::backend::{Backend, PvcCreate};
use super::error::AllocError;
use super::metrics::AllocMetrics;
use super::pods::{FREE, ann, shuffle, slot_of};
use crate::auth::Identity;
use crate::render::{
    self, ANN_ALLOC, Binding, KEY_CACHE_KEYS, KEY_CACHE_SCOPE, LABEL_VOLUME_HASH, keys_contain,
};

/// The cache layers that apply to one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Plan {
    /// The effective key.
    pub key: String,
    /// The registry layer's flags, when it applies.
    pub registry: Option<buildkit_controller_api::v1alpha1::RegistryCacheFlags>,
    /// Whether the local layer applies.
    pub local: bool,
    /// `keysPerVolume` when the local layer applies with `affinity: soft`.
    pub soft: Option<usize>,
}

/// Resolve `cacheKey` and `cache` against the pool. `None` when the pool
/// enables no layer. `cache: none` on a pool that enables one is a plan with
/// neither layer, so a cached slot it lands on can still report `reused`.
///
/// # Errors
/// [`AllocError::InvalidRequest`] when `cache` names a layer the pool does not enable.
pub(super) fn plan(
    pool: &BuildKitPool,
    key: Option<&CacheKey>,
    choice: Option<CacheChoice>,
) -> Result<Option<Plan>, AllocError> {
    let c = &pool.spec.workers.cache;
    let choice = choice.unwrap_or_default();
    let (registry, local) = (c.registry_enabled(), c.local.as_ref());
    let missing = |layer: &str| {
        AllocError::InvalidRequest(format!(
            "cache is {layer} but the pool has no {layer} cache"
        ))
    };
    if registry.is_none()
        && local.is_none()
        && matches!(choice, CacheChoice::Auto | CacheChoice::None)
    {
        return Ok(None);
    }
    let (registry, local) = match choice {
        CacheChoice::None => (None, None),
        CacheChoice::Auto => (registry, local),
        CacheChoice::Registry => (Some(registry.ok_or_else(|| missing("registry"))?), None),
        CacheChoice::Local => (None, Some(local.ok_or_else(|| missing("local"))?)),
    };
    let name = pool.name_any();
    let key = key.map_or_else(
        || c.default_key(&name).to_owned(),
        |k| k.as_str().to_owned(),
    );
    Ok(Some(Plan {
        registry: registry.map(|r| r.resolve(&name, &key)),
        local: local.is_some(),
        soft: local
            .filter(|l| l.affinity == CacheAffinity::Soft)
            .map(|l| usize::try_from(l.keys_per_volume).unwrap_or(1)),
        key,
    }))
}

impl Plan {
    /// The `201` response's `cache` object, absent when nothing applied.
    pub(super) fn response(&self, local: Option<&str>) -> Option<CacheResponse> {
        (self.registry.is_some() || local.is_some()).then(|| CacheResponse {
            key: self.key.clone(),
            registry: self.registry.clone(),
            local: local.map(|r| LocalCacheResult {
                result: r.to_owned(),
            }),
        })
    }
}

/// Whether `pod` sits in a slot with a volume.
pub(super) fn cached(pool: &BuildKitPool, pod: &Pod) -> bool {
    pod.metadata
        .name
        .as_deref()
        .and_then(|n| slot_of(n, &pool.name_any()))
        .is_some_and(|s| render::is_cached_slot(pool, s))
}

/// Warm-path order. With the local layer (`local`): pods whose
/// `P cache-keys` contains `key`, then other cached slots, then the rest.
/// Without it: uncached slots first, so the request leaves volumes alone when
/// it can. Each group is shuffled. A pool without `cache.local` only shuffles.
pub(super) fn order_warm<P: AsRef<Pod>>(
    pool: &BuildKitPool,
    cands: &mut [P],
    key: Option<&str>,
    local: bool,
) {
    shuffle(cands);
    if render::cache_limit(pool) == 0 {
        return;
    }
    // Stable, so each group keeps its shuffled order.
    cands.sort_by_key(|p| {
        let p = p.as_ref();
        if !local {
            u8::from(cached(pool, p))
        } else if key.is_some_and(|k| keys_contain(render::pod_cache_keys(p), k)) {
            0
        } else if cached(pool, p) {
            1
        } else {
            2
        }
    });
}

/// Cold-path order: free cached slots first with the local layer, last
/// without it. Each group is shuffled.
pub(super) fn order_cold(pool: &BuildKitPool, slots: &mut [u32], local: bool) {
    shuffle(slots);
    slots.sort_by_key(|&s| render::is_cached_slot(pool, s) != local);
}

/// What a slot's volume holds, as its annotations say.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Have {
    /// `PVC cache-keys`.
    pub keys: Option<String>,
    /// `PVC cache-scope`, `None` when unset, empty or malformed.
    pub binding: Option<Binding>,
}

impl Have {
    fn of(pvc: &PersistentVolumeClaim) -> Self {
        let a = pvc.annotations();
        Self {
            keys: a.get(KEY_CACHE_KEYS).cloned(),
            binding: a.get(KEY_CACHE_SCOPE).and_then(|v| Binding::parse(v)),
        }
    }
}

/// What a cold create in a slot may do about its volume.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Volume {
    /// No volume, or a current one with its `PVC cache-keys` and `PVC cache-scope`: create the pod.
    Ready(Have),
    /// Terminating, replaced, or not the pool's: try the next slot.
    Skip,
    /// 403 `exceeded quota`: queue the request.
    Quota,
}
fn owned_by(pvc: &PersistentVolumeClaim, pool: &BuildKitPool) -> bool {
    let uid = pool.uid();
    pvc.owner_references()
        .iter()
        .any(|o| o.controller == Some(true) && Some(&o.uid) == uid.as_ref())
}

/// Ensure volume before a cold create in `slot`.
pub(super) async fn ensure_volume(
    backend: &dyn Backend,
    metrics: &AllocMetrics,
    pool: &BuildKitPool,
    slot: u32,
) -> Result<Volume, AllocError> {
    let want = match render::cache_volume(pool, slot) {
        Ok(Some(v)) => v,
        Ok(None) => return Ok(Volume::Ready(Have::default())),
        Err(e) => {
            error!(error = %e, "rendering cache volume");
            return Err(AllocError::Internal);
        }
    };
    let (ns, name) = (want.namespace().unwrap_or_default(), want.name_any());
    let have = backend.get_pvc(&ns, &name).await.map_err(|error| {
        error!(namespace = %ns, volume = %name, error, "get cache volume");
        AllocError::Internal
    })?;
    let label = |p: &PersistentVolumeClaim| p.labels().get(LABEL_VOLUME_HASH).cloned();
    let Some(have) = have else {
        return match backend.create_pvc(&ns, want).await {
            PvcCreate::Created => {
                metrics.volume_op("create", "ok");
                Ok(Volume::Ready(Have::default()))
            }
            PvcCreate::Quota => {
                metrics.volume_op("create", "error");
                Ok(Volume::Quota)
            }
            PvcCreate::Failed(error) => {
                metrics.volume_op("create", "error");
                error!(namespace = %ns, volume = %name, error, "create cache volume");
                Err(AllocError::Internal)
            }
        };
    };
    if !owned_by(&have, pool) || have.metadata.deletion_timestamp.is_some() {
        return Ok(Volume::Skip);
    }
    if label(&have) != label(&want) {
        // Nobody holds it: the pod about to be created is the only one that may hold the slot.
        let uid = have.uid().unwrap_or_default();
        let result = match backend.delete_pvc(&ns, &name, &uid).await {
            Ok(()) => "ok",
            Err(error) => {
                warn!(namespace = %ns, volume = %name, error, "delete replaced cache volume");
                "error"
            }
        };
        metrics.volume_op("delete", result);
        return Ok(Volume::Skip);
    }
    Ok(Volume::Ready(Have::of(&have)))
}

/// The cache result of a won pod. `before` is its `P cache-keys` before the
/// claim. `created` is `Some(wiped)` when the pod was created for this
/// request, `wiped` when on a volume another scope owned. `spill` counts only busy
/// volumes of `scope`.
pub(super) fn result(
    pool: &BuildKitPool,
    won: &Pod,
    before: Option<&str>,
    created: Option<bool>,
    scope: Option<&str>,
    key: &str,
    pods: &[Arc<Pod>],
) -> &'static str {
    if !cached(pool, won) {
        "uncached"
    } else if let Some(wiped) = created {
        if wiped { "wiped" } else { "cold" }
    } else if keys_contain(before, key) {
        "hit"
    } else if pods.iter().any(|p| {
        p.metadata.name != won.metadata.name
            && ann(p, ANN_ALLOC).is_some_and(|a| a != FREE)
            && cached(pool, p)
            // Only a volume of the caller's own scope could have been a hit.
            && scope.is_none_or(|s| ann(p, KEY_CACHE_SCOPE) == Some(s))
            && keys_contain(render::pod_cache_keys(p), key)
    }) {
        "spill"
    } else {
        "miss"
    }
}

/// After a win in a cached slot, write `keys` to the slot's `PVC cache-keys`,
/// detached from the request. A failure only costs a later hit.
pub(super) fn annotate_detached(
    backend: Arc<dyn Backend>,
    metrics: AllocMetrics,
    pool: &BuildKitPool,
    pod: &Pod,
    keys: String,
) {
    let Some(slot) = pod
        .metadata
        .name
        .as_deref()
        .and_then(|n| slot_of(n, &pool.name_any()))
        .filter(|&s| render::is_cached_slot(pool, s))
    else {
        return;
    };
    let ns = pool.namespace().unwrap_or_default();
    let name = render::cache_volume_name(&pool.name_any(), slot);
    tokio::spawn(async move {
        let result = match backend
            .annotate_pvc(&ns, &name, KEY_CACHE_KEYS, &keys)
            .await
        {
            Ok(()) => "ok",
            Err(error) => {
                warn!(namespace = %ns, volume = %name, error, "annotating cache volume");
                "error"
            }
        };
        metrics.volume_op("label", result);
    });
}

/// The cache scope name of `who` under `scope`: the caller's trust boundary.
/// A GitHub Actions token (or any OIDC token with the claim) is scoped to
/// `repository_id`, else `repository`; any other OIDC token to its `sub`.
/// The issuer is always part of it, so another issuer cannot claim the same
/// scope. A `ServiceAccount` is scoped to its namespace and name, a
/// `BuildKitClaim` to its namespace (every claim of a namespace is one tenant).
#[must_use]
pub fn scope_name(scope: CacheScope, who: &Identity) -> String {
    let claim = |claims: &serde_json::Map<String, Value>, k: &str| match claims.get(k) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    };
    let name = match (scope, who) {
        (CacheScope::Pool, _) => json!(["pool"]),
        (
            CacheScope::Identity,
            Identity::Oidc {
                issuer,
                subject,
                claims,
            },
        ) => {
            let (k, v) = ["repository_id", "repository"]
                .into_iter()
                .find_map(|k| claim(claims, k).map(|v| (k, v)))
                .unwrap_or(("sub", subject.clone()));
            json!(["oidc", issuer, k, v])
        }
        (
            CacheScope::Identity,
            Identity::ServiceAccount {
                namespace, name, ..
            },
        ) => {
            json!(["sa", namespace, name])
        }
        (CacheScope::Identity, Identity::Claim { namespace, .. }) => json!(["claim", namespace]),
    };
    name.to_string()
}

/// The scope id of `who` in `pool`, or `None` when the pool has no `cache.local`.
#[must_use]
pub fn scope(pool: &BuildKitPool, who: &Identity) -> Option<String> {
    let lc = pool.spec.workers.cache.local.as_ref()?;
    Some(render::scope_id(&scope_name(lc.scope, who)))
}

/// Whether a warm candidate may serve `scope`: any pod outside a cached slot,
/// and a pod in one only when it serves `scope` or any scope (see [`render::pod_serves`]).
pub(super) fn serves(pool: &BuildKitPool, pod: &Pod, scope: Option<&str>) -> bool {
    let cached = cached(pool, pod);
    match scope {
        Some(s) => render::pod_serves(pod, cached, s),
        // No `cache.local`, so no cached slot either.
        None => !cached,
    }
}

/// After `scope` won `pod` in a cached slot, record the volume's new owner
/// on its `PVC cache-scope`, detached. A lost write is safe: the next pod
/// bound from a stale value finds another generation on the volume and empties it.
pub(super) fn bind_detached(
    backend: Arc<dyn Backend>,
    metrics: AllocMetrics,
    pool: &BuildKitPool,
    pod: &Pod,
    scope: &str,
) {
    let Some(slot) = cached_slot(pool, pod) else {
        return;
    };
    let Some(b) = render::won_binding(pod, scope) else {
        return;
    };
    let ns = pool.namespace().unwrap_or_default();
    let name = render::cache_volume_name(&pool.name_any(), slot);
    tokio::spawn(async move {
        let result = match backend
            .annotate_pvc(&ns, &name, KEY_CACHE_SCOPE, &b.value())
            .await
        {
            Ok(()) => "ok",
            Err(error) => {
                warn!(namespace = %ns, volume = %name, error, "binding cache volume");
                "error"
            }
        };
        metrics.volume_op("label", result);
    });
}

fn cached_slot(pool: &BuildKitPool, pod: &Pod) -> Option<u32> {
    pod.metadata
        .name
        .as_deref()
        .and_then(|n| slot_of(n, &pool.name_any()))
        .filter(|&s| render::is_cached_slot(pool, s))
}

/// A free, warm pod to retire so that `scope` can get a slot: one in a
/// cached slot bound to another scope, picked only when the request found no
/// candidate and no free slot, and nothing in the pool is already starting or
/// terminating (a replacement on its way). `None` otherwise.
pub(super) fn recycle_pick<'a>(
    pool: &BuildKitPool,
    pods: &'a [Arc<Pod>],
    free_slots: usize,
    foreign: &[&'a Arc<Pod>],
) -> Option<&'a Arc<Pod>> {
    if free_slots > 0 || foreign.is_empty() {
        return None;
    }
    let busy = pods.iter().any(|p| {
        p.metadata.deletion_timestamp.is_some()
            || (ann(p, ANN_ALLOC) == Some(FREE) && !super::pods::is_ready(p))
    });
    if busy {
        return None;
    }
    foreign.iter().copied().find(|p| cached(pool, p))
}

/// Empty the `PVC cache-scope` of the slot of `pod` before it is recycled, so
/// the next pod there serves any scope and empties the volume. Synchronous:
/// recycling without it would only bring the same scope back.
pub(super) async fn unbind(
    backend: &dyn Backend,
    metrics: &AllocMetrics,
    pool: &BuildKitPool,
    pod: &Pod,
) -> bool {
    let Some(slot) = cached_slot(pool, pod) else {
        return false;
    };
    let ns = pool.namespace().unwrap_or_default();
    let name = render::cache_volume_name(&pool.name_any(), slot);
    match backend.annotate_pvc(&ns, &name, KEY_CACHE_SCOPE, "").await {
        Ok(()) => {
            metrics.volume_op("label", "ok");
            true
        }
        Err(error) => {
            metrics.volume_op("label", "error");
            warn!(namespace = %ns, volume = %name, error, "unbinding cache volume");
            false
        }
    }
}

/// The claim patch's extra ops for a warm pod in a cached slot: `P cache-keys`
/// with the request's key in front (soft affinity), and `P cache-scope` set to
/// the caller's scope, so a pod that served any scope now records whose it is.
pub(super) fn claim_ops(
    pool: &BuildKitPool,
    pod: &Pod,
    before: Option<&str>,
    affinity: Option<&(String, usize)>,
    scope: Option<&str>,
    patch: &mut Value,
) {
    let Value::Array(ops) = patch else {
        return;
    };
    if !cached(pool, pod) {
        return;
    }
    let at = |k: &str| format!("/metadata/annotations/{}", k.replace('/', "~1"));
    if let Some((k, cap)) = affinity {
        let keys = render::push_key(before, k, *cap);
        ops.push(json!({"op": "add", "path": at(KEY_CACHE_KEYS), "value": keys}));
    }
    if let Some(s) = scope {
        ops.push(json!({"op": "add", "path": at(KEY_CACHE_SCOPE), "value": s}));
    }
}
