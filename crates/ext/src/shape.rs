//! The shape of a call is the type its tool's `call` returns, and nothing
//! else: its output (or why not) for a call done at once, a future of it
//! for one that waits, and a [`Streamed`] call for one that reports
//! partial outputs as it runs, its progress, before it ends with its
//! output.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::future::{Future, poll_fn};
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Poll;

use crate::BoxFuture;

/// What a call's progress goes to, as each partial output is reported.
pub type Progress<'a> = &'a (dyn Fn(&str) + Send + Sync);

/// What a tool's `call` may return, by shape `M`: the call, run to its
/// output, its progress reported.
pub trait Output<O, M>: Send {
    #[doc(hidden)]
    fn run<'a>(self, progress: Progress<'a>) -> BoxFuture<'a, Result<O, String>>
    where
        Self: 'a;
}

/// The shape of a call done at once.
pub enum Now {}

/// The shape of a call that waits.
pub enum Waits {}

/// The shape of a call that streams partial outputs of type `P`.
pub struct Streams<P>(Infallible, PhantomData<P>);

impl<O: Send> Output<O, Now> for Result<O, String> {
    fn run<'a>(self, _: Progress<'a>) -> BoxFuture<'a, Result<O, String>>
    where
        Self: 'a,
    {
        Box::pin(std::future::ready(self))
    }
}

impl<O, F> Output<O, Waits> for F
where
    F: Future<Output = Result<O, String>> + Send,
{
    fn run<'a>(self, _: Progress<'a>) -> BoxFuture<'a, Result<O, String>>
    where
        Self: 'a,
    {
        Box::pin(self)
    }
}

impl<O, P, F> Output<O, Streams<P>> for Streamed<P, F, O>
where
    O: Send,
    P: AsRef<str> + Send,
    F: Future<Output = Result<O, String>> + Send,
{
    fn run<'a>(mut self, progress: Progress<'a>) -> BoxFuture<'a, Result<O, String>>
    where
        Self: 'a,
    {
        Box::pin(async move {
            loop {
                match self.next().await {
                    Next::Partial(partial) => progress(partial.as_ref()),
                    Next::Done(output) => return output,
                }
            }
        })
    }
}

/// A call that reports partial outputs of type `P` as it runs, then ends
/// with its output, of type `O`: a stream of partials whose end is the
/// result. Made by [`stream`].
pub struct Streamed<P, F, O> {
    partials: Partials<P>,
    work: Pin<Box<F>>,
    /// The output, once the work is done and partials it sent are left.
    done: Option<Result<O, String>>,
}

/// What a [`Streamed`] call yields next.
#[derive(Debug, PartialEq, Eq)]
pub enum Next<P, O> {
    Partial(P),
    /// Its output, or why not: nothing follows.
    Done(Result<O, String>),
}

/// Where a [`Streamed`] call's work sends its partial outputs.
pub struct Partials<P>(Arc<Mutex<VecDeque<P>>>);

impl<P> Clone for Partials<P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<P> Partials<P> {
    /// Reports `partial`, in order.
    pub fn send(&self, partial: P) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(partial);
    }

    fn take(&self) -> Option<P> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }
}

/// A streaming call: `work`, given where to send its partial outputs, runs
/// to its output.
pub fn stream<P, O, F>(work: impl FnOnce(Partials<P>) -> F) -> Streamed<P, F, O>
where
    F: Future<Output = Result<O, String>>,
{
    let partials = Partials(Arc::default());
    let work = Box::pin(work(partials.clone()));
    Streamed {
        partials,
        work,
        done: None,
    }
}

impl<P, O, F: Future<Output = Result<O, String>>> Streamed<P, F, O> {
    /// The next partial output, or once the work is done and every partial
    /// it sent is taken, its output; nothing after that.
    pub async fn next(&mut self) -> Next<P, O> {
        poll_fn(|cx| {
            if let Some(partial) = self.partials.take() {
                return Poll::Ready(Next::Partial(partial));
            }
            if let Some(output) = self.done.take() {
                return Poll::Ready(Next::Done(output));
            }
            match self.work.as_mut().poll(cx) {
                Poll::Ready(output) => Poll::Ready(match self.partials.take() {
                    Some(partial) => {
                        self.done = Some(output);
                        Next::Partial(partial)
                    }
                    None => Next::Done(output),
                }),
                Poll::Pending => self
                    .partials
                    .take()
                    .map_or(Poll::Pending, |p| Poll::Ready(Next::Partial(p))),
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests;
