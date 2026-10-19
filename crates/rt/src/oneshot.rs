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

fn end(slot: &Mutex<Slot>) {
    let mut slot = lock(slot);
    slot.ended = true;
    if let Some(waker) = slot.waker.take() {
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
    match &mut slot.waker {
        Some(waker) if waker.will_wake(cx.waker()) => {}
        waker => *waker = Some(cx.waker().clone()),
    }
    Poll::Pending
}
