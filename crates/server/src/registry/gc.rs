//! Garbage collection planning.
//!
//! [`plan`] reads the index and returns what to remove; the store applies the
//! plan under its index lock, so no upload or push interleaves. Candidates are
//! the manifests, with every blob they reach, and the recent blob links that
//! no manifest reaches yet (a push in flight: its blobs land before the
//! manifest that references them). In order:
//!
//! 1. Per pool (repositories under `<namespace>/<pool>/`): candidates are
//!    kept newest first (a manifest by last pull or push, a link by when it
//!    was made or last read) while the distinct blobs they hold fit the pool
//!    budget. One that does not fit is removed and the scan goes on, so one
//!    oversized push cannot empty the pool. A repository keeps at most
//!    [`MAX_KEPT_MANIFESTS`] manifests.
//! 2. The same over every remaining candidate, against the volume budget.
//! 3. Mark and sweep: a repository keeps a blob link while a remaining
//!    manifest of it reaches the blob, or while the link is younger than the
//!    grace period and was not removed. A blob no repository links is deleted.
//!
//! So recent links count against the budgets like anything else: no amount
//! of unreferenced uploads can push a pool past its budget or evict more than
//! a newer manifest of the same size would.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::store::{Digest, Index, Repo};

/// Manifests one repository keeps; older ones are evicted whatever their size.
pub const MAX_KEPT_MANIFESTS: usize = 1024;

/// The byte budgets.
pub trait Budgets: Send + Sync {
    /// Bytes the pool `<namespace>/<pool>` may hold.
    fn pool(&self, pool: &str) -> u64;
    /// Bytes the whole store may hold.
    fn total(&self) -> u64;
}

/// One budget for every pool and one for the volume.
#[derive(Clone, Copy, Debug)]
pub struct FixedBudgets {
    pub pool: u64,
    pub total: u64,
}

impl Budgets for FixedBudgets {
    fn pool(&self, _: &str) -> u64 {
        self.pool
    }
    fn total(&self) -> u64 {
        self.total
    }
}

/// Which budget evicted a manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Pool,
    Volume,
}

impl Reason {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Pool => "pool",
            Self::Volume => "volume",
        }
    }
}

/// What one collection removes.
#[derive(Debug, Default)]
pub(super) struct Plan {
    pub evict: Vec<(String, Digest, Reason)>,
    pub unlink: Vec<(String, Digest)>,
    pub delete: Vec<Digest>,
}

/// `<namespace>/<pool>` of a repository.
pub(super) fn pool_of(repo: &str) -> &str {
    repo.match_indices('/')
        .nth(1)
        .map_or(repo, |(i, _)| &repo[..i])
}

/// The `roots`, everything they reference and, through manifests of the same
/// repository, everything those reference.
fn reach<'a>(repo: &'a Repo, roots: impl IntoIterator<Item = &'a Digest>) -> HashSet<&'a Digest> {
    let mut out = HashSet::new();
    let mut stack: Vec<&Digest> = roots.into_iter().collect();
    while let Some(d) = stack.pop() {
        if out.insert(d)
            && let Some(m) = repo.manifests.get(d)
        {
            stack.extend(m.refs.iter());
        }
    }
    out
}

/// A manifest, or a recent link no manifest reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Kind {
    Manifest,
    Link,
}

/// `(repository, digest, last used, kind)`.
type Candidate<'a> = (&'a str, &'a Digest, u64, Kind);

