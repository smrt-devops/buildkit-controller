//! Edge activity on claims. Every replica serves the edge, and only the
//! leader releases claims, so each replica records on the claim that it
//! proxies the allocation: it stamps `edge-seen-at` while it holds a
//! connection and once more after one closes, throttled like `seen-at`. The
//! leader releases a delivered claim whose stamps are all older than the
//! pool's `scaling.idleTimeout` ([`claims::idle`]).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use buildkit_controller_api::claim::BuildKitClaim;
use kube::runtime::reflector::{ObjectRef, Store};
use time::OffsetDateTime;
use tracing::{debug, warn};

use super::backend::{Backend, Write};
use super::claims;
use super::pods::fmt_ts;

/// Writes `edge-seen-at` for one replica's edge connections.
pub struct EdgeActivity {
    backend: Arc<dyn Backend>,
    claims: Store<BuildKitClaim>,
    /// The controller's namespace, where the API's claims live, named after
    /// their allocation ids.
    namespace: String,
    /// Allocations with no connection left on this replica, and when their
    /// last one closed, until their claim records it.
    closed: Mutex<HashMap<String, OffsetDateTime>>,
}

impl EdgeActivity {
    /// Stamps the claims in `claims` of controller namespace `namespace`.
    #[must_use]
    pub fn new(backend: Arc<dyn Backend>, claims: Store<BuildKitClaim>, namespace: String) -> Self {
        Self {
            backend,
            claims,
            namespace,
            closed: Mutex::default(),
        }
    }

    fn closed(&self) -> std::sync::MutexGuard<'_, HashMap<String, OffsetDateTime>> {
        self.closed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One round at `now`. `live` holds the allocation ids with a connection
    /// on this replica; `closed` the ids whose connections closed since the
    /// last round, with when. A claim is stamped when it is behind that
    /// activity and its stamp is due ([`claims::edge_due`]). A close the
    /// throttle holds back, or a write that fails, is retried next round.
    pub async fn stamp(
        &self,
        live: &HashSet<String>,
        closed: impl IntoIterator<Item = (String, OffsetDateTime)>,
        now: OffsetDateTime,
    ) {
        let wanted: Vec<(String, OffsetDateTime)> = {
            let mut pending = self.closed();
            for (id, at) in closed {
                let e = pending.entry(id).or_insert(at);
                *e = (*e).max(at);
            }
            // A live allocation is stamped for now, which covers its closes.
            live.iter()
                .map(|id| (id.clone(), now))
                .chain(
                    pending
                        .iter()
                        .filter(|(id, _)| !live.contains(*id))
                        .map(|(id, at)| (id.clone(), *at)),
                )
                .collect()
        };
        for (id, since) in wanted {
            let c = self
                .claims
                .get(&ObjectRef::new(&id).within(&self.namespace));
            let Some(c) = c.filter(|c| claims::edge_behind(c, &self.namespace, since)) else {
                // No claim of the API's, or it already records the activity.
                self.settle(&id, since);
                continue;
            };
            if claims::edge_due(&c, &self.namespace, since, now) && self.write(&c, now).await {
                self.settle(&id, since);
            }
        }
    }

    /// Forget the close of `id` at `since`, unless a later one came in.
    fn settle(&self, id: &str, since: OffsetDateTime) {
        let mut pending = self.closed();
        if pending.get(id).is_some_and(|at| *at <= since) {
            pending.remove(id);
        }
    }

    /// True when the stamp is written or the claim is gone.
    async fn write(&self, c: &BuildKitClaim, now: OffsetDateTime) -> bool {
        let (Some(name), Some(uid)) = (c.metadata.name.as_deref(), c.metadata.uid.as_deref())
        else {
            return true;
        };
        let set = BTreeMap::from([(claims::ANN_EDGE_SEEN_AT.to_owned(), fmt_ts(now))]);
        match self
            .backend
            .annotate_claim(&self.namespace, name, uid, set)
            .await
        {
            Write::Done => {
                debug!(claim = name, "recorded edge activity");
                true
            }
            Write::Gone => true,
            w => {
                warn!(claim = name, outcome = ?w, "recording edge activity on a claim");
                false
            }
        }
    }
}
