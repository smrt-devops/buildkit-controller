//! Upgrade from v0.1.0: delete the per-slot cache volumes it created.
//!
//! v0.1.0 kept build cache on one PVC per worker slot. Workers are stateless
//! now, so the leader deletes those PVCs at the first term of each process and
//! logs each one; once they are gone the sweep is a list that finds nothing.
//! A PVC is swept when it carries that release's cache labels and no
//! controller other than a `BuildKitPool` holds it. v0.1.0 upgrade shim: remove
//! in the release after v3, with the PVC `list` and `delete` rights in the chart.

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
        for pvc in pvcs.iter().filter(|p| is_old_cache_volume(p)) {
            done &= delete(client, pvc).await;
        }
    }
    done
}

/// No controller other than a `BuildKitPool` holds the PVC. v0.1.0 made each
/// cache volume's pool its controller; a volume another controller holds
/// (a `StatefulSet`, an operator) only shares the labels and is kept.
fn is_old_cache_volume(pvc: &PersistentVolumeClaim) -> bool {
    pvc.owner_references()
        .iter()
        .filter(|o| o.controller == Some(true))
        .all(|o| {
            o.kind == "BuildKitPool"
                && o.api_version.split('/').next() == Some("buildkit.smrt-devops.net")
        })
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

#[cfg(test)]
mod tests {
    use k8s_openapi::api::core::v1::PersistentVolumeClaim;

    use super::is_old_cache_volume;

    fn pvc(v: serde_json::Value) -> PersistentVolumeClaim {
        serde_json::from_value(v).unwrap()
    }

    /// A labelled volume held by a pool, or by no controller, is swept; one
    /// that another controller holds only shares the labels and is kept.
    #[test]
    fn volumes_of_other_controllers_are_kept() {
        let with = |owners: serde_json::Value| {
            pvc(serde_json::json!({
                "metadata": { "name": "cache-local-cache-0", "ownerReferences": owners }
            }))
        };
        let owner = |api: &str, kind: &str, controller: bool| {
            serde_json::json!([{
                "apiVersion": api, "kind": kind, "name": "x", "uid": "u",
                "controller": controller,
            }])
        };
        let pool = "buildkit.smrt-devops.net/v1alpha1";
        assert!(is_old_cache_volume(&with(owner(
            pool,
            "BuildKitPool",
            true
        ))));
        assert!(is_old_cache_volume(&with(serde_json::json!([]))));
        assert!(is_old_cache_volume(&with(owner(
            "apps/v1",
            "StatefulSet",
            false
        ))));
        assert!(!is_old_cache_volume(&with(owner(
            "apps/v1",
            "StatefulSet",
            true
        ))));
        assert!(!is_old_cache_volume(&with(owner(
            "example.com/v1",
            "BuildKitPool",
            true
        ))));
    }
}
