//! Election tests against a fake Lease API and paused tokio time.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use k8s_openapi::api::coordination::v1::LeaseSpec;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::{ApiError, Elector, LeaderState, LeaseApi, LeaseSnapshot, TokioClock};
use crate::config::ElectionTiming;

#[derive(Default)]
struct Cluster {
    lease: Option<LeaseSnapshot>,
    rv: u64,
    watchers: Vec<mpsc::UnboundedSender<Option<LeaseSnapshot>>>,
}

/// One replica's view of the shared fake cluster, with fault injection.
#[derive(Clone, Default)]
struct Fake {
    cluster: Arc<Mutex<Cluster>>,
    /// Every call hangs forever while set.
    block: Arc<AtomicBool>,
    /// The next `n` replace calls hang.
    hang_replaces: Arc<AtomicUsize>,
    replaces: Arc<AtomicUsize>,
    /// GET hangs while set.
    hang_gets: Arc<AtomicBool>,
    gets: Arc<AtomicUsize>,
}

impl Fake {
    fn peer(&self) -> Self {
        Self {
            cluster: Arc::clone(&self.cluster),
            ..Self::default()
        }
    }

    fn seed(&self, spec: LeaseSpec) {
        let mut c = self.cluster.lock().unwrap();
        c.rv += 1;
        c.lease = Some(LeaseSnapshot {
            resource_version: c.rv.to_string(),
            spec,
        });
    }

    /// Overwrite the Lease with an arbitrary resourceVersion and notify watchers.
    fn force(&self, rv: &str, spec: LeaseSpec) {
        let mut c = self.cluster.lock().unwrap();
        let snap = LeaseSnapshot {
            resource_version: rv.into(),
            spec,
        };
        c.lease = Some(snap.clone());
        c.watchers.retain(|w| w.send(Some(snap.clone())).is_ok());
    }

    fn holder(&self) -> Option<String> {
        let c = self.cluster.lock().unwrap();
        c.lease
            .as_ref()
            .and_then(|l| l.spec.holder_identity.clone())
    }

    fn transitions(&self) -> Option<i32> {
        let c = self.cluster.lock().unwrap();
        c.lease.as_ref().and_then(|l| l.spec.lease_transitions)
    }

    async fn gate(&self) {
        if self.block.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
    }

    /// Delete the Lease (`kubectl delete lease`) and notify watchers.
    fn delete(&self) {
        let mut c = self.cluster.lock().unwrap();
        c.lease = None;
        c.watchers.retain(|w| w.send(None).is_ok());
    }

    fn write(&self, expect: Option<&str>, spec: LeaseSpec) -> Result<LeaseSnapshot, ApiError> {
        let mut c = self.cluster.lock().unwrap();
        let cur = c.lease.as_ref().map(|l| l.resource_version.as_str());
        if expect.is_some() && cur.is_none() {
            return Err(ApiError::NotFound);
        }
        if cur != expect {
            return Err(ApiError::Conflict);
        }
        c.rv += 1;
        let snap = LeaseSnapshot {
            resource_version: c.rv.to_string(),
            spec,
        };
        c.lease = Some(snap.clone());
        c.watchers.retain(|w| w.send(Some(snap.clone())).is_ok());
        Ok(snap)
    }
}

impl LeaseApi for Fake {
    async fn get(&self) -> Result<Option<LeaseSnapshot>, ApiError> {
        self.gate().await;
        self.gets.fetch_add(1, Ordering::SeqCst);
        if self.hang_gets.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        Ok(self.cluster.lock().unwrap().lease.clone())
    }

    async fn create(&self, spec: LeaseSpec) -> Result<LeaseSnapshot, ApiError> {
        self.gate().await;
        self.write(None, spec)
    }

    async fn replace(
        &self,
        prev: &LeaseSnapshot,
        spec: LeaseSpec,
    ) -> Result<LeaseSnapshot, ApiError> {
        self.gate().await;
        self.replaces.fetch_add(1, Ordering::SeqCst);
        let hang = self
            .hang_replaces
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if hang {
            std::future::pending::<()>().await;
        }
        self.write(Some(&prev.resource_version), spec)
    }

