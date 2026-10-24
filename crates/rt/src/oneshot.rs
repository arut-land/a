//! One value from any thread to a future: what a blocking job or a spawned
//! task hands back, or an end with nothing when its sender is dropped (the
//! job panicked, or had nothing to say). The slot is one type for every
//! value, boxed, so each new type sent costs a box and a downcast rather
//! than a channel's code of its own.

use std::any::Any;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use crate::lock;

type Value = Box<dyn Any + Send>;

#[derive(Default)]
struct Slot {
    value: Option<Value>,
    /// The sender is gone: nothing more comes.
    ended: bool,
    waker: Option<Waker>,
}

/// The sending half; dropping it unsent ends the receiver with `None`.
pub struct Sender<T>(Arc<Mutex<Slot>>, PhantomData<fn(T)>);

/// The receiving half: a future of the value, `None` if none was sent.
pub struct Receiver<T>(Arc<Mutex<Slot>>, PhantomData<fn() -> T>);

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let slot = Arc::default();
    (
        Sender(Arc::clone(&slot), PhantomData),
        Receiver(slot, PhantomData),
    )
}

impl<T: Send + 'static> Sender<T> {
    pub fn send(self, value: T) {
        put(&self.0, Box::new(value));
    }
}

fn put(slot: &Mutex<Slot>, value: Value) {
    lock(slot).value = Some(value);
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        end(&self.0);
    }
}

// Wakers run arbitrary code, including a poll of this receiver: never under the slot's lock.
fn end(slot: &Mutex<Slot>) {
    let waker = {
        let mut slot = lock(slot);
        slot.ended = true;
        slot.waker.take()
    };
    if let Some(waker) = waker {
        waker.wake();
    }
}

impl<T: 'static> Future for Receiver<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        poll(&self.0, cx).map(|value| Some(*value?.downcast().ok()?))
    }
}

fn poll(slot: &Mutex<Slot>, cx: &Context<'_>) -> Poll<Option<Value>> {
    let mut slot = lock(slot);
    if slot.value.is_some() || slot.ended {
        return Poll::Ready(slot.value.take());
    }
    let displaced = match &slot.waker {
        Some(waker) if waker.will_wake(cx.waker()) => None,
        _ => slot.waker.replace(cx.waker().clone()),
    };
    drop(slot);
    drop(displaced);
    Poll::Pending
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex, mpsc};
    use std::task::{Wake, Waker};
    use std::time::Duration;

    struct Reenter {
        receiver: Mutex<Receiver<usize>>,
        finished: mpsc::Sender<bool>,
    }

    impl Wake for Reenter {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            let mut receiver = self.receiver.lock().unwrap();
            let result = Pin::new(&mut *receiver).poll(&mut Context::from_waker(Waker::noop()));
            let _ = self.finished.send(matches!(result, Poll::Ready(None)));
        }
    }

    #[test]
    fn cancellation_allows_a_waker_to_poll_its_receiver() {
        let (sender, receiver) = channel();
        let (finished, result) = mpsc::channel();
        let wake = Arc::new(Reenter {
            receiver: Mutex::new(receiver),
            finished,
        });
        let waker = Waker::from(Arc::clone(&wake));
        assert!(
            Pin::new(&mut *wake.receiver.lock().unwrap())
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        let cancelled = std::thread::spawn(move || drop(sender));
        assert_eq!(
            result.recv_timeout(Duration::from_secs(2)),
            Ok(true),
            "cancellation must permit receiver polling during wake delivery"
        );
        cancelled.join().unwrap();
    }
}
