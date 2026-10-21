//! PKI: both CAs and every leaf profile.
//!
//! This module does no Kubernetes I/O and reads no clock. Callers pass `now` and
//! the CA Secret's data map, and the module returns certificates, keys and verdicts.

mod alloc_id;
mod ca;
mod leaf;
mod serving;
mod verify;
mod worker;

#[cfg(test)]
mod tests_lifecycle;
#[cfg(test)]
mod tests_profiles;

pub use alloc_id::{AllocId, AllocationId, IdError, Namespace, PodUid, PoolName};
pub use ca::{
    CA_EXPIRY_WARNING, CA_VALIDITY, CLIENT_CA_CRT, CLIENT_CA_KEY, Ca, CaBundle, CaError, CaKind,
    CertKind, GENERATED_BY, GENERATED_BY_VALUE, INTERNAL_CA_CRT, INTERNAL_CA_KEY, LoadOutcome,
};
pub use leaf::{
    ClientPublicKey, EdgeNames, IssueError, IssuedCert, LeafKey, LeafValidity, Profile,
};
pub use serving::{NOT_BEFORE_SKEW, ServingCertError, UserServingCert, sanitize_ca_pem};
pub use verify::{AllocCertError, verify_allocation_cert};
pub use worker::{
    NONCE_LEN, WorkerIdentity, is_nonce as is_worker_nonce, secret_name as worker_secret_name,
};

use ring::digest;
use time::{Duration, OffsetDateTime};

/// Backdating applied to every `notBefore`, for clock skew between the
/// controller, buildkitd and clients.
pub const BACKDATE: Duration = Duration::minutes(5);

/// `t + d`, or [`IssueError::TimeOverflow`].
fn checked_add(t: OffsetDateTime, d: Duration) -> Result<OffsetDateTime, IssueError> {
    t.checked_add(d).ok_or(IssueError::TimeOverflow)
}

/// `t - d`, or [`IssueError::TimeOverflow`].
fn checked_sub(t: OffsetDateTime, d: Duration) -> Result<OffsetDateTime, IssueError> {
    t.checked_sub(d).ok_or(IssueError::TimeOverflow)
}

/// SHA-256 digest.
fn sha256(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(digest::digest(&digest::SHA256, data).as_ref());
    out
}

/// 16 CSPRNG bytes, bit 7 of byte 0 cleared and bit 6 set (positive, fixed length).
fn random_serial() -> Result<[u8; 16], ring::error::Unspecified> {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 16];
    ring::rand::SystemRandom::new().fill(&mut b)?;
    b[0] = (b[0] & 0x7f) | 0x40;
    Ok(b)
}

/// `now` truncated to whole seconds (X.509 time precision).
fn whole_seconds(now: OffsetDateTime) -> OffsetDateTime {
    // Zero nanoseconds is always in range, so this never falls back.
    now.replace_nanosecond(0).unwrap_or(now)
}

/// The instant at which two thirds of `[not_before, not_after]` has elapsed,
/// rounded up to a whole second.
fn renew_at(not_before: OffsetDateTime, not_after: OffsetDateTime) -> OffsetDateTime {
    let total = i128::from((not_after - not_before).whole_seconds()).max(0);
    let two_thirds = (2 * total).div_euclid(3) + i128::from((2 * total).rem_euclid(3) != 0);
    let secs = i64::try_from(two_thirds).unwrap_or(i64::MAX);
    not_before.saturating_add(Duration::seconds(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashSet;

    #[test]
    fn renewal_boundary() {
        let nb = OffsetDateTime::UNIX_EPOCH;
        let na = nb + Duration::seconds(30);
        assert_eq!(renew_at(nb, na), nb + Duration::seconds(20));
        // 2/3 of 10 s = 6.67 s rounds up to 7 s, never early.
        assert_eq!(
            renew_at(nb, nb + Duration::seconds(10)),
            nb + Duration::seconds(7)
        );
        assert_eq!(renew_at(nb, nb), nb);
    }

    proptest! {
        #[test]
        fn serials_positive_fixed_length(_i in 0..256u32) {
            let s = random_serial().unwrap();
            prop_assert_eq!(s[0] & 0x80, 0);
            prop_assert_eq!(s[0] & 0x40, 0x40);
        }
    }

    #[test]
    fn serials_unique() {
        let set: HashSet<_> = (0..10_000).map(|_| random_serial().unwrap()).collect();
        assert_eq!(set.len(), 10_000);
    }
}
