//! Pure pod logic of the allocation service, covering candidate selection, free slots,
//! the claim and retire patches, allocation ids and timestamps.

use std::collections::BTreeSet;

use k8s_openapi::api::core::v1::Pod;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Value, json};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

use crate::pki::AllocId;
use crate::render::{
    ANN_ALLOC, ANN_ALLOCATED_AT, ANN_EXPIRES_AT, ANN_INTERNAL_CA, ANN_OWNER,
    ANN_WORKER_CERT_NOT_AFTER, LABEL_POOL, LABEL_STATE, LABEL_TEMPLATE_HASH,
};

/// At most this many warm candidates or cold slots per attempt.
pub const MAX_TRIES: usize = 8;
/// `worker-cert-not-after` must reach `expires-at` plus this.
pub const CERT_MARGIN: time::Duration = time::Duration::minutes(5);
/// `P alloc` of an unclaimed pod.
pub const FREE: &str = "free";
/// `P alloc` of a retired pod.
pub const DRAINING: &str = "draining";

/// An annotation value.
#[must_use]
pub fn ann<'a>(pod: &'a Pod, key: &str) -> Option<&'a str> {
    pod.metadata
        .annotations
        .as_ref()?
        .get(key)
        .map(String::as_str)
}

fn label<'a>(pod: &'a Pod, key: &str) -> Option<&'a str> {
    pod.metadata.labels.as_ref()?.get(key).map(String::as_str)
}

/// Condition `Ready=True` and a pod IP (the edge dials it).
#[must_use]
pub fn is_ready(pod: &Pod) -> bool {
    let Some(st) = pod.status.as_ref() else {
        return false;
    };
    let ready = st
        .conditions
        .iter()
        .flatten()
        .any(|c| c.type_ == "Ready" && c.status == "True");
    ready && st.pod_ip.as_deref().is_some_and(|ip| !ip.is_empty())
}

/// The pod belongs to the pool when the pool label, the namespace and a controller
/// ownerReference to the cached pool's uid all match.
#[must_use]
pub fn of_pool(pod: &Pod, namespace: &str, pool: &str, pool_uid: &str) -> bool {
    pod.metadata.namespace.as_deref() == Some(namespace)
        && label(pod, LABEL_POOL) == Some(pool)
        && pod
            .metadata
            .owner_references
            .iter()
            .flatten()
            .any(|o| o.controller == Some(true) && o.uid == pool_uid)
}

/// What a warm candidate must match.
#[derive(Debug, Clone)]
pub struct Want<'a> {
    pub template_hash: &'a str,
    /// First 16 hex chars of the loaded internal CA fingerprint.
    pub internal_ca: &'a str,
    /// `expires_at + 5m`.
    pub cert_not_after: OffsetDateTime,
}

/// Classifies one pod of the pool.
#[must_use]
pub fn is_candidate(pod: &Pod, want: &Want<'_>) -> bool {
    pod.metadata.deletion_timestamp.is_none()
        && pod.metadata.uid.is_some()
        && ann(pod, ANN_ALLOC) == Some(FREE)
        && is_ready(pod)
        && label(pod, LABEL_TEMPLATE_HASH) == Some(want.template_hash)
        && ann(pod, ANN_INTERNAL_CA) == Some(want.internal_ca)
        && ann(pod, ANN_WORKER_CERT_NOT_AFTER)
            .and_then(parse_ts)
            .is_some_and(|t| t >= want.cert_not_after)
}

/// The slot of `<pool>-<slot>` (decimal, no leading zeros).
#[must_use]
pub fn slot_of(name: &str, pool: &str) -> Option<u32> {
    let s = name.strip_prefix(pool)?.strip_prefix('-')?;
    let ok = !s.is_empty() && (s == "0" || !s.starts_with('0'));
    ok.then(|| s.parse().ok()).flatten()
}

/// `0..max-1` minus the slot of every pod name given (any state).
#[must_use]
pub fn free_slots<'a>(names: impl Iterator<Item = &'a str>, pool: &str, max: u32) -> Vec<u32> {
    let used: BTreeSet<u32> = names.filter_map(|n| slot_of(n, pool)).collect();
    (0..max).filter(|s| !used.contains(s)).collect()
}

/// JSON pointer for a metadata key (`/` becomes `~1`).
pub(super) fn ptr(section: &str, key: &str) -> String {
    format!(
        "/metadata/{section}/{}",
        key.replace('~', "~0").replace('/', "~1")
    )
}

