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
}

impl Reactor {
    /// Continuously ready I/O must give other sessions a poll.
    pub async fn yield_now(&self) {
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            if yielded {
                return std::task::Poll::Ready(());
            }
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        })
        .await;
    }

    /// Resolves after `duration`, on this reactor's timers.
    pub fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        match self {
            Self::AsyncIo => Box::pin(async move {
                async_io::Timer::after(duration).await;
            }),
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
        complete_write: fn(&T) -> io::Result<()>,
        io: T,
    },
    /// A handle that is signalled, as a process is when it exits.
    #[cfg(windows)]
    Signal(async_io::os::windows::Waitable<T>),
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
                    complete_write: T::complete_write,
                    io,
                }))
            }
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
        }
    }
}

#[cfg(windows)]
impl<T: std::os::windows::io::AsSocket> Async<T> {
    /// Registers socket `io`, in non-blocking mode.
    pub fn socket(reactor: &Reactor, io: T) -> io::Result<Self> {
        match reactor {
            Reactor::AsyncIo => Ok(Self(Inner::AsyncIo(async_io::Async::new(io)?))),
        }
    }
}

#[cfg(unix)]
impl Async<std::fs::File> {
    /// Child output only needs read readiness.
    pub fn read_pipe(reactor: &Reactor, io: std::fs::File) -> io::Result<Self> {
        Self::for_reading(reactor, io)
    }
}

#[cfg(windows)]
impl Async<sys::ProcessPipe> {
    /// Child output completes overlapped reads on its owned event.
    pub fn read_pipe(reactor: &Reactor, io: sys::ProcessPipe) -> io::Result<Self> {
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
        }
    }

    /// Resolves once the descriptor was reported readable since this last
    /// resolved: soon after registering, for one already readable.
    pub async fn readable(&self) {
        let _ = self.ready(false).await;
    }

    /// Process supervisors must clean up if readiness observation fails.
    pub async fn readable_result(&self) -> io::Result<()> {
        self.ready(false).await
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
        let result = self.retry(true, op).await?;
        #[cfg(windows)]
        if let Inner::Completion { complete_write, .. } = &self.0 {
            self.retry(true, complete_write).await?;
        }
        Ok(result)
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
        }
    }
}

/// Copies captured process output without keeping a blocking worker while idle.
pub async fn capture_output<T: Source + Send + Sync>(
    runtime: &dyn crate::Runtime,
    input: Async<T>,
    mut file: std::fs::File,
    copied: fn(&std::fs::File) -> io::Result<()>,
) -> io::Result<()>
where
    for<'a> &'a T: io::Read,
{
    use std::io::{Read as _, Write as _};
    let mut buffer = vec![0; 64 << 10];
    loop {
        let read = input.read_with(|mut input| input.read(&mut buffer)).await?;
        if read == 0 {
            return Ok(());
        }
        let (state, result) = crate::unblock_with(
            runtime,
            crate::BlockingClass::Storage,
            buffer.len(),
            move || {
                let result = file.write_all(&buffer[..read]).and_then(|()| copied(&file));
                ((file, buffer), result)
            },
        )
        .await;
        (file, buffer) = state;
        result?;
    }
}

#[cfg(all(test, windows, feature = "threads"))]
mod capture_tests {
    use super::*;
    use crate::Runtime as _;
    use std::{fs, process::Command};

    #[test]
    fn captured_output_wakes_followers_before_exit_and_drains_past_pipe_capacity() {
        let runtime = crate::Threads::new(1);
        let root = std::env::temp_dir().join(format!("a-captured-output-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        for (case, command, expected) in [
            ("live", "printf progress; sleep 600", b"progress".to_vec()),
            ("drain", "printf '%*s' 1048576 ''", vec![b' '; 1 << 20]),
        ] {
            let path = root.join(case);
            let file = sys::fs::private_file().open(&path).unwrap();
            let watch =
                Async::for_reading(&runtime.reactor(), sys::write_watch(&path).unwrap()).unwrap();
            let mut command_line =
                Command::new(sys::find_bash(std::env::var("PATH").ok().as_deref()).unwrap());
            command_line.args(["-c", command]).envs(std::env::vars_os());
            let mut child = sys::spawn_group_captured(&mut command_line, file).unwrap();
            let capture = child.capture.take().unwrap();
            let input = Async::read_pipe(&runtime.reactor(), capture.reader).unwrap();
            let copying = capture_output(&runtime, input, capture.file, sys::fs::output_written);
            let observing = async {
                if case == "live" {
                    watch.readable_result().await.unwrap();
                    assert_eq!(fs::read(&path).unwrap(), expected);
                    assert!(child.try_wait().unwrap().is_none());
                    child.group.signal(sys::Signal::Kill).unwrap();
                }
            };
            crate::block_on(crate::or(
                async {
                    let (copied, ()) = crate::zip(copying, observing).await;
                    copied.unwrap();
                },
                async {
                    runtime.sleep(Duration::from_secs(10)).await;
                    panic!("captured output did not complete");
                },
            ));
            child.wait().unwrap();
            assert_eq!(fs::read(&path).unwrap(), expected);
            drop(watch);
        }
        fs::remove_dir_all(root).unwrap();
    }
}
