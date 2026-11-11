//! `/v1/tunnel` (edge.md §10): the edge byte stream inside a WebSocket on the
//! API listener, for gateways that terminate TLS and so never forward a client
//! certificate.
//!
//! The route takes no bearer token. It checks the WebSocket request, an
//! HTTP/1.1 upgrade (RFC 6455) or an HTTP/2 extended `CONNECT` (RFC 8441),
//! hands the stream to the edge ([`TunnelDoor::enter`]) and answers `101` or
//! `200` once the edge holds a handshake slot for it. Over HTTP/1.1 the API
//! listener then lets the connection go, and with it its `api.maxConnections`
//! and `api.maxConnectionsPerIp` slots. Over HTTP/2 the tunnel is one stream:
//! the connection keeps its slots and stays open while the tunnel lives
//! ([`KeepOpen`]). From there the edge runs the tunnel like an accepted TCP
//! connection, over [`WsBytes`].

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Version, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use buildkit_controller_api::dto::{ErrorCode, TUNNEL_PATH};
use bytes::Bytes;
use futures::{FutureExt as _, Sink, Stream};
use hyper::ext::Protocol;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{Interval, MissedTickBehavior};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tracing::debug;

use super::api::{ApiError, Peer};
use super::listener::{Active, KeepOpen};
use crate::edge::{Arrival, BoxIo, TunnelDoor};

/// The server pings every tunnel this often, so idle timers in gateways and
/// load balancers never cut a quiet build.
pub const PING_EVERY: Duration = Duration::from_secs(20);
/// A tunnel that leaves this many pings in a row unanswered is closed.
const MISSED_PINGS: u32 = 2;
/// The largest binary message a client may send, one frame or the sum of its
/// fragments; a larger one closes the tunnel. tungstenite buffers a whole
/// message, so this bounds what an unauthenticated tunnel can pin.
///
/// The worst case per tunnel is about 400-460 KiB, reached by a client that
/// sends into a stalled worker and never reads: the TCP edge part (two 64 KiB
/// copy buffers and two TLS sessions, about 200 KiB), the 64 KiB HTTP/2 stream
/// window, the 64-128 KiB read side (input buffer plus the pending message)
/// and the 65 KiB write buffer. Size `edge.maxConnections` against that, not
/// against a TCP connection.
pub const MAX_MESSAGE: usize = 64 * 1024;
/// The largest frame the server sends (clients send at most this much too).
const WRITE_CHUNK: usize = 64 * 1024;
/// Socket read size, the size of the edge's own copy buffers.
const READ_BUFFER: usize = 64 * 1024;
/// What the write buffer holds beyond one full frame: its header (at most 14
/// bytes) and control frames queued behind it (a ping or pong is under 130).
const WRITE_SLACK: usize = 1024;
/// RFC 6455 §1.3.
const ACCEPT_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// What the route needs: the door into the edge and the ping cadence.
#[derive(Clone, Debug)]
pub struct Tunnels {
    door: TunnelDoor,
    ping_every: Duration,
}

impl Tunnels {
    /// Tunnels into the edge behind `door`, pinged every [`PING_EVERY`].
    #[must_use]
    pub fn new(door: TunnelDoor) -> Self {
        Self {
            door,
            ping_every: PING_EVERY,
        }
    }

    /// The same with another ping cadence (tests).
    #[must_use]
    pub fn with_ping_every(self, ping_every: Duration) -> Self {
        Self { ping_every, ..self }
    }
}

/// The `/v1/tunnel` route, for any router state.
pub fn router<S: Clone + Send + Sync + 'static>(tunnels: Tunnels) -> Router<S> {
    Router::new()
        .route(TUNNEL_PATH, any(upgrade))
        .with_state(tunnels)
}

async fn upgrade(State(t): State<Tunnels>, mut req: Request) -> Response {
    let protocol = req.extensions().get::<Protocol>();
    let Some(form) = form(req.method(), req.version(), req.headers(), protocol) else {
        return upgrade_required(req.version());
    };
    let Some(Peer(peer)) = req.extensions().get::<Peer>().copied() else {
        return unavailable();
    };
    // Taken before the answer, so the connection is never idle in between.
    let keep = req.extensions().get::<KeepOpen>().map(KeepOpen::hold);
    let upgraded = hyper::upgrade::on(&mut req);
    let ping_every = t.ping_every;
    let stream = async move {
        let io = match upgraded.await {
            Ok(io) => io,
            Err(e) => {
                debug!(peer = %peer, error = %e, "tunnel upgrade failed");
                return None;
            }
        };
        let ws =
            WebSocketStream::from_raw_socket(TokioIo::new(io), Role::Server, Some(ws_config()))
                .await;
        let bytes = WsBytes {
            _keep: keep,
            ..WsBytes::new(ws, ping_every)
        };
        Some(Box::new(bytes) as BoxIo)
    }
    .boxed();
    match t.door.enter(Arrival { peer, stream }).await {
        Ok(()) => form.answer(),
        Err(e) => {
            debug!(peer = %peer, error = %e, "tunnel refused");
            unavailable()
        }
    }
}

