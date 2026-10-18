//! A single-value channel: the sender is consumed by `send`, and dropping it
//! unsent resolves the receiver to `None`.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

struct State<T> {
    value: Option<T>,
    closed: bool,
    waker: Option<Waker>,
}

pub struct Sender<T>(Arc<Mutex<State<T>>>);
pub struct Receiver<T>(Arc<Mutex<State<T>>>);

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let state = Arc::new(Mutex::new(State {
        value: None,
        closed: false,
        waker: None,
    }));
    (Sender(Arc::clone(&state)), Receiver(state))
}

impl<T> Sender<T> {
    pub fn send(self, value: T) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).value = Some(value);
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.closed = true;
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
    }
}

impl<T> Future for Receiver<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(value) = state.value.take() {
            return Poll::Ready(Some(value));
        }
        if state.closed {
            return Poll::Ready(None);
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}
