//! The async runtime seam. Library code is plain async Rust over [`Runtime`]:
//! it spawns, sleeps, runs blocking work and waits on the OS through
//! whichever runtime the host plugs in, and never names one itself.
//! [`Threads`] owns the workers; [`Async`] descriptors and timers share
//! its [`Reactor`], so a host never runs two.
//! [`block_on`] drives a future to completion on the calling thread.
//! [`reap`] waits for a detached child through its native exit watch.
//! [`EventSink`] is the observability seam, free when off.

pub mod channel;
mod events;
mod flush;
mod io;
mod notify;
pub mod oneshot;
#[cfg(feature = "threads")]
mod threads;
mod wakeup;

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
pub use futures_lite::future::yield_now;
/// Polls both futures to completion within their owner's lifetime.
pub use futures_lite::future::zip;
pub use io::{Async, Reactor, Source, capture_output};
pub use notify::Notify;
#[cfg(feature = "threads")]
pub use threads::{Config, PoolLimits, Threads};
pub use wakeup::Wakeup;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug)]
pub enum BlockingClass {
    Storage,
    Network,
    Cpu,
}

/// Default reservation for storage jobs whose buffers are owned by their caller.
pub const STORAGE_BYTES: usize = 8 << 20;

/// What the storage workers may hold at once, every job's reservation
/// together: the most one job can reserve, which runs it alone.
pub const STORAGE_BUDGET: usize = 64 << 20;

/// What the CPU workers may hold at once, as [`STORAGE_BUDGET`] is for
/// storage.
pub const CPU_BUDGET: usize = 64 << 20;

pub trait Runtime: Send + Sync + 'static {
    /// Runs `task` in the background; it is never awaited.
    fn spawn(&self, task: BoxFuture<'static, ()>);

    /// Admits `work` asynchronously, reserving `bytes` until it completes.
    /// Resolves when queued, not when finished. Dropping before admission
    /// cancels submission; accepted closures must arrange their own cancellation.
    fn spawn_blocking(
        &self,
        class: BlockingClass,
        bytes: usize,
        work: Box<dyn FnOnce() + Send>,
    ) -> BoxFuture<'static, std::io::Result<()>>;

    /// Flush leaders have a reserved worker, so storage reads waiting on an
    /// existing epoch cannot occupy the capacity needed to complete it.
    fn spawn_flush(
        &self,
        work: Box<dyn FnOnce() + Send>,
    ) -> BoxFuture<'static, std::io::Result<()>>;

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

#[cfg(feature = "watch")]
pub mod watch;

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
    unblock_with(rt, BlockingClass::Storage, STORAGE_BYTES, work)
}

/// `bytes` reserves queued captures and the job's working buffers until
/// completion. Waiting futures remain caller-owned and are cancellable.
pub fn unblock_with<T, F>(
    rt: &dyn Runtime,
    class: BlockingClass,
    bytes: usize,
    work: F,
) -> impl Future<Output = T> + Send + 'static
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let result = try_unblock_with(rt, class, bytes, work);
    async move { result.await.expect("blocking work failed") }
}

/// Runs blocking work, reporting refused admission or a lost worker as an error.
/// `bytes` reserves captures and working buffers until completion.
///
/// # Errors
/// Returns the runtime's admission error, or an error if the worker panics.
pub fn try_unblock_with<T, F>(
    rt: &dyn Runtime,
    class: BlockingClass,
    bytes: usize,
    work: F,
) -> impl Future<Output = std::io::Result<T>> + Send + 'static
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    let admitted = rt.spawn_blocking(class, bytes, Box::new(move || tx.send(work())));
    async move {
        admitted.await?;
        rx.await
            .ok_or_else(|| std::io::Error::other("blocking worker panicked"))
    }
}

/// The addresses `host` names, an IP literal without a lookup. A name goes
/// to the OS resolver (`getaddrinfo` and its equivalents), which only
/// blocks, so it runs briefly on the network lane: once per new
/// connection, never for a pooled one.
pub fn resolve(
    rt: &dyn Runtime,
    host: &str,
    port: u16,
) -> impl Future<Output = std::io::Result<Vec<std::net::SocketAddr>>> + Send + 'static {
    let literal = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .ok();
    let host = host.to_owned();
    let (tx, rx) = oneshot::channel();
    let admitted = literal.is_none().then(|| {
        rt.spawn_blocking(
            BlockingClass::Network,
            64 << 10,
            Box::new(move || {
                use std::net::ToSocketAddrs as _;
                tx.send(
                    (host.as_str(), port)
                        .to_socket_addrs()
                        .map(Iterator::collect),
                );
            }),
        )
    });
    async move {
        if let Some(ip) = literal {
            return Ok(vec![std::net::SocketAddr::new(ip, port)]);
        }
        admitted.expect("a name is looked up").await?;
        rx.await
            .unwrap_or_else(|| Err(std::io::Error::other("the resolver went away")))
    }
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

/// Reaps `child` once it exits, on a task waiting on its exit watch: a
/// child nothing else waits for leaves no zombie, and no thread waits for it.
pub fn reap(rt: &dyn Runtime, mut child: sys::Child) {
    let Ok(watch) = (child.exit_watch()).and_then(|watch| Async::for_reading(&rt.reactor(), watch))
    else {
        let admitted = rt.spawn_blocking(
            BlockingClass::Storage,
            0,
            Box::new(move || drop(child.wait())),
        );
        rt.spawn(Box::pin(async move { drop(admitted.await) }));
        return;
    };
    rt.spawn(Box::pin(async move {
        watch.readable().await;
        let _ = child.wait();
    }));
}

// Every lock here guards plain queues that a panicking holder cannot leave
// half-updated, so poisoning is ignored.

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn wait<'a, T>(cv: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    cv.wait(guard).unwrap_or_else(PoisonError::into_inner)
}

#[cfg(all(test, feature = "threads"))]
mod tests;