    fn watch(&self) -> BoxStream<'static, Option<LeaseSnapshot>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut c = self.cluster.lock().unwrap();
        if let Some(l) = &c.lease {
            let _ = tx.send(Some(l.clone()));
        }
        c.watchers.push(tx);
        futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (v, rx)) })
            .boxed()
    }
}

fn timing() -> ElectionTiming {
    ElectionTiming {
        lease_duration: Duration::from_secs(15),
        renew_interval: Duration::from_secs(2),
        fence_margin: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
    }
}

struct Replica {
    rx: watch::Receiver<LeaderState>,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

fn start(id: &str, api: &Fake) -> Replica {
    let (e, rx) = Elector::new(Arc::new(api.clone()), TokioClock, id.into(), timing());
    let stop = CancellationToken::new();
    let task = tokio::spawn(e.run(stop.clone()));
    Replica { rx, stop, task }
}

fn leads(r: &Replica) -> bool {
    r.rx.borrow().is_leader()
}

async fn secs(n: u64) {
    tokio::time::sleep(Duration::from_secs(n)).await;
}

fn spec(holder: Option<&str>, renewed_long_ago: bool) -> LeaseSpec {
    LeaseSpec {
        holder_identity: holder.map(String::from),
        lease_duration_seconds: Some(15),
        lease_transitions: Some(4),
        renew_time: Some(MicroTime(if renewed_long_ago {
            k8s_openapi::jiff::Timestamp::UNIX_EPOCH
        } else {
            k8s_openapi::jiff::Timestamp::now()
        })),
        ..LeaseSpec::default()
    }
}

#[tokio::test(start_paused = true)]
async fn acquires_an_absent_lease() {
    let f = Fake::default();
    let a = start("a", &f);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(*a.rx.borrow(), LeaderState::Leader { term: 1 });
    assert_eq!(f.holder().as_deref(), Some("a"));
    assert_eq!(f.transitions(), Some(0));
}

#[tokio::test(start_paused = true)]
async fn renews_every_two_seconds() {
    let f = Fake::default();
    let a = start("a", &f);
    secs(11).await;
    let n = f.replaces.load(Ordering::SeqCst);
    assert!((5..=6).contains(&n), "renews: {n}");
    assert!(leads(&a));
}

#[tokio::test(start_paused = true)]
async fn slow_renew_is_retried_without_fencing() {
    let f = Fake::default();
    let a = start("a", &f);
    secs(1).await;
    f.hang_replaces.store(1, Ordering::SeqCst);
    // The hung renew times out at 5 s after its start; the retry follows at once.
    secs(20).await;
    assert_eq!(
        *a.rx.borrow(),
        LeaderState::Leader { term: 1 },
        "never fenced"
    );
    assert_eq!(f.holder().as_deref(), Some("a"));
}

#[tokio::test(start_paused = true)]
async fn self_fences_when_renews_stop_landing() {
    let f = Fake::default();
    let a = start("a", &f);
    secs(1).await;
    f.block.store(true, Ordering::SeqCst);
    secs(9).await; // last good renew at 2 s or earlier, fence at 12 s or earlier
    secs(4).await;
    assert!(!leads(&a), "fenced after renew deadline");
    assert_eq!(f.holder().as_deref(), Some("a"), "lease untouched");
}

#[tokio::test(start_paused = true)]
async fn empty_holder_is_acquired_at_once() {
    let f = Fake::default();
    f.seed(spec(None, false));
    let a = start("a", &f);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(leads(&a));
    assert_eq!(f.transitions(), Some(5));
}

#[tokio::test(start_paused = true)]
async fn release_on_shutdown_clears_the_holder() {
    let f = Fake::default();
    let a = start("a", &f);
    secs(3).await;
    assert!(leads(&a));
    a.stop.cancel();
    a.task.await.unwrap();
    assert!(!a.rx.borrow().is_leader());
    assert_eq!(f.holder(), None);
    let b = start("b", &f);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(leads(&b), "released lease is taken immediately");
}

#[tokio::test(start_paused = true)]
async fn expiry_uses_observed_at_not_the_holders_renew_time() {
    let f = Fake::default();
    // renewTime is decades old, but nobody has seen it change for less than 15 s.
    f.seed(spec(Some("other"), true));
    let a = start("a", &f);
    secs(14).await;
    assert!(!leads(&a), "not expired locally yet");
    secs(2).await;
    assert!(leads(&a), "expired 15 s after first observation");
    assert_eq!(f.transitions(), Some(5));
}

#[tokio::test(start_paused = true)]
async fn a_live_holder_resets_observed_at() {
    let f = Fake::default();
    f.seed(spec(Some("other"), false));
    let a = start("a", &f);
    for _ in 0..10 {
        secs(5).await;
        // the other holder renews, and renewTime and resourceVersion change
        let snap = f.cluster.lock().unwrap().lease.clone().unwrap();
        let mut s = snap.spec.clone();
        s.renew_time = Some(MicroTime(k8s_openapi::jiff::Timestamp::now()));
        f.write(Some(&snap.resource_version), s).unwrap();
        assert!(!leads(&a));
    }
}

#[tokio::test(start_paused = true)]
async fn two_contenders_never_both_lead_and_failover_is_bounded() {
    let fa = Fake::default();
    let fb = fa.peer();
    let a = start("a", &fa);
    let b = start("b", &fb);
    let mut killed_at = None;
    let mut took_over = None;
    for tick in 0..2400u32 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!(leads(&a) && leads(&b)), "two leaders at tick {tick}");
        if tick == 400 {
            let (leader, other) = if leads(&a) { (&fa, &b) } else { (&fb, &a) };
            leader.block.store(true, Ordering::SeqCst);
            killed_at = Some((tokio::time::Instant::now(), leads(other)));
        }
        if let Some((t0, _)) = killed_at {
            let follower_leads = if fa.block.load(Ordering::SeqCst) {
                leads(&b)
            } else {
                leads(&a)
            };
            if follower_leads && took_over.is_none() {
                took_over = Some(t0.elapsed());
            }
        }
    }
    let took = took_over.expect("the survivor took over");
    assert!(took <= Duration::from_secs(16), "failover took {took:?}");
}

