//! How a client reaches a daemon. A [`Link`] opens a byte stream to one
//! daemon, ready for the hello; the hello, the retries and the drain wait
//! are written once, here, over any link. This machine's daemon is a
//! [`LocalLink`]; another's is an [`SshLink`](crate::SshLink). A new
//! transport is a new type, never another arm in an enum.

use std::fmt;
use std::io::{self, Read, Write};
use std::sync::{Arc, LazyLock};
use std::task::{Context, Waker};
use std::time::Duration;

use protocol::{Caps, ClientId, ClientMsg, DaemonMsg, Decode, Environment, PROTO, Versions};
use rt::{BoxFuture, Rt, Runtime};
use sys::{Home, Staged};

use crate::{Connection, Diagnosis, Error, Welcome};

/// A way to one daemon.
pub trait Link: fmt::Debug + Send + Sync + 'static {
    /// Opens a byte stream to the daemon, ready for the hello. `build` is
    /// this client's, which a link may install where `a` is missing.
    fn open<'a>(
        &'a self,
        rt: &'a Rt,
        build: &'a Staged,
        start: Start,
    ) -> BoxFuture<'a, Result<Opened, LinkError>>;

    /// Whether opening may need the person at this terminal (a password, a
    /// host key to confirm), so a full-screen client opens it before it
    /// takes the terminal over.
    fn needs_terminal(&self) -> bool {
        false
    }

    /// The daemon took the connection and did not answer the hello in time:
    /// what this side can tell about it without talking to it, the timeout
    /// recorded where the daemon will find it. `None` where nothing can be
    /// told. Blocking.
    fn unanswered(&self) -> Option<Diagnosis> {
        None
    }

    /// Why a daemon speaking `theirs` cannot serve `build`. Blocking.
    fn mismatch(&self, build: &Staged, theirs: Versions) -> Error {
        let _ = build;
        Error::Mismatch {
            theirs,
            newer: None,
            host: None,
        }
    }
}

/// What a connection may do to the daemon it reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// Use the daemon that runs, starting one if none does.
    Reuse,
    /// As `Reuse`, but a daemon started serves even after the crash-loop
    /// breaker stopped the last ones.
    Reset,
    /// Stop the daemon that runs first, then `Reset`.
    Replace,
}

impl Start {
    /// What a retry does after a connection opened with `self`: only the
    /// first stops a daemon.
    const fn again(self) -> Self {
        match self {
            Self::Replace => Self::Reset,
            start => start,
        }
    }
}

/// A byte stream a [`Link`] opened.
pub struct Opened {
    pub stream: Box<dyn Stream>,
    /// What the link read past its own preamble: the start of the daemon's
    /// answer.
    pub buffered: Vec<u8>,
    /// The far host's environment, as its bridge captured it; `None` for
    /// this machine's daemon.
    pub remote: Option<Environment>,
    /// How long the daemon may take to answer the hello.
    pub within: Duration,
}

/// The bytes between a client and its daemon, as a link carries them.
pub trait Stream: Send + Sync {
    fn read<'a>(&'a self, bytes: &'a mut [u8]) -> BoxFuture<'a, io::Result<usize>>;
    /// Writes all of `bytes`, and waits for the write to complete.
    fn write_all<'a>(&'a self, bytes: &'a [u8]) -> BoxFuture<'a, io::Result<()>>;
}

/// A [`Stream`] over two descriptors registered with the reactor, and
/// what must go with them (a child process, say).
pub struct Pipe<R: rt::Source, W: rt::Source, G> {
    read: rt::Async<R>,
    write: rt::Async<W>,
    guard: G,
}

impl<R: rt::Source, W: rt::Source, G> Pipe<R, W, G> {
    pub const fn new(read: rt::Async<R>, write: rt::Async<W>, guard: G) -> Self {
        Self { read, write, guard }
    }

    pub const fn guard_mut(&mut self) -> &mut G {
        &mut self.guard
    }
}

impl<R, W, G> Stream for Pipe<R, W, G>
where
    R: rt::Source + Send + Sync,
    W: rt::Source + Send + Sync,
    G: Send + Sync,
    for<'a> &'a R: Read,
    for<'a> &'a W: Write,
{
    fn read<'a>(&'a self, bytes: &'a mut [u8]) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(self.read.read_with(|mut io| io.read(bytes)))
    }

    fn write_all<'a>(&'a self, bytes: &'a [u8]) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(self.write.write_all(bytes))
    }
}

