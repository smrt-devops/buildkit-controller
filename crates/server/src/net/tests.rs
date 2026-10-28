#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::runtime::metrics::Metrics;

const SIG: &[u8; 12] = b"\r\n\r\n\0\r\nQUIT\n";

/// A v2 header: `ver_cmd`, `family`, then `block` with its length.
fn header(ver_cmd: u8, family: u8, block: &[u8]) -> Vec<u8> {
    let mut h = SIG.to_vec();
    h.push(ver_cmd);
    h.push(family);
    h.extend_from_slice(&u16::try_from(block.len()).unwrap().to_be_bytes());
    h.extend_from_slice(block);
    h
}

/// A `PROXY` header over TCP for `src` -> `dst`, plus `tlvs`.
fn proxy(src: SocketAddr, dst: SocketAddr, tlvs: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    let family = match (src.ip(), dst.ip()) {
        (IpAddr::V4(s), IpAddr::V4(d)) => {
            b.extend_from_slice(&s.octets());
            b.extend_from_slice(&d.octets());
            0x11
        }
        (IpAddr::V6(s), IpAddr::V6(d)) => {
            b.extend_from_slice(&s.octets());
            b.extend_from_slice(&d.octets());
            0x21
        }
        _ => panic!("mixed families"),
    };
    b.extend_from_slice(&src.port().to_be_bytes());
    b.extend_from_slice(&dst.port().to_be_bytes());
    b.extend_from_slice(tlvs);
    header(0x21, family, &b)
}

