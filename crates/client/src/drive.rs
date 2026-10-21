//! Ordered client I/O owned by one cancellable runtime task.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use protocol::{ClientId, ClientMsg, DaemonMsg, Decode, Environment};
use rt::{Rt, Runtime};

use crate::{
    Attach, ConnId, Connection, Daemon, Effect, Endpoint, Error, Event, EventKind, Io, Notice,
    Show, Transport,
};

type Sink = dyn Fn(&ClientHandle, Event) + Send + Sync;
type Origin = dyn Fn() -> io::Result<Environment> + Send + Sync;

/// What renders a session client's [`Show`] effects: an app's screen, a
/// web page, a feed to another language. Showing may ask the client for
/// more (a picker opening asks for the list), which comes back as effects.
pub trait Renderer {
    fn show(&mut self, shown: Vec<Show>) -> Vec<Effect>;
}

/// Shows nothing: for effects carried out where no one is watching, such
/// as the detach of a dropped session.
impl Renderer for () {
    fn show(&mut self, _: Vec<Show>) -> Vec<Effect> {
        Vec::new()
    }
}

/// The fixpoint every app runs: the client's I/O is carried out by `io`,
/// what it shows is rendered, and whatever rendering asks for goes round
/// again. A message that could not be sent is shown as a notice.
pub fn pump(
    mut io: impl FnMut(Io) -> Result<(), Error>,
    renderer: &mut impl Renderer,
    mut effects: Vec<Effect>,
) {
    while !effects.is_empty() {
        let mut shown = Vec::new();
        for effect in effects {
            match effect {
                Effect::Io(what) => {
                    if let Err(e) = io(what) {
                        shown.push(Show::Notice(Notice::NotSent(e)));
                    }
                }
                Effect::Show(show) => shown.push(show),
            }
        }
        if shown.is_empty() {
            return;
        }
        effects = renderer.show(shown);
    }
}

/// Configuration consumed when its connection task starts.
pub struct Driver {
    origin: Arc<Origin>,
    sink: Box<Sink>,
    pub(crate) watch: bool,
}

/// Queues effects without waiting for the daemon. The last handle revokes I/O.
#[derive(Clone)]
pub struct ClientHandle(Arc<Handle>);

enum Request {
    Io(Box<Io>),
    Flush(rt::oneshot::Sender<()>),
}

struct Handle {
    daemon: Daemon,
    tx: rt::channel::Sender<(u64, Request)>,
    stop: Arc<rt::Notify>,
    epoch: Arc<AtomicU64>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.stop.notify();
    }
}

impl Driver {
    pub fn new(sink: impl Fn(&ClientHandle, Event) + Send + Sync + 'static) -> Self {
        Self::with_origin(crate::capture, sink)
    }

    pub fn with_origin(
        origin: impl Fn() -> io::Result<Environment> + Send + Sync + 'static,
        sink: impl Fn(&ClientHandle, Event) + Send + Sync + 'static,
    ) -> Self {
        Self {
            origin: Arc::new(origin),
            sink: Box::new(sink),
            watch: false,
        }
    }

    pub fn start(self, rt: Rt, daemon: Daemon) -> ClientHandle {
        let (tx, rx) = rt::channel::unbounded();
        let stop = Arc::new(rt::Notify::new());
        let epoch = Arc::new(AtomicU64::new(0));
        let handle = ClientHandle(Arc::new(Handle {
            daemon: daemon.clone(),
            tx,
            stop: stop.clone(),
            epoch: epoch.clone(),
        }));
        let weak = Arc::downgrade(&handle.0);
        let runtime = rt;
        let spawner = runtime.clone();
        spawner.spawn(Box::pin(async move {
            let client = ClientId::new();
            let mut pending = None;
            while let Some((generation, command)) = match pending.take() {
                Some(command) => Some(command),
                None => rx.recv().await,
            } {
                if generation != epoch.load(Ordering::Acquire) {
                    continue;
                }
                let Request::Io(command) = command else {
                    continue;
                };
                if let Io::Connect {
                    conn,
                    attach,
                    restart,
                } = *command
                {
                    let cancelled = || epoch.load(Ordering::Acquire) != generation;
                    loop {
                        pending = rt::or(
                            async {
                                stop.until(cancelled).await;
                                None
                            },
                            self.connection(
                                &runtime,
                                &daemon,
                                client,
                                conn,
                                attach.clone(),
                                restart,
                                &rx,
                                generation,
                                &weak,
                            ),
                        )
                        .await;
                        if !self.watch || cancelled() {
                            break;
                        }
                        rt::or(stop.until(cancelled), runtime.sleep(Duration::from_secs(1))).await;
                        if cancelled() {
                            break;
                        }
                    }
                }
            }
        }));
        handle
    }

