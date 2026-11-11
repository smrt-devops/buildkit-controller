//! The leader's view of the `spec.network.egress.allowHosts` names: their last
//! known addresses and which names failed to resolve the last time.
//!
//! A lookup runs in a task of its own, at most [`MAX_LOOKUPS`] at once, and the
//! caller waits for it at most [`RESOLVE_TIMEOUT`]. The system resolver is
//! blocking (`getaddrinfo` on Tokio's blocking pool), so a lookup that outlives
//! its wait keeps its permit until it returns: a hung DNS server never ties up
//! more than [`MAX_LOOKUPS`] blocking threads, and never a runtime thread.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use buildkit_controller_api::v1alpha1::{BuildKitPool, parse_allow_host};
use futures::future::{BoxFuture, join_all};
use tokio::sync::Semaphore;
use tracing::{debug, warn};

use crate::render::HostAddrs;

/// How often the leader resolves every name of every pool.
pub const RESOLVE_EVERY: Duration = Duration::from_secs(30);
/// Longest wait for one name, permit included.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);
/// Lookups in flight at once.
pub const MAX_LOOKUPS: usize = 16;

/// Resolves a DNS name to its A and AAAA addresses.
pub trait Resolve: Send + Sync + 'static {
    /// The addresses of `name`. The future must not block the thread that polls it.
    fn lookup(&self, name: &str) -> BoxFuture<'static, std::io::Result<Vec<IpAddr>>>;
}

/// The process resolver (`getaddrinfo`, A and AAAA), run on Tokio's blocking pool.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemResolver;

