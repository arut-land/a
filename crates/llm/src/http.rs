//! The HTTP seam. A client is anything that sends one request and hands
//! back the status, headers and a stream of body bytes; middleware is a
//! client wrapping another. Nothing here names a runtime or an HTTP crate.
//! A request's body is the pieces it was spliced from, sent in order: a
//! conversation's encoded messages go as they are kept, not copied into
//! one buffer.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_core::Stream;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A response body: its bytes as they arrive.
pub type Body = Pin<Box<dyn Stream<Item = io::Result<Vec<u8>>> + Send>>;

pub type Response = http::Response<Body>;

pub trait Http: Send + Sync + 'static {
    /// Sends `request`. A non-success status is still `Ok`: its body is the
    /// provider's account of the failure.
    fn send(&self, request: http::Request<Payload>) -> BoxFuture<'static, io::Result<Response>>;
}

impl<H: Http + ?Sized> Http for Arc<H> {
    fn send(&self, request: http::Request<Payload>) -> BoxFuture<'static, io::Result<Response>> {
        (**self).send(request)
    }
}

/// A request body: its bytes in the pieces it was spliced from, shared
/// with the encodings a conversation keeps.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Payload(Vec<Arc<[u8]>>);

impl Payload {
    pub fn len(&self) -> usize {
        self.0.iter().map(|piece| piece.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|piece| piece.is_empty())
    }

    pub fn pieces(&self) -> &[Arc<[u8]>] {
        &self.0
    }

    /// The bytes, copied into one buffer.
    pub fn to_vec(&self) -> Vec<u8> {
        self.0.concat()
    }

    /// The bytes, read in order.
    pub fn reader(&self) -> impl io::Read + '_ {
        Reader(&self.0, 0)
    }
}

impl From<Vec<Arc<[u8]>>> for Payload {
    fn from(pieces: Vec<Arc<[u8]>>) -> Self {
        Self(pieces)
    }
}

impl From<Vec<u8>> for Payload {
    fn from(bytes: Vec<u8>) -> Self {
        Self(vec![bytes.into()])
    }
}

/// The pieces left and how far into the first.
struct Reader<'a>(&'a [Arc<[u8]>], usize);

impl io::Read for Reader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while let Some((piece, rest)) = self.0.split_first() {
            let left = &piece[self.1..];
            if left.is_empty() {
                (self.0, self.1) = (rest, 0);
                continue;
            }
            let n = left.len().min(buf.len());
            buf[..n].copy_from_slice(&left[..n]);
            self.1 += n;
            return Ok(n);
        }
        Ok(0)
    }
}

/// A body of bytes already in hand.
pub fn body(chunks: impl IntoIterator<Item = Vec<u8>>) -> Body {
    struct Ready(std::vec::IntoIter<Vec<u8>>);

    impl Stream for Ready {
        type Item = io::Result<Vec<u8>>;

        fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.0.next().map(Ok))
        }
    }

    Box::pin(Ready(chunks.into_iter().collect::<Vec<_>>().into_iter()))
}
