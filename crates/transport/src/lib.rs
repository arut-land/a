//! Request-scoped headers and bodies over reusable, policy-specific pools.
//! Payload pieces avoid copying a conversation into one request buffer.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_core::Stream;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub type Body = Pin<Box<dyn Stream<Item = io::Result<Vec<u8>>> + Send>>;

pub type Response = http::Response<Body>;

pub trait Http: Send + Sync + 'static {
    /// Non-success statuses retain the peer's error body.
    fn send(&self, request: http::Request<Payload>) -> BoxFuture<'static, io::Result<Response>>;
}

impl<H: Http + ?Sized> Http for Arc<H> {
    fn send(&self, request: http::Request<Payload>) -> BoxFuture<'static, io::Result<Response>> {
        (**self).send(request)
    }
}

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

    pub fn to_vec(&self) -> Vec<u8> {
        self.0.concat()
    }

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

/// Whether transport may resend a request at another URL.
#[derive(Clone, Copy, Debug)]
pub enum RedirectPolicy {
    Refuse,
    Follow,
}

pub trait HttpClients: Send + Sync {
    fn client(&self, policy: RedirectPolicy) -> Arc<dyn Http>;
}

#[cfg(feature = "ureq")]
mod ureq;
#[cfg(feature = "ureq")]
pub use ureq::{Clients, Ureq};
