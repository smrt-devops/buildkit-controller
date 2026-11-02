//! Live-connection index for revocation: connections keyed by
//! pod uid, cancelled from Pod events within the event's delivery. It also
//! tells the claim stamper which allocations hold or closed a connection.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use time::OffsetDateTime;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::metrics::Revocation;
use super::pods::{PodView, revocation};
use crate::pki::{AllocId, AllocationId};
use crate::runtime::stores::{PodEvent, PodSubscription};

struct Live {
    alloc: AllocId,
    tx: oneshot::Sender<Revocation>,
}

type Index = HashMap<String, HashMap<u64, Live>>;

/// Most proxied connections one allocation (one worker pod) may hold on a
/// replica. buildx opens one to five edge connections over a whole allocation
/// (two for a plain build, more for multi-platform and cache runs), so 64
/// leaves room for many concurrent builds on one allocation while stopping a
/// single cert from taking the replica's whole `edge.maxConnections`.
pub const MAX_CONNECTIONS_PER_ALLOCATION: usize = 64;

/// The share of `edge.maxConnections` one pool namespace may hold on a
/// replica: a quarter, so one tenant's allocations together cannot fill it.
#[must_use]
pub fn namespace_cap(max_connections: usize) -> usize {
    (max_connections / 4).max(1)
}

/// Open proxied connections, by pod uid.
pub struct Registry {
    next: AtomicU64,
    live: Mutex<Index>,
    /// Allocations whose connections closed since [`Registry::activity`] last
    /// ran, with when.
    closed: Mutex<HashMap<String, OffsetDateTime>>,
    /// Cap per pod uid; 0 disables it.
    max_per_pod: usize,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new(MAX_CONNECTIONS_PER_ALLOCATION)
    }
}

/// Removes its connection from the index when dropped.
pub struct Registration {
    registry: Arc<Registry>,
    uid: String,
    key: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut live = self.registry.lock();
        let Some(conns) = live.get_mut(&self.uid) else {
            return;
        };
        // A connection revoked is already out of the index; its allocation ended.
        let gone = conns.remove(&self.key);
        if conns.is_empty() {
            live.remove(&self.uid);
        }
        drop(live);
        if let Some(l) = gone {
            self.registry
                .closed_lock()
                .insert(l.alloc.as_str().to_owned(), OffsetDateTime::now_utc());
        }
    }
}

impl Registry {
    /// At most `max_per_pod` connections per pod uid; 0 disables the cap.
    #[must_use]
    pub fn new(max_per_pod: usize) -> Self {
        Self {
            next: AtomicU64::new(0),
            live: Mutex::default(),
            closed: Mutex::default(),
            max_per_pod,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Index> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn closed_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, OffsetDateTime>> {
        self.closed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The allocation ids holding a connection now, and the ones whose
    /// connections closed since the last call, with when.
    pub fn activity(&self) -> (HashSet<String>, HashMap<String, OffsetDateTime>) {
        let live = self
            .lock()
            .values()
            .flat_map(HashMap::values)
            .map(|l| l.alloc.as_str().to_owned())
            .collect();
        (live, std::mem::take(&mut *self.closed_lock()))
    }

    /// Indexes a connection; the receiver fires with the reason it must close.
    /// `None` when the pod already holds `max_per_pod` connections.
    pub fn register(
        self: &Arc<Self>,
        id: &AllocationId,
    ) -> Option<(Registration, oneshot::Receiver<Revocation>)> {
        let uid = id.pod_uid.as_str().to_owned();
        let mut live = self.lock();
        let conns = live.entry(uid.clone()).or_default();
        if self.max_per_pod != 0 && conns.len() >= self.max_per_pod {
            return None;
        }
        let (tx, rx) = oneshot::channel();
        let key = self.next.fetch_add(1, Ordering::Relaxed);
        conns.insert(
            key,
            Live {
                alloc: id.alloc_id.clone(),
                tx,
            },
        );
        drop(live);
        let reg = Registration {
            registry: Arc::clone(self),
            uid,
            key,
        };
        Some((reg, rx))
    }

    /// Number of indexed connections (tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().values().map(HashMap::len).sum()
    }

    /// Re-checks the connections on pod `uid` against `pod` (its latest state,
    /// `None` when deleted) and fires the ones that must close.
    fn recheck(&self, uid: &str, pod: Option<&k8s_openapi::api::core::v1::Pod>) {
        let mut live = self.lock();
        let Some(conns) = live.get_mut(uid) else {
            return;
        };
        let cut: Vec<(u64, Revocation)> = conns
            .iter()
            .filter_map(|(k, l)| revocation(pod, &l.alloc).map(|r| (*k, r)))
            .collect();
        for (k, reason) in cut {
            if let Some(l) = conns.remove(&k) {
                debug!(
                    pod_uid = uid,
                    reason = reason.label(),
                    "revoking edge connection"
                );
                let _ = l.tx.send(reason);
            }
        }
        if conns.is_empty() {
            live.remove(uid);
        }
    }

    /// Every indexed pod uid.
    fn uids(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    /// Follows Pod events until `stop` or the runtime closes the broadcast.
    pub async fn follow(&self, view: PodView, mut sub: PodSubscription, stop: CancellationToken) {
        loop {
            let ev = tokio::select! {
                () = stop.cancelled() => return,
                ev = sub.recv() => ev,
            };
            match ev {
                None => return,
                Some(PodEvent::Delete(p)) => {
                    if let Some(uid) = p.metadata.uid.as_deref() {
                        self.recheck(uid, None);
                    }
                }
                Some(PodEvent::Apply(p)) => {
                    if let Some(uid) = p.metadata.uid.as_deref() {
                        self.recheck(uid, Some(&p));
                    }
                }
                Some(PodEvent::Resync) => {
                    for uid in self.uids() {
                        let pod = view
                            .pods
                            .find(|p| p.metadata.uid.as_deref() == Some(uid.as_str()));
                        self.recheck(&uid, pod.as_deref());
                    }
                }
            }
        }
    }
}
