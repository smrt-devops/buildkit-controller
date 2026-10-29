//! Listener binding shared by the API, edge and ops servers.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;
use tracing::info;

/// Pending connections the kernel queues before `accept` (tokio's default).
const BACKLOG: i32 = 1024;

/// Bind a TCP listener on `addr`.
///
/// `0.0.0.0` means every address, of either family: the listener binds `[::]`
/// with `IPV6_V6ONLY` off, so it accepts IPv6 and IPv4 clients alike, and it
/// works on IPv6-only clusters. Where the host has no IPv6, it binds `0.0.0.0`.
/// IPv4 clients of a dual-stack listener arrive as IPv4-mapped IPv6 addresses;
/// pass each accepted peer through [`canonical`].
///
/// # Errors
///
/// The address cannot be bound.
pub fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    bind_with(addr, |port| {
        bind_socket(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port), true)
    })
}

fn bind_with(
    addr: SocketAddr,
    dual_stack: impl FnOnce(u16) -> io::Result<TcpListener>,
) -> io::Result<TcpListener> {
    if addr.ip() == IpAddr::V4(Ipv4Addr::UNSPECIFIED) {
        match dual_stack(addr.port()) {
            Ok(l) => return Ok(l),
            Err(e) => info!(error = %e, %addr, "IPv6 unavailable; listening on IPv4 only"),
        }
    }
    bind_socket(addr, false)
}

fn bind_socket(addr: SocketAddr, dual_stack: bool) -> io::Result<TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if dual_stack {
        socket.set_only_v6(false)?;
    }
    // As `TcpListener::bind` does: a restart can rebind while old connections linger.
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(BACKLOG)?;
    TcpListener::from_std(socket.into())
}

/// `peer` with an IPv4-mapped IPv6 address turned back into IPv4, so a client
/// of a dual-stack listener has the same address it has on an IPv4 one (for
/// per-address caps, trusted CIDRs and logs).
#[must_use]
pub fn canonical(peer: SocketAddr) -> SocketAddr {
    SocketAddr::new(peer.ip().to_canonical(), peer.port())
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpStream;

    use super::*;

    /// Accept one connection from `from` and return the peer as `canonical` sees it.
    async fn peer_via(l: &TcpListener, from: IpAddr) -> SocketAddr {
        let port = l.local_addr().unwrap().port();
        let (accepted, _client) = tokio::join!(l.accept(), TcpStream::connect((from, port)));
        canonical(accepted.unwrap().1)
    }

    fn has_ipv6() -> bool {
        std::net::TcpListener::bind("[::1]:0").is_ok()
    }

    #[tokio::test]
    async fn the_ipv4_wildcard_listens_on_both_families() {
        if !has_ipv6() {
            eprintln!("skipped: no IPv6 on this host");
            return;
        }
        let l = bind("0.0.0.0:0".parse().unwrap()).unwrap();
        assert_eq!(l.local_addr().unwrap().ip(), Ipv6Addr::UNSPECIFIED);
        let v4 = peer_via(&l, Ipv4Addr::LOCALHOST.into()).await;
        assert_eq!(v4.ip(), Ipv4Addr::LOCALHOST, "mapped back to IPv4");
        let v6 = peer_via(&l, Ipv6Addr::LOCALHOST.into()).await;
        assert_eq!(v6.ip(), Ipv6Addr::LOCALHOST);
    }

    #[tokio::test]
    async fn without_ipv6_the_ipv4_wildcard_falls_back_to_ipv4() {
        let l = bind_with("0.0.0.0:0".parse().unwrap(), |_| {
            Err(io::Error::from(io::ErrorKind::Unsupported))
        })
        .unwrap();
        assert_eq!(l.local_addr().unwrap().ip(), Ipv4Addr::UNSPECIFIED);
        let v4 = peer_via(&l, Ipv4Addr::LOCALHOST.into()).await;
        assert_eq!(v4.ip(), Ipv4Addr::LOCALHOST);
    }

    #[tokio::test]
    async fn other_addresses_bind_as_given() {
        let l = bind("127.0.0.1:0".parse().unwrap()).unwrap();
        assert_eq!(l.local_addr().unwrap().ip(), Ipv4Addr::LOCALHOST);
        if has_ipv6() {
            let l = bind("[::1]:0".parse().unwrap()).unwrap();
            assert_eq!(l.local_addr().unwrap().ip(), Ipv6Addr::LOCALHOST);
        }
    }
}
