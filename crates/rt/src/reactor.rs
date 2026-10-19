//! The reactor: one thread that waits in the kernel (kqueue, epoll or IOCP,
//! through mio) for registered file descriptors to become ready and for the
//! earliest timer, and wakes the tasks waiting on either. It is shared by
//! every runtime, starts on first use, and with nothing due it sleeps until
//! something is: an idle process never wakes.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use crate::lock;

/// The token of the reactor's own wake-up.
const WAKE: mio::Token = mio::Token(usize::MAX);

pub(crate) struct Reactor {
    #[cfg(unix)]
    registry: mio::Registry,
    waker: mio::Waker,
    /// Pending timers by deadline, then by id, so equal deadlines stay
    /// apart and fire in the order they were set.
    timers: Mutex<BTreeMap<(Instant, u64), Waker>>,
    #[cfg(unix)]
    sources: Mutex<std::collections::HashMap<usize, std::sync::Arc<io::Source>>>,
    next: AtomicU64,
    wakes: AtomicU64,
}

impl Reactor {
    pub(crate) fn get() -> &'static Self {
        static REACTOR: OnceLock<Reactor> = OnceLock::new();
        REACTOR.get_or_init(|| {
            let poll = mio::Poll::new().expect("create the reactor's poller");
            let reactor = Self {
                #[cfg(unix)]
                registry: poll.registry().try_clone().expect("clone the registry"),
                waker: mio::Waker::new(poll.registry(), WAKE).expect("create the waker"),
                timers: Mutex::default(),
                #[cfg(unix)]
                sources: Mutex::default(),
                next: AtomicU64::new(0),
                wakes: AtomicU64::new(0),
            };
            // It waits for `REACTOR` to be set before it first looks.
            thread::Builder::new()
                .name("rt-reactor".into())
                .spawn(move || run(poll, Self::get()))
                .expect("spawn the reactor thread");
            reactor
        })
    }

    fn id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// Wakes the timers that are due; how long until the next one is.
    fn fire(&self) -> Option<Duration> {
        let now = Instant::now();
        let mut timers = lock(&self.timers);
        while let Some(entry) = timers.first_entry() {
            let at = entry.key().0;
            if at > now {
                return Some(at - now);
            }
            entry.remove().wake();
        }
        None
    }

    /// Sets (or re-sets, under the same key) the timer `key`.
    fn set(&self, key: (Instant, u64), waker: &Waker) {
        let mut timers = lock(&self.timers);
        let earliest = timers
            .first_key_value()
            .is_none_or(|(first, _)| key < *first);
        match timers.get_mut(&key) {
            Some(set) if set.will_wake(waker) => return,
            Some(set) => set.clone_from(waker),
            None => drop(timers.insert(key, waker.clone())),
        }
        drop(timers);
        // The reactor waits for an earlier deadline than it knew of.
        if earliest {
            self.wake();
        }
    }

    fn unset(&self, key: (Instant, u64)) {
        lock(&self.timers).remove(&key);
    }

    fn wake(&self) {
        // Only fails when the poller is gone, which it never is.
        let _ = self.waker.wake();
    }
}

/// How many times the reactor has woken (an event, a timer or a wake-up),
/// since the process started: how an idle process shows it stays asleep.
pub fn wakes() -> u64 {
    Reactor::get().wakes.load(Ordering::Relaxed)
}

fn run(mut poll: mio::Poll, reactor: &Reactor) {
    let mut events = mio::Events::with_capacity(256);
    loop {
        let timeout = reactor.fire();
        if let Err(e) = poll.poll(&mut events, timeout) {
            assert!(
                e.kind() == std::io::ErrorKind::Interrupted,
                "the reactor's poll failed: {e}"
            );
        }
        reactor.wakes.fetch_add(1, Ordering::Relaxed);
        #[cfg(unix)]
        for event in &events {
            if event.token() != WAKE {
                reactor.ready(event);
            }
        }
    }
}

/// Resolves at a deadline, on the reactor's clock. Dropping it forgets the
/// deadline.
#[derive(Debug)]
pub struct Timer {
    key: (Instant, u64),
    set: bool,
}

impl Timer {
    pub fn at(deadline: Instant) -> Self {
        Self {
            key: (deadline, Reactor::get().id()),
            set: false,
        }
    }

    pub fn after(duration: Duration) -> Self {
        Self::at(Instant::now() + duration)
    }
}

impl Future for Timer {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if Instant::now() >= self.key.0 {
            if std::mem::take(&mut self.set) {
                Reactor::get().unset(self.key);
            }
            return Poll::Ready(());
        }
        Reactor::get().set(self.key, cx.waker());
        self.set = true;
        Poll::Pending
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if self.set {
            Reactor::get().unset(self.key);
        }
    }
}

#[cfg(unix)]
pub use io::Async;

#[cfg(unix)]
mod io {
    use std::future::{Future, poll_fn};
    use std::io;
    use std::os::fd::{AsFd, AsRawFd};
    use std::sync::{Arc, Mutex};
    use std::task::{Poll, Waker};

    use mio::Interest;
    use mio::unix::SourceFd;

