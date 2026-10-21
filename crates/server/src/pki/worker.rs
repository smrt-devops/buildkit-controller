//! Worker serving identity: one name per worker pod incarnation.

use std::fmt;

use super::{Namespace, PoolName};

/// Length of a worker nonce in hex characters (64 random bits).
pub const NONCE_LEN: usize = 16;

/// The pod a worker serving cert names: namespace, pool, pod name and the
/// random nonce chosen before the pod was created. Slot pod names are reused,
/// so the nonce is what tells two pods of the same slot apart.
#[derive(Clone, PartialEq, Eq)]
pub struct WorkerIdentity {
    /// Pool namespace.
    pub namespace: Namespace,
    /// Pool name.
    pub pool: PoolName,
    /// Pod name, `<pool>-<slot>`.
    pub pod: String,
    /// Lowercase hex nonce, [`NONCE_LEN`] characters.
    pub nonce: String,
}

impl WorkerIdentity {
    /// The identity with a fresh nonce from the system CSPRNG.
    pub fn generate(
        namespace: Namespace,
        pool: PoolName,
        pod: String,
    ) -> Result<Self, ring::error::Unspecified> {
        use ring::rand::SecureRandom;
        let mut b = [0u8; NONCE_LEN / 2];
        ring::rand::SystemRandom::new().fill(&mut b)?;
        let nonce = super::ca::hex(&b);
        Ok(Self {
            namespace,
            pool,
            pod,
            nonce,
        })
    }

    /// The DNS SAN: `<nonce>.<pod>.<pool>.<namespace>.worker.buildkit.internal`.
    #[must_use]
    pub fn dns_name(&self) -> String {
        format!(
            "{}.{}.{}.{}.worker.buildkit.internal",
            self.nonce, self.pod, self.pool, self.namespace
        )
    }

    /// Name of the per-pod Secret, [`secret_name`].
    #[must_use]
    pub fn secret_name(&self) -> String {
        secret_name(&self.pod, &self.nonce)
    }
}

/// Whether `s` is a nonce [`WorkerIdentity::generate`] could have made:
/// [`NONCE_LEN`] lowercase hex characters.
#[must_use]
pub fn is_nonce(s: &str) -> bool {
    s.len() == NONCE_LEN && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

/// Name of a worker pod's Secret: `<pod>-<nonce>`.
#[must_use]
pub fn secret_name(pod: &str, nonce: &str) -> String {
    format!("{pod}-{nonce}")
}

impl fmt::Debug for WorkerIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.dns_name())
    }
}
