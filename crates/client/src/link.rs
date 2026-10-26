//! How a client reaches a daemon. A [`Link`] opens a byte stream to one
//! daemon, ready for the hello; the hello, the retries and the drain wait
//! are written once, here, over any link. This machine's daemon is a
//! [`LocalLink`]; another's is an [`SshLink`](crate::SshLink). A new
//! transport is a new type, never another arm in an enum.

use std::fmt;
use std::io::{self, Read, Write};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use protocol::{Caps, ClientId, ClientMsg, DaemonMsg, Decode, Environment, MachineId, PROTO};
use rt::{BoxFuture, Rt};
use sys::{Home, Staged};

use crate::{Connection, Daemon, Diagnosis, Error, Welcome};

/// A way to one daemon.
pub trait Link: fmt::Debug + Send + Sync + 'static {
    /// Opens a byte stream to the daemon, ready for the hello. `build` is
    /// this client's, which a link may install where `a` is missing. A link
    /// that refuses says why as [`Error::Link`]; else it fails as its I/O did.
    fn open<'a>(
        &'a self,
        rt: &'a Rt,
        build: &'a Staged,
        start: Start,
    ) -> BoxFuture<'a, Result<Opened, Error>>;

    /// Whether opening may need the person at this terminal (a password, a
    /// host key to confirm), so a full-screen client opens it before it
    /// takes the terminal over.
    fn needs_terminal(&self) -> bool {
        false
    }

    /// The daemon took the connection and did not answer the hello in
    /// time: records that where the daemon will find it. Blocking.
    fn unanswered(&self) {}

    /// What this side can tell about the daemon without talking to it;
    /// `None` where nothing can be told. Blocking.
    fn diagnose(&self) -> Option<Diagnosis> {
        None
    }

    /// Why a daemon speaking `theirs` cannot serve `build`. Blocking.
    fn mismatch(&self, build: &Staged, theirs: u32) -> Error {
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
    /// This machine, as the hello names it to a daemon on another; `None`
    /// to this machine's own.
    pub machine: Option<MachineId>,
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
                let msg = T::from_json(body.json()?)?;
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
    pub(crate) async fn fill(&self, buffered: &mut Vec<u8>) -> io::Result<bool> {
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
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeedsInteractiveAuth(why) | Self::Unreachable(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for LinkError {}

impl From<LinkError> for Error {
    fn from(e: LinkError) -> Self {
        Self::Link(e)
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
    ) -> BoxFuture<'a, Result<Opened, Error>> {
        let home = self.0.clone();
        Box::pin(async move {
            if start == Start::Replace {
                crate::stop_async(rt, &home).await?;
            }
            let (stream, started) = crate::connect_or_spawn(rt, &home, start).await?;
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

    fn unanswered(&self) {
        if let Some(daemon) = crate::live_daemon(&self.0) {
            crate::note_timeout(&self.0, &daemon, &Error::NotResponding);
        }
    }

    fn diagnose(&self) -> Option<Diagnosis> {
        Some(crate::diagnose(&self.0))
    }

    fn mismatch(&self, build: &Staged, theirs: u32) -> Error {
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
        machine: None,
        buffered: Vec::new(),
        remote: None,
        within: crate::HELLO_TIMEOUT,
    })
}

pub(crate) enum Greeting {
    /// The daemon's machine, and what holds an upgrade it waits for.
    Welcome(MachineId, Option<String>),
    Draining,
    /// The daemon's exact protocol version.
    Mismatch(u32),
}

/// Opens `link` and completes the hello, as `client` when it is named,
/// sending what `first` makes for the connection (an attach, say) right
/// behind the hello, which saves a round trip. A daemon exiting (idle) as
/// we connect, or draining for a newer build (`on_drain` says so first),
/// gets one more try each; a daemon that does not welcome the client acts
/// on none of `first`, so each try sends it again.
pub(crate) async fn connect<F: Future<Output = ()> + Send>(
    rt: &Rt,
    daemon: &Daemon,
    client: Option<ClientId>,
    start: Start,
    first: &(dyn Fn(Option<&Environment>) -> io::Result<Vec<ClientMsg>> + Sync),
    on_drain: &(dyn Fn() -> F + Sync),
) -> Result<Connection, Error> {
    let (link, build) = (&daemon.link, &daemon.staged);
    let mut start = start;
    for _ in 0..3 {
        let Opened {
            stream,
            machine,
            mut buffered,
            remote,
            within,
        } = link.open(rt, build, start).await?;
        start = start.again();
        let first = first(remote.as_ref())?;
        // To a pinned machine, nothing (an environment, say) goes out
        // before its welcome says it is that machine.
        let behind = if daemon.pinned.is_some() {
            &[][..]
        } else {
            &first
        };
        let greeting = rt::or(
            hello(&*stream, &mut buffered, &build.id, client, machine, behind),
            async {
                rt.sleep(within).await;
                Err(Error::NotResponding)
            },
        )
        .await;
        match greeting {
            Ok((Greeting::Welcome(theirs, upgrade), welcome, commands, queries)) => {
                if let Some(pinned) = daemon.pinned.filter(|&pinned| pinned != theirs) {
                    return Err(Error::OtherMachine {
                        pinned,
                        found: theirs,
                    });
                }
                if daemon.pinned.is_some() {
                    for msg in &first {
                        stream.send(msg).await?;
                    }
                }
                return Ok(Connection {
                    stream,
                    buffered,
                    remote,
                    machine: theirs,
                    welcome,
                    commands,
                    queries,
                    upgrade,
                });
            }
            Ok((Greeting::Mismatch(theirs), ..)) => {
                let (link, build) = (link.clone(), build.clone());
                return Err(rt::try_unblock_with(
                    &**rt,
                    rt::BlockingClass::Storage,
                    0,
                    move || link.mismatch(&build, theirs),
                )
                .await?);
            }
            Ok((Greeting::Draining, ..)) => {
                on_drain().await;
                // One still draining past its grace is wedged.
                rt::or(stream.drained(&mut buffered), async {
                    rt.sleep(crate::DRAIN_TIMEOUT).await;
                    Err(Error::NotResponding)
                })
                .await?;
            }
            Err(Error::Closed) => {}
            Err(e) => return Err(e),
        }
    }
    Err(Error::Closed)
}

/// Sends the hello, from `machine` if it is named, and `first` back to
/// back, then reads the answer.
pub(crate) async fn hello(
    stream: &dyn Stream,
    buffered: &mut Vec<u8>,
    build: &str,
    client: Option<ClientId>,
    machine: Option<MachineId>,
    first: &[ClientMsg],
) -> Result<
    (
        Greeting,
        Welcome,
        Vec<protocol::CommandInfo>,
        Vec<protocol::QueryInfo>,
    ),
    Error,
> {
    stream
        .send(&ClientMsg::Hello {
            proto: PROTO,
            build: build.to_owned(),
            caps: Caps::ALL,
            client,
            machine,
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
            machine,
            commands,
            queries,
            upgrade,
            ..
        }) => {
            sent?;
            let welcome = Welcome { proto, caps };
            Ok((
                Greeting::Welcome(machine, upgrade),
                welcome,
                commands,
                queries,
            ))
        }
        // The upgrade cannot happen yet, and this client cannot be served
        // by the build that holds it.
        Some(DaemonMsg::Draining { held: Some(held) }) => Err(Error::Refused(format!(
            "the daemon waits to upgrade: {held}; `a daemon stop` ends what holds it"
        ))),
        Some(DaemonMsg::Draining { held: None }) => Ok((
            Greeting::Draining,
            Welcome::default(),
            Vec::new(),
            Vec::new(),
        )),
        Some(DaemonMsg::Mismatch { supported }) => Ok((
            Greeting::Mismatch(supported),
            Welcome::default(),
            Vec::new(),
            Vec::new(),
        )),
        // A daemon refusing to serve at all, after a crash loop.
        Some(DaemonMsg::Error { message }) => Err(Error::Refused(message)),
        _ => Err(Error::Closed),
    }
}

/// The shared runtime behind the blocking SDK entry points.
pub(crate) static BLOCKING: LazyLock<Rt> = LazyLock::new(|| Arc::new(rt::Threads::new(1)));
