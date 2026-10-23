//! A channel from any producer to an async consumer. The receiver is a
//! stream; which sender a channel has says what sending costs: an
//! unbounded channel's never waits, a bounded channel's parks its thread
//! while the channel is full, so an async task cannot hold one.

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
    waiting: usize,
}

struct Shared<T> {
    state: Mutex<State<T>>,
    space: Condvar,
    capacity: usize,
    #[cfg(test)]
    blocked: crate::Notify,
}

impl<T> Shared<T> {
    fn state(&self) -> MutexGuard<'_, State<T>> {
        lock(&self.state)
    }

    /// Queues `item` once there is room, parking while there is none;
    /// `Err` gives it back once the receiver is gone.
    fn push(&self, item: T) -> Result<(), T> {
        let mut state = self.state();
        while state.items.len() >= self.capacity && !state.receiver_gone {
            #[cfg(test)]
            self.blocked.notify();
            state.waiting += 1;
            state = wait(&self.space, state);
            state.waiting -= 1;
        }
        if state.receiver_gone {
            return Err(item);
        }
        state.items.push_back(item);
        let waker = state.waker.take();
        drop(state);
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
    }

    fn close(&self) {
        let mut state = self.state();
        state.closed = true;
        let waker = state.waker.take();
        drop(state);
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// The sender of an unbounded channel: it never waits, so an async task
/// may hold one.
pub struct Sender<T>(Arc<Shared<T>>);

/// The sender of a bounded channel: it parks its thread while the channel
/// is full, so only a blocking thread may hold one.
pub struct BlockingSender<T>(Arc<Shared<T>>);

pub struct Receiver<T>(Arc<Shared<T>>);

fn channel<T>(capacity: usize) -> (Arc<Shared<T>>, Receiver<T>) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            items: VecDeque::new(),
            closed: false,
            receiver_gone: false,
            waker: None,
            waiting: 0,
        }),
        space: Condvar::new(),
        capacity,
        #[cfg(test)]
        blocked: crate::Notify::new(),
    });
    (Arc::clone(&shared), Receiver(shared))
}

pub fn bounded<T>(capacity: usize) -> (BlockingSender<T>, Receiver<T>) {
    let (shared, rx) = channel(capacity.max(1));
    (BlockingSender(shared), rx)
}

pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    let (shared, rx) = channel(usize::MAX);
    (Sender(shared), rx)
}

impl<T> Sender<T> {
    /// Sends `item`; `Err` gives it back once the receiver is gone.
    pub fn send(&self, item: T) -> Result<(), T> {
        self.0.push(item)
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.0.close();
    }
}

impl<T> BlockingSender<T> {
    /// Sends `item`, parking while the channel is full. `Err` gives the
    /// item back once the receiver is gone.
    pub fn send(&self, item: T) -> Result<(), T> {
        self.0.push(item)
    }
}

impl<T> Drop for BlockingSender<T> {
    fn drop(&mut self) {
        self.0.close();
    }
}

impl<T> Receiver<T> {
    /// The next item, `None` once the sender is gone and nothing is left.
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut state = self.0.state();
        if let Some(item) = state.items.pop_front() {
            if state.waiting > 0 {
                self.0.space.notify_one();
            }
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

    /// The next item if one is queued, without waiting.
    pub fn try_recv(&self) -> Option<T> {
        let mut state = self.0.state();
        let item = state.items.pop_front()?;
        if state.waiting > 0 {
            self.0.space.notify_one();
        }
        Some(item)
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
        let items = std::mem::take(&mut state.items);
        let waker = state.waker.take();
        drop(state);
        self.0.space.notify_all();
        drop((items, waker));
    }
}

impl<T> futures_core::Stream for Receiver<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consumer_wakes_and_discarded_values_never_run_under_the_queue_lock() {
        struct Observe(Arc<Shared<Observe>>);
        impl std::task::Wake for Observe {
            fn wake(self: Arc<Self>) {
                assert!(self.0.state.try_lock().is_ok());
            }
        }
        impl Drop for Observe {
            fn drop(&mut self) {
                assert!(self.0.state.try_lock().is_ok());
            }
        }
        let (tx, rx) = unbounded();
        let waker = Waker::from(Arc::new(Observe(Arc::clone(&tx.0))));
        assert!(rx.poll_recv(&mut Context::from_waker(&waker)).is_pending());
        assert!(tx.send(Observe(Arc::clone(&tx.0))).is_ok());
        drop(rx);
        drop(tx);
    }

    #[test]
    fn a_dropped_receiver_releases_a_waiting_sender() {
        let (tx, rx) = bounded(1);
        tx.send(1).unwrap();
        let shared = Arc::clone(&tx.0);
        let waiting = std::thread::spawn(move || tx.send(2));
        crate::block_on(shared.blocked.until(|| shared.blocked.count() > 0));
        drop(rx);
        assert_eq!(waiting.join().unwrap(), Err(2));
    }
}
