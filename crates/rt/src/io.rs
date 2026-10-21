//! Waiting on the OS, through the reactor of the runtime the waiting tasks
//! run on: its timers, and descriptors registered with it, whose operations
//! wait for readiness instead of blocking a thread.

use std::io;
use std::time::Duration;

use crate::BoxFuture;

/// The reactor a runtime waits on the OS through.
#[derive(Clone, Debug)]
pub enum Reactor {
    /// async-io's: one thread waiting in kqueue, epoll or IOCP (through
    /// polling) for registered descriptors and the earliest timer. With
    /// nothing due it sleeps until something is: an idle process never
    /// wakes.
    AsyncIo,
    /// The reactor of the tokio runtime behind the handle.
    #[cfg(feature = "tokio")]
    Tokio(tokio::runtime::Handle),
}

impl Reactor {
    /// Resolves after `duration`, on this reactor's timers.
    pub fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        match self {
            Self::AsyncIo => Box::pin(async move {
                async_io::Timer::after(duration).await;
            }),
            #[cfg(feature = "tokio")]
            Self::Tokio(handle) => {
                let _entered = handle.enter();
                Box::pin(tokio::time::sleep(duration))
            }
        }
    }
}

/// What a reactor can watch: a file descriptor on Unix; on Windows a
/// socket, or a handle that is signalled.
#[cfg(unix)]
pub trait Source: std::os::fd::AsFd {}

#[cfg(unix)]
impl<T: std::os::fd::AsFd> Source for T {}

#[cfg(windows)]
pub trait Source {}

#[cfg(windows)]
impl<T> Source for T {}

/// A descriptor registered with a runtime's [`Reactor`].
pub struct Async<T: Source>(Inner<T>);

enum Inner<T: Source> {
    AsyncIo(async_io::Async<T>),
    #[cfg(windows)]
    Completion {
        events: [async_io::os::windows::Waitable<std::os::windows::io::OwnedHandle>; 2],
        io: T,
    },
    /// A handle that is signalled, as a process is when it exits.
    #[cfg(windows)]
    Signal(async_io::os::windows::Waitable<T>),
    #[cfg(all(unix, feature = "tokio"))]
    Tokio(tokio::io::unix::AsyncFd<Fd<T>>),
}

#[cfg(unix)]
impl<T: std::os::fd::AsFd> Async<T> {
    /// Registers `io`, in non-blocking mode, for reading and writing.
    pub fn new(reactor: &Reactor, io: T) -> io::Result<Self> {
        Self::register(reactor, io, true)
    }

    /// For a descriptor only ever read, or one that only signals (a pipe's
    /// read end, a pidfd, a kqueue): it is not watched for writing, which
    /// some descriptors refuse.
    pub fn for_reading(reactor: &Reactor, io: T) -> io::Result<Self> {
        Self::register(reactor, io, false)
    }

    fn register(reactor: &Reactor, io: T, write: bool) -> io::Result<Self> {
        match rustix::io::ioctl_fionbio(&io, true) {
            // Nothing is read from a descriptor that has no modes, such as
            // a kqueue watching a process: it only signals.
            Ok(()) | Err(rustix::io::Errno::NOTTY) => {}
            Err(e) => return Err(e.into()),
        }
        let inner = match reactor {
            // async-io watches a direction only while it is waited for.
            Reactor::AsyncIo => {
                let _ = write;
                Inner::AsyncIo(async_io::Async::new_nonblocking(io)?)
            }
            #[cfg(feature = "tokio")]
            Reactor::Tokio(handle) => {
                use tokio::io::Interest;
                let interest = if write {
                    Interest::READABLE | Interest::WRITABLE
                } else {
                    Interest::READABLE
                };
                let _entered = handle.enter();
                // The safe registration; its replacement is `unsafe`, and
                // unsafe code lives in `sys` only.
                #[expect(deprecated)]
                let fd = tokio::io::unix::AsyncFd::with_interest(Fd(io), interest);
                Inner::Tokio(fd?)
            }
        };
        Ok(Self(inner))
    }
}

#[cfg(unix)]
impl<T: std::os::fd::AsFd> Async<T> {
    /// Registers socket `io`, in non-blocking mode: [`Async::new`], by the
    /// name Windows registers a socket by.
    pub fn socket(reactor: &Reactor, io: T) -> io::Result<Self> {
        Self::new(reactor, io)
    }
}

#[cfg(windows)]
impl<T: sys::CompletionSource> Async<T> {
    /// Awaits overlapped I/O through async-io's waitable-event support.
    /// sys owns every submitted buffer until cancellation has completed.
    pub fn new(reactor: &Reactor, io: T) -> io::Result<Self> {
        match reactor {
            Reactor::AsyncIo => {
                io.nonblocking()?;
                let [read, write] = io.completion_events()?;
                Ok(Self(Inner::Completion {
                    events: [
                        async_io::os::windows::Waitable::new(read)?,
                        async_io::os::windows::Waitable::new(write)?,
                    ],
                    io,
                }))
            }
            #[cfg(feature = "tokio")]
            Reactor::Tokio(_) => Err(io::ErrorKind::Unsupported.into()),
        }
    }
}

