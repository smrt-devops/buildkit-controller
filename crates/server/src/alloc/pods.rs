//! Pure pod logic of allocation: candidate selection, the claim and retire
//! patches, allocation ids and timestamps.

use k8s_openapi::api::core::v1::Pod;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Value, json};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

use crate::pki::{AllocId, ClientPublicKey};
use crate::render::{
    ANN_ALLOC, ANN_ALLOCATED_AT, ANN_CLAIM, ANN_CLAIM_UID, ANN_EXPIRES_AT, ANN_INTERNAL_CA,
    ANN_OWNER, ANN_WORKER_CERT_NOT_AFTER, DRAINING, FREE, LABEL_POOL, LABEL_STATE,
    LABEL_TEMPLATE_HASH,
};

/// At most this many warm candidates per claim.
pub const MAX_TRIES: usize = 8;
/// `worker-cert-not-after` must reach `expires-at` plus this.
pub const CERT_MARGIN: time::Duration = time::Duration::minutes(5);

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

/// JSON pointer for a metadata key (`/` becomes `~1`).
pub(super) fn ptr(section: &str, key: &str) -> String {
    format!(
        "/metadata/{section}/{}",
        key.replace('~', "~0").replace('/', "~1")
    )
}

/// What a bind writes onto the pod.
#[derive(Debug, Clone, Copy)]
pub struct Bind<'a> {
    /// Allocation id.
    pub alloc_id: &'a str,
    /// Owner string.
    pub owner: &'a str,
    /// `<namespace>/<name>` of the claim.
    pub claim: &'a str,
    /// The claim's uid.
    pub claim_uid: &'a str,
    /// `allocated-at`.
    pub now: &'a str,
    /// `expires-at`, the allocation's expiry.
    pub expires: &'a str,
}

/// The claim patch. The `test`s on uid and `alloc == free` make it lose to any
/// other writer, so a deposed leader cannot book a pod twice.
#[must_use]
pub fn claim_patch(uid: &str, b: &Bind<'_>) -> Value {
    json!([
        {"op": "test", "path": "/metadata/uid", "value": uid},
        {"op": "test", "path": ptr("annotations", ANN_ALLOC), "value": FREE},
        {"op": "replace", "path": ptr("annotations", ANN_ALLOC), "value": b.alloc_id},
        {"op": "replace", "path": ptr("labels", LABEL_STATE), "value": "allocated"},
        {"op": "add", "path": ptr("annotations", ANN_OWNER), "value": b.owner},
        {"op": "add", "path": ptr("annotations", ANN_CLAIM), "value": b.claim},
        {"op": "add", "path": ptr("annotations", ANN_CLAIM_UID), "value": b.claim_uid},
        {"op": "add", "path": ptr("annotations", ANN_ALLOCATED_AT), "value": b.now},
        {"op": "add", "path": ptr("annotations", ANN_EXPIRES_AT), "value": b.expires},
    ])
}

/// The retire patch. The `test`s make it lose to any other writer.
#[must_use]
pub fn retire_patch(uid: &str, expected: &str) -> Value {
    json!([
        {"op": "test", "path": "/metadata/uid", "value": uid},
        {"op": "test", "path": ptr("annotations", ANN_ALLOC), "value": expected},
        {"op": "replace", "path": ptr("annotations", ANN_ALLOC), "value": DRAINING},
        {"op": "replace", "path": ptr("labels", LABEL_STATE), "value": DRAINING},
    ])
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
    alloc_id_of(random_bytes::<16>()?)
}

/// The allocation id of an in-cluster claim: the first 16 bytes of SHA-256
/// over its uid, as [`new_alloc_id`] encodes them. Fixed for the claim, so a
/// leader that finds a pod carrying it knows the bind already landed.
#[must_use]
pub fn derived_alloc_id(uid: &str) -> Option<AllocId> {
    let d = ring::digest::digest(&ring::digest::SHA256, uid.as_bytes());
    alloc_id_of(d.as_ref().get(..16)?.try_into().ok()?)
}

/// The allocation id of an HTTP allocation: the first 16 bytes of SHA-256 over
/// the client's public key point, as [`new_alloc_id`] encodes them. The same
/// key always names the same claim, so a client that repeats its `POST` finds
/// the allocation it already made.
#[must_use]
pub fn key_alloc_id(key: &ClientPublicKey) -> Option<AllocId> {
    let d = ring::digest::digest(&ring::digest::SHA256, key.point());
    alloc_id_of(d.as_ref().get(..16)?.try_into().ok()?)
}

fn alloc_id_of(bytes: [u8; 16]) -> Option<AllocId> {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
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
