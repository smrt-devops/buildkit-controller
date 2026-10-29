//! Garbage collection planning.
//!
//! [`plan`] reads the index and returns what to remove; the store applies the
//! plan under its index lock, so no upload or push interleaves. In order:
//!
//! 1. Per pool (repositories under `<namespace>/<pool>/`): manifests are kept
//!    newest first (by last pull or push) while the distinct blobs they reach
//!    fit the pool budget. The first manifest that does not fit, and every
//!    older one, is evicted.
//! 2. The same over every remaining manifest, against the volume budget.
//! 3. Mark and sweep: a repository keeps a blob link while a remaining
//!    manifest of it reaches the blob, or while the link is younger than the
//!    grace period. The grace period covers a push in flight: its blobs land
//!    before the manifest that references them, and a HEAD or GET of a blob
//!    renews its link. A blob no repository links is deleted.
//!
//! Recent links count against both budgets first, so a push in flight is
//! never evicted to make room for older manifests.

use std::collections::{BTreeMap, HashSet};

use super::store::{Digest, Index, Repo};

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

/// The manifest `root`, everything it references and, through manifests of
/// the same repository, everything they reference.
fn closure(repo: &Repo, root: &Digest) -> HashSet<Digest> {
    let mut out = HashSet::new();
    let mut stack = vec![root];
    while let Some(d) = stack.pop() {
        if out.insert(d.clone())
            && let Some(m) = repo.manifests.get(d)
        {
            stack.extend(m.refs.iter());
        }
    }
    out
}

type Candidate = (String, Digest, u64);

/// The candidates to evict so that `seed` plus the kept manifests fit `budget`.
fn fit(
    idx: &Index,
    mut cands: Vec<Candidate>,
    seed: HashSet<Digest>,
    budget: u64,
) -> Vec<(String, Digest)> {
    cands.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then_with(|| a.0.cmp(&b.0))
            .then_with(|| a.1.cmp(&b.1))
    });
    let size = |d: &Digest| idx.blobs.get(d).copied().unwrap_or(0);
    let mut used: u64 = seed.iter().map(size).sum();
    let mut blobs = seed;
    let mut kept: HashSet<(String, Digest)> = HashSet::new();
    let mut full = false;
    let mut evict = Vec::new();
    for (name, d, _) in cands {
        let Some(repo) = idx.repos.get(&name) else {
            continue;
        };
        if kept.contains(&(name.clone(), d.clone())) {
            continue;
        }
        if !full {
            let reach = closure(repo, &d);
            let add: u64 = reach.iter().filter(|b| !blobs.contains(*b)).map(size).sum();
            if used.saturating_add(add) <= budget {
                used += add;
                for b in reach {
                    if repo.manifests.contains_key(&b) {
                        kept.insert((name.clone(), b.clone()));
                    }
                    blobs.insert(b);
                }
                continue;
            }
            full = true;
        }
        evict.push((name, d));
    }
    evict
}

/// The collection for `idx` at `now` (unix seconds).
pub(super) fn plan(idx: &Index, budgets: &dyn Budgets, now: u64, grace: u64) -> Plan {
    let young = |t: u64| now.saturating_sub(t) < grace;
    let mut pools: BTreeMap<&str, (Vec<Candidate>, HashSet<Digest>)> = BTreeMap::new();
    for (name, repo) in &idx.repos {
        let (cands, seed) = pools.entry(pool_of(name)).or_default();
        cands.extend(
            repo.manifests
                .iter()
                .map(|(d, m)| (name.clone(), d.clone(), m.last_used)),
        );
        seed.extend(
            repo.links
                .iter()
                .filter(|(_, t)| young(**t))
                .map(|(d, _)| d.clone()),
        );
    }
    let mut out = Plan::default();
    let mut gone: HashSet<(String, Digest)> = HashSet::new();
    let mut all = Vec::new();
    let mut all_seed = HashSet::new();
    for (pool, (cands, seed)) in pools {
        all.extend(cands.iter().cloned());
        all_seed.extend(seed.iter().cloned());
        for (name, d) in fit(idx, cands, seed, budgets.pool(pool)) {
            gone.insert((name.clone(), d.clone()));
            out.evict.push((name, d, Reason::Pool));
        }
    }
    all.retain(|(n, d, _)| !gone.contains(&(n.clone(), d.clone())));
    for (name, d) in fit(idx, all, all_seed, budgets.total()) {
        gone.insert((name.clone(), d.clone()));
        out.evict.push((name, d, Reason::Volume));
    }
    let mut linked = HashSet::new();
    for (name, repo) in &idx.repos {
        let mut reached = HashSet::new();
        for d in repo.manifests.keys() {
            if !gone.contains(&(name.clone(), d.clone())) {
                reached.extend(closure(repo, d));
            }
        }
        for (d, t) in &repo.links {
            if reached.contains(d) || young(*t) {
                linked.insert(d.clone());
            } else {
                out.unlink.push((name.clone(), d.clone()));
            }
        }
    }
    out.delete = idx
        .blobs
        .keys()
        .filter(|d| !linked.contains(*d))
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

    #[test]
    fn young_links_are_kept_and_counted_first() {
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
}