    fn emit(&self, handle: &Weak<Handle>, conn: ConnId, generation: u64, kind: EventKind) -> bool {
        if let Some(handle) = handle.upgrade()
            && handle.epoch.load(Ordering::Acquire) == generation
        {
            let handle = ClientHandle(handle);
            (self.sink)(&handle, Event { conn, kind });
            return handle.0.epoch.load(Ordering::Acquire) == generation;
        }
        false
    }

    #[allow(clippy::too_many_arguments)]
    async fn connection(
        &self,
        rt: &Rt,
        daemon: &Daemon,
        client: ClientId,
        conn: ConnId,
        attach: Attach,
        restart: bool,
        rx: &rt::channel::Receiver<(u64, Request)>,
        generation: u64,
        handle: &Weak<Handle>,
    ) -> Option<(u64, Request)> {
        let made = self
            .open(
                rt, daemon, client, conn, &attach, restart, generation, handle,
            )
            .await;
        let Connected {
            io,
            mut buffered,
            welcome,
            remote,
            commands,
        } = match made {
            Ok(connection) => connection,
            Err(error) => {
                let kind = failure(rt, daemon, error).await;
                self.emit(handle, conn, generation, kind);
                return None;
            }
        };
        if !self.emit(
            handle,
            conn,
            generation,
            EventKind::Connected {
                welcome,
                remote,
                commands,
            },
        ) {
            return None;
        }
        let read = async {
            loop {
                match io.recv(&mut buffered).await {
                    Ok(Some(msg)) => {
                        if !self.emit(handle, conn, generation, EventKind::Msg(msg)) {
                            return (Error::Closed, None);
                        }
                    }
                    Ok(None) => return (Error::Closed, None),
                    Err(error) => return (error.into(), None),
                }
            }
        };
        let write = async {
            while let Some((epoch, command)) = rx.recv().await {
                if epoch != generation {
                    return (Error::Closed, Some((epoch, command)));
                }
                let command = match command {
                    Request::Flush(done) => {
                        done.send(());
                        continue;
                    }
                    Request::Io(command) => command,
                };
                let result = match *command {
                    Io::Send(msg) => io.send(&msg).await,
                    Io::SendEnvironment => {
                        let origin = self.origin.clone();
                        match rt::unblock_with(
                            &**rt,
                            rt::BlockingClass::Network,
                            64 << 10,
                            move || origin(),
                        )
                        .await
                        {
                            Ok(env) => io.send(&ClientMsg::Environment(env)).await,
                            Err(error) => Err(error),
                        }
                    }
                    // A replacement revokes the previous generation before it is queued.
                    Io::Connect { .. } | Io::Hangup => {
                        return (Error::Closed, Some((epoch, Request::Io(command))));
                    }
                };
                if let Err(error) = result {
                    return (error.into(), None);
                }
            }
            (Error::Closed, None)
        };
        let (why, pending) = rt::or(read, write).await;
        self.emit(handle, conn, generation, EventKind::Closed(why));
        pending
    }

