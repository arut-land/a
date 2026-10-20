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
    waiting: Mutex<Vec<Waker>>,
}

impl Flag {
    pub fn set(&self) {
        self.0.set.store(true, Ordering::SeqCst);
        let waiting = std::mem::take(&mut *self.waiting());
        waiting.into_iter().for_each(Waker::wake);
    }

    pub fn is_set(&self) -> bool {
        self.0.set.load(Ordering::SeqCst)
    }

    /// Resolves once set.
    pub fn wait(&self) -> impl Future<Output = ()> + Send + '_ {
        poll_fn(|cx| {
            if self.is_set() {
                return Poll::Ready(());
            }
            let mut waiting = self.waiting();
            // Checked again under the lock `set` takes to wake.
            if self.is_set() {
                return Poll::Ready(());
            }
            if !waiting.iter().any(|w| w.will_wake(cx.waker())) {
                waiting.push(cx.waker().clone());
            }
            Poll::Pending
        })
    }

    fn waiting(&self) -> std::sync::MutexGuard<'_, Vec<Waker>> {
        self.0
            .waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
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
}