fn sa(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

/// The first bytes of a TLS 1.2+ `ClientHello` record.
const CLIENT_HELLO: &[u8] = &[
    0x16, 0x03, 0x01, 0x00, 0xf4, 0x01, 0x00, 0x00, 0xf0, 0x03, 0x03,
];

async fn parse(bytes: &[u8]) -> Result<Header, ProxyError> {
    let mut r = bytes;
    read_header(&mut r).await
}

#[tokio::test]
async fn v2_ipv4_header_and_nothing_past_it() {
    let mut wire = proxy(sa("192.0.2.7:51000"), sa("10.0.0.1:8443"), &[]);
    wire.extend_from_slice(CLIENT_HELLO);
    let mut r = wire.as_slice();
    let h = read_header(&mut r).await.unwrap();
    assert_eq!(
        h,
        Header::Proxy {
            source: sa("192.0.2.7:51000"),
            destination: sa("10.0.0.1:8443"),
        }
    );
    assert_eq!(r, CLIENT_HELLO, "the TLS bytes are left for the handshake");
}

#[tokio::test]
async fn v2_ipv6_header() {
    let h = parse(&proxy(sa("[2001:db8::7]:4000"), sa("[fd00::1]:8444"), &[]))
        .await
        .unwrap();
    assert_eq!(
        h,
        Header::Proxy {
            source: sa("[2001:db8::7]:4000"),
            destination: sa("[fd00::1]:8444"),
        }
    );
}

#[tokio::test]
async fn tlvs_are_skipped_up_to_the_size_cap() {
    // An AWS-style TLV (type 0xEA) after the addresses.
    let tlv = [0xEA, 0x00, 0x05, 0x01, b'v', b'p', b'c', b'e'];
    let mut wire = proxy(sa("192.0.2.7:1"), sa("10.0.0.1:2"), &tlv);
    wire.push(0x16);
    let mut r = wire.as_slice();
    assert!(matches!(
        read_header(&mut r).await.unwrap(),
        Header::Proxy { .. }
    ));
    assert_eq!(r, [0x16]);
    // Exactly MAX_ADDRESS_BLOCK bytes is still accepted.
    let pad = vec![0u8; MAX_ADDRESS_BLOCK - 12];
    let h = parse(&proxy(sa("192.0.2.7:1"), sa("10.0.0.1:2"), &pad)).await;
    assert!(matches!(h, Ok(Header::Proxy { .. })), "{h:?}");
}

#[tokio::test]
async fn local_command_ignores_the_address_block() {
    assert_eq!(
        parse(&header(0x20, 0x00, &[])).await.unwrap(),
        Header::Local
    );
    // LOCAL with a family and addresses: still LOCAL, and the block is consumed.
    let mut wire = header(0x20, 0x11, &[9; 12]);
    wire.push(0x16);
    let mut r = wire.as_slice();
    assert_eq!(read_header(&mut r).await.unwrap(), Header::Local);
    assert_eq!(r, [0x16]);
}

#[tokio::test]
async fn truncated_headers() {
    let full = proxy(sa("192.0.2.7:1"), sa("10.0.0.1:2"), &[]);
    for cut in [0, 5, 12, 15, 16, 20, full.len() - 1] {
        let e = parse(&full[..cut]).await.unwrap_err();
        assert!(matches!(e, ProxyError::Truncated), "cut {cut}: {e:?}");
        assert_eq!(e.label(), "truncated");
    }
}

#[tokio::test]
async fn oversized_block_is_refused_before_reading_it() {
    let mut wire = SIG.to_vec();
    wire.extend_from_slice(&[0x21, 0x11]);
    wire.extend_from_slice(&u16::try_from(MAX_ADDRESS_BLOCK + 1).unwrap().to_be_bytes());
    // No block follows: the length alone is refused.
    let e = parse(&wire).await.unwrap_err();
    assert!(
        matches!(e, ProxyError::Oversized(n) if n == MAX_ADDRESS_BLOCK + 1),
        "{e:?}"
    );
    wire.truncate(14);
    wire.extend_from_slice(&u16::MAX.to_be_bytes());
    assert_eq!(parse(&wire).await.unwrap_err().label(), "oversized");
}

#[tokio::test]
async fn wrong_signature_tls_and_v1_are_refused() {
    let mut hello = CLIENT_HELLO.to_vec();
    hello.extend_from_slice(&[0; 32]);
    assert!(matches!(
        parse(&hello).await.unwrap_err(),
        ProxyError::Signature
    ));
    let v1 = b"PROXY TCP4 192.0.2.7 10.0.0.1 51000 8443\r\n";
    assert_eq!(parse(v1).await.unwrap_err().label(), "bad_signature");
    let mut bent = proxy(sa("192.0.2.7:1"), sa("10.0.0.1:2"), &[]);
    bent[11] = b'X';
    assert_eq!(parse(&bent).await.unwrap_err().label(), "bad_signature");
}

#[tokio::test]
async fn bad_version_command_family_and_short_blocks() {
    let block = [0u8; 36];
    for (ver_cmd, family, len, label) in [
        (0x11, 0x11, 12, "bad_version"),
        (0x31, 0x11, 12, "bad_version"),
        (0x22, 0x11, 12, "bad_command"),
        (0x2f, 0x11, 12, "bad_command"),
        (0x21, 0x12, 12, "unsupported_family"), // UDP over IPv4
        (0x21, 0x22, 36, "unsupported_family"), // UDP over IPv6
        (0x21, 0x31, 36, "unsupported_family"), // UNIX stream
        (0x21, 0x00, 0, "unsupported_family"),  // AF_UNSPEC under PROXY
        (0x21, 0x10, 12, "unsupported_family"), // IPv4, transport unspecified
        (0x21, 0x11, 11, "short"),
        (0x21, 0x21, 35, "short"),
        (0x21, 0x21, 12, "short"),
    ] {
        let e = parse(&header(ver_cmd, family, &block[..len]))
            .await
            .unwrap_err();
        assert_eq!(e.label(), label, "{ver_cmd:#04x} {family:#04x} {len}: {e}");
    }
}

fn trusting(cidrs: &[&str], timeout: Duration) -> (ProxyProtocol, ProxyMetrics) {
    let metrics = ProxyMetrics::register(&Metrics::new(), "api");
    let cidrs = cidrs.iter().map(|c| c.parse().unwrap()).collect();
    (
        ProxyProtocol::trusting(cidrs, timeout, metrics.clone()),
        metrics,
    )
}

#[tokio::test]
async fn trusted_peer_with_a_header_becomes_the_header_source() {
    let (p, m) = trusting(&["10.244.0.0/16"], HEADER_TIMEOUT);
    let gw = sa("10.244.1.9:40000");
    let mut wire = proxy(sa("192.0.2.7:51000"), sa("10.244.0.5:8443"), &[]);
    wire.extend_from_slice(CLIENT_HELLO);
    let mut r = wire.as_slice();
    let c = p.client(&mut r, gw).await.unwrap();
    assert_eq!(c.addr, sa("192.0.2.7:51000"));
    assert_eq!(c.origin, Origin::Proxied { via: gw });
    assert_eq!(c.origin.via(), Some(gw));
    assert_eq!(r, CLIENT_HELLO);
    assert_eq!(m.count("proxied"), 1);
}

#[tokio::test]
async fn trusted_peer_without_a_header_is_refused() {
    let (p, m) = trusting(&["10.244.0.0/16"], HEADER_TIMEOUT);
    let mut hello = CLIENT_HELLO.to_vec();
    hello.extend_from_slice(&[0; 32]);
    let mut r = hello.as_slice();
    let e = p.client(&mut r, sa("10.244.1.9:1")).await.unwrap_err();
    assert!(matches!(e, ProxyError::Signature), "{e:?}");
    assert_eq!(m.count("bad_signature"), 1);
    assert_eq!(m.count("proxied"), 0);
}

#[tokio::test]
async fn untrusted_peer_is_direct_and_its_header_is_left_for_tls() {
    let (p, m) = trusting(&["10.244.0.0/16"], HEADER_TIMEOUT);
    let forged = proxy(sa("203.0.113.9:1"), sa("10.244.0.5:8443"), &[]);
    let mut r = forged.as_slice();
    let peer = sa("10.245.0.3:5555");
    let c = p.client(&mut r, peer).await.unwrap();
    assert_eq!(
        c,
        Client {
            addr: peer,
            origin: Origin::Direct
        }
    );
    assert_eq!(r, forged.as_slice(), "nothing was read");
    // Not counted: the counter covers trusted sources only.
    for l in ["proxied", "direct", "bad_signature"] {
        assert_eq!(m.count(l), 0, "{l}");
    }
    // Off: the same.
    let mut r = forged.as_slice();
    let c = ProxyProtocol::off().client(&mut r, peer).await.unwrap();
    assert_eq!(c.origin, Origin::Direct);
    assert_eq!(r.len(), forged.len());
    assert!(!ProxyProtocol::off().trusts(peer.ip()));
}

#[tokio::test]
async fn local_keeps_the_gateway_address() {
    let (p, m) = trusting(&["10.244.0.0/16"], HEADER_TIMEOUT);
    let gw = sa("10.244.1.9:40000");
    let wire = header(0x20, 0x00, &[]);
    let c = p.client(&mut wire.as_slice(), gw).await.unwrap();
    assert_eq!(
        c,
        Client {
            addr: gw,
            origin: Origin::Local
        }
    );
    assert_eq!(m.count("local"), 1);
}

#[tokio::test]
async fn mapped_addresses_match_and_are_canonical() {
    let (p, _) = trusting(&["10.244.0.0/16"], HEADER_TIMEOUT);
    // A dual-stack socket reports an IPv4 gateway as ::ffff:a.b.c.d.
    let gw = sa("[::ffff:10.244.1.9]:40000");
    assert!(p.trusts(gw.ip()));
    let wire = proxy(
        sa("[::ffff:192.0.2.7]:9"),
        sa("[::ffff:10.244.0.5]:8443"),
        &[],
    );
    let c = p.client(&mut wire.as_slice(), gw).await.unwrap();
    assert_eq!(c.addr, sa("192.0.2.7:9"));
}

#[tokio::test(start_paused = true)]
async fn a_slow_sender_runs_into_the_header_timeout() {
    let (p, m) = trusting(&["10.244.0.0/16"], Duration::from_secs(5));
    let (mut client, mut server) = tokio::io::duplex(1024);
    let wire = proxy(sa("192.0.2.7:1"), sa("10.0.0.1:2"), &[]);
    // One byte a second: each read makes progress, but the whole header takes
    // 28 s, and the timeout bounds the whole header, not each read.
    let sender = tokio::spawn(async move {
        for b in wire {
            if client.write_all(&[b]).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let _ = client.read_u8().await;
    });
    let start = tokio::time::Instant::now();
    let e = p.client(&mut server, sa("10.244.1.9:1")).await.unwrap_err();
    assert!(
        matches!(e, ProxyError::Timeout(d) if d == Duration::from_secs(5)),
        "{e:?}"
    );
    assert_eq!(start.elapsed(), Duration::from_secs(5));
    assert_eq!(m.count("timeout"), 1);
    drop(server);
    sender.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_silent_trusted_peer_runs_into_the_header_timeout() {
    let (p, m) = trusting(&["10.244.0.0/16"], Duration::from_secs(2));
    let (_client, mut server) = tokio::io::duplex(64);
    let e = p.client(&mut server, sa("10.244.1.9:1")).await.unwrap_err();
    assert_eq!(e.label(), "timeout");
    assert_eq!(m.count("timeout"), 1);
}

#[test]
fn cidrs_parse_canonical_blocks_only() {
    for ok in [
        "10.0.0.0/8",
        "10.244.1.9/32",
        "0.0.0.0/0",
        "fd00::/8",
        "2001:db8::1/128",
        "::/0",
    ] {
        assert_eq!(ok.parse::<Cidr>().unwrap().to_string(), ok);
    }
    for bad in [
        "10.0.0.1/8",
        "10.0.0.0/33",
        "fd00::1/8",
        "::ffff:10.0.0.0/104",
        "10.0.0.0",
        "10.0.0.0/",
        "10.0.0.0/+8",
        "10.0.0.0/ 8",
        "10.0.0.0/0008",
        "nonsense/8",
        "",
    ] {
        assert!(bad.parse::<Cidr>().is_err(), "{bad}");
    }
}

#[test]
fn cidr_contains() {
    let c: Cidr = "10.244.0.0/16".parse().unwrap();
    assert!(c.contains("10.244.255.1".parse().unwrap()));
    assert!(c.contains("::ffff:10.244.0.1".parse().unwrap()));
    assert!(!c.contains("10.245.0.1".parse().unwrap()));
    assert!(!c.contains("fd00::1".parse().unwrap()));
    let host: Cidr = "10.244.1.9/32".parse().unwrap();
    assert!(host.contains("10.244.1.9".parse().unwrap()));
    assert!(!host.contains("10.244.1.8".parse().unwrap()));
    let v6: Cidr = "fd00:1::/32".parse().unwrap();
    assert!(v6.contains("fd00:1:ffff::9".parse().unwrap()));
    assert!(!v6.contains("fd00:2::9".parse().unwrap()));
    assert!(!v6.contains("10.0.0.1".parse().unwrap()));
    let all: Cidr = "0.0.0.0/0".parse().unwrap();
    assert!(all.contains("203.0.113.9".parse().unwrap()));
    assert!(!all.contains("::1".parse().unwrap()));
}
