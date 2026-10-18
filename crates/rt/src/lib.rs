//! The async runtime seam. Library code is plain async Rust over [`Runtime`]:
//! it spawns, sleeps and runs blocking work through whichever runtime the
//! host plugs in, and never names one itself. [`Threads`] is the built-in
//! runtime; `smol` and `tokio` adapters sit behind features; [`block_on`]
//! drives a future to completion on the calling thread.

mod block;
pub mod channel;
mod oneshot;
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
use std::sync::Arc;
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

#[cfg(all(test, feature = "threads"))]
mod tests;
