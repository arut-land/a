//! A coalescing wake for one consumer. Registration may be suspended without
//! preventing a producer from publishing a wake.

use std::future::poll_fn;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;

use atomic_waker::AtomicWaker;

#[derive(Debug, Default)]
pub struct Wakeup {
    pending: AtomicBool,
    waker: AtomicWaker,
}

impl Wakeup {
    pub const fn new() -> Self {
        Self {
            pending: AtomicBool::new(false),
            waker: AtomicWaker::new(),
        }
    }

    /// Non-blocking even while the consumer replaces its registered waker.
    pub fn notify(&self) {
        self.pending.store(true, Ordering::Release);
        self.waker.wake();
    }

    /// Only one consumer may wait at a time. Multiple notifications coalesce
    /// and a notification without a waiter is remembered.
    pub async fn wait(&self) {
        poll_fn(|cx| {
            if self.pending.swap(false, Ordering::Acquire) {
                return Poll::Ready(());
            }
            self.waker.register(cx.waker());
            if self.pending.swap(false, Ordering::Acquire) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Waker};

    #[test]
    fn wakes_are_remembered_and_coalesced() {
        let wake = Wakeup::new();
        wake.notify();
        wake.notify();
        crate::block_on(wake.wait());
        assert!(
            pin!(wake.wait())
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
    }
}
