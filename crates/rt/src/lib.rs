//! The async runtime seam. Library code is plain async Rust over [`Runtime`]:
//! it spawns, sleeps, runs blocking work and waits on the OS through
//! whichever runtime the host plugs in, and never names one itself.
//! [`Threads`] is the built-in runtime; `smol` and `tokio` adapters sit
//! behind features. A runtime brings its own reactor ([`Reactor`]), which
//! [`Async`] descriptors register with, so a host never runs two.
//! [`block_on`] drives a future to completion on the calling thread.
//! [`EventSink`] is the observability seam, free when off.

pub mod channel;
mod events;
mod flush;
mod io;
mod notify;
pub mod oneshot;
#[cfg(feature = "threads")]
mod threads;

#[cfg(feature = "smol")]
mod smol;
#[cfg(feature = "tokio")]
mod tokio;

use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

pub use events::{Event, EventSink, Noop, Stderr};
pub use flush::{FlushEpoch, FlushLeader, FlushRequest, Flusher, unblock_flush};
/// The first of two futures to resolve, the left polled first: every
/// race in the engine and the extensions is one of these.
pub use futures_lite::future::or;
pub use io::{Async, Reactor, Source};
pub use notify::Notify;
#[cfg(feature = "threads")]
pub use threads::Threads;

#[cfg(feature = "smol")]
pub use self::smol::Smol;
#[cfg(feature = "tokio")]
pub use self::tokio::Tokio;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait Runtime: Send + Sync + 'static {
    /// Runs `task` in the background; it is never awaited.
    fn spawn(&self, task: BoxFuture<'static, ()>);

    /// Runs `work`, which may block, off the async workers.
    fn spawn_blocking(&self, work: Box<dyn FnOnce() + Send>);

    /// The reactor this runtime's tasks wait on the OS through: its timers
    /// and the descriptors registered with it ([`Async`]).
    fn reactor(&self) -> Reactor;

    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        self.reactor().sleep(duration)
    }

    /// The runtime's clock: a simulated runtime runs its own.
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A shared handle to the runtime a component was given.
pub type Rt = Arc<dyn Runtime>;

/// How many times a thread waiting in [`block_on`] has woken.
static WAKES: AtomicU64 = AtomicU64::new(0);

/// Drives `future` to completion on the calling thread, which sleeps
/// between its wakes.
pub fn block_on<F: Future>(future: F) -> F::Output {
    struct Unpark(Thread);

    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        // A stray unpark only polls once more.
        thread::park();
        WAKES.fetch_add(1, Ordering::Relaxed);
    }
}

/// How many times, since the process started, a thread waiting in
/// [`block_on`] (an async worker of [`Threads`] among them) has woken: how
/// an idle process shows it stays asleep.
pub fn wakes() -> u64 {
    WAKES.load(Ordering::Relaxed)
}

/// The result of blocking `work`, run through `rt` without blocking the
/// caller's task.
pub fn unblock<T, F>(rt: &dyn Runtime, work: F) -> impl Future<Output = T> + Send + 'static
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    rt.spawn_blocking(Box::new(move || tx.send(work())));
    async move { rx.await.expect("blocking work panicked") }
}

/// Spawns `task` and returns a future of its output.
pub fn spawn<T, F>(rt: &dyn Runtime, task: F) -> impl Future<Output = T> + Send + 'static
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    rt.spawn(Box::pin(async move { tx.send(task.await) }));
    async move { rx.await.expect("spawned task panicked") }
}

// Every lock here guards plain queues that a panicking holder cannot leave
// half-updated, so poisoning is ignored.

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn wait<'a, T>(cv: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    cv.wait(guard).unwrap_or_else(PoisonError::into_inner)
}

/// Waits up to `timeout`; `true` if it timed out.
#[cfg(feature = "threads")]
fn wait_timeout<'a, T>(
    cv: &Condvar,
    guard: MutexGuard<'a, T>,
    timeout: Duration,
) -> (MutexGuard<'a, T>, bool) {
    let (guard, result) = cv
        .wait_timeout(guard, timeout)
        .unwrap_or_else(PoisonError::into_inner);
    (guard, result.timed_out())
}

#[cfg(all(test, feature = "threads"))]
mod tests;