fn ws_config() -> WebSocketConfig {
    // Writes go out at once; what waits for a slow peer is at most one
    // `WRITE_CHUNK` frame and control frames. The buffer counts headers, so the
    // cap is one full frame plus [`WRITE_SLACK`].
    WebSocketConfig::default()
        .read_buffer_size(READ_BUFFER)
        .write_buffer_size(0)
        .max_write_buffer_size(WRITE_CHUNK + WRITE_SLACK)
        .max_frame_size(Some(MAX_MESSAGE))
        .max_message_size(Some(MAX_MESSAGE))
}

/// How a client asked for the WebSocket, with what the answer carries.
#[derive(Debug, PartialEq, Eq)]
enum Form {
    /// HTTP/1.1 `GET` with `Upgrade: websocket`, answered `101` with this
    /// `Sec-WebSocket-Accept`.
    Upgrade(HeaderValue),
    /// HTTP/2 extended `CONNECT` with `:protocol` `websocket`, answered `200`.
    /// RFC 8441 needs no key, but a gateway that turns an HTTP/1.1 upgrade
    /// into this form forwards the client's key and passes the answer's
    /// `Sec-WebSocket-Accept` back, which the client checks (Envoy computes
    /// none itself). So a key, when sent, is checked and answered.
    Connect(Option<HeaderValue>),
}

/// The form of a well-formed WebSocket request, `None` for anything else.
fn form(
    method: &Method,
    version: Version,
    h: &HeaderMap,
    protocol: Option<&Protocol>,
) -> Option<Form> {
    let has = |name: HeaderName, token: &str| {
        h.get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|t| t.trim().eq_ignore_ascii_case(token))
    };
    if h.get(header::SEC_WEBSOCKET_VERSION)?.as_bytes() != b"13" {
        return None;
    }
    if *method == Method::GET && version == Version::HTTP_11 {
        if !has(header::CONNECTION, "upgrade") || !has(header::UPGRADE, "websocket") {
            return None;
        }
        return accept_key(h).map(Form::Upgrade);
    }
    if *method == Method::CONNECT
        && version == Version::HTTP_2
        && protocol.is_some_and(|p| p.as_str().eq_ignore_ascii_case("websocket"))
    {
        if !h.contains_key(header::SEC_WEBSOCKET_KEY) {
            return Some(Form::Connect(None));
        }
        return accept_key(h).map(|a| Form::Connect(Some(a)));
    }
    None
}

/// The RFC 6455 `Sec-WebSocket-Accept` for the request's one well-formed
/// `Sec-WebSocket-Key`.
fn accept_key(h: &HeaderMap) -> Option<HeaderValue> {
    let mut keys = h.get_all(header::SEC_WEBSOCKET_KEY).iter();
    let key = keys.next()?;
    if keys.next().is_some() || BASE64.decode(key.as_bytes()).ok()?.len() != 16 {
        return None;
    }
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    ctx.update(key.as_bytes());
    ctx.update(ACCEPT_GUID);
    HeaderValue::from_str(&BASE64.encode(ctx.finish())).ok()
}

impl Form {
    /// The `101` or `200` that opens the tunnel.
    fn answer(self) -> Response {
        let mut r = Response::new(Body::empty());
        let h = r.headers_mut();
        let accept = match self {
            Form::Upgrade(accept) => {
                h.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
                h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
                *r.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
                Some(accept)
            }
            Form::Connect(accept) => accept,
        };
        if let Some(a) = accept {
            r.headers_mut().insert(header::SEC_WEBSOCKET_ACCEPT, a);
        }
        r
    }
}

fn upgrade_required(version: Version) -> Response {
    let mut r = StatusCode::UPGRADE_REQUIRED.into_response();
    let h = r.headers_mut();
    // HTTP/2 forbids connection-specific header fields.
    if version < Version::HTTP_2 {
        h.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    }
    h.insert(
        header::SEC_WEBSOCKET_VERSION,
        HeaderValue::from_static("13"),
    );
    r
}

/// The edge has no serving cert yet, or it stopped.
fn unavailable() -> Response {
    ApiError::new(ErrorCode::NotReady, "the edge is not ready").into_response()
}

