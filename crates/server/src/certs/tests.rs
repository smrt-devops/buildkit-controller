use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::follow::{Config, Loader, RETRY, drive};
use super::{CaCertRule, CertMaterial, startup_decision};
use crate::pki::{CaBundle, EdgeNames, Namespace};
use crate::runtime::metrics::Metrics;
use crate::runtime::readiness::Readiness;
use crate::runtime::stores::SecretState;

const NAME: &str = "bk.example.com";

fn state(data: Option<BTreeMap<String, Vec<u8>>>, rv: &str) -> SecretState {
    let secret = data.map(|d| {
        let mut s = Secret {
            data: Some(d.into_iter().map(|(k, v)| (k, ByteString(v))).collect()),
            ..Secret::default()
        };
        s.metadata.resource_version = Some(rv.into());
        Arc::new(s)
    });
    SecretState {
        init_done: true,
        secret,
    }
}

fn absent() -> SecretState {
    state(None, "")
}

fn ca_state(b: &CaBundle, rv: &str) -> SecretState {
    state(Some(b.to_secret_data()), rv)
}

fn ca() -> CaBundle {
    CaBundle::generate(OffsetDateTime::now_utc()).unwrap()
}

fn names() -> EdgeNames {
    EdgeNames::new(NAME, &[], "bk", &"ns".parse::<Namespace>().unwrap()).unwrap()
}

fn tls_secret(b: &CaBundle, ca_crt: Option<&str>, rv: &str) -> SecretState {
    tls_secret_issued_at(b, ca_crt, rv, now())
}

/// Leaves are backdated 5 minutes, so `issued_at` = now + 7 min gives a
/// `notBefore` 2 minutes ahead of the clock.
fn tls_secret_issued_at(
    b: &CaBundle,
    ca_crt: Option<&str>,
    rv: &str,
    issued_at: OffsetDateTime,
) -> SecretState {
    let c = b.issue_edge_serving(&names(), issued_at).unwrap();
    let mut d = BTreeMap::from([
        ("tls.crt".to_owned(), c.cert_pem().as_bytes().to_vec()),
        (
            "tls.key".to_owned(),
            c.private_key_pem().unwrap().into_bytes(),
        ),
    ]);
    if let Some(v) = ca_crt {
        d.insert("ca.crt".into(), v.as_bytes().to_vec());
    }
    state(Some(d), rv)
}

struct T {
    loader: Loader,
    metrics: Arc<Metrics>,
    readiness: Arc<Readiness>,
}

fn t(user_serving: bool) -> T {
    let metrics = Arc::new(Metrics::new());
    let readiness = Arc::new(Readiness::with_spec_checks());
    let loader = Loader::new(
        Config {
            user_serving,
            server_name: NAME.into(),
            names: Ok(names()),
            validity: crate::pki::LeafValidity::default(),
        },
        Arc::clone(&metrics),
        readiness.register("ca_loaded"),
        readiness.register("serving_cert"),
    );
    T {
        loader,
        metrics,
        readiness,
    }
}

