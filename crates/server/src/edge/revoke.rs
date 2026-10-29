//! Live-connection index for revocation: connections keyed by
//! pod uid, cancelled from Pod events within the event's delivery.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

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

/// Open proxied connections per pool namespace, capped at `max` each. The
/// namespace comes from the verified allocation certificate.
pub struct Namespaces {
    max: usize,
    open: Mutex<HashMap<String, usize>>,
}

/// One connection's share of its namespace's cap; released on drop.
pub struct NamespaceSlot {
    namespaces: Arc<Namespaces>,
    ns: String,
}

impl Drop for NamespaceSlot {
    fn drop(&mut self) {
        let mut open = self.namespaces.lock();
        if let Some(n) = open.get_mut(&self.ns) {
            *n -= 1;
            if *n == 0 {
                open.remove(&self.ns);
            }
        }
    }
}

impl Namespaces {
    /// At most `max` connections per namespace; 0 disables the cap.
    #[must_use]
    pub fn new(max: usize) -> Self {
        Self {
            max,
            open: Mutex::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, usize>> {
        self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A slot for namespace `ns`, or `None` when it already holds `max`.
    pub fn try_take(self: &Arc<Self>, ns: &str) -> Option<NamespaceSlot> {
        let mut open = self.lock();
        if self.max != 0 && open.get(ns).copied().unwrap_or(0) >= self.max {
            return None;
        }
        *open.entry(ns.to_owned()).or_insert(0) += 1;
        Some(NamespaceSlot {
            namespaces: Arc::clone(self),
            ns: ns.to_owned(),
        })
    }
}

/// Open proxied connections, by pod uid.
pub struct Registry {
    next: AtomicU64,
    live: Mutex<Index>,
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
        if let Some(conns) = live.get_mut(&self.uid) {
            conns.remove(&self.key);
            if conns.is_empty() {
                live.remove(&self.uid);
            }
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
            max_per_pod,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Index> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
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
