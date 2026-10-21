//! A wake-up any number of tasks can wait for. Each [`Notify::notify`]
//! counts one more and wakes every waiter; a waiter says what it waits
//! for as a condition, checked before it sleeps and after every wake, so
//! a notify between its check and its sleep is never lost.

use std::future::{Future, poll_fn};
use std::sync::Mutex;
use std::task::{Poll, Waker};

use crate::lock;

#[derive(Debug, Default)]
pub struct Notify(Mutex<State>);

#[derive(Debug, Default)]
struct State {
    count: u64,
    waiters: Vec<Waker>,
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
        waiters.into_iter().for_each(Waker::wake);
    }

    /// How many times it was notified.
    pub fn count(&self) -> u64 {
        lock(&self.0).count
    }

    /// Resolves once `ready` holds; it is checked now and after each
    /// notify.
    pub fn until<'a>(
        &'a self,
        mut ready: impl FnMut() -> bool + Send + 'a,
    ) -> impl Future<Output = ()> + Send + 'a {
        poll_fn(move |cx| {
            if ready() {
                return Poll::Ready(());
            }
            let mut state = lock(&self.0);
            if !state.waiters.iter().any(|w| w.will_wake(cx.waker())) {
                state.waiters.push(cx.waker().clone());
            }
            drop(state);
            // A notify between the check and the registration may have
            // missed this waker.
            if ready() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_on;
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