/// Splits `cands` into those kept within `budget` (and `per_repo` manifests
/// per repository), newest first, and those removed.
fn fit<'a>(
    idx: &'a Index,
    mut cands: Vec<Candidate<'a>>,
    budget: u64,
    per_repo: usize,
) -> (Vec<Candidate<'a>>, Vec<Candidate<'a>>) {
    cands.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then_with(|| a.0.cmp(b.0))
            .then_with(|| a.1.cmp(b.1))
            .then_with(|| a.3.cmp(&b.3))
    });
    let size = |d: &Digest| idx.blobs.get(d).copied().unwrap_or(0);
    let mut used: u64 = 0;
    let mut blobs: HashSet<&Digest> = HashSet::new();
    let mut kept_manifests: HashSet<(&str, &Digest)> = HashSet::new();
    let mut removed_manifests: HashSet<(&str, &Digest)> = HashSet::new();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let (mut kept, mut removed) = (Vec::new(), Vec::new());
    for c in cands {
        let (name, d, _, kind) = c;
        let fits = match kind {
            Kind::Link => {
                let add = if blobs.contains(d) { 0 } else { size(d) };
                let fits = used.saturating_add(add) <= budget;
                if fits {
                    used += add;
                    blobs.insert(d);
                }
                fits
            }
            Kind::Manifest if kept_manifests.contains(&(name, d)) => true,
            Kind::Manifest => {
                let Some(repo) = idx.repos.get(name) else {
                    continue;
                };
                let reached = reach(repo, [d]);
                let manifests: Vec<&Digest> = reached
                    .iter()
                    .copied()
                    .filter(|b| repo.manifests.contains_key(*b))
                    .collect();
                let new = manifests
                    .iter()
                    .filter(|b| !kept_manifests.contains(&(name, **b)))
                    .count();
                let add: u64 = reached
                    .iter()
                    .filter(|b| !blobs.contains(**b))
                    .map(|b| size(b))
                    .sum();
                let count = counts.entry(name).or_default();
                // A manifest whose closure lost a manifest already cannot be whole.
                let fits = used.saturating_add(add) <= budget
                    && count.saturating_add(new) <= per_repo
                    && !manifests
                        .iter()
                        .any(|b| removed_manifests.contains(&(name, *b)));
                if fits {
                    used += add;
                    *count += new;
                    kept_manifests.extend(manifests.into_iter().map(|b| (name, b)));
                    blobs.extend(reached);
                } else {
                    removed_manifests.insert((name, d));
                }
                fits
            }
        };
        if fits {
            kept.push(c);
        } else {
            removed.push(c);
        }
    }
    (kept, removed)
}

