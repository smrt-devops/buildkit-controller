//! Leaf lifecycle on a fake clock. Across many renewal cycles every leaf ever
//! issued verifies until its own `notAfter`, and each held leaf is renewed
//! before the one it replaces expires.

use std::collections::BTreeMap;

use proptest::prelude::*;
use rustls_pki_types::{CertificateDer, DnsName, ServerName, UnixTime};
use time::{Duration, OffsetDateTime};
use webpki::{EndEntityCert, KeyUsage, anchor_from_trusted_cert};

use super::alloc_id::tests::sample;
use super::*;

const EDGE_NAME: &str = "buildkit.example.com";
const WORKER_NAME: &str = "00000000000000aa.default-0.default.ci.worker.buildkit.internal";

type Live = Vec<(Profile, CertificateDer<'static>, OffsetDateTime)>;

fn t0() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap() // 2026-01-01
}

fn unix(t: OffsetDateTime) -> UnixTime {
    UnixTime::since_unix_epoch(std::time::Duration::from_secs(
        u64::try_from(t.unix_timestamp()).unwrap(),
    ))
}

fn chains(
    cert: &CertificateDer<'_>,
    ca: &CertificateDer<'_>,
    usage: KeyUsage,
    name: Option<&str>,
    now: OffsetDateTime,
) -> bool {
    let anchors = [anchor_from_trusted_cert(ca).unwrap()];
    let ee = EndEntityCert::try_from(cert).unwrap();
    let chain_ok = ee
        .verify_for_usage(
            &[webpki::ring::ECDSA_P256_SHA256],
            &anchors,
            &[],
            unix(now),
            usage,
            None,
            None,
        )
        .is_ok();
    let name_ok = name.is_none_or(|n| {
        let n = ServerName::DnsName(DnsName::try_from(n).unwrap().to_owned());
        ee.verify_is_valid_for_subject_name(&n).is_ok()
    });
    chain_ok && name_ok
}

/// Does a leaf of profile `p` verify where it is used?
fn trusted(b: &CaBundle, p: Profile, der: &CertificateDer<'_>, now: OffsetDateTime) -> bool {
    let (client, internal) = (b.client_ca().cert_der(), b.internal_ca().cert_der());
    match p {
        Profile::Allocation => verify_allocation_cert(der, client, unix(now)).is_ok(),
        Profile::EdgeServing => chains(der, client, KeyUsage::server_auth(), Some(EDGE_NAME), now),
        Profile::WorkerServing => chains(
            der,
            internal,
            KeyUsage::server_auth(),
            Some(WORKER_NAME),
            now,
        ),
        Profile::EdgeClient => chains(der, internal, KeyUsage::client_auth(), None, now),
    }
}

fn edge_names() -> EdgeNames {
    EdgeNames::new(
        EDGE_NAME,
        &[],
        "buildkit-controller",
        &"bk-system".parse().unwrap(),
    )
    .unwrap()
}

/// Minimum time left on a held leaf when it is renewed, unless it was capped
/// at its CA's end.
fn min_left_at_renewal(p: Profile) -> Duration {
    match p {
        // With 30 days, renewal leaves at least 10 days.
        Profile::WorkerServing | Profile::EdgeServing => Duration::days(10),
        // With 24 hours, renewal leaves at least 8 hours.
        Profile::EdgeClient => Duration::hours(8),
        Profile::Allocation => Duration::ZERO,
    }
}

/// A renewable leaf in use, with the CA that signed it.
struct Held {
    cert: IssuedCert,
    ca_fp: String,
    ca_end: OffsetDateTime,
}

#[derive(Debug, Default)]
struct Stats {
    /// Renewals of worker, edge serving and edge client certs.
    renewals: [usize; 3],
    /// Leaves issued with `notAfter` capped at their CA's end.
    capped: usize,
    /// Allocations refused because they would outlive the client CA.
    refused_allocations: usize,
    /// Self-heal replacements after the CA expired.
    replacements: usize,
}

/// Keeps one renewable leaf current; returns 1 when it renewed a held leaf.
/// `slack` is the time since the previous pass, so renewal can be that late.
fn hold(
    held: &mut Option<Held>,
    ca: &Ca,
    now: OffsetDateTime,
    slack: Duration,
    issue: impl FnOnce() -> IssuedCert,
    live: &mut Live,
    stats: &mut Stats,
) -> usize {
    let fp = ca.fingerprint_hex();
    if let Some(old) = held.as_ref() {
        let ca_changed = old.ca_fp != fp;
        if !ca_changed && !old.cert.needs_renewal(now) {
            return 0;
        }
        if ca_changed {
            // Only after self-heal, when the old CA (and so this leaf) ended.
            assert!(old.cert.not_after() <= now && old.ca_end <= now);
        } else {
            assert!(
                now < old.cert.not_after(),
                "{:?} renewed only after it expired",
                old.cert.profile()
            );
            if old.cert.not_after() < old.ca_end {
                assert!(
                    old.cert.not_after() - now >= min_left_at_renewal(old.cert.profile()) - slack,
                    "{:?} renewed with only {} left",
                    old.cert.profile(),
                    old.cert.not_after() - now
                );
            }
        }
    }
    let renewed = usize::from(held.is_some());
    let new = issue();
    assert!(new.not_before() <= now && now < new.not_after());
    assert!(new.not_after() <= ca.not_after());
    stats.capped += usize::from(new.not_after() == ca.not_after());
    live.push((new.profile(), new.cert_der().clone(), new.not_after()));
    *held = Some(Held {
        cert: new,
        ca_fp: fp,
        ca_end: ca.not_after(),
    });
    renewed
}

