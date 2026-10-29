//! IP prefixes for the PROXY protocol trust list.

use std::net::IpAddr;
use std::str::FromStr;

/// A canonical CIDR block: `10.244.0.0/16`, `2001:db8::/48`. Host bits past the
/// prefix and IPv4-mapped IPv6 blocks are refused, so a list reads the way it
/// matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

/// Why a string is not a [`Cidr`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "`{0}` is not a canonical CIDR such as 10.0.0.0/8 or fd00::/8 (no bits set past the prefix, no IPv4-mapped IPv6)"
)]
pub struct CidrError(String);

impl FromStr for Cidr {
    type Err = CidrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || CidrError(s.to_owned());
        let (addr, prefix) = s.split_once('/').ok_or_else(bad)?;
        let addr: IpAddr = addr.parse().map_err(|_| bad())?;
        // Only plain decimal digits: `u8::from_str` would also take "+8".
        if prefix.is_empty() || prefix.len() > 3 || !prefix.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        let prefix: u8 = prefix.parse().map_err(|_| bad())?;
        let cidr = Self { addr, prefix };
        let canonical = match addr {
            IpAddr::V4(a) => prefix <= 32 && u32::from(a) & !mask32(prefix) == 0,
            IpAddr::V6(a) => {
                prefix <= 128
                    && a.to_ipv4_mapped().is_none()
                    && u128::from(a) & !mask128(prefix) == 0
            }
        };
        if canonical { Ok(cidr) } else { Err(bad()) }
    }
}

impl Cidr {
    /// Whether `ip` is inside the block. An IPv4-mapped IPv6 address (a peer of
    /// a dual-stack socket) matches as its IPv4 address.
    #[must_use]
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(a)) => {
                (u32::from(net) ^ u32::from(a)) & mask32(self.prefix) == 0
            }
            (IpAddr::V6(net), IpAddr::V6(a)) => {
                (u128::from(net) ^ u128::from(a)) & mask128(self.prefix) == 0
            }
            _ => false,
        }
    }

    /// Whether every address of `other` is inside this block.
    #[must_use]
    pub fn covers(&self, other: &Self) -> bool {
        self.prefix <= other.prefix && self.contains(other.addr)
    }

    /// The prefix length.
    #[must_use]
    pub fn prefix(&self) -> u8 {
        self.prefix
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

fn mask32(prefix: u8) -> u32 {
    u32::MAX
        .checked_shl(32 - u32::from(prefix.min(32)))
        .unwrap_or(0)
}

fn mask128(prefix: u8) -> u128 {
    u128::MAX
        .checked_shl(128 - u32::from(prefix.min(128)))
        .unwrap_or(0)
}
