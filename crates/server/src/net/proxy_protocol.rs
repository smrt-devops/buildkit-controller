//! PROXY protocol v2 (the binary form of the `HAProxy` spec) on the listeners.
//!
//! Only v2 over TCP is read. The v1 text form is not supported: Envoy, `HAProxy`
//! and the cloud load balancers all send v2, and v1 adds a text parser for no
//! gain. A v1 header fails the signature check.
//!
//! Trust is decided on the socket's peer address, before a byte is read:
//!
//! - Off, or a peer outside the trusted CIDRs: the connection is direct. Nothing
//!   is parsed, so header bytes from such a peer reach TLS and fail the
//!   handshake. A client cannot claim another address.
//! - A peer inside the trusted CIDRs: the connection must open with a valid v2
//!   header within the header timeout, or it is closed. There is no fallback to
//!   the peer address, so a misconfigured gateway fails loudly instead of
//!   putting every client behind its own address.
//!
//! A `PROXY` command over TCP/IPv4 or TCP/IPv6 makes the header's source the
//! client address. A `LOCAL` command (a gateway's own health check) is accepted
//! and the connection keeps the gateway's address, as the spec asks. So is a
//! `PROXY` command with the `AF_UNSPEC` family, which some load balancers send
//! for their own probes: the spec says to ignore its address block and lets the
//! receiver use the real endpoint addresses. Other families (UNIX, UDP) are
//! refused: neither can carry a client of these TCP listeners. TLVs are skipped.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use tokio::io::{AsyncRead, AsyncReadExt};

use super::cidr::{Cidr, CidrError};
use crate::runtime::metrics::Metrics;

/// The 12-byte v2 signature.
const SIGNATURE: [u8; 12] = *b"\r\n\r\n\0\r\nQUIT\n";
/// Signature, version/command, family/transport and the 2-byte length.
const FIXED_LEN: usize = 16;
/// The longest address block (addresses and TLVs) accepted. The largest address
/// form is 216 bytes (UNIX); the rest leaves room for the TLVs AWS and GCP load
/// balancers add. The length field could claim up to 64 KiB.
pub const MAX_ADDRESS_BLOCK: usize = 1024;
/// Default for the header timeout (`*.proxyProtocol.headerTimeout`).
pub const HEADER_TIMEOUT: Duration = Duration::from_secs(5);

/// A parsed v2 header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Header {
    /// `LOCAL`, or `PROXY` with `AF_UNSPEC`: the sender's own connection, e.g. a
    /// health check.
    Local,
    /// `PROXY` over TCP: the original client and the address it dialled.
    Proxy {
        /// The client.
        source: SocketAddr,
        /// The address the client connected to on the proxy.
        destination: SocketAddr,
    },
}

/// Why a trusted connection was closed before TLS.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// The header did not arrive in time.
    #[error("no complete PROXY header within {0:?}")]
    Timeout(Duration),
    /// The connection ended inside the header.
    #[error("connection closed inside the PROXY header")]
    Truncated,
    /// Reading the socket failed.
    #[error("reading the PROXY header: {0}")]
    Io(std::io::Error),
    /// The first 12 bytes are not the v2 signature (a TLS `ClientHello`, or v1).
    #[error("no PROXY protocol v2 signature")]
    Signature,
    /// A version other than 2.
    #[error("PROXY protocol version {0}, want 2")]
    Version(u8),
    /// A command other than `LOCAL` (0) or `PROXY` (1).
    #[error("unknown PROXY command {0}")]
    Command(u8),
    /// The address block is longer than [`MAX_ADDRESS_BLOCK`].
    #[error("PROXY address block of {0} bytes, more than {MAX_ADDRESS_BLOCK}")]
    Oversized(usize),
    /// The address block is shorter than its family's addresses.
    #[error("PROXY address block of {len} bytes is too short for family {family:#04x}")]
    Short {
        /// Family and transport byte.
        family: u8,
        /// Declared block length.
        len: usize,
    },
    /// `PROXY` with a family other than TCP over IPv4 or IPv6.
    #[error("PROXY family {0:#04x} is not TCP over IPv4 or IPv6")]
    Family(u8),
}

impl ProxyError {
    /// The `result` label of the header counter.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Timeout(_) => "timeout",
            Self::Truncated => "truncated",
            Self::Io(_) => "io_error",
            Self::Signature => "bad_signature",
            Self::Version(_) => "bad_version",
            Self::Command(_) => "bad_command",
            Self::Oversized(_) => "oversized",
            Self::Short { .. } => "short",
            Self::Family(_) => "unsupported_family",
        }
    }
}