    #[allow(clippy::too_many_arguments)]
    async fn open(
        &self,
        rt: &Rt,
        daemon: &Daemon,
        client: ClientId,
        conn: ConnId,
        attach: &Attach,
        restart: bool,
        generation: u64,
        handle: &Weak<Handle>,
    ) -> Result<Connected, Error> {
        for attempt in 0..3 {
            let (io, mut buffered, remote, timeout) = match &daemon.endpoint {
                Endpoint::Local(home) => {
                    let home = home.clone();
                    let (opened, started) =
                        rt::unblock_with(&**rt, rt::BlockingClass::Network, 64 << 10, move || {
                            if restart && attempt == 0 {
                                crate::stop(&home)?;
                            }
                            let (stream, started) = crate::connect_or_spawn(&home, restart)?;
                            Ok::<_, Error>((crate::split(&stream)?, started))
                        })
                        .await?;
                    let (io, buffered) = AsyncConnection::new(rt.clone(), opened)?;
                    (
                        io,
                        buffered,
                        None,
                        if started {
                            crate::START_TIMEOUT
                        } else {
                            crate::HELLO_TIMEOUT
                        },
                    )
                }
                Endpoint::Ssh(host) => {
                    let (io, buffered, env) = open_remote(rt, host, &daemon.staged).await?;
                    (io, buffered, Some(env), crate::HELLO_TIMEOUT)
                }
            };
            let first = if self.watch {
                ClientMsg::WatchSessions
            } else {
                crate::attaching(attach, || remote.clone().map_or_else(crate::capture, Ok))?
            };
            let greeting = io.hello(&mut buffered, &daemon.staged.id, client, &first);
            let deadline = async {
                rt.sleep(timeout).await;
                Err(io::Error::new(io::ErrorKind::TimedOut, "daemon hello"))
            };
            match rt::or(greeting, deadline).await? {
                Some(DaemonMsg::Welcome {
                    proto,
                    caps,
                    commands,
                    ..
                }) => {
                    return Ok(Connected {
                        io,
                        buffered,
                        welcome: crate::Welcome { proto, caps },
                        remote: remote.is_some(),
                        commands,
                    });
                }
                Some(DaemonMsg::Mismatch { supported, min }) => {
                    let daemon = daemon.clone();
                    return Err(rt::unblock(&**rt, move || {
                        daemon.mismatch(protocol::Versions::new(min, supported))
                    })
                    .await);
                }
                Some(DaemonMsg::Draining) => {
                    self.emit(handle, conn, generation, EventKind::Waiting);
                    rt::or(
                        async {
                            while io.recv::<DaemonMsg>(&mut buffered).await?.is_some() {}
                            Ok::<_, io::Error>(())
                        },
                        async {
                            rt.sleep(crate::DRAIN_TIMEOUT).await;
                            Ok(())
                        },
                    )
                    .await?;
                }
                Some(DaemonMsg::Error { message }) => return Err(Error::Refused(message)),
                _ => {}
            }
        }
        Err(Error::Closed)
    }
}

impl ClientHandle {
    pub fn daemon(&self) -> &Daemon {
        &self.0.daemon
    }

    pub fn pump(&self, renderer: &mut impl Renderer, effects: Vec<Effect>) {
        pump(|io| self.io(io), renderer, effects);
    }

    /// Resolves once all preceding sends reached the transport, or it closed.
    pub async fn flush(&self) -> Result<(), Error> {
        let (done, sent) = rt::oneshot::channel();
        self.0
            .tx
            .send((self.0.epoch.load(Ordering::Acquire), Request::Flush(done)))
            .map_err(|_| Error::Closed)?;
        sent.await.ok_or(Error::Closed)
    }

    pub fn io(&self, io: Io) -> Result<(), Error> {
        if matches!(io, Io::Hangup | Io::Connect { .. }) {
            self.0.epoch.fetch_add(1, Ordering::AcqRel);
            self.0.stop.notify();
        }
        if matches!(io, Io::Hangup) {
            return Ok(());
        }
        self.0
            .tx
            .send((
                self.0.epoch.load(Ordering::Acquire),
                Request::Io(Box::new(io)),
            ))
            .map_err(|_| Error::Closed)
    }
}

async fn failure(rt: &Rt, daemon: &Daemon, error: Error) -> EventKind {
    if error == Error::NotResponding {
        match &daemon.endpoint {
            Endpoint::Local(home) => {
                let home = home.clone();
                EventKind::Wedged(
                    rt::unblock(&**rt, move || {
                        if let Some(daemon) = crate::live_daemon(&home) {
                            crate::note_timeout(&home, &daemon, &Error::NotResponding);
                        }
                        crate::diagnose(&home)
                    })
                    .await,
                )
            }
            Endpoint::Ssh(_) => EventKind::Failed(error),
        }
    } else {
        EventKind::Failed(error)
    }
}

struct Connected {
    io: AsyncConnection,
    buffered: Vec<u8>,
    welcome: crate::Welcome,
    remote: bool,
    commands: Vec<protocol::CommandInfo>,
}