/// A WebSocket as a byte stream: the payloads of the binary frames read, in
/// order, and each write sent as one binary frame. A close frame or EOF ends
/// the read side (the close is answered, as RFC 6455 requires); a text frame,
/// a message over [`MAX_MESSAGE`] (one frame or its fragments), or two unanswered pings in a row
/// fail it. Shutting down sends a close frame.
///
/// The pings go out from [`AsyncRead::poll_read`], which the edge polls for
/// as long as it waits for the client.
pub struct WsBytes<S> {
    ws: WebSocketStream<S>,
    /// The rest of the last binary payload.
    pending: Bytes,
    ping: Interval,
    /// Pings sent since the last pong.
    unanswered: u32,
    /// A ping is due and not yet queued.
    ping_due: bool,
    /// Queued control frames (a ping, the answer to a close) are not yet
    /// flushed.
    flush_due: bool,
    /// The peer closed or the stream ended: reads return EOF.
    eof: bool,
    /// Keeps the HTTP/2 connection that carries the tunnel open while it lives.
    _keep: Option<Active>,
}

impl<S> WsBytes<S> {
    /// Wraps `ws`, pinging every `ping_every`.
    pub fn new(ws: WebSocketStream<S>, ping_every: Duration) -> Self {
        let mut ping =
            tokio::time::interval_at(tokio::time::Instant::now() + ping_every, ping_every);
        ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self {
            ws,
            pending: Bytes::new(),
            ping,
            unanswered: 0,
            ping_due: false,
            flush_due: false,
            eof: false,
            _keep: None,
        }
    }
}

fn io_error(e: WsError) -> io::Error {
    match e {
        WsError::Io(e) => e,
        WsError::ConnectionClosed | WsError::AlreadyClosed => io::ErrorKind::BrokenPipe.into(),
        e => io::Error::new(io::ErrorKind::InvalidData, e),
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> WsBytes<S> {
    /// Ticks the ping timer, queues a due ping and drives queued control
    /// frames out. Fails once [`MISSED_PINGS`] pings went unanswered.
    fn poll_control(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        while self.ping.poll_tick(cx).is_ready() {
            if self.unanswered >= MISSED_PINGS {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "tunnel pings unanswered",
                ));
            }
            self.unanswered += 1;
            self.ping_due = true;
        }
        if self.ping_due {
            match Pin::new(&mut self.ws).poll_ready(cx) {
                Poll::Ready(Ok(())) => {
                    Pin::new(&mut self.ws)
                        .start_send(Message::Ping(Bytes::new()))
                        .map_err(io_error)?;
                    self.ping_due = false;
                    self.flush_due = true;
                }
                Poll::Ready(Err(e)) => return Err(io_error(e)),
                Poll::Pending => {}
            }
        }
        if self.flush_due {
            match Pin::new(&mut self.ws).poll_flush(cx) {
                Poll::Ready(Ok(())) => self.flush_due = false,
                Poll::Ready(Err(e)) => return Err(io_error(e)),
                Poll::Pending => {}
            }
        }
        Ok(())
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for WsBytes<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if !this.pending.is_empty() {
                let n = this.pending.len().min(buf.remaining());
                buf.put_slice(&this.pending.split_to(n));
                return Poll::Ready(Ok(()));
            }
            if this.eof {
                // Best effort: the answer to a close frame goes out before EOF.
                if this.flush_due {
                    let _ = ready!(Pin::new(&mut this.ws).poll_flush(cx));
                    this.flush_due = false;
                }
                return Poll::Ready(Ok(()));
            }
            this.poll_control(cx)?;
            match ready!(Pin::new(&mut this.ws).poll_next(cx)) {
                Some(Ok(Message::Binary(b))) => this.pending = b,
                Some(Ok(Message::Pong(_))) => this.unanswered = 0,
                // tungstenite queues the pong itself.
                Some(Ok(Message::Ping(_))) => this.flush_due = true,
                Some(Ok(Message::Frame(_))) => {}
                // EOF ends the stream as a close frame does.
                Some(
                    Ok(Message::Close(_))
                    | Err(WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake)),
                )
                | None => {
                    this.eof = true;
                    this.flush_due = true;
                }
                Some(Ok(Message::Text(_))) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "text frame on a tunnel",
                    )));
                }
                Some(Err(e)) => return Poll::Ready(Err(io_error(e))),
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for WsBytes<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(Pin::new(&mut this.ws).poll_ready(cx)).map_err(io_error)?;
        let n = buf.len().min(WRITE_CHUNK);
        Pin::new(&mut this.ws)
            .start_send(Message::Binary(Bytes::copy_from_slice(&buf[..n])))
            .map_err(io_error)?;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().ws)
            .poll_flush(cx)
            .map_err(io_error)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().ws)
            .poll_close(cx)
            .map_err(io_error)
    }
}

#[cfg(test)]
#[path = "tunnel_tests.rs"]
mod tests;
