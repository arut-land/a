//! A channel from any producer to an async consumer. The receiver is a
//! stream. Unbounded senders never wait; bounded senders either park a
//! blocking producer or yield an async producer until space is available.

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
    sender: Option<Waker>,
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

/// A bounded sender that yields its task while the channel is full.
pub struct AsyncSender<T>(Arc<Shared<T>>);

pub struct Receiver<T>(Arc<Shared<T>>);

fn channel<T>(capacity: usize) -> (Arc<Shared<T>>, Receiver<T>) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            items: VecDeque::new(),
            closed: false,
            receiver_gone: false,
            waker: None,
            waiting: 0,
            sender: None,
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

/// A bounded channel whose producer waits asynchronously for space.
pub fn bounded_async<T>(capacity: usize) -> (AsyncSender<T>, Receiver<T>) {
    let (shared, rx) = channel(capacity.max(1));
    (AsyncSender(shared), rx)
}

impl<T: Send> AsyncSender<T> {
    /// Sends once there is room; returns the item if the receiver is gone.
    pub async fn send(&mut self, item: T) -> Result<(), T> {
        let mut item = Some(item);
        std::future::poll_fn(|cx| {
            let mut state = self.0.state();
            if state.receiver_gone {
                return Poll::Ready(Err(item.take().expect("send completes once")));
            }
            if state.items.len() == self.0.capacity {
                state.sender = Some(cx.waker().clone());
                return Poll::Pending;
            }
            state
                .items
                .push_back(item.take().expect("send completes once"));
            let waker = state.waker.take();
            drop(state);
            if let Some(waker) = waker {
                waker.wake();
            }
            Poll::Ready(Ok(()))
        })
        .await
    }
}

impl<T> Drop for AsyncSender<T> {
    fn drop(&mut self) {
        self.0.close();
    }
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
            let sender = state.sender.take();
            drop(state);
            if let Some(sender) = sender {
                sender.wake();
            }
            return Poll::Ready(Some(item));
        }
        if state.closed {
            return Poll::Ready(None);
        }
        let previous = match &state.waker {
            Some(waker) if waker.will_wake(cx.waker()) => None,
            _ => state.waker.replace(cx.waker().clone()),
        };
        drop(state);
        drop(previous);
        Poll::Pending
    }

    /// The next item if one is queued, without waiting.
    pub fn try_recv(&self) -> Option<T> {
        let mut state = self.0.state();
        let item = state.items.pop_front()?;
        if state.waiting > 0 {
            self.0.space.notify_one();
        }
        let sender = state.sender.take();
        drop(state);
        if let Some(sender) = sender {
            sender.wake();
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
        let waiting = state.waiting > 0;
        let items = std::mem::take(&mut state.items);
        let waker = state.waker.take();
        let sender = state.sender.take();
        drop(state);
        if let Some(sender) = sender {
            sender.wake();
        }
        if waiting {
            self.0.space.notify_all();
        }
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
        drop(rx.try_recv());
        drop(waker);
        let waker = Waker::from(Arc::new(Observe(Arc::clone(&tx.0))));
        assert!(rx.poll_recv(&mut Context::from_waker(&waker)).is_pending());
        drop(waker);
        assert!(
            rx.poll_recv(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert!(tx.send(Observe(Arc::clone(&tx.0))).is_ok());
        drop(rx);
        drop(tx);
    }

    #[test]
    fn async_backpressure_wakes_for_space_and_receiver_drop() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::Wake;

        #[derive(Default)]
        struct Wakes(AtomicUsize);
        impl Wake for Wakes {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let counter = Arc::new(Wakes::default());
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);
        let (mut tx, rx) = bounded_async(1);
        crate::block_on(tx.send(1)).unwrap();
        {
            let send = tx.send(2);
            let mut send = std::pin::pin!(send);
            assert!(send.as_mut().poll(&mut cx).is_pending());
            assert_eq!(rx.try_recv(), Some(1));
            assert_eq!(counter.0.load(Ordering::Relaxed), 1);
            assert_eq!(crate::block_on(send), Ok(()));
        }
        let send = tx.send(3);
        let mut send = std::pin::pin!(send);
        assert!(send.as_mut().poll(&mut cx).is_pending());
        drop(rx);
        assert_eq!(counter.0.load(Ordering::Relaxed), 2);
        assert_eq!(crate::block_on(send), Err(3));
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