async fn open_remote(
    rt: &Rt,
    host: &str,
    staged: &sys::Staged,
) -> io::Result<(AsyncConnection, Vec<u8>, Environment)> {
    let token = crate::remote::token();
    for installed in [false, true] {
        let script = if installed {
            let host = host.to_owned();
            let installing = staged.clone();
            rt::unblock_with(&**rt, rt::BlockingClass::Network, 64 << 10, move || {
                crate::remote::install(&host, &installing)
            })
            .await?;
            format!(r#"exec "$HOME/.a/runtime/{}/a" bridge {token}"#, staged.id)
        } else {
            crate::remote::find_script(&token)
        };
        let host = host.to_owned();
        let opened = rt::unblock_with(&**rt, rt::BlockingClass::Network, 64 << 10, move || {
            crate::remote::launch(&host, &script)
        })
        .await?;
        let (mut io, mut buffered) = AsyncConnection::new(rt.clone(), opened)?;
        let sync = format!("{} {token}", crate::remote::SYNC);
        loop {
            let Some(line) = io.line(&mut buffered).await? else {
                let child = io.bridge.child.as_mut().expect("bridge child");
                if child.try_wait()?.is_none()
                    && let Ok(watch) = sys::exit_watch(child.id())
                {
                    rt::Async::for_reading(&rt.reactor(), watch)?
                        .readable()
                        .await;
                }
                if !installed && child.try_wait()?.and_then(|status| status.code()) == Some(127) {
                    break;
                }
                return Err(io::Error::other(
                    "ssh: the bridge ended before it was ready",
                ));
            };
            if line.trim_ascii_end().strip_prefix(sync.as_bytes()) == Some(crate::remote::WITH_ENV)
            {
                return match io.recv(&mut buffered).await? {
                    Some(ClientMsg::Environment(env)) => Ok((io, buffered, env)),
                    _ => Err(io::Error::other("ssh: no environment")),
                };
            }
        }
    }
    Err(io::Error::other("ssh: the installed bridge did not start"))
}

enum AsyncTransport {
    Local(rt::Async<sys::Stream>),
    Pipe(rt::Async<sys::ProcessPipe>),
}

impl AsyncTransport {
    fn new(rt: &dyn Runtime, io: Transport) -> io::Result<Self> {
        Ok(match io {
            Transport::Local(io) => Self::Local(rt::Async::new(&rt.reactor(), io)?),
            Transport::Pipe(io) => Self::Pipe(rt::Async::new(&rt.reactor(), io)?),
        })
    }
    async fn read(&self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Local(io) => io.read_with(|mut io| io.read(bytes)).await,
            Self::Pipe(io) => io.read_with(|mut io| io.read(bytes)).await,
        }
    }
    async fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Local(io) => io.write_with(|mut io| io.write(bytes)).await,
            Self::Pipe(io) => io.write_with(|mut io| io.write(bytes)).await,
        }
    }
}

struct AsyncConnection {
    reader: AsyncTransport,
    writer: AsyncTransport,
    bridge: Reaper,
}

struct Reaper {
    child: Option<sys::Child>,
    rt: Rt,
}

impl AsyncConnection {
    fn new(rt: Rt, mut connection: Connection) -> io::Result<(Self, Vec<u8>)> {
        let bridge = Reaper {
            child: connection.writer.child.take(),
            rt,
        };
        let buffered = connection.reader.0.buffer().to_vec();
        let reader = AsyncTransport::new(&*bridge.rt, connection.reader.0.into_inner())?;
        let writer = AsyncTransport::new(
            &*bridge.rt,
            connection.writer.io.take().expect("owned writer"),
        )?;
        Ok((
            Self {
                reader,
                writer,
                bridge,
            },
            buffered,
        ))
    }

    async fn hello(
        &self,
        buffered: &mut Vec<u8>,
        build: &str,
        client: ClientId,
        first: &ClientMsg,
    ) -> io::Result<Option<DaemonMsg>> {
        self.send(&ClientMsg::Hello {
            proto: protocol::PROTO,
            build: build.to_owned(),
            min: protocol::Versions::OURS.min_field(),
            caps: protocol::Caps::ALL,
            client: Some(client),
        })
        .await?;
        let sent = self.send(first).await;
        let reply = self.recv(buffered).await?;
        if matches!(reply, Some(DaemonMsg::Welcome { .. })) {
            sent?;
        }
        Ok(reply)
    }

