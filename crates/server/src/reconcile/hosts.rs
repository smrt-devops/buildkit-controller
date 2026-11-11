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

/// Last known addresses of every name the leader resolves.
pub struct Hosts {
    resolver: Arc<dyn Resolve>,
    permits: Arc<Semaphore>,
    known: Mutex<BTreeMap<String, Known>>,
}

impl Hosts {
    /// A cache that resolves through `resolver`.
    #[must_use]
    pub fn new(resolver: Arc<dyn Resolve>) -> Self {
        Self {
            resolver,
            permits: Arc::new(Semaphore::new(MAX_LOOKUPS)),
            known: Mutex::new(BTreeMap::new()),
        }
    }

    fn known(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Known>> {
        self.known.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Resolves each of `names` at once and records the result. A failed lookup
    /// keeps the name's last known addresses. Returns the names whose addresses
    /// or failed state changed.
    pub async fn refresh(&self, names: &BTreeSet<String>) -> BTreeSet<String> {
        let results = join_all(
            names
                .iter()
                .map(|n| async move { (n, self.lookup(n).await) }),
        )
        .await;
        let mut known = self.known();
        let mut changed = BTreeSet::new();
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
                changed.insert(name.clone());
            }
            known.insert(name.clone(), new);
        }
        changed
    }

    /// One lookup, bounded by [`RESOLVE_TIMEOUT`] and [`MAX_LOOKUPS`].
    async fn lookup(&self, name: &str) -> Result<Vec<IpAddr>, String> {
        let timeout = || format!("timed out after {}s", RESOLVE_TIMEOUT.as_secs());
        let deadline = tokio::time::Instant::now() + RESOLVE_TIMEOUT;
        let permit = tokio::time::timeout_at(deadline, Arc::clone(&self.permits).acquire_owned())
            .await
            .map_err(|_| timeout())?
            .map_err(|e| e.to_string())?;
        let fut = self.resolver.lookup(name);
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

    /// Forgets every name not in `names`.
    pub fn retain(&self, names: &BTreeSet<String>) {
        self.known().retain(|n, _| names.contains(n));
    }
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

    /// Answers from a table a test edits; a missing name never answers.
    #[derive(Default)]
    pub(crate) struct FakeResolver {
        pub answers: Mutex<HashMap<String, Result<Vec<IpAddr>, String>>>,
    }

    impl FakeResolver {
        pub fn set(&self, name: &str, answer: Result<&[&str], &str>) {
            let a = answer
                .map(|v| v.iter().map(|s| s.parse().unwrap()).collect())
                .map_err(str::to_owned);
            self.answers.lock().unwrap().insert(name.into(), a);
        }
    }

    impl Resolve for FakeResolver {
        fn lookup(&self, name: &str) -> BoxFuture<'static, std::io::Result<Vec<IpAddr>>> {
            let a = self.answers.lock().unwrap().get(name).cloned();
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
        assert_eq!(hosts.refresh(&n).await, n);
        let v = hosts.view(&n);
        // IPv4-mapped answers fold into their IPv4 address.
        assert_eq!(
            v.addrs["reg.example.com"],
            ips(&["203.0.113.9", "2001:db8::9"])
        );
        assert!(v.unresolved.is_empty());
        // Unchanged: nothing to report.
        assert!(hosts.refresh(&n).await.is_empty());

        r.set("reg.example.com", Err("NXDOMAIN"));
        assert_eq!(hosts.refresh(&n).await, n, "turning unresolved is a change");
        let v = hosts.view(&n);
        assert_eq!(
            v.addrs["reg.example.com"],
            ips(&["203.0.113.9", "2001:db8::9"])
        );
        assert!(v.unresolved["reg.example.com"].contains("NXDOMAIN"));
        // An empty answer is a failure too, and keeps the addresses.
        r.set("reg.example.com", Ok(&[]));
        assert!(hosts.refresh(&n).await.is_empty());
        assert_eq!(hosts.view(&n).unresolved["reg.example.com"], "no addresses");

        r.set("reg.example.com", Ok(&["203.0.113.10"]));
        assert_eq!(hosts.refresh(&n).await, n);
        let v = hosts.view(&n);
        assert_eq!(v.addrs["reg.example.com"], ips(&["203.0.113.10"]));
        assert!(v.unresolved.is_empty());

        hosts.retain(&BTreeSet::new());
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
}