    use super::Reactor;
    use crate::lock;

    /// One direction of a registered descriptor: how many times it was
    /// reported ready, and the task waiting for the next time.
    #[derive(Default)]
    struct Direction {
        tick: u64,
        seen: u64,
        waker: Option<Waker>,
    }

    impl Direction {
        fn ready(&mut self) {
            self.tick += 1;
            if let Some(waker) = self.waker.take() {
                waker.wake();
            }
        }
    }

    #[derive(Default)]
    pub(super) struct Source {
        directions: Mutex<[Direction; 2]>,
    }

    const READ: usize = 0;
    const WRITE: usize = 1;

    impl Reactor {
        pub(super) fn ready(&self, event: &mio::event::Event) {
            let Some(source) = lock(&self.sources).get(&event.token().0).cloned() else {
                return;
            };
            let mut directions = lock(&source.directions);
            // An error or a hang-up ends what either side waits for.
            let failed = event.is_error();
            if failed || event.is_readable() || event.is_read_closed() {
                directions[READ].ready();
            }
            if failed || event.is_writable() || event.is_write_closed() {
                directions[WRITE].ready();
            }
        }
    }

    /// A file descriptor in non-blocking mode, registered with the
    /// reactor: its operations wait for readiness instead of blocking a
    /// thread. Readiness is edge-triggered, so an operation is retried
    /// until it would block, and only then is the next edge awaited.
    pub struct Async<T: AsFd> {
        io: T,
        key: usize,
        source: Arc<Source>,
    }

    impl<T: AsFd> Async<T> {
        pub fn new(io: T) -> io::Result<Self> {
            Self::register(io, Interest::READABLE | Interest::WRITABLE)
        }

        /// For a descriptor only ever read, or one that only signals (a
        /// pipe's read end, a pidfd): it is not watched for writing, which
        /// some descriptors refuse.
        pub fn for_reading(io: T) -> io::Result<Self> {
            Self::register(io, Interest::READABLE)
        }

        fn register(io: T, interest: Interest) -> io::Result<Self> {
            match rustix::io::ioctl_fionbio(&io, true) {
                // Nothing is read from a descriptor that has no modes,
                // such as a kqueue watching a process: it only signals.
                Ok(()) | Err(rustix::io::Errno::NOTTY) => {}
                Err(e) => return Err(e.into()),
            }
            let reactor = Reactor::get();
            let key = usize::try_from(reactor.id()).map_err(io::Error::other)?;
            let source = Arc::new(Source::default());
            lock(&reactor.sources).insert(key, Arc::clone(&source));
            let fd = io.as_fd().as_raw_fd();
            let registered =
                reactor
                    .registry
                    .register(&mut SourceFd(&fd), mio::Token(key), interest);
            if let Err(e) = registered {
                lock(&reactor.sources).remove(&key);
                return Err(e);
            }
            Ok(Self { io, key, source })
        }

        pub fn get_ref(&self) -> &T {
            &self.io
        }

        /// Resolves once the descriptor was reported readable since this
        /// last resolved: at once for one readable when registered.
        pub async fn readable(&self) {
            self.edge(READ, None).await;
        }

        /// As [`Async::readable`], for writing.
        pub async fn writable(&self) {
            self.edge(WRITE, None).await;
        }

        /// Runs `op` until it does not would-block, waiting for the
        /// descriptor to turn readable in between.
        pub async fn read_with<R>(&self, op: impl FnMut(&T) -> io::Result<R>) -> io::Result<R> {
            self.retry(READ, op).await
        }

        /// As [`Async::read_with`], for writing.
        pub async fn write_with<R>(&self, op: impl FnMut(&T) -> io::Result<R>) -> io::Result<R> {
            self.retry(WRITE, op).await
        }

        async fn retry<R>(
            &self,
            side: usize,
            mut op: impl FnMut(&T) -> io::Result<R>,
        ) -> io::Result<R> {
            loop {
                // Taken before trying, so readiness that comes while `op`
                // runs is not missed.
                let tick = lock(&self.source.directions)[side].tick;
                match op(&self.io) {
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        self.edge(side, Some(tick)).await;
                    }
                    result => return result,
                }
            }
        }

        /// Waits for a readiness edge after `after`, or after the last one
        /// this resolved for.
        fn edge(&self, side: usize, after: Option<u64>) -> impl Future<Output = ()> + '_ {
            poll_fn(move |cx| {
                let mut directions = lock(&self.source.directions);
                let direction = &mut directions[side];
                if direction.tick != after.unwrap_or(direction.seen) {
                    direction.seen = direction.tick;
                    return Poll::Ready(());
                }
                match &mut direction.waker {
                    Some(waker) if waker.will_wake(cx.waker()) => {}
                    slot => *slot = Some(cx.waker().clone()),
                }
                Poll::Pending
            })
        }
    }

    impl<T: AsFd> Drop for Async<T> {
        fn drop(&mut self) {
            let reactor = Reactor::get();
            let fd = self.io.as_fd().as_raw_fd();
            let _ = reactor.registry.deregister(&mut SourceFd(&fd));
            lock(&reactor.sources).remove(&self.key);
        }
    }
}