/// The claim patch.
#[must_use]
pub fn claim_patch(uid: &str, alloc_id: &str, owner: &str, now: &str, expires: &str) -> Value {
    json!([
        {"op": "test", "path": "/metadata/uid", "value": uid},
        {"op": "test", "path": ptr("annotations", ANN_ALLOC), "value": FREE},
        {"op": "replace", "path": ptr("annotations", ANN_ALLOC), "value": alloc_id},
        {"op": "replace", "path": ptr("labels", LABEL_STATE), "value": "allocated"},
        {"op": "add", "path": ptr("annotations", ANN_OWNER), "value": owner},
        {"op": "add", "path": ptr("annotations", ANN_ALLOCATED_AT), "value": now},
        {"op": "add", "path": ptr("annotations", ANN_EXPIRES_AT), "value": expires},
    ])
}

/// The commit patch: moves `expires-at` from the claim's short value `from`
/// to the allocation's real expiry `to`. The test on `from` makes it lose to
/// a leader that retired the pod at `from`, and makes that retire lose to it.
#[must_use]
pub fn extend_patch(uid: &str, alloc_id: &str, from: &str, to: &str) -> Value {
    json!([
        {"op": "test", "path": "/metadata/uid", "value": uid},
        {"op": "test", "path": ptr("annotations", ANN_ALLOC), "value": alloc_id},
        {"op": "test", "path": ptr("annotations", ANN_EXPIRES_AT), "value": from},
        {"op": "replace", "path": ptr("annotations", ANN_EXPIRES_AT), "value": to},
    ])
}

/// The retire patch; `uid: None` only for the cold-create undo.
#[must_use]
pub fn retire_patch(uid: Option<&str>, expected: &str) -> Value {
    let mut ops = Vec::with_capacity(4);
    if let Some(uid) = uid {
        ops.push(json!({"op": "test", "path": "/metadata/uid", "value": uid}));
    }
    ops.extend([
        json!({"op": "test", "path": ptr("annotations", ANN_ALLOC), "value": expected}),
        json!({"op": "replace", "path": ptr("annotations", ANN_ALLOC), "value": DRAINING}),
        json!({"op": "replace", "path": ptr("labels", LABEL_STATE), "value": DRAINING}),
    ]);
    Value::Array(ops)
}

/// RFC 3339 UTC, seconds precision.
#[must_use]
pub fn fmt_ts(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// Parses `YYYY-MM-DDTHH:MM:SS[.frac]Z` (fraction dropped).
#[must_use]
pub fn parse_ts(s: &str) -> Option<OffsetDateTime> {
    let b = s.as_bytes();
    let tail = s.get(19..)?;
    let tail_ok = tail == "Z"
        || tail
            .strip_prefix('.')
            .and_then(|f| f.strip_suffix('Z'))
            .is_some_and(|f| !f.is_empty() && f.bytes().all(|c| c.is_ascii_digit()));
    if !tail_ok || b.get(4) != Some(&b'-') || b.get(10) != Some(&b'T') {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<u16>().ok();
    let date = Date::from_calendar_date(
        i32::from(n(0..4)?),
        Month::try_from(u8::try_from(n(5..7)?).ok()?).ok()?,
        u8::try_from(n(8..10)?).ok()?,
    )
    .ok()?;
    let hms = |r| n(r).and_then(|v| u8::try_from(v).ok());
    let time = Time::from_hms(hms(11..13)?, hms(14..16)?, hms(17..19)?).ok()?;
    Some(PrimitiveDateTime::new(date, time).assume_utc())
}

/// `now` truncated to whole seconds.
#[must_use]
pub fn whole_seconds(t: OffsetDateTime) -> OffsetDateTime {
    t.replace_nanosecond(0).unwrap_or(t)
}

fn random_bytes<const N: usize>() -> Option<[u8; N]> {
    let mut b = [0u8; N];
    SystemRandom::new().fill(&mut b).ok()?;
    Some(b)
}

/// 16 CSPRNG bytes, lowercase RFC 4648 base32, no padding (26 chars).
#[must_use]
pub fn new_alloc_id() -> Option<AllocId> {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let bytes = random_bytes::<16>()?;
    let (mut acc, mut bits, mut out) = (0u32, 0u32, String::with_capacity(26));
    for b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(ALPHABET[((acc >> bits) & 31) as usize]));
        }
    }
    if bits > 0 {
        out.push(char::from(ALPHABET[((acc << (5 - bits)) & 31) as usize]));
    }
    out.parse().ok()
}

/// Uniform Fisher-Yates shuffle from the OS CSPRNG (left as is if it fails).
pub fn shuffle<T>(v: &mut [T]) {
    for i in (1..v.len()).rev() {
        let Some(r) = random_bytes::<8>() else { return };
        let j = u64::from_le_bytes(r) % (i as u64 + 1);
        v.swap(i, usize::try_from(j).unwrap_or(i));
    }
}