#[tokio::test(start_paused = true)]
async fn resource_versions_going_backwards_do_not_stall_or_spin() {
    let f = Fake::default();
    f.force("900", spec(Some("other"), false));
    let a = start("a", &f);
    secs(3).await;
    assert!(!leads(&a));
    // etcd restored from a snapshot, so the holder is gone and the version is lower.
    f.force("7", spec(None, false));
    secs(1).await;
    assert!(leads(&a), "acquired with the lower resourceVersion");
    assert!(
        f.gets.load(Ordering::SeqCst) <= 3,
        "no GET loop: {}",
        f.gets.load(Ordering::SeqCst)
    );
    // And a renew keeps working against whatever version the API reports.
    let held = f.cluster.lock().unwrap().lease.clone().unwrap().spec;
    f.force("2", held);
    secs(10).await;
    assert!(leads(&a), "renew recovers via 409 then GET");
    assert!(f.gets.load(Ordering::SeqCst) < 10, "bounded API traffic");
}

#[tokio::test(start_paused = true)]
async fn renew_conflict_near_the_deadline_cannot_extend_leadership() {
    let f = Fake::default();
    let a = start("a", &f);
    secs(1).await;
    assert!(leads(&a));
    // Every renew now gets a 409 (version moved, holder still a) and GET hangs.
    f.hang_gets.store(true, Ordering::SeqCst);
    let snap = f.cluster.lock().unwrap().lease.clone().unwrap();
    f.force("5000", snap.spec);
    secs(8).await; // t is about 9 s, last good renew at 0 s, deadline 10 s
    assert!(leads(&a));
    secs(2).await; // t is about 11 s
    assert!(
        !leads(&a),
        "fenced at the deadline, not requestTimeout later"
    );
}

