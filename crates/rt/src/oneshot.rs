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