impl dyn Stream + '_ {
    pub(crate) async fn send(&self, msg: &ClientMsg) -> io::Result<()> {
        let mut bytes = Vec::new();
        msg.write_frame(&mut bytes)?;
        self.write_all(&bytes).await
    }

    /// The next frame, from what is `buffered` and then the stream.
    pub(crate) async fn recv<T: Decode>(&self, buffered: &mut Vec<u8>) -> io::Result<Option<T>> {
        loop {
            if let Some((len, body)) = protocol::frame_in(buffered)? {
                let msg = T::from_json(body)?;
                buffered.drain(..len);
                return Ok(Some(msg));
            }
            if !self.fill(buffered).await? {
                return Ok(None);
            }
        }
    }

    /// The next line, for a link's preamble; `None` at the end of the stream.
    pub(crate) async fn line(&self, buffered: &mut Vec<u8>) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(end) = buffered.iter().position(|&byte| byte == b'\n') {
                return Ok(Some(buffered.drain(..=end).collect()));
            }
            if buffered.len() > 64 << 10 {
                return Err(io::Error::other("oversized preamble line"));
            }
            if !self.fill(buffered).await? {
                return Ok(None);
            }
        }
    }

    /// Waits for a draining daemon to close the connection as it exits.
    pub(crate) async fn drained(&self, buffered: &mut Vec<u8>) -> Result<(), Error> {
        loop {
            match self.recv::<DaemonMsg>(buffered).await {
                Ok(Some(_)) => {}
                Ok(None) => return Ok(()),
                // It exited with what we sent behind the hello unread.
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Reads more into `buffered`; `false` at the end of the stream.
    async fn fill(&self, buffered: &mut Vec<u8>) -> io::Result<bool> {
        let kept = buffered.len();
        buffered.resize(kept + 8192, 0);
        let read = self.read(&mut buffered[kept..]).await;
        buffered.truncate(kept + *read.as_ref().unwrap_or(&0));
        Ok(read? > 0)
    }
}

/// Why a link could not be opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkError {
    /// Opening needs a person to authenticate (a password, a passphrase, a
    /// host key to confirm), and this link may not ask; the text says
    /// where to do it once.
    NeedsInteractiveAuth(String),
    /// The other machine could not be reached, as the text says.
    Unreachable(String),
    /// Opening failed here: the socket, a spawn, or a bridge that ended.
    Io(io::ErrorKind, String),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeedsInteractiveAuth(why) | Self::Unreachable(why) | Self::Io(_, why) => {
                f.write_str(why)
            }
        }
    }
}

impl std::error::Error for LinkError {}

impl From<io::Error> for LinkError {
    fn from(e: io::Error) -> Self {
        Self::Io(e.kind(), e.to_string())
    }
}

impl From<LinkError> for Error {
    fn from(e: LinkError) -> Self {
        match e {
            // As any I/O error reads: a timeout is a daemon not responding.
            LinkError::Io(kind, why) => io::Error::new(kind, why).into(),
            e => Self::Link(e),
        }
    }
}

/// This machine's daemon, over its socket, started if none runs.
#[derive(Clone, Debug)]
pub struct LocalLink(Home);

impl LocalLink {
    pub const fn new(home: Home) -> Self {
        Self(home)
    }

    pub const fn home(&self) -> &Home {
        &self.0
    }
}

impl Link for LocalLink {
    fn open<'a>(
        &'a self,
        rt: &'a Rt,
        _: &'a Staged,
        start: Start,
    ) -> BoxFuture<'a, Result<Opened, LinkError>> {
        let home = self.0.clone();
        Box::pin(async move {
            let (stream, started) =
                rt::unblock_with(&**rt, rt::BlockingClass::Network, 64 << 10, move || {
                    if start == Start::Replace {
                        crate::stop(&home)?;
                    }
                    crate::connect_or_spawn(&home, start != Start::Reuse)
                })
                .await?;
            Ok(Opened {
                within: if started {
                    crate::START_TIMEOUT
                } else {
                    crate::HELLO_TIMEOUT
                },
                ..socket(rt, stream)?
            })
        })
    }

    fn unanswered(&self) -> Option<Diagnosis> {
        if let Some(daemon) = crate::live_daemon(&self.0) {
            crate::note_timeout(&self.0, &daemon, &Error::NotResponding);
        }
        Some(crate::diagnose(&self.0))
    }

    fn mismatch(&self, build: &Staged, theirs: Versions) -> Error {
        let runtime = self.0.runtime();
        Error::Mismatch {
            theirs,
            newer: runtime
                .current()
                .filter(|current| *current != build.id)
                .map(|_| runtime.current_exe()),
            host: None,
        }
    }
}

/// A connected local socket as a stream, answered within [`HELLO_TIMEOUT`](crate::HELLO_TIMEOUT).
pub(crate) fn socket(rt: &Rt, stream: sys::OwnedConnection) -> io::Result<Opened> {
    let reactor = rt.reactor();
    Ok(Opened {
        stream: Box::new(Pipe::new(
            rt::Async::new(&reactor, stream.try_clone()?)?,
            rt::Async::new(&reactor, stream)?,
            (),
        )),
        buffered: Vec::new(),
        remote: None,
        within: crate::HELLO_TIMEOUT,
    })
}

