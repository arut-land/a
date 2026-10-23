//! A single value from a task or blocking worker; sender cancellation is `None`.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pub struct Sender<T>(oneshot::Sender<T>);
pub struct Receiver<T>(oneshot::AsyncReceiver<T>);

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let (sender, receiver) = oneshot::async_channel();
    (Sender(sender), Receiver(receiver))
}

impl<T> Sender<T> {
    pub fn send(self, value: T) {
        let _ = self.0.send(value);
    }
}

impl<T> Future for Receiver<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.get_mut().0).poll(cx).map(Result::ok)
    }
}