#[cfg(windows)]
impl<T: std::os::windows::io::AsHandle> Async<T> {
    /// For a handle that is signalled, as a process is when it exits: it
    /// turns readable once it is.
    pub fn for_reading(reactor: &Reactor, io: T) -> io::Result<Self> {
        match reactor {
            Reactor::AsyncIo => Ok(Self(Inner::Signal(async_io::os::windows::Waitable::new(
                io,
            )?))),
            #[cfg(feature = "tokio")]
            Reactor::Tokio(_) => Err(io::ErrorKind::Unsupported.into()),
        }
    }
}

#[cfg(windows)]
impl<T: std::os::windows::io::AsSocket> Async<T> {
    /// Registers socket `io`, in non-blocking mode.
    pub fn socket(reactor: &Reactor, io: T) -> io::Result<Self> {
        match reactor {
            Reactor::AsyncIo => Ok(Self(Inner::AsyncIo(async_io::Async::new(io)?))),
            #[cfg(feature = "tokio")]
            Reactor::Tokio(_) => Err(io::ErrorKind::Unsupported.into()),
        }
    }
}

impl Async<sys::ProcessPipe> {
    /// Child output needs readiness on Unix and overlapped completion on Windows.
    pub fn read_pipe(reactor: &Reactor, io: sys::ProcessPipe) -> io::Result<Self> {
        #[cfg(unix)]
        return Self::for_reading(reactor, io);
        #[cfg(windows)]
        Self::new(reactor, io)
    }
}

impl<T: Source> Async<T> {
    pub fn get_ref(&self) -> &T {
        match &self.0 {
            Inner::AsyncIo(io) => io.get_ref(),
            #[cfg(windows)]
            Inner::Signal(handle) => handle.get_ref(),
            #[cfg(windows)]
            Inner::Completion { io, .. } => io,
            #[cfg(all(unix, feature = "tokio"))]
            Inner::Tokio(fd) => &fd.get_ref().0,
        }
    }

    /// Resolves once the descriptor was reported readable since this last
    /// resolved: soon after registering, for one already readable.
    pub async fn readable(&self) {
        let _ = self.ready(false).await;
    }

    /// As [`Async::readable`], for writing.
    pub async fn writable(&self) {
        let _ = self.ready(true).await;
    }

    /// Runs `op` until it does not would-block, waiting for the descriptor
    /// to turn readable in between.
    pub async fn read_with<R>(&self, op: impl FnMut(&T) -> io::Result<R>) -> io::Result<R> {
        self.retry(false, op).await
    }

    /// As [`Async::read_with`], for writing.
    pub async fn write_with<R>(&self, op: impl FnMut(&T) -> io::Result<R>) -> io::Result<R> {
        self.retry(true, op).await
    }

    async fn retry<R>(
        &self,
        write: bool,
        mut op: impl FnMut(&T) -> io::Result<R>,
    ) -> io::Result<R> {
        loop {
            match op(self.get_ref()) {
                // Readiness that comes after the attempt is the next.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.ready(write).await?;
                }
                result => return result,
            }
        }
    }

    /// Resolves once the reactor reports the direction ready after this
    /// started waiting.
    async fn ready(&self, write: bool) -> io::Result<()> {
        match &self.0 {
            // A future that keeps the tick it started at, not
            // `poll_readable`, which takes a new one each poll: an
            // already-ready descriptor would race the reactor for it,
            // spinning both.
            Inner::AsyncIo(io) if write => io.writable().await,
            Inner::AsyncIo(io) => io.readable().await,
            #[cfg(windows)]
            Inner::Signal(_) if write => std::future::pending().await,
            #[cfg(windows)]
            Inner::Signal(handle) => handle.ready().await,
            #[cfg(windows)]
            Inner::Completion { events, .. } => events[usize::from(write)].ready().await,
            #[cfg(all(unix, feature = "tokio"))]
            Inner::Tokio(fd) => {
                let mut ready = if write {
                    fd.writable().await?
                } else {
                    fd.readable().await?
                };
                // Taken as it is seen, so what comes after is the next.
                ready.clear_ready();
                Ok(())
            }
        }
    }
}

/// What tokio's `AsyncFd` takes: a raw descriptor.
#[cfg(all(unix, feature = "tokio"))]
struct Fd<T>(T);

#[cfg(all(unix, feature = "tokio"))]
impl<T: std::os::fd::AsFd> std::os::fd::AsRawFd for Fd<T> {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0.as_fd().as_raw_fd()
    }
}