/// The collection for `idx` at `now` (unix seconds).
pub(super) fn plan(idx: &Index, budgets: &dyn Budgets, now: u64, grace: u64) -> Plan {
    let young = |t: u64| now.saturating_sub(t) < grace;
    let mut pools: BTreeMap<&str, Vec<Candidate>> = BTreeMap::new();
    for (name, repo) in &idx.repos {
        let cands = pools.entry(pool_of(name)).or_default();
        let reached = reach(repo, repo.manifests.keys());
        cands.extend(
            repo.manifests
                .iter()
                .map(|(d, m)| (name.as_str(), d, m.last_used, Kind::Manifest)),
        );
        cands.extend(
            repo.links
                .iter()
                .filter(|(d, t)| young(**t) && !reached.contains(d))
                .map(|(d, t)| (name.as_str(), d, *t, Kind::Link)),
        );
    }
    let mut removed: Vec<(Candidate, Reason)> = Vec::new();
    let mut all = Vec::new();
    for (pool, cands) in pools {
        let (kept, gone) = fit(idx, cands, budgets.pool(pool), MAX_KEPT_MANIFESTS);
        all.extend(kept);
        removed.extend(gone.into_iter().map(|c| (c, Reason::Pool)));
    }
    let (_, gone) = fit(idx, all, budgets.total(), usize::MAX);
    removed.extend(gone.into_iter().map(|c| (c, Reason::Volume)));

    let mut out = Plan::default();
    let mut evicted: HashSet<(&str, &Digest)> = HashSet::new();
    let mut dropped: HashSet<(&str, &Digest)> = HashSet::new();
    for ((name, d, _, kind), reason) in removed {
        match kind {
            Kind::Manifest => {
                evicted.insert((name, d));
                out.evict.push((name.to_owned(), d.clone(), reason));
            }
            Kind::Link => {
                dropped.insert((name, d));
            }
        }
    }
    let mut linked = HashSet::new();
    for (name, repo) in &idx.repos {
        let reached = reach(
            repo,
            repo.manifests
                .keys()
                .filter(|d| !evicted.contains(&(name.as_str(), *d))),
        );
        for (d, t) in &repo.links {
            let keep = reached.contains(d) || (young(*t) && !dropped.contains(&(name.as_str(), d)));
            if keep {
                linked.insert(d);
            } else {
                out.unlink.push((name.clone(), d.clone()));
            }
        }
    }
    out.delete = idx
        .blobs
        .keys()
        .filter(|d| !linked.contains(d))
        .cloned()
        .collect();
    out.delete.sort();
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::super::store::Manifest;
    use super::*;

    fn dg(n: u8) -> Digest {
        Digest::of(&[n])
    }

    /// Repo `name` with manifest `m` (last used `t`) referencing `refs`, all linked at time 0.
    fn put(idx: &mut Index, name: &str, m: u8, t: u64, refs: &[u8], sizes: u64) {
        let repo = idx.repos.entry(name.to_owned()).or_default();
        for r in refs.iter().chain([&m]) {
            repo.links.insert(dg(*r), 0);
            idx.blobs.insert(dg(*r), sizes);
        }
        repo.manifests.insert(
            dg(m),
            Manifest {
                media_type: "application/vnd.oci.image.index.v1+json".into(),
                refs: refs.iter().map(|r| dg(*r)).collect(),
                last_used: t,
                persisted: t,
            },
        );
    }

    fn evicted(p: &Plan) -> Vec<(String, u8, Reason)> {
        let mut v: Vec<_> = p
            .evict
            .iter()
            .map(|(n, d, r)| {
                let i = (0..=255u8).find(|i| dg(*i) == *d).unwrap();
                (n.clone(), i, *r)
            })
            .collect();
        v.sort_by_key(|(n, i, _)| (n.clone(), *i));
        v
    }

    const BIG: FixedBudgets = FixedBudgets {
        pool: u64::MAX,
        total: u64::MAX,
    };

    #[test]
    fn pool_of_takes_two_components() {
        assert_eq!(pool_of("ns/pool/scope/x"), "ns/pool");
        assert_eq!(pool_of("ns/pool"), "ns/pool");
    }

    #[test]
    fn within_budget_nothing_is_removed_but_unreferenced_old_blobs() {
        let mut idx = Index::default();
        put(&mut idx, "ns/p/a", 1, 10, &[2, 3], 10);
        idx.repos.get_mut("ns/p/a").unwrap().links.insert(dg(9), 0);
        idx.blobs.insert(dg(9), 10);
        idx.blobs.insert(dg(8), 10);
        let p = plan(&idx, &BIG, 100, 50);
        assert!(p.evict.is_empty());
        assert_eq!(p.unlink, vec![("ns/p/a".to_owned(), dg(9))]);
        let mut want = vec![dg(8), dg(9)];
        want.sort();
        assert_eq!(p.delete, want);
    }

    #[test]
    fn over_the_pool_budget_the_oldest_manifests_go_first() {
        let mut idx = Index::default();
        // Three manifests of 3 blobs × 10 bytes each, sharing nothing.
        put(&mut idx, "ns/p/a", 1, 30, &[11, 12], 10);
        put(&mut idx, "ns/p/b", 2, 20, &[21, 22], 10);
        put(&mut idx, "ns/p/a", 3, 10, &[31, 32], 10);
        // Another pool, untouched by p's budget.
        put(&mut idx, "ns/q/a", 4, 1, &[41], 10);
        let b = FixedBudgets {
            pool: 45,
            total: u64::MAX,
        };
        let p = plan(&idx, &b, 100, 0);
        assert_eq!(
            evicted(&p),
            vec![
                ("ns/p/a".into(), 3, Reason::Pool),
                ("ns/p/b".into(), 2, Reason::Pool)
            ]
        );
        assert!(p.delete.contains(&dg(21)) && p.delete.contains(&dg(3)));
        assert!(!p.delete.contains(&dg(11)) && !p.delete.contains(&dg(41)));
    }

    #[test]
    fn shared_blobs_count_once() {
        let mut idx = Index::default();
        put(&mut idx, "ns/p/a", 1, 30, &[10, 11], 10);
        put(&mut idx, "ns/p/a", 2, 20, &[10, 11], 10);
        // 30 + 10 (only manifest 2 itself is new).
        let b = FixedBudgets {
            pool: 40,
            total: u64::MAX,
        };
        assert!(plan(&idx, &b, 100, 0).evict.is_empty());
    }

    #[test]
    fn the_volume_budget_evicts_across_pools_by_age() {
        let mut idx = Index::default();
        put(&mut idx, "ns/p/a", 1, 30, &[], 10);
        put(&mut idx, "ns/q/a", 2, 20, &[], 10);
        put(&mut idx, "ns/r/a", 3, 25, &[], 10);
        let b = FixedBudgets {
            pool: u64::MAX,
            total: 20,
        };
        assert_eq!(
            evicted(&plan(&idx, &b, 100, 0)),
            vec![("ns/q/a".into(), 2, Reason::Volume)]
        );
    }

    #[test]
    fn a_kept_index_keeps_its_child_manifests() {
        let mut idx = Index::default();
        put(&mut idx, "ns/p/a", 5, 1, &[50], 10); // child, old
        put(&mut idx, "ns/p/a", 6, 90, &[5], 10); // index, new
        let b = FixedBudgets {
            pool: 30,
            total: u64::MAX,
        };
        let p = plan(&idx, &b, 100, 0);
        assert!(p.evict.is_empty(), "{:?}", p.evict);
        assert!(p.delete.is_empty());
    }

    /// A busy pool, whose recent pushes and pulls exceed its budget, keeps its
    /// newest manifest instead of losing all of them.
    #[test]
    fn young_manifests_over_budget_keep_the_newest() {
        for budgets in [
            FixedBudgets {
                pool: 15,
                total: u64::MAX,
            },
            FixedBudgets {
                pool: u64::MAX,
                total: 15,
            },
        ] {
            let mut idx = Index::default();
            put(&mut idx, "ns/p/a", 1, 50, &[], 10);
            put(&mut idx, "ns/p/a", 2, 90, &[], 10);
            let repo = idx.repos.get_mut("ns/p/a").unwrap();
            repo.links.insert(dg(1), 50);
            repo.links.insert(dg(2), 90);
            let p = plan(&idx, &budgets, 100, 60);
            let reason = if budgets.pool == 15 {
                Reason::Pool
            } else {
                Reason::Volume
            };
            assert_eq!(evicted(&p), vec![("ns/p/a".into(), 1, reason)]);
        }
    }

    #[test]
    fn young_links_are_kept_and_compete_by_age() {
        let mut idx = Index::default();
        put(&mut idx, "ns/p/a", 1, 10, &[], 10);
        // A push in flight: a blob uploaded at t=95, no manifest yet.
        idx.repos.get_mut("ns/p/a").unwrap().links.insert(dg(7), 95);
        idx.blobs.insert(dg(7), 10);
        let b = FixedBudgets {
            pool: 15,
            total: u64::MAX,
        };
        let p = plan(&idx, &b, 100, 60);
        assert_eq!(evicted(&p), vec![("ns/p/a".into(), 1, Reason::Pool)]);
        // The evicted manifest's own link is old; the fresh blob survives.
        assert_eq!(p.delete, vec![dg(1)]);
    }

    /// Links in `name` at times `ts`, each to a fresh 10-byte blob numbered from `first`.
    fn junk(idx: &mut Index, name: &str, first: u8, ts: &[u64]) -> Vec<Digest> {
        let repo = idx.repos.entry(name.to_owned()).or_default();
        ts.iter()
            .zip(first..)
            .map(|(t, n)| {
                repo.links.insert(dg(n), *t);
                idx.blobs.insert(dg(n), 10);
                dg(n)
            })
            .collect()
    }

    fn kept_bytes(idx: &Index, p: &Plan) -> u64 {
        idx.blobs
            .iter()
            .filter(|(d, _)| !p.delete.contains(d))
            .map(|(_, s)| s)
            .sum()
    }

    /// Unreferenced uploads, kept fresh by reads, cannot hold a pool or the
    /// volume past its budget, and lose to a newer manifest.
    #[test]
    fn unreferenced_links_count_by_age_and_stay_within_budget() {
        for budgets in [
            FixedBudgets {
                pool: 30,
                total: u64::MAX,
            },
            FixedBudgets {
                pool: u64::MAX,
                total: 30,
            },
        ] {
            let mut idx = Index::default();
            put(&mut idx, "ns/p/a", 1, 99, &[], 10);
            idx.repos.get_mut("ns/p/a").unwrap().links.insert(dg(1), 99);
            let j = junk(&mut idx, "ns/p/x", 100, &[90, 91, 92, 93, 94]);
            let p = plan(&idx, &budgets, 100, 60);
            assert!(p.evict.is_empty(), "{:?}", p.evict);
            assert_eq!(kept_bytes(&idx, &p), 30);
            // The two newest links stay, the older ones go.
            for (d, gone) in j.iter().zip([true, true, true, false, false]) {
                assert_eq!(p.delete.contains(d), gone, "{d}");
                assert_eq!(p.unlink.contains(&("ns/p/x".into(), d.clone())), gone);
            }
        }
    }

    /// One manifest larger than the budget is evicted alone; older ones that
    /// fit stay.
    #[test]
    fn an_oversized_manifest_does_not_take_the_older_ones_with_it() {
        for budgets in [
            FixedBudgets {
                pool: 25,
                total: u64::MAX,
            },
            FixedBudgets {
                pool: u64::MAX,
                total: 25,
            },
        ] {
            let mut idx = Index::default();
            put(&mut idx, "ns/p/a", 1, 99, &[11, 12], 10);
            put(&mut idx, "ns/p/b", 2, 50, &[], 10);
            put(&mut idx, "ns/p/a", 3, 40, &[], 10);
            let reason = if budgets.pool == 25 {
                Reason::Pool
            } else {
                Reason::Volume
            };
            let p = plan(&idx, &budgets, 100, 0);
            assert_eq!(evicted(&p), vec![("ns/p/a".into(), 1, reason)]);
            assert_eq!(kept_bytes(&idx, &p), 20);
        }
    }

    #[test]
    fn a_repository_keeps_a_bounded_number_of_manifests() {
        let mut idx = Index::default();
        let repo = idx.repos.entry("ns/p/a".to_owned()).or_default();
        let n = MAX_KEPT_MANIFESTS + 3;
        let digests: Vec<Digest> = (0..n).map(|i| Digest::of(&i.to_be_bytes())).collect();
        for (i, d) in digests.iter().enumerate() {
            repo.links.insert(d.clone(), 0);
            repo.manifests.insert(
                d.clone(),
                Manifest {
                    media_type: "application/vnd.oci.image.index.v1+json".into(),
                    refs: Vec::new(),
                    last_used: i as u64,
                    persisted: 0,
                },
            );
            idx.blobs.insert(d.clone(), 1);
        }
        let mut gone: Vec<Digest> = plan(&idx, &BIG, 10_000, 0)
            .evict
            .into_iter()
            .map(|(_, d, r)| {
                assert_eq!(r, Reason::Pool);
                d
            })
            .collect();
        gone.sort();
        let mut want = digests[..3].to_vec();
        want.sort();
        assert_eq!(gone, want);
    }
}
