//! The allocation identity carried in the allocation cert's URI SAN.
//!
//! Every type here is a parsed value. Once constructed it matches the byte-exact
//! grammar, so a malformed identity cannot be represented.

use std::fmt;
use std::str::FromStr;

/// URI scheme prefix of the allocation SAN.
const SCHEME: &str = "buildkit://";

/// Why a name or URI did not match the allocation-ID grammar.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("invalid namespace")]
    Namespace,
    #[error("invalid pool name")]
    Pool,
    #[error("invalid pod uid")]
    PodUid,
    #[error("invalid allocation id")]
    AllocId,
    #[error("allocation URI does not match buildkit://NS/POOL/UID/ALLOC")]
    Uri,
}

/// `[a-z0-9]([-a-z0-9]{0,max-2}[a-z0-9])?`: a DNS-1123 label of at most `max` bytes.
fn is_label(s: &str, max: usize) -> bool {
    let b = s.as_bytes();
    let alnum = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    match (b.first(), b.last()) {
        (Some(first), Some(last)) => {
            b.len() <= max
                && alnum(first)
                && alnum(last)
                && b.iter().all(|c| alnum(c) || *c == b'-')
        }
        _ => false,
    }
}

/// `[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}`.
fn is_uid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_digit() || (b'a'..=b'f').contains(c),
        })
}

/// `[a-z2-7]{26}`.
fn is_alloc_id(s: &str) -> bool {
    s.len() == 26
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || (b'2'..=b'7').contains(&c))
}

macro_rules! name_type {
    ($(#[$doc:meta])* $name:ident, $check:expr, $err:expr) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            /// The validated string.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, IdError> {
                let check: fn(&str) -> bool = $check;
                if check(s) { Ok(Self(s.to_owned())) } else { Err($err) }
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;
            fn try_from(s: String) -> Result<Self, IdError> {
                let check: fn(&str) -> bool = $check;
                if check(&s) { Ok(Self(s)) } else { Err($err) }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

name_type!(
    /// A Kubernetes namespace (DNS-1123 label, at most 63 bytes).
    Namespace, |s| is_label(s, 63), IdError::Namespace
);
name_type!(
    /// A pool name (DNS-1123 label, at most 52 bytes).
    PoolName, |s| is_label(s, 52), IdError::Pool
);
name_type!(
    /// A pod `metadata.uid` (lowercase canonical UUID).
    PodUid, is_uid, IdError::PodUid
);
name_type!(
    /// An allocation id (26 chars of lowercase RFC 4648 base32).
    AllocId, is_alloc_id, IdError::AllocId
);

/// The identity bound into an allocation cert:
/// `buildkit://<namespace>/<pool>/<pod-uid>/<alloc-id>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AllocationId {
    pub namespace: Namespace,
    pub pool: PoolName,
    pub pod_uid: PodUid,
    pub alloc_id: AllocId,
}

impl AllocationId {
    /// The URI SAN, byte-exact.
    #[must_use]
    pub fn to_uri(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for AllocationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{SCHEME}{}/{}/{}/{}",
            self.namespace, self.pool, self.pod_uid, self.alloc_id
        )
    }
}

impl FromStr for AllocationId {
    type Err = IdError;

    /// Parses the URI byte-exactly, so no percent-encoding, port, query, fragment,
    /// userinfo, trailing slash or extra segment can pass the part grammars.
    fn from_str(s: &str) -> Result<Self, IdError> {
        let rest = s.strip_prefix(SCHEME).ok_or(IdError::Uri)?;
        let mut parts = rest.split('/');
        let (Some(ns), Some(pool), Some(uid), Some(alloc), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(IdError::Uri);
        };
        Ok(Self {
            namespace: ns.parse()?,
            pool: pool.parse()?,
            pod_uid: uid.parse()?,
            alloc_id: alloc.parse()?,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use proptest::prelude::*;

    pub(crate) fn arb_allocation_id() -> impl Strategy<Value = AllocationId> {
        (
            "[a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?",
            "[a-z0-9]([-a-z0-9]{0,50}[a-z0-9])?",
            "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
            "[a-z2-7]{26}",
        )
            .prop_map(|(ns, pool, uid, alloc)| AllocationId {
                namespace: Namespace::try_from(ns).unwrap(),
                pool: PoolName::try_from(pool).unwrap(),
                pod_uid: PodUid::try_from(uid).unwrap(),
                alloc_id: AllocId::try_from(alloc).unwrap(),
            })
    }

    pub(crate) fn sample() -> AllocationId {
        "buildkit://ci/default/0f8fad5b-d9cb-469f-a165-70867728950e/abcdefghijklmnopqrstuvwxyz"
            .parse()
            .unwrap()
    }

    #[test]
    fn rejects_malformed_uris() {
        let ok = sample().to_uri();
        let bad = [
            ok.replace("buildkit://", "https://"),
            ok.replace("buildkit://ci", "buildkit://ci:443"),
            ok.replace("buildkit://", "buildkit://u@"),
            format!("{ok}/"),
            format!("{ok}?x=1"),
            format!("{ok}#f"),
            ok.replace("/ci/", "/c%69/")
                .replace("buildkit://ci", "buildkit://c%69"),
            ok.replace("ci/", "CI/"),
            ok.replace("ci/", "-ci/"),
            ok.replace("/default/", "//"),
            ok.replace("0f8fad5b", "0F8FAD5B"),
            ok.replace("abcdefghijklmnopqrstuvwxyz", "abcdefghijklmnopqrstuvwxy1"),
            ok.replace("abcdefghijklmnopqrstuvwxyz", "abcdefghijklmnopqrstuvwxy"),
            format!("buildkit://{}/default/x/y", "a".repeat(64)),
            String::new(),
        ];
        for uri in bad {
            assert!(uri.parse::<AllocationId>().is_err(), "accepted {uri}");
        }
        assert!("a".repeat(53).parse::<PoolName>().is_err());
        assert!("a".repeat(52).parse::<PoolName>().is_ok());
        assert!("a".repeat(63).parse::<Namespace>().is_ok());
    }

    proptest! {
        #[test]
        fn uri_round_trips(id in arb_allocation_id()) {
            prop_assert_eq!(id.to_uri().parse::<AllocationId>().unwrap(), id);
        }
    }
}
