//! The async runtime seam. Library code is plain async Rust over [`Runtime`]:
//! it spawns, sleeps and runs blocking work through whichever runtime the
//! host plugs in, and never names one itself. [`Threads`] is the built-in
//! runtime; `smol` and `tokio` adapters sit behind features; [`block_on`]
//! drives a future to completion on the calling thread.

mod block;
pub mod channel;
#[cfg(feature = "threads")]
mod threads;
#[cfg(feature = "threads")]
mod timer;

#[cfg(feature = "smol")]
mod smol;
#[cfg(feature = "tokio")]
mod tokio;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

pub use block::block_on;
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

    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()>;
}

/// A shared handle to the runtime a component was given.
pub type Rt = Arc<dyn Runtime>;

/// The result of blocking `work`, run through `rt` without blocking the
/// caller's task.
pub fn unblock<T, F>(rt: &dyn Runtime, work: F) -> impl Future<Output = T> + Send + 'static
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = channel::bounded(1);
    rt.spawn_blocking(Box::new(move || drop(tx.send(work()))));
    async move { rx.recv().await.expect("blocking work panicked") }
}

/// Spawns `task` and returns a future of its output.
pub fn spawn<T, F>(rt: &dyn Runtime, task: F) -> impl Future<Output = T> + Send + 'static
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let (tx, rx) = channel::bounded(1);
    rt.spawn(Box::pin(async move { drop(tx.send(task.await)) }));
    async move { rx.recv().await.expect("spawned task panicked") }
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