pub(crate) enum Greeting {
    Welcome,
    Draining,
    /// The daemon's versions.
    Mismatch(Versions),
}

/// Opens `link` and completes the hello, as `client` when it is named,
/// sending what `first` makes for the connection (an attach, say) right
/// behind the hello, which saves a round trip. A daemon exiting (idle) as
/// we connect, or draining for a newer build (`on_drain` says so first),
/// gets one more try each; a daemon that does not welcome the client acts
/// on none of `first`, so each try sends it again.
pub(crate) async fn connect(
    rt: &Rt,
    link: &Arc<dyn Link>,
    build: &Staged,
    client: Option<ClientId>,
    start: Start,
    first: &(dyn Fn(Option<&Environment>) -> io::Result<Vec<ClientMsg>> + Sync),
    on_drain: &(dyn Fn() + Sync),
) -> Result<Connection, Error> {
    let mut start = start;
    for _ in 0..3 {
        let Opened {
            stream,
            mut buffered,
            remote,
            within,
        } = link.open(rt, build, start).await?;
        start = start.again();
        let first = first(remote.as_ref())?;
        let greeting = rt::or(
            hello(&*stream, &mut buffered, &build.id, client, &first),
            async {
                rt.sleep(within).await;
                Err(Error::NotResponding)
            },
        )
        .await;
        match greeting {
            Ok((Greeting::Welcome, welcome, commands)) => {
                return Ok(Connection {
                    stream,
                    buffered,
                    remote,
                    welcome,
                    commands,
                });
            }
            Ok((Greeting::Mismatch(theirs), ..)) => {
                let (link, build) = (link.clone(), build.clone());
                return Err(rt::unblock(&**rt, move || link.mismatch(&build, theirs)).await);
            }
            Ok((Greeting::Draining, ..)) => {
                on_drain();
                // One still draining past its grace is tried again.
                rt::or(stream.drained(&mut buffered), async {
                    rt.sleep(crate::DRAIN_TIMEOUT).await;
                    Ok(())
                })
                .await?;
            }
            Err(Error::Closed) => {}
            Err(e) => return Err(e),
        }
    }
    Err(Error::Closed)
}

/// Sends the hello and `first` back to back, then reads the answer.
pub(crate) async fn hello(
    stream: &dyn Stream,
    buffered: &mut Vec<u8>,
    build: &str,
    client: Option<ClientId>,
    first: &[ClientMsg],
) -> Result<(Greeting, Welcome, Vec<protocol::CommandInfo>), Error> {
    stream
        .send(&ClientMsg::Hello {
            proto: PROTO,
            build: build.to_owned(),
            min: Versions::OURS.min_field(),
            caps: Caps::ALL,
            client,
        })
        .await?;
    // A daemon that refuses the hello may hang up before reading these, so
    // failing to send them matters only once welcomed.
    let mut sent = Ok(());
    for msg in first {
        sent = stream.send(msg).await;
        if sent.is_err() {
            break;
        }
    }
    match stream.recv(buffered).await? {
        Some(DaemonMsg::Welcome {
            proto,
            caps,
            commands,
            ..
        }) => {
            sent?;
            Ok((Greeting::Welcome, Welcome { proto, caps }, commands))
        }
        Some(DaemonMsg::Draining) => Ok((Greeting::Draining, Welcome::default(), Vec::new())),
        Some(DaemonMsg::Mismatch { supported, min }) => Ok((
            Greeting::Mismatch(Versions::new(min, supported)),
            Welcome::default(),
            Vec::new(),
        )),
        // A daemon refusing to serve at all, after a crash loop.
        Some(DaemonMsg::Error { message }) => Err(Error::Refused(message)),
        _ => Err(Error::Closed),
    }
}

/// The runtime the blocking API drives a link on: its caller's thread is
/// already one that may block, so blocking work runs right there.
pub(crate) static BLOCKING: LazyLock<Rt> = LazyLock::new(|| Arc::new(Inline));

struct Inline;

impl Runtime for Inline {
    fn spawn(&self, mut task: BoxFuture<'static, ()>) {
        // Most finish at once (a reaped child); the rest get a thread.
        if task
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
        {
            std::thread::spawn(move || rt::block_on(task));
        }
    }

    fn spawn_blocking(
        &self,
        _: rt::BlockingClass,
        _: usize,
        work: Box<dyn FnOnce() + Send>,
    ) -> BoxFuture<'static, io::Result<()>> {
        work();
        Box::pin(std::future::ready(Ok(())))
    }

    fn spawn_flush(&self, work: Box<dyn FnOnce() + Send>) -> BoxFuture<'static, io::Result<()>> {
        self.spawn_blocking(rt::BlockingClass::Storage, 0, work)
    }

    fn reactor(&self) -> rt::Reactor {
        rt::Reactor::AsyncIo
    }
}