impl Resolve for SystemResolver {
    fn lookup(&self, name: &str) -> BoxFuture<'static, std::io::Result<Vec<IpAddr>>> {
        let name = name.to_owned();
        Box::pin(async move {
            Ok(tokio::net::lookup_host((name.as_str(), 0))
                .await?
                .map(|a| a.ip())
                .collect())
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Known {
    /// Last addresses the name resolved to, canonical, sorted, deduplicated.
    addrs: Vec<IpAddr>,
    /// Why the latest lookup failed; `None` when it succeeded.
    failed: Option<String>,
}

/// The `allowHosts` view of one pool for one pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostView {
    /// Last known addresses of each name that has any.
    pub addrs: HostAddrs,
    /// Each name whose latest lookup failed, with why.
    pub unresolved: BTreeMap<String, String>,
}

/// `(namespace, name)` of a pool.
pub type PoolKey = (String, String);

/// Last known addresses of every name the leader resolves in one leadership
/// term. A term builds its own, so it starts with no addresses.
pub struct Hosts {
    resolver: Arc<dyn Resolve>,
    permits: Arc<Semaphore>,
    known: Mutex<BTreeMap<String, Known>>,
    /// The view each pool's latest pass used.
    passed: Mutex<BTreeMap<PoolKey, HostView>>,
}

impl Hosts {
    /// A cache that resolves through `resolver`.
    #[must_use]
    pub fn new(resolver: Arc<dyn Resolve>) -> Self {
        Self {
            resolver,
            permits: Arc::new(Semaphore::new(MAX_LOOKUPS)),
            known: Mutex::new(BTreeMap::new()),
            passed: Mutex::new(BTreeMap::new()),
        }
    }

    fn known(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Known>> {
        lock(&self.known)
    }

    /// Resolves each of `names` at once and records the result. A failed lookup
    /// keeps the name's last known addresses.
    pub async fn refresh(&self, names: &BTreeSet<String>) {
        let results = join_all(
            names
                .iter()
                .map(|n| async move { (n, self.lookup(n).await) }),
        )
        .await;
        let mut known = self.known();
        for (name, res) in results {
            let old = known.get(name).cloned().unwrap_or_default();
            let new = match res {
                Ok(addrs) => Known {
                    addrs,
                    failed: None,
                },
                Err(why) => {
                    warn!(host = %name, error = %why, "allowHosts name did not resolve; keeping its last known addresses");
                    Known {
                        addrs: old.addrs.clone(),
                        failed: Some(why),
                    }
                }
            };
            if new.addrs != old.addrs || new.failed.is_some() != old.failed.is_some() {
                debug!(host = %name, addrs = ?new.addrs, "allowHosts addresses changed");
            }
            known.insert(name.clone(), new);
        }
    }

    /// One lookup, bounded by [`RESOLVE_TIMEOUT`] and [`MAX_LOOKUPS`].
    async fn lookup(&self, name: &str) -> Result<Vec<IpAddr>, String> {
        let timeout = || format!("timed out after {}s", RESOLVE_TIMEOUT.as_secs());
        let deadline = tokio::time::Instant::now() + RESOLVE_TIMEOUT;
        let permit = tokio::time::timeout_at(deadline, Arc::clone(&self.permits).acquire_owned())
            .await
            .map_err(|_| timeout())?
            .map_err(|e| e.to_string())?;
        // Absolute: the controller pod's search path never applies.
        let fut = self.resolver.lookup(&format!("{name}."));
        // The task keeps the permit until the lookup returns, even past the wait.
        let task = tokio::spawn(async move {
            let res = fut.await;
            drop(permit);
            res
        });
        let mut addrs: Vec<IpAddr> = tokio::time::timeout_at(deadline, task)
            .await
            .map_err(|_| timeout())?
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|ip| ip.to_canonical())
            .collect();
        addrs.sort_unstable();
        addrs.dedup();
        if addrs.is_empty() {
            return Err("no addresses".into());
        }
        Ok(addrs)
    }

    /// The view of `names` as last resolved.
    #[must_use]
    pub fn view(&self, names: &BTreeSet<String>) -> HostView {
        let known = self.known();
        let mut v = HostView::default();
        for n in names {
            let Some(k) = known.get(n) else { continue };
            if !k.addrs.is_empty() {
                v.addrs.insert(n.clone(), k.addrs.clone());
            }
            if let Some(why) = &k.failed {
                v.unresolved.insert(n.clone(), why.clone());
            }
        }
        v
    }

    /// Records the view the pass of `pool` used.
    pub fn passed(&self, pool: &PoolKey, view: HostView) {
        lock(&self.passed).insert(pool.clone(), view);
    }

    /// The pools of `pools` (key and `allowHosts` names) whose view differs from
    /// the one their latest pass used, whichever pass or tick changed it. Forgets
    /// the names and pools not in `pools`.
    pub fn due(&self, pools: &[(PoolKey, BTreeSet<String>)]) -> Vec<PoolKey> {
        let names: BTreeSet<&String> = pools.iter().flat_map(|(_, n)| n).collect();
        self.known().retain(|n, _| names.contains(n));
        let mut passed = lock(&self.passed);
        passed.retain(|k, _| pools.iter().any(|(p, _)| p == k));
        pools
            .iter()
            .filter(|(k, n)| passed.get(k) != Some(&self.view(n)))
            .map(|(k, _)| k.clone())
            .collect()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The names of the pool's `allowHosts` entries that parse.
#[must_use]
pub fn names(pool: &BuildKitPool) -> BTreeSet<String> {
    pool.spec
        .network
        .egress
        .allow_hosts
        .iter()
        .filter_map(|h| parse_allow_host(h))
        .map(|h| h.name.to_owned())
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::net::IpAddr;
    use std::sync::{Arc, Mutex};

    use futures::future::BoxFuture;

    use super::{Hosts, MAX_LOOKUPS, RESOLVE_TIMEOUT, Resolve};

    /// Answers absolute names from a table a test edits; any other query never
    /// answers. Records every query.
    #[derive(Default)]
    pub(crate) struct FakeResolver {
        pub answers: Mutex<HashMap<String, Result<Vec<IpAddr>, String>>>,
        pub queries: Mutex<Vec<String>>,
    }

    impl FakeResolver {
        pub fn set(&self, name: &str, answer: Result<&[&str], &str>) {
            let a = answer
                .map(|v| v.iter().map(|s| s.parse().unwrap()).collect())
                .map_err(str::to_owned);
            self.answers.lock().unwrap().insert(format!("{name}."), a);
        }
    }

    impl Resolve for FakeResolver {
        fn lookup(&self, name: &str) -> BoxFuture<'static, std::io::Result<Vec<IpAddr>>> {
            let a = self.answers.lock().unwrap().get(name).cloned();
            self.queries.lock().unwrap().push(name.to_owned());
            Box::pin(async move {
                match a {
                    Some(Ok(v)) => Ok(v),
                    Some(Err(e)) => Err(std::io::Error::other(e)),
                    None => std::future::pending().await,
                }
            })
        }
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|&n| n.to_owned()).collect()
    }

    fn ips(v: &[&str]) -> Vec<IpAddr> {
        v.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[tokio::test]
    async fn a_failed_lookup_keeps_the_last_known_addresses() {
        let r = Arc::new(FakeResolver::default());
        let hosts = Hosts::new(r.clone());
        let n = set(&["reg.example.com"]);
        r.set(
            "reg.example.com",
            Ok(&["203.0.113.9", "::ffff:203.0.113.9", "2001:db8::9"]),
        );
        hosts.refresh(&n).await;
        let v = hosts.view(&n);
        // IPv4-mapped answers fold into their IPv4 address.
        assert_eq!(
            v.addrs["reg.example.com"],
            ips(&["203.0.113.9", "2001:db8::9"])
        );
        assert!(v.unresolved.is_empty());

        r.set("reg.example.com", Err("NXDOMAIN"));
        hosts.refresh(&n).await;
        let v = hosts.view(&n);
        assert_eq!(
            v.addrs["reg.example.com"],
            ips(&["203.0.113.9", "2001:db8::9"])
        );
        assert!(v.unresolved["reg.example.com"].contains("NXDOMAIN"));
        // An empty answer is a failure too, and keeps the addresses.
        r.set("reg.example.com", Ok(&[]));
        hosts.refresh(&n).await;
        assert_eq!(hosts.view(&n).unresolved["reg.example.com"], "no addresses");

        r.set("reg.example.com", Ok(&["203.0.113.10"]));
        hosts.refresh(&n).await;
        let v = hosts.view(&n);
        assert_eq!(v.addrs["reg.example.com"], ips(&["203.0.113.10"]));
        assert!(v.unresolved.is_empty());

        hosts.due(&[]);
        assert_eq!(hosts.view(&n), super::HostView::default());
    }

    /// A lookup that never returns fails after the timeout, and hung lookups
    /// hold at most `MAX_LOOKUPS` permits, so the next one fails at once
    /// instead of starting another blocking lookup.
    #[tokio::test(start_paused = true)]
    async fn a_hung_lookup_times_out_and_bounds_the_lookups_in_flight() {
        let r = Arc::new(FakeResolver::default());
        let hosts = Hosts::new(r.clone());
        let many: BTreeSet<String> = (0..MAX_LOOKUPS)
            .map(|i| format!("h{i}.example.com"))
            .collect();
        let started = tokio::time::Instant::now();
        hosts.refresh(&many).await;
        assert_eq!(started.elapsed(), RESOLVE_TIMEOUT);
        let v = hosts.view(&many);
        assert_eq!(v.unresolved.len(), MAX_LOOKUPS);
        assert!(v.unresolved.values().all(|w| w.contains("timed out")));
        assert_eq!(hosts.permits.available_permits(), 0);
        // Every permit is still held by a hung lookup.
        r.set("ok.example.com", Ok(&["203.0.113.1"]));
        let n = set(&["ok.example.com"]);
        hosts.refresh(&n).await;
        assert!(hosts.view(&n).unresolved["ok.example.com"].contains("timed out"));
    }

    /// Names go to the resolver as absolute names, so the controller pod's
    /// search path (`<ns>.svc.cluster.local`, ndots:5) never applies.
    #[tokio::test]
    async fn names_are_looked_up_as_absolute() {
        let r = Arc::new(FakeResolver::default());
        let hosts = Hosts::new(r.clone());
        r.set("registry.corp", Ok(&["203.0.113.9"]));
        let n = set(&["registry.corp"]);
        hosts.refresh(&n).await;
        assert_eq!(*r.queries.lock().unwrap(), ["registry.corp."]);
        assert_eq!(hosts.view(&n).addrs["registry.corp"], ips(&["203.0.113.9"]));
    }

    /// Pools A and B list one name. A's pass sees the new addresses first; the
    /// next tick still finds B due, because B's last pass used the old view.
    #[tokio::test]
    async fn a_change_another_pool_noticed_first_still_makes_every_pool_due() {
        let r = Arc::new(FakeResolver::default());
        let hosts = Hosts::new(r.clone());
        let n = set(&["reg.example.com"]);
        let (a, b) = (
            ("ns".to_owned(), "a".to_owned()),
            ("ns".to_owned(), "b".to_owned()),
        );
        let pools = [(a.clone(), n.clone()), (b.clone(), n.clone())];
        r.set("reg.example.com", Ok(&["203.0.113.9"]));
        hosts.refresh(&n).await;
        hosts.passed(&a, hosts.view(&n));
        hosts.passed(&b, hosts.view(&n));
        assert!(hosts.due(&pools).is_empty());

        // A's pass resolves the new address and uses it.
        r.set("reg.example.com", Ok(&["203.0.113.10"]));
        hosts.refresh(&n).await;
        hosts.passed(&a, hosts.view(&n));
        // The tick resolves again and sees no new change, yet B is due.
        hosts.refresh(&n).await;
        assert_eq!(hosts.due(&pools), [b.clone()]);
        hosts.passed(&b, hosts.view(&n));
        assert!(hosts.due(&pools).is_empty());

        // A failure another pass noticed first makes B due too.
        r.set("reg.example.com", Err("SERVFAIL"));
        hosts.refresh(&n).await;
        hosts.passed(&a, hosts.view(&n));
        assert_eq!(hosts.due(&pools), [b]);
    }

    /// A new leadership term starts with no addresses, whatever an earlier
    /// term of the same process learnt, even while DNS fails.
    #[tokio::test(start_paused = true)]
    async fn a_new_term_starts_with_no_addresses() {
        let r = Arc::new(FakeResolver::default());
        let resolver: Arc<dyn Resolve> = r.clone();
        let n = set(&["reg.example.com"]);
        r.set("reg.example.com", Ok(&["203.0.113.9"]));
        let first = crate::reconcile::driver::term_hosts(&resolver);
        first.refresh(&n).await;
        assert!(!first.view(&n).addrs.is_empty());
        drop(first);

        r.answers.lock().unwrap().clear();
        let second = crate::reconcile::driver::term_hosts(&resolver);
        second.refresh(&n).await;
        let v = second.view(&n);
        assert!(v.addrs.is_empty(), "{v:?}");
        assert!(v.unresolved["reg.example.com"].contains("timed out"));
    }
}
