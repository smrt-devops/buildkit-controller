//! Grants and budgets from the cluster: a worker may use the cache of the
//! allocation it holds now, and each pool's caches may hold `cache.maxSize`.

use buildkit_controller_api::v1alpha1::BuildKitPool;
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::reflector::{ObjectRef, Store};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::auth::{Authorizer, Grant};
use super::gc::Budgets;
use crate::pki::WorkerIdentity;
use crate::render::{
    ANN_ALLOC, ANN_EXPIRES_AT, ANN_SCOPE, ANN_TIER, ANN_WORKER_NONCE, CONTROLLER, DRAINING, FREE,
    LABEL_MANAGED_BY, LABEL_POOL, is_scope_id, quantity_bytes,
};
use crate::runtime::stores::StoreFreshness;

/// Grants from the Pod and Pool watches.
pub struct PodGrants {
    pub pods: Store<Pod>,
    pub pools: Store<BuildKitPool>,
    /// Both stores listed and current; a stale store grants nothing.
    pub fresh: StoreFreshness,
}

impl Authorizer for PodGrants {
    fn grant(&self, peer: &WorkerIdentity) -> Option<Grant> {
        if !self.fresh.is_fresh() {
            return None;
        }
        let ns = peer.namespace.as_str();
        let pod = self.pods.get(&ObjectRef::new(&peer.pod).within(ns))?;
        let pool = self
            .pools
            .get(&ObjectRef::new(peer.pool.as_str()).within(ns))?;
        grant_of(
            &pod,
            peer,
            pool.metadata.uid.as_deref()?,
            OffsetDateTime::now_utc(),
        )
    }
}

/// The grant of the worker `peer` names, read from its pod: the pod is that
/// incarnation of a worker of the pool with uid `pool_uid` (labels, controller
/// ownerReference, `P worker-nonce`), it is not terminating, and it holds an
/// allocation that has not expired and carries a scope. Pull from
/// `<namespace>/<pool>/<scope>`; push too when `P tier` is `trusted`.
#[must_use]
pub fn grant_of(
    pod: &Pod,
    peer: &WorkerIdentity,
    pool_uid: &str,
    now: OffsetDateTime,
) -> Option<Grant> {
    let m = &pod.metadata;
    let ann = |k: &str| m.annotations.as_ref()?.get(k).map(String::as_str);
    let label = |k: &str| m.labels.as_ref()?.get(k).map(String::as_str);
    let ours = m.namespace.as_deref() == Some(peer.namespace.as_str())
        && label(LABEL_POOL) == Some(peer.pool.as_str())
        && label(LABEL_MANAGED_BY) == Some(CONTROLLER)
        && m.owner_references
            .iter()
            .flatten()
            .any(|o| o.controller == Some(true) && o.uid == pool_uid)
        && ann(ANN_WORKER_NONCE) == Some(peer.nonce.as_str())
        && m.deletion_timestamp.is_none();
    let allocated = ann(ANN_ALLOC).is_some_and(|a| a != FREE && a != DRAINING)
        && ann(ANN_EXPIRES_AT)
            .and_then(|t| OffsetDateTime::parse(t, &Rfc3339).ok())
            .is_some_and(|t| t > now);
    let scope = ann(ANN_SCOPE).filter(|s| is_scope_id(s))?;
    (ours && allocated).then(|| Grant {
        repository: format!("{}/{}/{scope}", peer.namespace, peer.pool),
        push: ann(ANN_TIER) == Some("trusted"),
    })
}

/// Budgets from the Pool watch: `cache.maxSize` of the pool, else `default`.
/// A pool that is gone gets nothing, so its caches go at the next collection;
/// until the store is current every pool gets `default`.
pub struct PoolBudgets {
    pub pools: Store<BuildKitPool>,
    pub fresh: StoreFreshness,
    /// Budget of a pool without `cache.maxSize`.
    pub default: u64,
    /// Budget of the whole volume.
    pub total: u64,
}

impl Budgets for PoolBudgets {
    fn pool(&self, pool: &str) -> u64 {
        if !self.fresh.is_fresh() {
            return self.default;
        }
        let Some((ns, name)) = pool.split_once('/') else {
            return 0;
        };
        match self.pools.get(&ObjectRef::new(name).within(ns)) {
            None => 0,
            Some(p) => p
                .spec
                .workers
                .cache
                .max_size
                .as_ref()
                .and_then(|q| quantity_bytes(&q.0))
                .unwrap_or(self.default),
        }
    }

