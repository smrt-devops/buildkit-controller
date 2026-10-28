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
