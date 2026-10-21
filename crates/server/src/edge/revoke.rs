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

/// Open proxied connections, by pod uid.
#[derive(Default)]
pub struct Registry {
    next: AtomicU64,
    live: Mutex<Index>,
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
    fn lock(&self) -> std::sync::MutexGuard<'_, Index> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Indexes a connection; the receiver fires with the reason it must close.
    pub fn register(
        self: &Arc<Self>,
        id: &AllocationId,
    ) -> (Registration, oneshot::Receiver<Revocation>) {
        let (tx, rx) = oneshot::channel();
        let key = self.next.fetch_add(1, Ordering::Relaxed);
        let uid = id.pod_uid.as_str().to_owned();
        self.lock().entry(uid.clone()).or_default().insert(
            key,
            Live {
                alloc: id.alloc_id.clone(),
                tx,
            },
        );
        let reg = Registration {
            registry: Arc::clone(self),
            uid,
            key,
        };
        (reg, rx)
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
