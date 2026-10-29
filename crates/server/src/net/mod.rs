//! Connection-level helpers shared by the API and edge listeners.
//!
//! [`ProxyProtocol`] decides which address a connection belongs to. Off, or for
//! a source outside the trusted CIDRs, that is the socket's peer. From a trusted
//! source (a gateway or load balancer) the connection must open with a PROXY
//! protocol v2 header, and the header's source address replaces the peer for
//! the per-address caps and the logs.

mod cidr;
mod proxy_protocol;
#[cfg(test)]
mod tests;

pub use cidr::{Cidr, CidrError};
pub use proxy_protocol::{
    Client, HEADER_TIMEOUT, Header, MAX_ADDRESS_BLOCK, Origin, ProxyError, ProxyMetrics,
    ProxyProtocol, read_header,
};

use std::net::{IpAddr, Ipv6Addr};

/// The address the API's per-address caps count: IPv4 as is, an IPv4-mapped
/// IPv6 address as its IPv4 address, any other IPv6 address as its /56. A
/// routed delegation is usually a /56 or larger, so one subscriber is one
/// client. The edge's [`PeerKey`](crate::edge::PeerKey) groups at /64; masking
/// here first makes it group the /56.
#[must_use]
pub fn client_group(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => {
            let s = v6.segments();
            IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3] & 0xff00, 0, 0, 0, 0))
        }
        _ => ip,
    }
}