impl From<std::io::Error> for ProxyError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            Self::Truncated
        } else {
            Self::Io(e)
        }
    }
}

/// Reads one v2 header from `io` and nothing past it, so the next byte is the
/// first of the TLS handshake. The caller bounds the time.
pub async fn read_header<R: AsyncRead + Unpin>(io: &mut R) -> Result<Header, ProxyError> {
    let mut fixed = [0u8; FIXED_LEN];
    // The signature first: a peer that sends something else is refused without
    // waiting for 16 bytes.
    io.read_exact(&mut fixed[..SIGNATURE.len()]).await?;
    if fixed[..SIGNATURE.len()] != SIGNATURE {
        return Err(ProxyError::Signature);
    }
    io.read_exact(&mut fixed[SIGNATURE.len()..]).await?;
    let version = fixed[12] >> 4;
    let command = fixed[12] & 0x0f;
    let family = fixed[13];
    let len = usize::from(u16::from_be_bytes([fixed[14], fixed[15]]));
    if version != 2 {
        return Err(ProxyError::Version(version));
    }
    if command > 1 {
        return Err(ProxyError::Command(command));
    }
    if len > MAX_ADDRESS_BLOCK {
        return Err(ProxyError::Oversized(len));
    }
    let mut block = [0u8; MAX_ADDRESS_BLOCK];
    let block = &mut block[..len];
    io.read_exact(block).await?;
    // LOCAL, or PROXY with AF_UNSPEC (any transport): no address to use.
    if command == 0 || family >> 4 == 0 {
        return Ok(Header::Local);
    }
    let short = || ProxyError::Short { family, len };
    match family {
        // TCP over IPv4: 4 + 4 address bytes, 2 + 2 port bytes.
        0x11 => {
            let b: &[u8; 12] = block
                .get(..12)
                .and_then(|b| b.try_into().ok())
                .ok_or_else(short)?;
            let ip = |o: usize| IpAddr::V4(Ipv4Addr::new(b[o], b[o + 1], b[o + 2], b[o + 3]));
            Ok(Header::Proxy {
                source: SocketAddr::new(ip(0), u16::from_be_bytes([b[8], b[9]])),
                destination: SocketAddr::new(ip(4), u16::from_be_bytes([b[10], b[11]])),
            })
        }
        // TCP over IPv6: 16 + 16 address bytes, 2 + 2 port bytes.
        0x21 => {
            let b: &[u8; 36] = block
                .get(..36)
                .and_then(|b| b.try_into().ok())
                .ok_or_else(short)?;
            let ip = |o: usize| {
                let mut a = [0u8; 16];
                a.copy_from_slice(&b[o..o + 16]);
                IpAddr::V6(Ipv6Addr::from(a))
            };
            Ok(Header::Proxy {
                source: SocketAddr::new(ip(0), u16::from_be_bytes([b[32], b[33]])),
                destination: SocketAddr::new(ip(16), u16::from_be_bytes([b[34], b[35]])),
            })
        }
        f => Err(ProxyError::Family(f)),
    }
}

/// How a connection's client address was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The socket's peer: PROXY protocol off, or an untrusted peer.
    Direct,
    /// A `LOCAL` header from a trusted peer; the address is that peer's.
    Local,
    /// A `PROXY` header from the trusted peer `via`.
    Proxied {
        /// The gateway's address.
        via: SocketAddr,
    },
}

impl Origin {
    /// The `result` label of the header counter (`direct` is never counted).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Local => "local",
            Self::Proxied { .. } => "proxied",
        }
    }

    /// The gateway's address, for a proxied connection.
    #[must_use]
    pub fn via(self) -> Option<SocketAddr> {
        match self {
            Self::Proxied { via } => Some(via),
            Self::Direct | Self::Local => None,
        }
    }
}

/// The address a connection counts and logs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Client {
    /// The client: the PROXY source when proxied, else the socket's peer.
    pub addr: SocketAddr,
    /// Where `addr` came from.
    pub origin: Origin,
}