    async fn send(&self, msg: &ClientMsg) -> io::Result<()> {
        let mut bytes = Vec::new();
        protocol::write_frame(&mut bytes, msg)?;
        let mut remaining = bytes.as_slice();
        while !remaining.is_empty() {
            let written = self.writer.write(remaining).await?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            remaining = &remaining[written..];
        }
        Ok(())
    }

    async fn line(&self, buffered: &mut Vec<u8>) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(end) = buffered.iter().position(|&byte| byte == b'\n') {
                return Ok(Some(buffered.drain(..=end).collect()));
            }
            if buffered.len() > 64 << 10 {
                return Err(io::Error::other("ssh: oversized preamble line"));
            }
            let mut bytes = [0; 8192];
            let read = self.reader.read(&mut bytes).await?;
            if read == 0 {
                return Ok(None);
            }
            buffered.extend_from_slice(&bytes[..read]);
        }
    }

    async fn recv<T: Decode>(&self, buffered: &mut Vec<u8>) -> io::Result<Option<T>> {
        loop {
            if let Some((len, body)) = protocol::frame_in(buffered)? {
                let msg = T::from_json(body)?;
                buffered.drain(..len);
                return Ok(Some(msg));
            }
            let mut bytes = [0; 8192];
            let read = self.reader.read(&mut bytes).await?;
            if read == 0 {
                return Ok(None);
            }
            buffered.extend_from_slice(&bytes[..read]);
        }
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Revocation cannot wait behind network admission or process exit.
            let _ = child.kill();
            let admitted = self.rt.spawn_blocking(
                rt::BlockingClass::Network,
                0,
                Box::new(move || {
                    let _ = child.wait();
                }),
            );
            self.rt.spawn(Box::pin(async move {
                let _ = admitted.await;
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Caps, Target, read_frame, write_frame};
    use std::future::{Future, poll_fn};
    use std::pin::pin;
    use std::sync::mpsc;
    use std::task::Poll;

    struct Fixture {
        home: sys::Home,
        daemon: Daemon,
        listener: sys::Listener,
        root: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("a-client-{:?}", ClientId::new()));
            std::fs::create_dir_all(&root).unwrap();
            let home = sys::Home::from(root.join("home"));
            let exe = root.join("a");
            std::fs::write(&exe, "never run").unwrap();
            let staged = home.runtime().stage(&exe).unwrap();
            let listener = home.bind().unwrap();
            Self {
                daemon: Daemon::local(home.clone(), staged),
                home,
                listener,
                root,
            }
        }

        fn connect(&self, rt: Rt) -> (ClientHandle, sys::Stream) {
            let (events, received) = mpsc::channel();
            let handle = Driver::new(move |_, event| {
                let _ = events.send(event);
            })
            .start(rt, self.daemon.clone());
            handle
                .io(Io::Connect {
                    conn: ConnId(1),
                    attach: Attach::new(Target::New, crate::Env::Keep),
                    restart: false,
                })
                .unwrap();
            let mut peer = self.listener.accept().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
            read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
            write_frame(
                &mut peer,
                &DaemonMsg::Welcome {
                    proto: protocol::PROTO,
                    build: "test".into(),
                    caps: Caps::ALL,
                    commands: Vec::new(),
                },
            )
            .unwrap();
            assert!(matches!(
                received.recv_timeout(Duration::from_secs(2)).unwrap().kind,
                EventKind::Connected { .. }
            ));
            (handle, peer)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.home.endpoint().remove();
            std::fs::remove_dir_all(&self.root).unwrap();
        }
    }

    fn large_input() -> ClientMsg {
        ClientMsg::Input {
            text: "x".repeat(8 << 20),
            client_seq: Some(1),
            mode: protocol::Mode::default(),
        }
    }

    #[test]
    fn stalled_daemon_writes_never_block_an_async_worker() {
        let fixture = Fixture::new();
        let runtime: Rt = Arc::new(rt::Threads::new(1));
        let mut stream = fixture.home.endpoint().connect().unwrap();
        let _peer = fixture.listener.accept().unwrap();
        stream.set_nonblocking(true).unwrap();
        loop {
            match stream.write(&[0; 8192]) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("{error}"),
            }
        }
        let (io, _) =
            AsyncConnection::new(runtime.clone(), crate::split(&stream).unwrap()).unwrap();
        drop(stream);
        let stop = Arc::new(rt::Notify::new());
        let cancel = stop.clone();
        let (stalled, pending) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        runtime.spawn(Box::pin(async move {
            let msg = large_input();
            let mut send = pin!(io.send(&msg));
            poll_fn(|cx| {
                assert!(send.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            stalled.send(()).unwrap();
            rt::or(
                async {
                    cancel.until(|| cancel.count() > 0).await;
                },
                async {
                    send.await.unwrap();
                },
            )
            .await;
            finished.send(()).unwrap();
        }));
        pending.recv_timeout(Duration::from_secs(2)).unwrap();
        let (progress, progressed) = mpsc::channel();
        runtime.spawn(Box::pin(async move {
            progress.send(()).unwrap();
        }));
        progressed.recv_timeout(Duration::from_secs(2)).unwrap();
        stop.notify();
        done.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn drop_does_not_wait_for_detach() {
        let fixture = Fixture::new();
        let runtime: Rt = Arc::new(rt::Threads::new(1));
        let (handle, mut peer) = fixture.connect(runtime.clone());
        handle.io(Io::Send(large_input())).unwrap();
        // A prefix proves the writer started; the unread body exceeds the socket buffer.
        peer.read_exact(&mut [0; 4]).unwrap();
        handle.io(Io::Send(ClientMsg::Detach)).unwrap();
        let (dropped, done) = mpsc::channel();
        runtime.spawn(Box::pin(async move {
            drop(handle);
            dropped.send(()).unwrap();
        }));
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        peer.read_to_end(&mut Vec::new()).unwrap();
    }

    #[test]
    fn flush_waits_for_a_stalled_write_and_needs_no_daemon_reply() {
        let fixture = Fixture::new();
        let runtime: Rt = Arc::new(rt::Threads::new(1));
        let (handle, mut peer) = fixture.connect(runtime);
        handle.io(Io::Send(large_input())).unwrap();
        let mut prefix = [0; 4];
        peer.read_exact(&mut prefix).unwrap();
        let reader;
        {
            let mut flush = pin!(handle.flush());
            rt::block_on(poll_fn(|cx| {
                assert!(flush.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            }));
            reader = std::thread::spawn(move || {
                let mut input = io::Cursor::new(prefix).chain(&mut peer);
                assert!(matches!(
                    read_frame::<ClientMsg>(&mut input).unwrap(),
                    Some(ClientMsg::Input { .. })
                ));
                peer
            });
            rt::block_on(flush).unwrap();
        }
        let mut peer = reader.join().unwrap();
        drop(handle);
        assert!(read_frame::<ClientMsg>(&mut peer).unwrap().is_none());
    }

    #[test]
    fn cancel_during_connect_rejects_late_installation() {
        let fixture = Fixture::new();
        let runtime: Rt = Arc::new(rt::Threads::new(1));
        let (events, received) = mpsc::channel();
        let handle = Driver::new(move |_, event| {
            let _ = events.send(event);
        })
        .start(runtime, fixture.daemon.clone());
        handle
            .io(Io::Connect {
                conn: ConnId(1),
                attach: Attach::new(Target::New, crate::Env::Keep),
                restart: false,
            })
            .unwrap();
        let mut peer = fixture.listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        handle.io(Io::Hangup).unwrap();
        assert!(read_frame::<ClientMsg>(&mut peer).unwrap().is_none());
        drop(handle);
        assert!(received.recv_timeout(Duration::from_secs(2)).is_err());
    }

    struct Backoff {
        rt: rt::Threads,
        sleeping: mpsc::Sender<()>,
    }

    impl Runtime for Backoff {
        fn spawn(&self, task: rt::BoxFuture<'static, ()>) {
            self.rt.spawn(task);
        }
        fn spawn_blocking(
            &self,
            class: rt::BlockingClass,
            bytes: usize,
            work: Box<dyn FnOnce() + Send>,
        ) -> rt::BoxFuture<'static, io::Result<()>> {
            self.rt.spawn_blocking(class, bytes, work)
        }
        fn spawn_flush(
            &self,
            work: Box<dyn FnOnce() + Send>,
        ) -> rt::BoxFuture<'static, io::Result<()>> {
            self.rt.spawn_flush(work)
        }
        fn reactor(&self) -> rt::Reactor {
            self.rt.reactor()
        }
        fn sleep(&self, duration: Duration) -> rt::BoxFuture<'static, ()> {
            let sleeping = self.sleeping.clone();
            Box::pin(async move {
                if duration == Duration::from_secs(1) {
                    sleeping.send(()).unwrap();
                }
                std::future::pending::<()>().await;
            })
        }
    }

    #[test]
    fn cancel_interrupts_reconnect_backoff() {
        struct Released(mpsc::Sender<()>);
        impl Drop for Released {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let fixture = Fixture::new();
        let (sleeping, asleep) = mpsc::channel();
        let runtime: Rt = Arc::new(Backoff {
            rt: rt::Threads::new(1),
            sleeping,
        });
        let (released, done) = mpsc::channel();
        let released = Released(released);
        let mut driver = Driver::new(move |_, _| {
            let _ = &released;
        });
        driver.watch = true;
        let handle = driver.start(runtime.clone(), fixture.daemon.clone());
        handle
            .io(Io::Connect {
                conn: ConnId(1),
                attach: Attach::new(Target::New, crate::Env::Keep),
                restart: false,
            })
            .unwrap();
        let mut peer = fixture.listener.accept().unwrap();
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        write_frame(
            &mut peer,
            &DaemonMsg::Welcome {
                proto: protocol::PROTO,
                build: "test".into(),
                caps: Caps::ALL,
                commands: Vec::new(),
            },
        )
        .unwrap();
        drop(peer);
        asleep.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(handle);
        done.recv_timeout(Duration::from_secs(2)).unwrap();
    }
    struct Again(bool);

    impl Renderer for Again {
        fn show(&mut self, _: Vec<Show>) -> Vec<Effect> {
            if std::mem::replace(&mut self.0, true) {
                Vec::new()
            } else {
                vec![Effect::Io(Io::Hangup), Effect::Show(Show::Link)]
            }
        }
    }

    #[test]
    fn the_pump_runs_until_the_renderer_asks_for_nothing_more() {
        let mut done = Vec::new();
        let mut renderer = Again(false);
        pump(
            |io| {
                done.push(io);
                Ok(())
            },
            &mut renderer,
            vec![Effect::Show(Show::Sessions)],
        );
        assert!(renderer.0 && matches!(done[..], [Io::Hangup]));
        // Nothing to render, nothing to pump; a failed send is rendered.
        let mut renderer = Again(false);
        pump(|_| Ok(()), &mut renderer, vec![Effect::Io(Io::Hangup)]);
        assert!(!renderer.0);
        pump(
            |_| Err(Error::Closed),
            &mut renderer,
            vec![Effect::Io(Io::Hangup)],
        );
        assert!(renderer.0);
    }
    #[test]
    fn hangup_during_connection_publication_ends_the_reader() {
        let fixture = Fixture::new();
        let runtime: Rt = Arc::new(rt::Threads::new(1));
        let (events, received) = mpsc::channel();
        let handle = Driver::new(move |handle, event| {
            if matches!(event.kind, EventKind::Connected { .. }) {
                handle.io(Io::Hangup).unwrap();
            }
            let _ = events.send(event);
        })
        .start(runtime, fixture.daemon.clone());
        handle
            .io(Io::Connect {
                conn: ConnId(1),
                attach: Attach::new(Target::New, crate::Env::Keep),
                restart: false,
            })
            .unwrap();
        let mut peer = fixture.listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        write_frame(
            &mut peer,
            &DaemonMsg::Welcome {
                proto: protocol::PROTO,
                build: "test".into(),
                caps: Caps::ALL,
                commands: Vec::new(),
            },
        )
        .unwrap();
        assert!(matches!(
            received.recv_timeout(Duration::from_secs(2)).unwrap().kind,
            EventKind::Connected { .. }
        ));
        assert!(read_frame::<ClientMsg>(&mut peer).unwrap().is_none());
        assert!(received.try_recv().is_err());
        drop(handle);
    }
}
