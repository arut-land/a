//! A channel from any producer to an async consumer: the sender of a
//! bounded channel parks while it is full, the receiver is a stream.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

use crate::{lock, wait};

struct State<T> {
    items: VecDeque<T>,
    closed: bool,
    receiver_gone: bool,
    waker: Option<Waker>,
}

impl<T> State<T> {
    fn wake(&mut self) {
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

struct Shared<T> {
    state: Mutex<State<T>>,
    space: Condvar,
    capacity: usize,
}

impl<T> Shared<T> {
    fn state(&self) -> MutexGuard<'_, State<T>> {
        lock(&self.state)
    }
}

pub struct Sender<T>(Arc<Shared<T>>);

pub struct Receiver<T>(Arc<Shared<T>>);

pub fn bounded<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            items: VecDeque::new(),
            closed: false,
            receiver_gone: false,
            waker: None,
        }),
        space: Condvar::new(),
        capacity: capacity.max(1),
    });
    (Sender(Arc::clone(&shared)), Receiver(shared))
}

/// A channel whose sender never waits, for producers that must not block,
/// such as async tasks.
pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    bounded(usize::MAX)
}

impl<T> Sender<T> {
    /// Sends `item`, blocking while the channel is full. `Err` gives the item
    /// back once the receiver is gone.
    pub fn send(&self, item: T) -> Result<(), T> {
        let shared = &self.0;
        let mut state = shared.state();
        while state.items.len() >= shared.capacity && !state.receiver_gone {
            state = wait(&shared.space, state);
        }
        if state.receiver_gone {
            return Err(item);
        }
        state.items.push_back(item);
        state.wake();
        Ok(())
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.closed = true;
        state.wake();
    }
}

impl<T> Receiver<T> {
    /// The next item, `None` once the sender is gone and nothing is left.
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut state = self.0.state();
        if let Some(item) = state.items.pop_front() {
            self.0.space.notify_one();
            return Poll::Ready(Some(item));
        }
        if state.closed {
            return Poll::Ready(None);
        }
        match &mut state.waker {
            Some(waker) if waker.will_wake(cx.waker()) => {}
            slot => *slot = Some(cx.waker().clone()),
        }
        Poll::Pending
    }

    /// The next item, as a future.
    pub fn recv(&self) -> impl Future<Output = Option<T>> + '_ {
        std::future::poll_fn(|cx| self.poll_recv(cx))
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.receiver_gone = true;
        state.items.clear();
        self.0.space.notify_all();
    }
}

impl<T> futures_core::Stream for Receiver<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.poll_recv(cx)
    }
}