fn counter(m: &Metrics, needle: &str) -> u64 {
    m.render()
        .lines()
        .filter(|l| !l.starts_with('#') && l.contains(needle))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
        .sum()
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

#[test]
fn valid_ca_publishes_trust_issuer_client_and_serving() {
    let mut t = t(false);
    let b = ca();
    let s = t.loader.step(Some(&ca_state(&b, "1")), None, now());
    assert!(s.changed);
    let m = t.loader.snapshot();
    assert_eq!(m.client_ca(), Some(b.client_ca().cert_der()));
    assert_eq!(m.internal_ca(), Some(b.internal_ca().cert_der()));
    assert!(m.issuer().is_some() && m.serving().is_some() && m.edge_client().is_some());
    assert!(
        t.readiness
            .failing()
            .iter()
            .all(|c| c != "ca_loaded" && c != "serving_cert")
    );
    assert!(!t.readiness.failing().contains(&"serving_cert".into()));
    assert_eq!(counter(&t.metrics, "buildkit_ca_valid"), 1);
    assert_eq!(counter(&t.metrics, "cert_load_errors_total"), 0);
    // When self-issued, the client CA is the caCert.
    assert_eq!(m.ca_cert_pem(&b), Some(b.client_ca().cert_pem().to_owned()));
}

#[test]
fn missing_ca_is_not_ready_and_not_an_error() {
    let mut t = t(false);
    t.loader.step(Some(&absent()), None, now());
    let m = t.loader.snapshot();
    assert!(m.issuer().is_none() && m.serving().is_none());
    assert!(t.readiness.failing().contains(&"ca_loaded".into()));
    assert_eq!(counter(&t.metrics, "buildkit_ca_valid"), 0);
    assert_eq!(counter(&t.metrics, "cert_load_errors_total"), 0);
}

#[test]
fn invalid_ca_keeps_trust_withdraws_issuer_and_counts_once() {
    let mut t = t(false);
    let b = ca();
    t.loader.step(Some(&ca_state(&b, "1")), None, now());
    let bad = state(Some(BTreeMap::from([("junk".to_owned(), vec![1])])), "2");
    t.loader.step(Some(&bad), None, now());
    t.loader.step(Some(&bad), None, now()); // same resourceVersion, so a resync
    let m = t.loader.snapshot();
    assert_eq!(m.client_ca(), Some(b.client_ca().cert_der()));
    assert!(m.issuer().is_none());
    assert_eq!(counter(&t.metrics, "buildkit_ca_valid"), 0);
    assert_eq!(
        counter(&t.metrics, r#"cert_load_errors_total{cert="ca"}"#),
        1
    );
}

#[test]
fn ca_change_swaps_trust_and_reissues() {
    let mut t = t(false);
    let (a, b) = (ca(), ca());
    t.loader.step(Some(&ca_state(&a, "1")), None, now());
    let before = t.loader.snapshot().serving().unwrap();
    t.loader.step(Some(&ca_state(&b, "2")), None, now());
    let m = t.loader.snapshot();
    assert_eq!(m.client_ca(), Some(b.client_ca().cert_der()));
    assert!(!Arc::ptr_eq(&before, &m.serving().unwrap()));
}

#[test]
fn user_serving_secret_loads_reloads_and_rejects_once() {
    let mut t = t(true);
    let b = ca();
    t.loader
        .step(Some(&ca_state(&b, "1")), Some(&absent()), now());
    // The CA is loaded but the user cert has not arrived, so the edge is not serving.
    assert!(t.loader.snapshot().serving().is_none());
    assert!(t.readiness.failing().contains(&"serving_cert".into()));
    t.loader.step(
        None,
        Some(&tls_secret(&b, Some(b.client_ca().cert_pem()), "1")),
        now(),
    );
    let first = t.loader.snapshot().serving().unwrap();
    assert!(!t.readiness.failing().contains(&"serving_cert".into()));
    t.loader.step(
        None,
        Some(&tls_secret(&b, Some(b.internal_ca().cert_pem()), "2")),
        now(),
    );
    let second = t.loader.snapshot().serving().unwrap();
    assert!(!Arc::ptr_eq(&first, &second));
    // An invalid update keeps the previous cert and counts once, even when repeated.
    let bad = state(Some(BTreeMap::from([("tls.crt".to_owned(), vec![1])])), "3");
    t.loader.step(None, Some(&bad), now());
    t.loader.step(None, Some(&bad), now());
    assert!(Arc::ptr_eq(
        &second,
        &t.loader.snapshot().serving().unwrap()
    ));
    assert_eq!(
        counter(&t.metrics, r#"cert_load_errors_total{cert="edge_serving"}"#),
        1
    );
}

#[test]
fn ca_cert_tracks_last_known_ca_crt_and_never_falls_back_to_client_ca() {
    let mut t = t(true);
    let b = ca();
    // A cert configured but deleted before anyone saw it gives null, not the client CA.
    t.loader
        .step(Some(&ca_state(&b, "1")), Some(&absent()), now());
    assert_eq!(t.loader.snapshot().ca_cert_pem(&b), None);
    assert_eq!(
        t.loader.snapshot().issuer().map(|_| ()),
        Some(()),
        "allocation still has an issuer"
    );
    t.loader.step(
        None,
        Some(&tls_secret(&b, Some(b.internal_ca().cert_pem()), "1")),
        now(),
    );
    assert_eq!(
        t.loader.snapshot().ca_cert_pem(&b).as_deref(),
        Some(b.internal_ca().cert_pem())
    );
    // If it is deleted before the next allocation, the last known cert stays.
    t.loader.step(None, Some(&state(None, "")), now());
    t.loader.step(None, Some(&absent()), now());
    assert_eq!(
        t.loader.snapshot().ca_cert_pem(&b).as_deref(),
        Some(b.internal_ca().cert_pem())
    );
    // A Secret present without ca.crt gives null.
    t.loader.step(None, Some(&tls_secret(&b, None, "2")), now());
    assert_eq!(t.loader.snapshot().ca_cert_pem(&b), None);
    assert_eq!(
        t.loader.snapshot().rule_for_test(),
        CaCertRule::Configured(None)
    );
}

#[test]
fn invalid_user_secret_at_startup_exits_2_but_absent_waits() {
    let b = ca();
    let bad = state(Some(BTreeMap::from([("tls.crt".to_owned(), vec![1])])), "1");
    let e = startup_decision(true, &bad, NAME, now()).unwrap_err();
    assert!(e.to_string().contains("tls.crt or tls.key"));
    let wrong_name = tls_secret(&b, None, "1");
    assert!(startup_decision(true, &wrong_name, "other.example.com", now()).is_err());
    assert!(startup_decision(true, &wrong_name, NAME, now()).is_ok());
    assert!(startup_decision(true, &absent(), NAME, now()).is_ok());
    assert!(startup_decision(false, &bad, NAME, now()).is_ok());
}

#[tokio::test(start_paused = true)]
async fn failed_self_issue_is_retried_on_a_timer_not_at_renew_at() {
    let t = t(false);
    let fails = Arc::new(AtomicUsize::new(2));
    let calls = Arc::clone(&fails);
    let loader = t.loader.with_issue_serving(Box::new(move |b, n, now| {
        if calls
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1))
            .is_ok()
        {
            return Err("injected".into());
        }
        b.issue_edge_serving(n, now).map_err(|e| e.to_string())
    }));
    let b = ca();
    let (_ca_tx, ca_rx) = watch::channel(ca_state(&b, "1"));
    let (_srv_tx, srv_rx) = watch::channel(absent());
    let (tx, mut rx) = watch::channel(CertMaterial::empty());
    let stop = CancellationToken::new();
    let task = tokio::spawn(drive(loader, ca_rx, srv_rx, tx, stop.clone()));

    tokio::time::sleep(Duration::from_secs(1)).await;
    // The client cert and trust are published; the serving cert is missing.
    assert!(rx.borrow_and_update().edge_client().is_some());
    assert!(rx.borrow().serving().is_none());
    assert!(t.readiness.failing().contains(&"serving_cert".into()));
    tokio::time::sleep(RETRY).await; // second attempt fails too
    assert!(rx.borrow().serving().is_none());
    tokio::time::sleep(RETRY).await; // third succeeds
    assert!(rx.borrow().serving().is_some());
    assert!(!t.readiness.failing().contains(&"serving_cert".into()));
    // Two failures, one count.
    assert_eq!(
        counter(&t.metrics, r#"cert_load_errors_total{cert="edge_serving"}"#),
        1
    );
    stop.cancel();
    task.await.unwrap();
}

#[test]
fn unusable_ca_with_overdue_certs_never_yields_a_zero_wake() {
    let mut t = t(false);
    let b = ca();
    t.loader.step(Some(&ca_state(&b, "1")), None, now());
    t.loader.step(Some(&absent()), None, now());
    let late = now() + time::Duration::days(60);
    for _ in 0..3 {
        let s = t.loader.step(None, None, late);
        assert_eq!(s.wake, None);
    }
}

#[test]
fn rejected_user_update_leaves_cert_and_ca_cert_unchanged() {
    let mut t = t(true);
    let b = ca();
    t.loader
        .step(Some(&ca_state(&b, "1")), Some(&absent()), now());
    t.loader.step(
        None,
        Some(&tls_secret(&b, Some(b.client_ca().cert_pem()), "1")),
        now(),
    );
    let before = t.loader.snapshot().serving().unwrap();
    let bad = state(
        Some(BTreeMap::from([
            ("tls.crt".to_owned(), vec![1]),
            ("tls.key".to_owned(), vec![1]),
            (
                "ca.crt".to_owned(),
                b.internal_ca().cert_pem().as_bytes().to_vec(),
            ),
        ])),
        "2",
    );
    t.loader.step(None, Some(&bad), now());
    let m = t.loader.snapshot();
    assert!(Arc::ptr_eq(&before, &m.serving().unwrap()));
    assert_eq!(m.ca_cert_pem(&b).as_deref(), Some(b.client_ca().cert_pem()));
}

fn gauge(m: &Metrics, kind: &str) -> Option<i64> {
    let needle = format!("buildkit_cert_not_after_seconds{{kind=\"{kind}\"}}");
    m.render()
        .lines()
        .find(|l| l.starts_with(&needle))
        .and_then(|l| l.rsplit(' ').next()?.parse().ok())
}

/// Regression (soak): `buildkit_cert_not_after_seconds` was never exported,
/// so leaf renewals were invisible to monitoring.
#[test]
fn cert_not_after_gauges_follow_issue_and_renewal() {
    let mut t = t(false);
    assert_eq!(gauge(&t.metrics, "edge_serving"), None);
    let b = ca();
    t.loader.step(Some(&ca_state(&b, "1")), None, now());
    let ca_end = b.client_ca().not_after().unix_timestamp();
    assert_eq!(gauge(&t.metrics, "client_ca"), Some(ca_end));
    assert!(gauge(&t.metrics, "internal_ca").is_some());
    let serving = gauge(&t.metrics, "edge_serving").unwrap();
    let client = gauge(&t.metrics, "edge_client").unwrap();
    assert_eq!(client, (now() + time::Duration::hours(24)).unix_timestamp());
    // Past the renewal point both leaves are reissued and the gauges move.
    let later = now() + time::Duration::days(21);
    t.loader.step(None, None, later);
    assert!(gauge(&t.metrics, "edge_serving").unwrap() > serving);
    assert!(gauge(&t.metrics, "edge_client").unwrap() > client);
}

/// Review fix. A renewal time already due (too short a validity) never yields
/// a zero wake; the loop sleeps at least `MIN_WAKE`.
#[test]
fn due_renewal_never_yields_a_zero_wake() {
    let metrics = Arc::new(Metrics::new());
    let readiness = Arc::new(Readiness::with_spec_checks());
    let short = time::Duration::minutes(2);
    let mut loader = Loader::new(
        Config {
            user_serving: false,
            server_name: NAME.into(),
            names: Ok(names()),
            validity: crate::pki::LeafValidity {
                worker_serving: short,
                edge_serving: short,
                edge_client: short,
            },
        },
        metrics,
        readiness.register("ca_loaded"),
        readiness.register("serving_cert"),
    );
    let b = ca();
    let s = loader.step(Some(&ca_state(&b, "1")), None, now());
    assert!(
        s.wake.is_some_and(|w| w >= super::follow::MIN_WAKE),
        "{s:?}"
    );
    let s = loader.step(None, None, now());
    assert!(
        s.wake.is_some_and(|w| w >= super::follow::MIN_WAKE),
        "{s:?}"
    );
}

fn skewed(b: &CaBundle, minutes: i64, rv: &str) -> SecretState {
    // The 5-minute backdating puts notBefore at (minutes - 5) ahead of now.
    tls_secret_issued_at(b, None, rv, now() + time::Duration::minutes(minutes))
}

#[test]
fn not_before_skew_within_five_minutes_is_accepted() {
    let mut t = t(true);
    let b = ca();
    t.loader
        .step(Some(&ca_state(&b, "1")), Some(&absent()), now());
    t.loader.step(None, Some(&skewed(&b, 7, "1")), now());
    assert!(t.loader.snapshot().serving().is_some());
    assert_eq!(
        counter(&t.metrics, r#"cert_load_errors_total{cert="edge_serving"}"#),
        0
    );
}

#[test]
fn startup_with_a_future_cert_does_not_exit_and_stays_not_ready() {
    let b = ca();
    let future = skewed(&b, 65, "1");
    assert!(startup_decision(true, &future, NAME, now()).is_ok());
    let mut t = t(true);
    t.loader
        .step(Some(&ca_state(&b, "1")), Some(&future), now());
    assert!(t.loader.snapshot().serving().is_none());
    assert!(t.readiness.failing().contains(&"serving_cert".into()));
}

#[test]
fn far_future_cert_keeps_previous_and_is_rechecked_at_not_before() {
    let mut t = t(true);
    let b = ca();
    t.loader
        .step(Some(&ca_state(&b, "1")), Some(&absent()), now());
    t.loader.step(None, Some(&tls_secret(&b, None, "1")), now());
    let old = t.loader.snapshot().serving().unwrap();
    let s = t.loader.step(None, Some(&skewed(&b, 65, "2")), now());
    assert!(Arc::ptr_eq(&old, &t.loader.snapshot().serving().unwrap()));
    assert_eq!(
        counter(&t.metrics, r#"cert_load_errors_total{cert="edge_serving"}"#),
        1
    );
    let wake = s.wake.unwrap();
    assert!(wake > Duration::from_mins(59) && wake <= Duration::from_hours(1));
    // Nothing happens before it is due, and no wake is ever zero.
    let s = t
        .loader
        .step(None, None, now() + time::Duration::minutes(30));
    assert!(!s.changed && s.wake.unwrap() >= Duration::from_secs(1));
    assert!(Arc::ptr_eq(&old, &t.loader.snapshot().serving().unwrap()));
    // At notBefore the same Secret version is validated again and swapped in.
    let s = t
        .loader
        .step(None, None, now() + time::Duration::minutes(61));
    assert!(s.changed);
    assert!(!Arc::ptr_eq(&old, &t.loader.snapshot().serving().unwrap()));
    assert_eq!(
        counter(&t.metrics, r#"cert_load_errors_total{cert="edge_serving"}"#),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn future_cert_is_rechecked_by_the_driver_without_spinning() {
    let b = ca();
    let t = t(true);
    let metrics = Arc::clone(&t.metrics);
    let (_ca_tx, ca_rx) = watch::channel(ca_state(&b, "1"));
    // Paused tokio time and the wall clock move apart, so make the cert due
    // on the tokio clock: notBefore is 10 minutes ahead of the wall clock.
    let (_srv_tx, srv_rx) = watch::channel(skewed(&b, 15, "1"));
    let (tx, mut rx) = watch::channel(CertMaterial::empty());
    let stop = CancellationToken::new();
    let task = tokio::spawn(drive(t.loader, ca_rx, srv_rx, tx, stop.clone()));
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(rx.borrow_and_update().serving().is_none());
    // Many virtual minutes pass in a few wakes: one error, not a stream.
    tokio::time::sleep(Duration::from_mins(5)).await;
    assert_eq!(
        counter(&metrics, r#"cert_load_errors_total{cert="edge_serving"}"#),
        1
    );
    stop.cancel();
    task.await.unwrap();
}

#[test]
fn ca_crt_with_a_key_block_publishes_only_the_certificate() {
    let mut t = t(true);
    let b = ca();
    let mut d = BTreeMap::new();
    let c = b.issue_edge_serving(&names(), now()).unwrap();
    let key = c.private_key_pem().unwrap();
    let mixed = format!("{key}{}", b.internal_ca().cert_pem());
    d.insert("tls.crt".to_owned(), c.cert_pem().as_bytes().to_vec());
    d.insert("tls.key".to_owned(), key.into_bytes());
    d.insert("ca.crt".to_owned(), mixed.into_bytes());
    t.loader
        .step(Some(&ca_state(&b, "1")), Some(&state(Some(d), "1")), now());
    let pem = t.loader.snapshot().ca_cert_pem(&b).unwrap();
    assert_eq!(pem, b.internal_ca().cert_pem());
    assert!(!pem.contains("PRIVATE KEY"));
    // Garbage alone leaves nothing to publish.
    assert_eq!(crate::pki::sanitize_ca_pem(b"NEW-CA"), None);
}

#[test]
fn future_cert_for_the_wrong_name_still_exits_2() {
    let b = ca();
    let future = skewed(&b, 65, "1");
    assert!(startup_decision(true, &future, "other.example.com", now()).is_err());
    assert!(startup_decision(true, &future, NAME, now()).is_ok());
}

#[test]
fn newer_valid_secret_wins_over_a_held_future_one() {
    let mut t = t(true);
    let b = ca();
    t.loader
        .step(Some(&ca_state(&b, "1")), Some(&absent()), now());
    t.loader.step(None, Some(&skewed(&b, 65, "2")), now());
    t.loader.step(None, Some(&tls_secret(&b, None, "3")), now());
    let v3 = t.loader.snapshot().serving().unwrap();
    let s = t
        .loader
        .step(None, None, now() + time::Duration::minutes(61));
    assert!(!s.changed);
    assert!(Arc::ptr_eq(&v3, &t.loader.snapshot().serving().unwrap()));
}
