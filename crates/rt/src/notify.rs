//! A wake-up any number of tasks can wait for. Each [`Notify::notify`]
//! counts one more and wakes every waiter; a waiter says what it waits
//! for as a condition, checked before it sleeps and after every wake, so
//! a notify between its check and its sleep is never lost.

use std::future::poll_fn;
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};

use crate::lock;

#[derive(Debug, Default)]
pub struct Notify(Mutex<State>);

#[derive(Debug, Default)]
struct State {
    count: u64,
    waiters: Vec<(Arc<()>, Waker)>,
}

impl Notify {
    pub const fn new() -> Self {
        Self(Mutex::new(State {
            count: 0,
            waiters: Vec::new(),
        }))
    }

    pub fn notify(&self) {
        let mut state = lock(&self.0);
        state.count += 1;
        let waiters = std::mem::take(&mut state.waiters);
        drop(state);
        for (_, waker) in waiters {
            waker.wake();
        }
    }

    /// How many times it was notified.
    pub fn count(&self) -> u64 {
        lock(&self.0).count
    }

    /// Resolves once `ready` holds; it is checked now and after each
    /// notify.
    pub async fn until(&self, mut ready: impl FnMut() -> bool + Send) {
        let waiter = Waiter {
            notify: self,
            token: Arc::new(()),
        };
        poll_fn(|cx| {
            if ready() {
                return Poll::Ready(());
            }
            let mut state = lock(&self.0);
            let previous = if let Some((_, waker)) = state
                .waiters
                .iter_mut()
                .find(|(token, _)| Arc::ptr_eq(token, &waiter.token))
            {
                Some(std::mem::replace(waker, cx.waker().clone()))
            } else {
                state
                    .waiters
                    .push((Arc::clone(&waiter.token), cx.waker().clone()));
                None
            };
            drop(state);
            drop(previous);
            // A notify between the check and the registration may have
            // missed this waker.
            if ready() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }

    /// Resolves once notified since `seen` was taken from
    /// [`Notify::count`], and moves `seen` up to now: notifies that came
    /// while no one waited are still seen, once.
    pub async fn changed(&self, seen: &mut u64) {
        let since = *seen;
        self.until(|| self.count() != since).await;
        *seen = self.count();
    }
}

struct Waiter<'a> {
    notify: &'a Notify,
    token: Arc<()>,
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        let removed = {
            let mut state = lock(&self.notify.0);
            state
                .waiters
                .iter()
                .position(|(token, _)| Arc::ptr_eq(token, &self.token))
                .map(|index| state.waiters.swap_remove(index))
        };
        drop(removed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_on;
    use std::future::Future;
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Wake};

    struct Count(AtomicUsize);

    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn cancelled_and_completed_waiters_release_their_wakers() {
        let notify = Notify::new();
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&count));
        let mut cx = Context::from_waker(&waker);
        let mut cancelled = Box::pin(notify.until(|| false));
        assert!(cancelled.as_mut().poll(&mut cx).is_pending());
        drop(cancelled);
        assert_eq!(Arc::strong_count(&count), 2);

        let ready = std::sync::atomic::AtomicBool::new(false);
        let mut completed = pin!(notify.until(|| ready.load(Ordering::SeqCst)));
        assert!(completed.as_mut().poll(&mut cx).is_pending());
        ready.store(true, Ordering::SeqCst);
        assert!(completed.as_mut().poll(&mut cx).is_ready());
        assert_eq!(Arc::strong_count(&count), 2);

        let mut checks = 0;
        let mut during_registration = pin!(notify.until(|| {
            checks += 1;
            checks == 2
        }));
        assert!(during_registration.as_mut().poll(&mut cx).is_ready());
        assert_eq!(Arc::strong_count(&count), 2);
        notify.notify();
        assert_eq!(count.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cancelling_one_waiter_preserves_another_with_the_same_waker() {
        let notify = Notify::new();
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&count));
        let mut cx = Context::from_waker(&waker);
        let mut first = Box::pin(notify.until(|| false));
        let mut second = pin!(notify.until(|| false));
        assert!(first.as_mut().poll(&mut cx).is_pending());
        assert!(second.as_mut().poll(&mut cx).is_pending());
        drop(first);
        notify.notify();
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_notify_wakes_every_waiter_and_is_remembered() {
        let notify = Notify::new();
        let mut waiters: Vec<_> = (0..3)
            .map(|_| {
                let count = Arc::new(Count(AtomicUsize::new(0)));
                let mut changed = Box::pin(notify.until(|| notify.count() != 0));
                let waker = Waker::from(Arc::clone(&count));
                assert!(
                    changed
                        .as_mut()
                        .poll(&mut Context::from_waker(&waker))
                        .is_pending()
                );
                (changed, count)
            })
            .collect();
        notify.notify();
        for (waiter, count) in &mut waiters {
            assert_eq!(count.0.load(Ordering::SeqCst), 1);
            block_on(waiter);
        }
        let mut seen = 0;
        block_on(notify.changed(&mut seen));
        assert_eq!(seen, 1);
        assert!(
            pin!(notify.changed(&mut seen))
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
    }
}