/// Runs the replica/reconciler loop from `start` for `horizon`, advancing the
/// fake clock by `step(i)`. The CAs are generated at [`t0`]; once they expire,
/// the load reports `Invalid(expired)` and the leader self-heals.
fn run(start: OffsetDateTime, horizon: Duration, mut step: impl FnMut(usize) -> Duration) -> Stats {
    let mut secret: BTreeMap<String, Vec<u8>> = CaBundle::generate(t0()).unwrap().to_secret_data();
    let wid = WorkerIdentity {
        namespace: sample().namespace,
        pool: sample().pool,
        pod: "default-0".into(),
        nonce: "00000000000000aa".into(),
    };
    let (mut worker, mut serving, mut edge_client) = (None, None, None);
    let mut stats = Stats::default();
    let mut live = Live::new();
    let mut now = start;
    let mut slack = Duration::ZERO;
    let mut i = 0;
    while now < start + horizon {
        // Every pass reads the Secret afresh, as each replica does.
        let b = match CaBundle::load(Some(&secret), now) {
            LoadOutcome::Valid(b) => b,
            LoadOutcome::Invalid(e) => {
                assert_eq!(e.reason(), "expired", "{e}");
                stats.replacements += 1;
                secret = CaBundle::generate(now).unwrap().to_secret_data();
                // Leaves of the old CAs ended with them.
                live.retain(|(_, _, na)| *na > now);
                assert!(live.is_empty(), "a leaf outlived its CA");
                match CaBundle::load(Some(&secret), now) {
                    LoadOutcome::Valid(b) => b,
                    other => panic!("{other:?}"),
                }
            }
            LoadOutcome::Missing => unreachable!(),
        };
        stats.renewals[0] += hold(
            &mut worker,
            b.internal_ca(),
            now,
            slack,
            || b.issue_worker_serving(&wid, now).unwrap(),
            &mut live,
            &mut stats,
        );
        stats.renewals[1] += hold(
            &mut serving,
            b.client_ca(),
            now,
            slack,
            || b.issue_edge_serving(&edge_names(), now).unwrap(),
            &mut live,
            &mut stats,
        );
        stats.renewals[2] += hold(
            &mut edge_client,
            b.internal_ca(),
            now,
            slack,
            || b.issue_edge_client(now).unwrap(),
            &mut live,
            &mut stats,
        );
        match b.issue_allocation(&sample(), LeafKey::Generate, now, now + Duration::hours(24)) {
            Ok(alloc) => {
                assert_eq!(alloc.renew_at(), None);
                live.push((alloc.profile(), alloc.cert_der().clone(), alloc.not_after()));
            }
            Err(IssueError::OutlivesCa) => {
                assert!(now + Duration::hours(24) > b.client_ca().not_after());
                stats.refused_allocations += 1;
            }
            Err(e) => panic!("{e}"),
        }

        // Every leaf ever issued and not yet expired still verifies.
        live.retain(|(_, _, na)| now <= *na);
        for (p, der, na) in &live {
            assert!(
                trusted(&b, *p, der, now),
                "{p:?} expiring {na} stopped verifying at {now}"
            );
        }
        // The leaves in use are valid now, so no renewal leaves a gap.
        for h in [&worker, &serving, &edge_client] {
            let h = &h.as_ref().unwrap().cert;
            assert!(h.not_before() <= now && now <= h.not_after());
        }

        slack = step(i);
        now += slack;
        i += 1;
    }
    stats
}

#[test]
fn leaves_renew_before_expiry_over_many_cycles() {
    // 100 days in 1-hour steps.
    let s = run(t0(), Duration::days(100), |_| Duration::hours(1));
    let [w, e, c] = s.renewals;
    assert!(w >= 4 && e >= 4, "worker {w}, serving {e}");
    assert!(c >= 140, "edge client {c}");
    assert_eq!((s.capped, s.refused_allocations, s.replacements), (0, 0, 0));
}

#[test]
fn leaves_renew_up_to_the_ca_end_then_self_heal() {
    // From 60 days before the CAs' notAfter to 5 days after, in 1-hour steps.
    let ca_end = t0() + CA_VALIDITY;
    let s = run(ca_end - Duration::days(60), Duration::days(65), |_| {
        Duration::hours(1)
    });
    assert_eq!(s.replacements, 1, "{s:?}");
    assert!(s.capped >= 3, "{s:?}");
    // Hourly passes strictly inside the CA's last 24 hours; at its end the
    // load reports expired and the fresh CA takes allocations again.
    assert_eq!(s.refused_allocations, 23, "{s:?}");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(6))]

    /// Irregular clocks (passes 1 minute to 4 hours apart, well under a third
    /// of the shortest renewable lifetime) never let a leaf lapse, including
    /// across the CAs' end.
    #[test]
    fn irregular_clock_never_lets_a_leaf_lapse(
        steps in proptest::collection::vec(60i64..4 * 3600, 64),
        near_end in any::<bool>(),
    ) {
        let start = if near_end { t0() + CA_VALIDITY - Duration::days(40) } else { t0() };
        let s = run(start, Duration::days(45), |i| Duration::seconds(steps[i % steps.len()]));
        prop_assert!(s.renewals.iter().all(|r| *r >= 1));
        prop_assert_eq!(s.replacements, usize::from(near_end));
    }
}