    fn total(&self) -> u64 {
        self.total
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use kube::runtime::reflector;
    use kube::runtime::watcher;
    use rustls_pki_types::CertificateDer;
    use serde_json::json;

    use super::*;
    use crate::pki::CaBundle;
    use crate::registry::auth::{Action, peer_identity};
    use crate::render::fixtures;

    const SCOPE: &str = "0123456789abcdef0123456789abcdef";
    const NONCE: &str = "00000000000000aa";
    const UID: &str = "2f6c1f4e-3b0a-4c43-9a55-5d4f0c7b1a10";

    fn peer() -> WorkerIdentity {
        WorkerIdentity {
            namespace: "ci".parse().unwrap(),
            pool: "p".parse().unwrap(),
            pod: "p-0".into(),
            nonce: NONCE.into(),
        }
    }

    /// An allocated worker pod of pool `p` in `ci`; `edit` changes its JSON.
    fn pod(edit: impl FnOnce(&mut serde_json::Value)) -> Pod {
        let mut v = json!({
            "metadata": {
                "name": "p-0", "namespace": "ci",
                "labels": {"app.kubernetes.io/managed-by": "buildkit-controller", "buildkit.smrt-devops.net/pool": "p"},
                "annotations": {
                    "buildkit.smrt-devops.net/worker-nonce": NONCE,
                    "buildkit.smrt-devops.net/alloc": "mfrggzdfmztwq2lknnwg23tpoa",
                    "buildkit.smrt-devops.net/expires-at": "2099-01-01T00:00:00Z",
                    "buildkit.smrt-devops.net/scope": SCOPE,
                    "buildkit.smrt-devops.net/tier": "trusted"
                },
                "ownerReferences": [{"apiVersion": "buildkit.smrt-devops.net/v1alpha1", "kind": "BuildKitPool",
                    "name": "p", "uid": UID, "controller": true}]
            }
        });
        edit(&mut v);
        serde_json::from_value(v).unwrap()
    }

    fn grant(edit: impl FnOnce(&mut serde_json::Value)) -> Option<Grant> {
        grant_of(&pod(edit), &peer(), UID, OffsetDateTime::now_utc())
    }

    #[test]
    fn a_trusted_allocation_pulls_and_pushes_its_scope_only() {
        let g = grant(|_| {}).unwrap();
        let repo = format!("ci/p/{SCOPE}");
        assert_eq!(g.repository, repo);
        assert!(g.allows(&repo, Action::Push) && g.allows(&repo, Action::Pull));
        assert!(!g.allows("ci/p/ffffffffffffffffffffffffffffffff", Action::Pull));
        assert!(!g.allows("ci/other/x", Action::Pull));
    }

    #[test]
    fn an_untrusted_allocation_only_pulls() {
        let ann = "/metadata/annotations/buildkit.smrt-devops.net~1tier";
        for tier in [Some("untrusted"), Some("TRUSTED"), None] {
            let g = grant(|v| match tier {
                Some(t) => *v.pointer_mut(ann).unwrap() = json!(t),
                None => {
                    v["metadata"]["annotations"]
                        .as_object_mut()
                        .unwrap()
                        .remove("buildkit.smrt-devops.net/tier");
                }
            })
            .unwrap();
            assert!(!g.push, "{tier:?}");
            assert!(g.allows(&format!("ci/p/{SCOPE}"), Action::Pull));
        }
    }

    #[test]
    fn no_live_allocation_of_this_worker_gets_nothing() {
        let a = |k: &str| format!("/metadata/annotations/buildkit.smrt-devops.net~1{k}");
        let set = |path: String, val: serde_json::Value| {
            move |v: &mut serde_json::Value| *v.pointer_mut(&path).unwrap() = val
        };
        assert!(grant(set(a("alloc"), json!("free"))).is_none(), "free");
        assert!(
            grant(set(a("alloc"), json!("draining"))).is_none(),
            "released"
        );
        assert!(
            grant(set(a("expires-at"), json!("2020-01-01T00:00:00Z"))).is_none(),
            "expired"
        );
        assert!(
            grant(set(a("worker-nonce"), json!("00000000000000bb"))).is_none(),
            "another incarnation"
        );
        assert!(
            grant(set(a("scope"), json!("../../x"))).is_none(),
            "bad scope"
        );
        assert!(
            grant(set(
                "/metadata/ownerReferences/0/uid".into(),
                json!("other")
            ))
            .is_none(),
            "another pool"
        );
        assert!(
            grant(set(
                "/metadata/labels/buildkit.smrt-devops.net~1pool".into(),
                json!("q")
            ))
            .is_none(),
            "pool label"
        );
        assert!(
            grant(|v| v["metadata"]["deletionTimestamp"] = json!("2026-01-01T00:00:00Z")).is_none(),
            "terminating"
        );
        assert!(
            grant(|v| {
                v["metadata"]["annotations"]
                    .as_object_mut()
                    .unwrap()
                    .remove("buildkit.smrt-devops.net/scope");
            })
            .is_none(),
            "no scope"
        );
    }

    /// The registry client certificate the reconciler gives a worker names the
    /// pod the registry looks up; its serving certificate is no client cert.
    #[test]
    fn a_worker_certificate_identifies_its_pod() {
        let now = OffsetDateTime::now_utc();
        let ca = CaBundle::generate(now).unwrap();
        let serving = ca.issue_worker_serving(&peer(), now).unwrap();
        assert!(peer_identity(&CertificateDer::from(serving.cert_der().to_vec())).is_err());
        let c = ca
            .issue_worker_client_for(&peer(), now, time::Duration::days(30))
            .unwrap();
        let id = peer_identity(&CertificateDer::from(c.cert_der().to_vec())).unwrap();
        assert_eq!(id, peer());
        assert!(grant_of(&pod(|_| {}), &id, UID, OffsetDateTime::now_utc()).is_some());
    }

    #[test]
    fn budgets_follow_pool_max_size() {
        let (store, mut w) = reflector::store::<BuildKitPool>();
        let mut sized =
            fixtures::pool("sized", json!({"workers": {"cache": {"maxSize": "2Ki"}}})).unwrap();
        sized.metadata.namespace = Some("ci".into());
        let mut plain = fixtures::pool("plain", json!({})).unwrap();
        plain.metadata.namespace = Some("ci".into());
        w.apply_watcher_event(&watcher::Event::Apply(sized));
        w.apply_watcher_event(&watcher::Event::Apply(plain));
        let b = |fresh| PoolBudgets {
            pools: store.clone(),
            fresh: StoreFreshness::fixed(fresh),
            default: 100,
            total: 1000,
        };
        let fresh = b(true);
        assert_eq!(fresh.pool("ci/sized"), 2048);
        assert_eq!(fresh.pool("ci/plain"), 100);
        assert_eq!(fresh.pool("ci/gone"), 0, "a deleted pool's caches go");
        assert_eq!(fresh.total(), 1000);
        assert_eq!(
            b(false).pool("ci/gone"),
            100,
            "unknown until the store is current"
        );
    }
}