#[tokio::test(start_paused = true)]
async fn expiry_honours_a_longer_holder_lease_duration() {
    let f = Fake::default();
    let mut s = spec(Some("other"), true);
    s.lease_duration_seconds = Some(30);
    f.seed(s);
    let a = start("a", &f);
    secs(20).await;
    assert!(!leads(&a), "holder promised 30 s");
    secs(11).await;
    assert!(leads(&a));
}

#[tokio::test(start_paused = true)]
async fn a_lease_deleted_while_leading_is_created_again() {
    let f = Fake::default();
    let a = start("a", &f);
    secs(1).await;
    assert_eq!(*a.rx.borrow(), LeaderState::Leader { term: 1 });
    // The leader ignores watch events, so only its next renew can notice.
    f.delete();
    secs(3).await;
    assert_eq!(
        *a.rx.borrow(),
        LeaderState::Leader { term: 2 },
        "the deleted Lease was created again by the only replica"
    );
    assert_eq!(f.holder().as_deref(), Some("a"));
    secs(20).await;
    assert!(leads(&a), "and it keeps renewing the new Lease");
}

#[tokio::test(start_paused = true)]
async fn a_follower_that_last_held_a_deleted_lease_creates_it() {
    let f = Fake::default();
    let a = start("a", &f);
    secs(1).await;
    assert!(leads(&a));
    // Renews stop landing until the leader fences; the Lease is deleted
    // meanwhile, and the watch event is dropped because a still leads.
    f.block.store(true, Ordering::SeqCst);
    f.delete();
    secs(12).await;
    assert!(!leads(&a), "fenced");
    f.block.store(false, Ordering::SeqCst);
    secs(10).await;
    assert!(leads(&a), "the follower re-read the Lease and created it");
    assert_eq!(f.holder().as_deref(), Some("a"));
}

#[tokio::test(start_paused = true)]
async fn release_after_a_lost_renew_response_still_clears_the_holder() {
    let f = Fake::default();
    let a = start("a", &f);
    secs(1).await;
    assert!(leads(&a));
    // A renew committed but its response was lost: the version moved, holder still a.
    let snap = f.cluster.lock().unwrap().lease.clone().unwrap();
    f.force("5000", snap.spec);
    a.stop.cancel();
    a.task.await.unwrap();
    assert_eq!(f.holder(), None, "released after re-reading the Lease");
}

/// A follower that sees the Lease deleted while another replica held it waits
/// out that holder's lease before it creates a new one, so the two never lead
/// at once.
#[tokio::test(start_paused = true)]
async fn a_lease_deleted_under_a_live_holder_is_not_taken_at_once() {
    let fa = Fake::default();
    let fb = fa.peer();
    let a = start("a", &fa);
    secs(1).await;
    assert!(leads(&a));
    let b = start("b", &fb);
    secs(1).await;
    assert!(!leads(&b));
    // a stops renewing (it still believes it leads until it fences), and the
    // Lease is deleted.
    fa.block.store(true, Ordering::SeqCst);
    fa.delete();
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!(leads(&a) && leads(&b)), "two leaders");
    }
    secs(10).await;
    assert!(leads(&b), "b took over once a's lease ran out");
}

/// A holder-written `leaseDurationSeconds` counts for at most twice ours.
#[tokio::test(start_paused = true)]
async fn a_huge_holder_lease_duration_is_capped() {
    let f = Fake::default();
    let mut s = spec(Some("other"), true);
    s.lease_duration_seconds = Some(i32::MAX);
    f.seed(s);
    let a = start("a", &f);
    secs(29).await;
    assert!(!leads(&a), "twice our 15 s");
    secs(2).await;
    assert!(leads(&a));
}
