//! The 201 body that commits its allocation only once fully handed to the connection.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};

use super::api::Grant;

/// Yields the JSON once; on the poll after that (the connection has taken
/// every byte) it commits the grant and ends. Dropped earlier (client gone,
/// write error, shutdown), the grant drops uncommitted and the orphan guard
/// retires the pod.
///
/// The size hint is deliberately inexact. With an exact length, HTTP/1 stops
/// polling after the last byte and drops the body without the final poll.
pub struct CommitBody<G> {
    data: Option<Bytes>,
    grant: Option<G>,
}

impl<G: Grant> CommitBody<G> {
    /// `data` is the serialized response of `grant`.
    pub fn new(data: Bytes, grant: G) -> Self {
        Self {
            data: Some(data),
            grant: Some(grant),
        }
    }
}

impl<G: Grant + Unpin> Body for CommitBody<G> {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();
        if let Some(data) = this.data.take() {
            return Poll::Ready(Some(Ok(Frame::data(data))));
        }
        if let Some(grant) = this.grant.take() {
            grant.commit();
        }
        Poll::Ready(None)
    }

    fn is_end_stream(&self) -> bool {
        self.data.is_none() && self.grant.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}
