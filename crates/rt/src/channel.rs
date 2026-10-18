//! A bounded channel from a blocking producer to an async consumer: the
//! sender parks while the channel is full, the receiver is a stream.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

struct State<T> {
    items: VecDeque<T>,
    closed: bool,
    receiver_gone: bool,
    waker: Option<Waker>,
}

struct Shared<T> {
    state: Mutex<State<T>>,
    space: Condvar,
    capacity: usize,
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

impl<T> Sender<T> {
    /// Sends `item`, blocking while the channel is full. `Err` gives the item
    /// back once the receiver is gone.
    pub fn send(&self, item: T) -> Result<(), T> {
        let shared = &self.0;
        let mut state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
        while state.items.len() >= shared.capacity && !state.receiver_gone {
            state = shared
                .space
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if state.receiver_gone {
            return Err(item);
        }
        state.items.push_back(item);
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
        Ok(())
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.closed = true;
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
    }
}

impl<T> Receiver<T> {
    /// The next item, `None` once the sender is gone and nothing is left.
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let shared = &self.0;
        let mut state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(item) = state.items.pop_front() {
            shared.space.notify_one();
            return Poll::Ready(Some(item));
        }
        if state.closed {
            return Poll::Ready(None);
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
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
