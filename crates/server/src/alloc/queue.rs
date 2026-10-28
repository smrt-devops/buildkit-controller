//! The per-replica FIFO per pool and its `status.waiters`
//! entry.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use buildkit_controller_api::v1alpha1::BuildKitPool;
use kube::runtime::reflector::{ObjectRef, Store};
use tokio::sync::watch;
use tokio::time::Instant;
use tracing::warn;

use super::backend::Backend;
use super::metrics::AllocMetrics;
use super::pods::fmt_ts;

/// `(namespace, pool)`.
pub type PoolKey = (String, String);

/// At most this many waiters per pool per replica.
pub const MAX_WAITERS: usize = 1000;
/// Minimum spacing of `status.waiters` writes.
pub const APPLY_MIN_INTERVAL: Duration = Duration::from_secs(1);
/// Heartbeat while the queue is non-empty.
pub const HEARTBEAT: Duration = Duration::from_secs(30);

struct PoolQueue {
    order: VecDeque<u64>,
    next: u64,
    /// Bumped on every wake and on every head change.
    wake: watch::Sender<u64>,
}

impl PoolQueue {
    fn bump(&self) {
        self.wake.send_modify(|v| *v = v.wrapping_add(1));
    }
}

/// All queues of this replica.
pub struct Queues {
    inner: Mutex<HashMap<PoolKey, PoolQueue>>,
    cap: usize,
    metrics: AllocMetrics,
}

impl Queues {
    /// Queues of at most `cap` waiters each.
    #[must_use]
    pub fn new(cap: usize, metrics: AllocMetrics) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            cap,
            metrics,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<PoolKey, PoolQueue>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn entry<'a>(map: &'a mut HashMap<PoolKey, PoolQueue>, key: &PoolKey) -> &'a mut PoolQueue {
        map.entry(key.clone()).or_insert_with(|| PoolQueue {
            order: VecDeque::new(),
            next: 0,
            wake: watch::channel(0).0,
        })
    }

    /// The wake version; snapshot it before an attempt so a wake during the
    /// attempt is not lost.
    pub fn version(&self, key: &PoolKey) -> u64 {
        *Self::entry(&mut self.lock(), key).wake.borrow()
    }

    /// Join the tail; `None` when the queue is full.
    pub fn enqueue(self: &Arc<Self>, key: &PoolKey) -> Option<Ticket> {
        let mut map = self.lock();
        let q = Self::entry(&mut map, key);
        if q.order.len() >= self.cap {
            return None;
        }
        let id = q.next;
        q.next += 1;
        q.order.push_back(id);
        let rx = q.wake.subscribe();
        self.metrics.depth(&key.0, &key.1, q.order.len());
        Some(Ticket {
            queues: Arc::clone(self),
            key: key.clone(),
            id,
            rx,
        })
    }

    /// Whether anyone waits in the queue of `key`.
    #[must_use]
    pub fn is_waiting(&self, key: &PoolKey) -> bool {
        self.lock().get(key).is_some_and(|q| !q.order.is_empty())
    }

    /// Let the head of `key` retry.
    pub fn wake(&self, key: &PoolKey) {
        if let Some(q) = self.lock().get(key) {
            q.bump();
        }
    }

    /// Let every head retry (pod store resync).
    pub fn wake_all(&self) {
        self.lock().values().for_each(PoolQueue::bump);
    }

    /// Drop the queue of `key` if nobody waits in it (its pool is gone).
    pub fn forget_empty(&self, key: &PoolKey) {
        let mut map = self.lock();
        if map.get(key).is_some_and(|q| q.order.is_empty()) {
            map.remove(key);
        }
    }

    /// Current lengths, including empty queues.
    #[must_use]
    pub fn lengths(&self) -> Vec<(PoolKey, usize)> {
        self.lock()
            .iter()
            .map(|(k, q)| (k.clone(), q.order.len()))
            .collect()
    }
}

/// A place in a queue; leaving (drop) wakes the next head.
pub struct Ticket {
    queues: Arc<Queues>,
    key: PoolKey,
    id: u64,
    rx: watch::Receiver<u64>,
}

impl Ticket {
    /// First in line.
    #[must_use]
    pub fn is_head(&self) -> bool {
        self.queues
            .lock()
            .get(&self.key)
            .and_then(|q| q.order.front().copied())
            == Some(self.id)
    }

