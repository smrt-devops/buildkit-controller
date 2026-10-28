//! Cache volumes on the leader: ensure the slot's PVC
//! before a create, and reap PVCs above the limit or after a failed pod.

use std::collections::BTreeSet;

use buildkit_controller_api::v1alpha1::BuildKitPool;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use kube::ResourceExt;

use super::exec::Report;
use super::ops::{KubeOps, OpError};
use super::plan::{CreateResult, RetireReason};
use super::pods::parse_slot;
use crate::render::{
    self, Binding, Bound, CACHE_VOLUME_NAME, KEY_CACHE_KEYS, KEY_CACHE_SCOPE, LABEL_NAME,
    LABEL_POOL, LABEL_VOLUME_HASH,
};

fn label<'a>(pvc: &'a PersistentVolumeClaim, key: &str) -> Option<&'a str> {
    pvc.labels().get(key).map(String::as_str)
}

/// The PVC's controller ownerReference is this pool.
fn owned_by(pvc: &PersistentVolumeClaim, pool: &BuildKitPool) -> bool {
    let uid = pool.uid();
    pvc.owner_references()
        .iter()
        .any(|o| o.controller == Some(true) && Some(&o.uid) == uid.as_ref())
}

/// DELETE with a uid precondition; 404 and 409 are done. Returns false on any other error.
async fn delete<O: KubeOps>(ops: &O, pvc: &PersistentVolumeClaim, r: &mut Report) -> bool {
    let (ns, name) = (pvc.namespace().unwrap_or_default(), pvc.name_any());
    let uid = pvc.uid().unwrap_or_default();
    match ops.delete_pvc(&ns, &name, &uid).await {
        Ok(()) | Err(OpError::NotFound | OpError::Conflict(_)) => {
            r.volume_ops.push(("delete", "ok"));
            true
        }
        Err(e) => {
            r.volume_ops.push(("delete", "error"));
            r.errors.push(format!("delete volume {name}: {e}"));
            false
        }
    }
}

/// Ensure volume for a pod about to be created in
/// `slot`. `None` lets the create go on; `Some` is the create's outcome
/// without a POST. A current volume's `PVC cache-scope` binds `pod`
/// ([`render::bind_pod`]), and its `PVC cache-keys` are copied when the pod keeps it.
pub(super) async fn ensure_volume<O: KubeOps>(
    ops: &O,
    pool: &BuildKitPool,
    slot: u32,
    pod: &mut Pod,
    r: &mut Report,
) -> Option<CreateResult> {
    let failed = |message: String| Some(CreateResult::Failed { message });
    let want = match render::cache_volume(pool, slot) {
        Ok(Some(v)) => v,
        Ok(None) => return None,
        Err(e) => return failed(e.to_string()),
    };
    let (ns, name) = (want.namespace().unwrap_or_default(), want.name_any());
    match ops.get_pvc(&ns, &name).await {
        Err(e) => {
            r.errors.push(format!("get volume {name}: {e}"));
            failed(e.to_string())
        }
        Ok(None) => match ops.create_pvc(&want).await {
            // 409: an API replica created it for a cold-path pod in this slot first.
            Ok(()) | Err(OpError::Conflict(_)) => {
                r.volume_ops.push(("create", "ok"));
                None
            }
            Err(e) => {
                r.volume_ops.push(("create", "error"));
                r.errors.push(format!("create volume {name}: {e}"));
                failed(e.to_string())
            }
        },
        Ok(Some(have)) if !owned_by(&have, pool) => failed(format!(
            "volume `{name}` exists and the pool does not own it"
        )),
        Ok(Some(have)) if have.metadata.deletion_timestamp.is_some() => {
            Some(CreateResult::VolumePending)
        }
        Ok(Some(have)) if label(&have, LABEL_VOLUME_HASH) != label(&want, LABEL_VOLUME_HASH) => {
            // Nobody holds it: the pod about to be created is the only one that may hold the slot.
            delete(ops, &have, r).await;
            Some(CreateResult::VolumePending)
        }
        Ok(Some(have)) => {
            // A warm pod serves the scope that owns the volume, or any scope
            // when none does (its init container then empties the volume).
            let binding = have
                .annotations()
                .get(KEY_CACHE_SCOPE)
                .and_then(|v| Binding::parse(v));
            if render::bind_pod(pod, binding.as_ref(), None) == Bound::Kept
                && let Some(keys) = have.annotations().get(KEY_CACHE_KEYS)
            {
                render::set_pod_cache_keys(pod, keys);
            }
            None
        }
    }
}

/// After a retire for `StartTimeout` or `DaemonCrashed`, delete the slot's
/// volume: one that will not attach or that crashes buildkitd is worth less
/// than an empty one. A released, expired, evicted or allocated-and-finished
/// pod keeps it: a build that kills its own daemon leaves bolt databases that
/// the next start either loads or crashes on, and that crash is reaped here.
/// `pvc-protection` keeps the volume until the pod is gone.
pub(super) async fn reap_after_failure<O: KubeOps>(
    ops: &O,
    pool: &BuildKitPool,
    pod: &str,
    reason: RetireReason,
    r: &mut Report,
) {
    if !matches!(
        reason,
        RetireReason::StartTimeout | RetireReason::DaemonCrashed
    ) {
        return;
    }
    let pool_name = pool.name_any();
    let Some(slot) = parse_slot(pod, &pool_name) else {
        return;
    };
    if !render::is_cached_slot(pool, slot) {
        return;
    }
    let ns = pool.namespace().unwrap_or_default();
    let name = render::cache_volume_name(&pool_name, slot);
    match ops.get_pvc(&ns, &name).await {
        Ok(Some(pvc)) if owned_by(&pvc, pool) && pvc.metadata.deletion_timestamp.is_none() => {
            delete(ops, &pvc, r).await;
        }
        Ok(_) => {}
        Err(e) => r.errors.push(format!("get volume {name}: {e}")),
    }
}

/// Delete the pool's volumes in slots `>= limit` (all of them when
/// `cache.local` is unset). A volume whose slot a pod in `held` still holds
/// is skipped. Returns true when nothing is left to reap, so the caller can
/// record the generation as reaped.
pub(super) async fn reap_above_limit<O: KubeOps>(
    ops: &O,
    pool: &BuildKitPool,
    held: &BTreeSet<u32>,
    r: &mut Report,
) -> bool {
    let (ns, name) = (pool.namespace().unwrap_or_default(), pool.name_any());
    let selector = format!("{LABEL_NAME}={CACHE_VOLUME_NAME},{LABEL_POOL}={name}");
    let pvcs = match ops.list_pvcs(&ns, &selector).await {
        Ok(v) => v,
        Err(e) => {
            r.errors.push(format!("list volumes: {e}"));
            return false;
        }
    };
    let limit = render::cache_limit(pool);
    let mut complete = true;
    for pvc in &pvcs {
        let Some(slot) = render::cache_volume_slot(&name, &pvc.name_any()) else {
            continue;
        };
        if slot < limit || !owned_by(pvc, pool) || pvc.metadata.deletion_timestamp.is_some() {
            continue;
        }
        if held.contains(&slot) {
            complete = false;
            continue;
        }
        complete &= delete(ops, pvc, r).await;
    }
    complete
}
