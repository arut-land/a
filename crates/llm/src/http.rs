//! The HTTP seam. A client is anything that sends one request and hands
//! back the status, headers and a stream of body bytes; middleware is a
//! client wrapping another. Nothing here names a runtime or an HTTP crate.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A response body: its bytes as they arrive.
pub type Body = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>;

pub type Response = http::Response<Body>;

pub trait Http: Send + Sync + 'static {
    /// Sends `request`. A non-success status is still `Ok`: its body is the
    /// provider's account of the failure.
    fn send(&self, request: http::Request<Bytes>) -> BoxFuture<'static, io::Result<Response>>;
}

impl<H: Http + ?Sized> Http for Arc<H> {
    fn send(&self, request: http::Request<Bytes>) -> BoxFuture<'static, io::Result<Response>> {
        (**self).send(request)
    }
}

/// A body of bytes already in hand.
pub fn body(chunks: impl IntoIterator<Item = Bytes>) -> Body {
    struct Ready(std::vec::IntoIter<Bytes>);

    impl Stream for Ready {
        type Item = io::Result<Bytes>;

        fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.0.next().map(Ok))
        }
    }

    Box::pin(Ready(chunks.into_iter().collect::<Vec<_>>().into_iter()))
}

/// Reads a whole body.
pub async fn collect(mut body: Body) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = std::future::poll_fn(|cx| body.as_mut().poll_next(cx)).await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out)
}
