//! Pool helpers shared by the front door, the allocator and the HTTP layer:
//! access rules, readiness and durations.

use buildkit_controller_api::v1alpha1::{self as crd, BuildKitPool};

use crate::auth::{AccessRule, AccessRules};

/// The shortest TTL a claim may ask for.
pub const MIN_TTL: u64 = 60;

/// `spec.access.rules` into the auth rule type. Any malformed rule
/// denies the whole pool (fails closed).
#[must_use]
pub fn access_rules(pool: &BuildKitPool) -> Option<AccessRules> {
    rules(&pool.spec.access.rules)
}

/// Pool access rules into the auth rule type; `None` when one is malformed.
#[must_use]
pub fn rules(list: &[crd::AccessRule]) -> Option<AccessRules> {
    let rules = list
        .iter()
        .map(|r| match (&r.oidc, &r.service_account, &r.claims) {
            (None, None, Some(c)) => Some(AccessRule::Claims {
                namespaces: c.namespaces.clone(),
            }),
            (Some(o), None, None) => Some(AccessRule::Oidc {
                issuer: o.issuer.clone(),
                claims: o
                    .claims
                    .iter()
                    .map(|(k, v)| (k.clone(), v.0.clone()))
                    .collect(),
            }),
            (None, Some(s), None) => Some(AccessRule::ServiceAccount {
                namespace: s.namespace.clone(),
                name: s.name.clone(),
            }),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    AccessRules::new(rules).ok()
}

/// Whether allocation may go ahead on `pool`: `Ready` is True, or False only
/// for the CA (the binder checks the CA itself).
#[must_use]
pub fn pool_ready(pool: &BuildKitPool) -> bool {
    let ready = pool
        .status
        .iter()
        .flat_map(|s| &s.conditions)
        .find(|c| c.type_ == "Ready");
    matches!(ready, Some(c) if c.status == "True" || c.reason == "CAUnavailable")
}

/// Whole seconds of a pool duration.
pub fn seconds(d: &crd::Duration) -> Result<u64, String> {
    d.seconds().map_err(|e| e.to_string())
}
