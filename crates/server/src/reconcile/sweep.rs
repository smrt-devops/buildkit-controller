//! Upgrade from v0.1.0: delete the per-slot cache volumes it created.
//!
//! v0.1.0 kept build cache on one PVC per worker slot. Workers are stateless
//! now, so the leader deletes every PVC with that release's cache labels once,
//! at its first term, and logs each one. v0.1.0 upgrade shim: remove in the
//! release after v3, with the PVC `list` and `delete` rights in the chart.

use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use kube::api::{Api, DeleteParams, ListParams, Preconditions};
use kube::{Client, ResourceExt};
use tracing::{info, warn};

/// The labels v0.1.0 set on its cache volumes.
pub const SELECTOR: &str =
    "app.kubernetes.io/managed-by=buildkit-controller,app.kubernetes.io/name=buildkit-cache";

/// Deletes the old cache volumes in `namespaces` (`[]` = all). True when
/// every list and delete succeeded, so the caller need not run it again.
pub async fn old_cache_volumes(client: &Client, namespaces: &[String]) -> bool {
    let apis: Vec<Api<PersistentVolumeClaim>> = if namespaces.is_empty() {
        vec![Api::all(client.clone())]
    } else {
        namespaces
            .iter()
            .map(|ns| Api::namespaced(client.clone(), ns))
            .collect()
    };
    let mut done = true;
    for api in apis {
        let pvcs = match api.list(&ListParams::default().labels(SELECTOR)).await {
            Ok(l) => l.items,
            Err(error) => {
                warn!(%error, "listing old cache volumes");
                done = false;
                continue;
            }
        };
        for pvc in pvcs {
            done &= delete(client, &pvc).await;
        }
    }
    done
}

async fn delete(client: &Client, pvc: &PersistentVolumeClaim) -> bool {
    let (ns, name) = (pvc.namespace().unwrap_or_default(), pvc.name_any());
    let dp = DeleteParams {
        preconditions: Some(Preconditions {
            uid: pvc.uid(),
            resource_version: None,
        }),
        ..DeleteParams::default()
    };
    let api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
    match api.delete(&name, &dp).await {
        Ok(_) => {
            info!(namespace = %ns, volume = %name, "deleted a cache volume of v0.1.0");
            true
        }
        Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => true,
        Err(error) => {
            warn!(namespace = %ns, volume = %name, %error, "deleting a cache volume of v0.1.0");
            false
        }
    }
}