    /// Mark the current version seen and return it.
    pub fn seen(&mut self) -> u64 {
        *self.rx.borrow_and_update()
    }

    /// Resolves on the next wake or head change.
    pub async fn changed(&mut self) {
        if self.rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut map = self.queues.lock();
        if let Some(q) = map.get_mut(&self.key) {
            let was_head = q.order.front() == Some(&self.id);
            q.order.retain(|i| *i != self.id);
            if was_head {
                q.bump();
            }
            self.queues
                .metrics
                .depth(&self.key.0, &self.key.1, q.order.len());
        }
    }
}

/// Writes this replica's `status.waiters` entries on change (at most once a
/// second), every 30 s while non-zero, and `0` once a queue empties.
pub struct WaitersWriter {
    replica: String,
    pools: Store<BuildKitPool>,
    last: HashMap<PoolKey, (usize, Instant)>,
    /// Entries that may hold a non-zero count: a non-zero apply was sent
    /// (whatever its result, since a timed-out PATCH can still land) and no
    /// apply of 0 has succeeded since.
    unsettled: HashSet<PoolKey>,
}

impl WaitersWriter {
    /// For replica `POD_NAME`. Only pools in `pools` get an entry written.
    #[must_use]
    pub fn new(replica: String, pools: Store<BuildKitPool>) -> Self {
        Self {
            replica,
            pools,
            last: HashMap::new(),
            unsettled: HashSet::new(),
        }
    }

    /// Sets every entry this writer may have left non-zero back to 0
    /// (shutdown), including one whose apply failed, because a PATCH that
    /// timed out can still land. The applies run concurrently, each bounded by
    /// the backend's request timeout.
    pub async fn clear(&mut self, backend: &dyn Backend) {
        let ts = fmt_ts(time::OffsetDateTime::now_utc());
        let keys: Vec<PoolKey> = self.unsettled.iter().cloned().collect();
        let applies = keys
            .iter()
            .map(|(ns, pool)| backend.apply_waiters(ns, pool, &self.replica, 0, &ts));
        let results = futures::future::join_all(applies).await;
        let now = Instant::now();
        for (key, result) in keys.into_iter().zip(results) {
            match result {
                Ok(()) => {
                    self.unsettled.remove(&key);
                    self.last.insert(key, (0, now));
                }
                Err(error) => {
                    warn!(namespace = %key.0, pool = %key.1, error, "clearing status.waiters");
                }
            }
        }
    }

    /// One pass; call about once a second. The due applies run concurrently,
    /// so one slow pool does not hold back another pool's heartbeat.
    pub async fn tick(&mut self, queues: &Queues, backend: &dyn Backend) {
        let now = Instant::now();
        let ts = fmt_ts(time::OffsetDateTime::now_utc());
        let mut due = Vec::new();
        for (key, len) in queues.lengths() {
            // A deleted pool has no status to write; its apply would 404 every tick.
            if self
                .pools
                .get(&ObjectRef::new(&key.1).within(&key.0))
                .is_none()
            {
                self.last.remove(&key);
                self.unsettled.remove(&key);
                queues.forget_empty(&key);
                continue;
            }
            let is_due = match self.last.get(&key) {
                None => len > 0,
                Some(&(n, at)) => {
                    let since = now.saturating_duration_since(at);
                    (n != len && since >= APPLY_MIN_INTERVAL) || (len > 0 && since >= HEARTBEAT)
                }
            };
            if is_due {
                due.push((key, len));
            }
        }
        // Recorded before the apply: if it times out, the PATCH may still land.
        self.unsettled.extend(
            due.iter()
                .filter(|(_, len)| *len > 0)
                .map(|(key, _)| key.clone()),
        );
        let applies = due.iter().map(|((ns, pool), len)| {
            let count = i32::try_from(*len).unwrap_or(i32::MAX);
            backend.apply_waiters(ns, pool, &self.replica, count, &ts)
        });
        let results = futures::future::join_all(applies).await;
        for ((key, len), result) in due.into_iter().zip(results) {
            match result {
                Ok(()) => {
                    if len == 0 {
                        self.unsettled.remove(&key);
                    }
                    self.last.insert(key, (len, now));
                }
                Err(error) => {
                    warn!(namespace = %key.0, pool = %key.1, error, "applying status.waiters");
                }
            }
        }
    }
}
