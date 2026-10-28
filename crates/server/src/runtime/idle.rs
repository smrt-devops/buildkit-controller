//! A per-request silence limit around a kube client.
//!
//! Each request gets [`super::READ_TIMEOUT`] from the moment it is sent until
//! its response headers arrive, and then the same limit between body frames.
//! The timers belong to the request, not the connection, so a request sent on
//! a pooled connection that sat idle gets the full limit too.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use http_body::{Body as HttpBody, Frame, SizeHint};
use hyper::{Request, Response};
use kube::Client;
use kube::client::Body;
use tokio::time::{Instant, Sleep};

/// A service that sends through `inner` and fails a request that receives
/// nothing for `after`.
#[derive(Clone)]
pub(super) struct IdleTimeout {
    pub(super) inner: Client,
    pub(super) after: Duration,
}

fn timed_out(what: &str, after: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("no {what} from the API server for {after:?}"),
    )
}

impl tower::Service<Request<Body>> for IdleTimeout {
    type Response = Response<IdleBody>;
    type Error = kube::Error;
    type Future = BoxFuture<'static, Result<Self::Response, kube::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // `inner` is a buffered client that waits for capacity in `send`.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let (inner, after) = (self.inner.clone(), self.after);
        Box::pin(async move {
            // The timer starts with the send, so it covers connecting, writing
            // the request and waiting for the response headers.
            match tokio::time::timeout(after, inner.send(req)).await {
                Ok(Ok(resp)) => Ok(resp.map(|body| IdleBody::new(body, after))),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(kube::Error::Service(Box::new(timed_out(
                    "response headers",
                    after,
                )))),
            }
        })
    }
}

/// A response body that fails once no frame has arrived for `after`. Every
/// frame restarts the timer, so a long body that keeps flowing (a watch with
/// bookmarks, a big list) is never cut.
pub(super) struct IdleBody {
    inner: Body,
    after: Duration,
    deadline: Pin<Box<Sleep>>,
}

impl IdleBody {
    fn new(inner: Body, after: Duration) -> Self {
        Self {
            inner,
            after,
            deadline: Box::pin(tokio::time::sleep(after)),
        }
    }
}

impl HttpBody for IdleBody {
    type Data = Bytes;
    type Error = kube::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, kube::Error>>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                this.deadline.as_mut().reset(Instant::now() + this.after);
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(other) => Poll::Ready(other),
            Poll::Pending => match this.deadline.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(Some(Err(kube::Error::Service(Box::new(
                    timed_out("response data", this.after),
                ))))),
                Poll::Pending => Poll::Pending,
            },
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
