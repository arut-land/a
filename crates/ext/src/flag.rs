//! A flag set once, awaited by many: a call's cancel, an actor's end.

use std::future::{Future, poll_fn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Poll, Waker};

/// Set once, and never cleared; cheap to clone, every clone the same flag.
/// Tasks that block poll it; async work awaits [`Flag::wait`].
#[derive(Clone, Debug, Default)]
pub struct Flag(Arc<Inner>);

#[derive(Debug, Default)]
struct Inner {
    set: AtomicBool,
    /// The tasks awaiting it.
    waiting: Mutex<Vec<(Arc<()>, Waker)>>,
}

impl Flag {
    pub fn set(&self) {
        self.0.set.store(true, Ordering::SeqCst);
        let waiting = std::mem::take(&mut *self.waiting());
        for (_, waker) in waiting {
            waker.wake();
        }
    }

    pub fn is_set(&self) -> bool {
        self.0.set.load(Ordering::SeqCst)
    }

    /// Resolves once set.
    pub async fn wait(&self) {
        if self.is_set() {
            return;
        }
        let waiter = Waiter {
            flag: self,
            token: Arc::new(()),
        };
        poll_fn(|cx| {
            if self.is_set() {
                return Poll::Ready(());
            }
            let mut waiting = self.waiting();
            // Checked again under the lock `set` takes to wake.
            if self.is_set() {
                return Poll::Ready(());
            }
            let previous = if let Some((_, waker)) = waiting
                .iter_mut()
                .find(|(token, _)| Arc::ptr_eq(token, &waiter.token))
            {
                Some(std::mem::replace(waker, cx.waker().clone()))
            } else {
                waiting.push((Arc::clone(&waiter.token), cx.waker().clone()));
                None
            };
            drop(waiting);
            drop(previous);
            Poll::Pending
        })
        .await;
    }

    /// Cancellation drops the operation, including any registered timer.
    pub async fn until<T>(&self, future: impl Future<Output = T>) -> Option<T> {
        let mut future = std::pin::pin!(future);
        let mut cancelled = std::pin::pin!(self.wait());
        poll_fn(|cx| {
            if cancelled.as_mut().poll(cx).is_ready() {
                return Poll::Ready(None);
            }
            future.as_mut().poll(cx).map(Some)
        })
        .await
    }

    fn waiting(&self) -> std::sync::MutexGuard<'_, Vec<(Arc<()>, Waker)>> {
        self.0
            .waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

struct Waiter<'a> {
    flag: &'a Flag,
    token: Arc<()>,
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        let removed = {
            let mut waiting = self.flag.waiting();
            waiting
                .iter()
                .position(|(token, _)| Arc::ptr_eq(token, &self.token))
                .map(|index| waiting.swap_remove(index))
        };
        drop(removed);
    }
}

/// Set when a running turn should stop: the user interrupted it, or the
/// session or daemon is going away.
pub type Cancel = Flag;

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Wake, Waker};

    use super::Flag;

    #[derive(Default)]
    struct Count(AtomicUsize);

    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn set_wakes_whoever_waits_and_stays_set() {
        let flag = Flag::default();
        let count = Arc::new(Count::default());
        let waker = Waker::from(Arc::clone(&count));
        let mut cx = Context::from_waker(&waker);
        let mut waiting = pin!(flag.wait());
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        flag.clone().set();
        assert!(count.0.load(Ordering::SeqCst) > 0);
        assert!(waiting.as_mut().poll(&mut cx).is_ready());
        assert!(pin!(flag.wait()).poll(&mut cx).is_ready());
        assert!(flag.is_set());
    }

    #[test]
    fn dropped_waits_release_their_wakers_without_setting_the_flag() {
        let flag = Flag::default();
        let count = Arc::new(Count::default());
        let waker = Waker::from(Arc::clone(&count));
        let mut waiting = Box::pin(flag.wait());
        assert!(
            waiting
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        drop(waiting);
        assert_eq!(Arc::strong_count(&count), 2);
        flag.set();
        assert_eq!(count.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn each_wait_owns_its_registration_even_when_wakers_are_shared() {
        let flag = Flag::default();
        let count = Arc::new(Count::default());
        let waker = Waker::from(Arc::clone(&count));
        let mut cx = Context::from_waker(&waker);
        let mut first = Box::pin(flag.wait());
        let mut second = pin!(flag.wait());
        assert!(first.as_mut().poll(&mut cx).is_pending());
        assert!(second.as_mut().poll(&mut cx).is_pending());
        assert_eq!(Arc::strong_count(&count), 4);
        drop(first);
        assert_eq!(Arc::strong_count(&count), 3);
        flag.set();
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
        assert!(second.as_mut().poll(&mut cx).is_ready());
    }

    #[test]
    fn repolling_replaces_and_releases_the_previous_waker() {
        let flag = Flag::default();
        let old = Arc::new(Count::default());
        let new = Arc::new(Count::default());
        let mut waiting = pin!(flag.wait());
        for count in [&old, &new] {
            let waker = Waker::from(Arc::clone(count));
            assert!(
                waiting
                    .as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
        }
        assert_eq!(Arc::strong_count(&old), 1);
        flag.set();
        assert_eq!(old.0.load(Ordering::SeqCst), 0);
        assert_eq!(new.0.load(Ordering::SeqCst), 1);
        assert_eq!(Arc::strong_count(&new), 1);
    }

    struct Unlocked(std::sync::Weak<super::Inner>);

    impl Unlocked {
        fn check(&self) {
            if let Some(inner) = self.0.upgrade() {
                assert!(
                    inner.waiting.try_lock().is_ok(),
                    "waker callback held the flag lock"
                );
            }
        }
    }

    impl Wake for Unlocked {
        fn wake(self: Arc<Self>) {
            self.check();
        }
    }

    impl Drop for Unlocked {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                self.check();
            }
        }
    }

    #[test]
    fn wakers_run_and_are_released_outside_the_flag_lock() {
        for cancelled in [false, true] {
            let flag = Flag::default();
            let mut waiting = Box::pin(flag.wait());
            for _ in 0..2 {
                let waker = Waker::from(Arc::new(Unlocked(Arc::downgrade(&flag.0))));
                assert!(
                    waiting
                        .as_mut()
                        .poll(&mut Context::from_waker(&waker))
                        .is_pending()
                );
            }
            if cancelled {
                drop(waiting);
            } else {
                flag.set();
                assert!(
                    waiting
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_ready()
                );
            }
        }
    }
}