/// `buildkit_<listener>_proxy_protocol_headers_total{result}`: one per
/// connection from a trusted source. `result` is `proxied`, `local`, or the
/// reason the connection was closed ([`ProxyError::label`]).
#[derive(Debug, Clone, Default)]
pub struct ProxyMetrics(Family<Vec<(&'static str, String)>, Counter>);

impl ProxyMetrics {
    /// Registers the family for `listener` (`api` or `edge`).
    #[must_use]
    pub fn register(metrics: &Metrics, listener: &'static str) -> Self {
        let m = Self::default();
        metrics.with_registry(|r| {
            r.register(
                format!("{listener}_proxy_protocol_headers"),
                "PROXY protocol headers from trusted sources, by result",
                m.0.clone(),
            );
        });
        m
    }

    fn inc(&self, result: &'static str) {
        self.0
            .get_or_create(&vec![("result", result.to_owned())])
            .inc();
    }

    /// The count for one result (tests, debugging).
    #[must_use]
    pub fn count(&self, result: &'static str) -> u64 {
        self.0
            .get_or_create(&vec![("result", result.to_owned())])
            .get()
    }
}

#[derive(Debug)]
struct Trusted {
    cidrs: Vec<Cidr>,
    header_timeout: Duration,
    metrics: ProxyMetrics,
}

/// One listener's PROXY protocol setting; [`ProxyProtocol::off`] by default.
#[derive(Debug, Clone, Default)]
pub struct ProxyProtocol(Option<Arc<Trusted>>);

impl ProxyProtocol {
    /// Every connection is direct.
    #[must_use]
    pub fn off() -> Self {
        Self(None)
    }

    /// Connections from `cidrs` must carry a header within `header_timeout`.
    #[must_use]
    pub fn trusting(cidrs: Vec<Cidr>, header_timeout: Duration, metrics: ProxyMetrics) -> Self {
        Self(Some(Arc::new(Trusted {
            cidrs,
            header_timeout,
            metrics,
        })))
    }

    /// From `api.proxyProtocol` or `edge.proxyProtocol`; registers the counter
    /// for `listener` when enabled.
    pub fn from_config(
        c: &crate::config::ProxyProtocol,
        metrics: &Metrics,
        listener: &'static str,
    ) -> Result<Self, CidrError> {
        if !c.enabled {
            return Ok(Self::off());
        }
        let cidrs = c
            .trusted_cidrs
            .iter()
            .map(|s| s.parse())
            .collect::<Result<Vec<Cidr>, _>>()?;
        let timeout = c
            .header_timeout
            .seconds()
            .map_or(HEADER_TIMEOUT, Duration::from_secs);
        Ok(Self::trusting(
            cidrs,
            timeout,
            ProxyMetrics::register(metrics, listener),
        ))
    }

    /// Whether any source is trusted.
    #[must_use]
    pub fn is_on(&self) -> bool {
        self.0.is_some()
    }

    /// Whether a connection from `ip` must carry a header.
    #[must_use]
    pub fn trusts(&self, ip: IpAddr) -> bool {
        self.0
            .as_deref()
            .is_some_and(|t| t.cidrs.iter().any(|c| c.contains(ip)))
    }

    /// The trusted CIDRs, for the startup log.
    #[must_use]
    pub fn trusted(&self) -> Vec<String> {
        self.0.as_deref().map_or_else(Vec::new, |t| {
            t.cidrs.iter().map(ToString::to_string).collect()
        })
    }

    /// The client of a connection whose socket peer is `peer`. Reads the
    /// header from `io` only when `peer` is trusted, and then consumes exactly
    /// the header. Counts every trusted connection.
    pub async fn client<R: AsyncRead + Unpin>(
        &self,
        io: &mut R,
        peer: SocketAddr,
    ) -> Result<Client, ProxyError> {
        let direct = Client {
            addr: peer,
            origin: Origin::Direct,
        };
        let Some(t) = self.0.as_deref() else {
            return Ok(direct);
        };
        if !t.cidrs.iter().any(|c| c.contains(peer.ip())) {
            return Ok(direct);
        }
        let read = tokio::time::timeout(t.header_timeout, read_header(io)).await;
        let r = match read {
            Ok(Ok(Header::Local)) => Ok(Client {
                addr: peer,
                origin: Origin::Local,
            }),
            Ok(Ok(Header::Proxy { source, .. })) => Ok(Client {
                addr: SocketAddr::new(source.ip().to_canonical(), source.port()),
                origin: Origin::Proxied { via: peer },
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(ProxyError::Timeout(t.header_timeout)),
        };
        t.metrics.inc(match &r {
            Ok(c) => c.origin.label(),
            Err(e) => e.label(),
        });
        r
    }
}
