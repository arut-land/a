//! The shape of a call is the type its tool's `call` returns, and nothing
//! else: its output (or why not) for a call done at once, a future of it
//! for one that waits, and a [`Streamed`] call for one that reports
//! partial outputs as it runs, its progress, before it ends with its
//! output.

use std::convert::Infallible;
use std::future::{Future, poll_fn};
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::task::{Poll, Waker};

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

impl<O, M, T> Output<O, M> for T
where
    T: Run<O, M>,
    T::Partial: AsRef<str>,
{
    fn run<'a>(self, progress: Progress<'a>) -> BoxFuture<'a, Result<O, String>>
    where
        Self: 'a,
    {
        Box::pin(self.run_with(move |partial| {
            progress(partial.as_ref());
            std::future::ready(Ok(()))
        }))
    }
}

/// The shared runner; adapters only decide how a partial is delivered.
#[doc(hidden)]
pub trait Run<O, M>: Send {
    type Partial: Send;

    fn run_with<'a, F: Future<Output = Result<(), String>> + Send + 'a>(
        self,
        send: impl FnMut(Self::Partial) -> F + Send + 'a,
    ) -> impl Future<Output = Result<O, String>> + Send + 'a
    where
        Self: 'a;
}

impl<O: Send> Run<O, Now> for Result<O, String> {
    type Partial = String;

    fn run_with<'a, F: Future<Output = Result<(), String>> + Send + 'a>(
        self,
        _: impl FnMut(String) -> F + Send + 'a,
    ) -> impl Future<Output = Result<O, String>> + Send + 'a
    where
        Self: 'a,
    {
        std::future::ready(self)
    }
}

impl<O, W> Run<O, Waits> for W
where
    W: Future<Output = Result<O, String>> + Send,
{
    type Partial = String;

    fn run_with<'a, F: Future<Output = Result<(), String>> + Send + 'a>(
        self,
        _: impl FnMut(String) -> F + Send + 'a,
    ) -> impl Future<Output = Result<O, String>> + Send + 'a
    where
        Self: 'a,
    {
        self
    }
}

impl<O, P, W> Run<O, Streams<P>> for Streamed<P, W, O>
where
    O: Send,
    P: Send,
    W: Future<Output = Result<O, String>> + Send,
{
    type Partial = P;

    async fn run_with<'a, F: Future<Output = Result<(), String>> + Send + 'a>(
        mut self,
        mut send: impl FnMut(P) -> F + Send + 'a,
    ) -> Result<O, String>
    where
        Self: 'a,
    {
        loop {
            match self.next().await {
                Next::Partial(partial) => send(partial).await?,
                Next::Done(output) => return output,
            }
        }
    }
}

/// A call that reports partial outputs of type `P` as it runs, then ends
/// with its output, of type `O`: a stream of partials whose end is the
/// result. Made by [`stream`].
pub struct Streamed<P, F, O> {
    partials: Partials<P>,
    work: Pin<Box<F>>,
    output: PhantomData<O>,
}

/// What a [`Streamed`] call yields next.
#[derive(Debug, PartialEq, Eq)]
pub enum Next<P, O> {
    Partial(P),
    /// Its output, or why not: nothing follows.
    Done(Result<O, String>),
}

/// Where a [`Streamed`] call's work sends its partial outputs.
pub struct Partials<P>(Arc<Mutex<Handoff<P>>>);

struct Handoff<P> {
    // Only the send future owns an unconsumed update; cancelling it
    // withdraws the registration without leaving a result to drain.
    pending: Weak<Mutex<Pending<P>>>,
}

struct Pending<P> {
    partial: Option<P>,
    producer: Option<Waker>,
}

impl<P> Partials<P> {
    /// Hands off `partial` in order, suspending until the consumer takes it.
    /// Work resumes when the consumer asks for the next update. Dropping
    /// the send before consumption withdraws the update.
    pub async fn send(&mut self, partial: P) {
        let pending = Arc::new(Mutex::new(Pending {
            partial: Some(partial),
            producer: None,
        }));
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending = Arc::downgrade(&pending);
        poll_fn(|cx| {
            let waker = cx.waker().clone();
            let previous = {
                let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
                if pending.partial.is_none() {
                    return Poll::Ready(());
                }
                pending.producer.replace(waker)
            };
            drop(previous);
            Poll::Pending
        })
        .await;
    }
}

/// A streaming call: `work`, given where to send its partial outputs, runs
/// to its output.
pub fn stream<P, O, F>(work: impl FnOnce(Partials<P>) -> F) -> Streamed<P, F, O>
where
    F: Future<Output = Result<O, String>>,
{
    let partials = Partials(Arc::new(Mutex::new(Handoff {
        pending: Weak::new(),
    })));
    let work = Box::pin(work(Partials(Arc::clone(&partials.0))));
    Streamed {
        partials,
        work,
        output: PhantomData,
    }
}

impl<P, O, F: Future<Output = Result<O, String>>> Streamed<P, F, O> {
    /// The next partial output, or once the work is done and every partial
    /// it sent is taken, its output; nothing after that.
    pub async fn next(&mut self) -> Next<P, O> {
        poll_fn(|cx| {
            let output = self.work.as_mut().poll(cx);
            let pending = self
                .partials
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pending
                .upgrade();
            if let Some(pending) = pending {
                let (partial, producer) = {
                    let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
                    (pending.partial.take(), pending.producer.take())
                };
                if let Some(producer) = producer {
                    producer.wake();
                }
                if let Some(partial) = partial {
                    return Poll::Ready(Next::Partial(partial));
                }
            }
            output.map(Next::Done)
        })
        .await
    }
}

#[cfg(test)]
mod tests;
