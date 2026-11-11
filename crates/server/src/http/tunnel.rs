//! `GET /v1/tunnel` (edge.md §10): the edge byte stream inside a WebSocket on
//! the API listener, for gateways that terminate TLS and so never forward a
//! client certificate.
//!
//! The route takes no bearer token. It checks the RFC 6455 upgrade request,
//! hands the connection to the edge ([`TunnelDoor::enter`]) and answers `101`
//! once the edge holds a handshake slot for it. The API listener then lets the
//! connection go, and with it its `api.maxConnections` and
//! `api.maxConnectionsPerIp` slots. From there the edge runs the tunnel like
//! an accepted TCP connection, over [`WsBytes`].

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
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{Interval, MissedTickBehavior};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tracing::debug;

use super::api::{ApiError, Peer};
use crate::edge::{Arrival, BoxIo, TunnelDoor};

/// The server pings every tunnel this often, so idle timers in gateways and
/// load balancers never cut a quiet build.
pub const PING_EVERY: Duration = Duration::from_secs(20);
/// A tunnel that leaves this many pings in a row unanswered is closed.
const MISSED_PINGS: u32 = 2;
/// The largest binary message a client may send, one frame or the sum of its
/// fragments; a larger one closes the tunnel. tungstenite buffers a whole
/// message, so this bounds what an unauthenticated tunnel can pin.
pub const MAX_MESSAGE: usize = 256 * 1024;
/// The largest frame the server sends (clients send at most this much too).
const WRITE_CHUNK: usize = 64 * 1024;
/// Socket read size, the size of the edge's own copy buffers.
const READ_BUFFER: usize = 64 * 1024;
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
    let Some(accept) = accept_key(req.method(), req.version(), req.headers()) else {
        return upgrade_required();
    };
    let Some(Peer(peer)) = req.extensions().get::<Peer>().copied() else {
        return unavailable();
    };
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
        Some(Box::new(WsBytes::new(ws, ping_every)) as BoxIo)
    }
    .boxed();
    match t.door.enter(Arrival { peer, stream }).await {
        Ok(()) => switching(accept),
        Err(e) => {
            debug!(peer = %peer, error = %e, "tunnel refused");
            unavailable()
        }
    }
}

fn ws_config() -> WebSocketConfig {
    // Writes go out at once; what waits for a slow peer is at most one
    // `WRITE_CHUNK` frame and control frames, far under the write cap.
    WebSocketConfig::default()
        .read_buffer_size(READ_BUFFER)
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_MESSAGE)
        .max_frame_size(Some(MAX_MESSAGE))
        .max_message_size(Some(MAX_MESSAGE))
}

/// The `Sec-WebSocket-Accept` value for a well-formed RFC 6455 upgrade over
/// HTTP/1.1, `None` for anything else.
fn accept_key(method: &Method, version: Version, h: &HeaderMap) -> Option<HeaderValue> {
    if method != Method::GET || version != Version::HTTP_11 {
        return None;
    }
    let has = |name: HeaderName, token: &str| {
        h.get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|t| t.trim().eq_ignore_ascii_case(token))
    };
    if !has(header::CONNECTION, "upgrade") || !has(header::UPGRADE, "websocket") {
        return None;
    }
    if h.get(header::SEC_WEBSOCKET_VERSION)?.as_bytes() != b"13" {
        return None;
    }
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

fn switching(accept: HeaderValue) -> Response {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let h = r.headers_mut();
    h.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    h.insert(header::SEC_WEBSOCKET_ACCEPT, accept);
    r
}

fn upgrade_required() -> Response {
    let mut r = StatusCode::UPGRADE_REQUIRED.into_response();
    let h = r.headers_mut();
    h.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
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
