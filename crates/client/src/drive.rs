//! Ordered client I/O owned by one cancellable runtime task.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use protocol::{ClientId, ClientMsg, Environment};
use rt::Rt;

use crate::link::Start;
use crate::{
    Attach, ConnId, Connection, Daemon, Effect, Error, Event, EventKind, Io, Notice, Show,
};

type Sink = dyn Fn(&ClientHandle, Event) -> rt::BoxFuture<'static, bool> + Send + Sync;
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
}

/// Queues effects without waiting for the daemon. The last handle revokes I/O.
#[derive(Clone)]
pub struct ClientHandle(Arc<Handle>);

#[derive(Clone)]
enum Purpose {
    Session(Attach),
    Watch(ClientId),
}

impl Purpose {
    fn client(&self) -> ClientId {
        match self {
            Self::Session(attach) => attach.client,
            Self::Watch(client) => *client,
        }
    }
}

enum Request {
    Connect {
        conn: ConnId,
        purpose: Purpose,
        restart: bool,
    },
    Io(Box<Io>),
    Flush(rt::oneshot::Sender<()>),
}

const CONTROL_SLOTS: usize = 16;
// Control traffic cannot consume the reducer's mutation window or interrupt slot.
#[cfg(test)]
const MUTATION_SLOTS: usize = crate::session::UNANSWERED + 1;

#[derive(Default)]
struct Outbox {
    state: Mutex<Queued>,
    ready: rt::Notify,
    stop: rt::Notify,
    epoch: AtomicU64,
}

#[derive(Default)]
struct Queued {
    items: VecDeque<(u64, usize, Request)>,
    closed: bool,
}

impl Request {
    fn mutation(&self) -> bool {
        matches!(self, Self::Io(io) if matches!(&**io, Io::Send(msg) if crate::session::is_mutation(msg)))
    }

    fn environment(&self) -> bool {
        matches!(self, Self::Io(io) if matches!(&**io, Io::SendEnvironment | Io::Send(ClientMsg::Environment(_))))
    }
}

impl Outbox {
    fn send(&self, request: Request) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.closed {
            return Err(Error::Closed);
        }
        let replace = matches!(&request, Request::Connect { .. })
            || matches!(&request, Request::Io(io) if matches!(&**io, Io::Hangup));
        let coalesced =
            request.environment() && state.items.iter().any(|(_, _, item)| item.environment());
        let bytes = match &request {
            Request::Io(io) => match &**io {
                Io::Send(msg) => msg.retained_bytes(),
                _ => 0,
            },
            Request::Connect {
                purpose:
                    Purpose::Session(Attach {
                        env: crate::Env::Given(env),
                        ..
                    }),
                ..
            } => env.retained_bytes(),
            Request::Connect { .. } | Request::Flush(_) => 0,
        };
        let mutation = request.mutation();
        let kept = |item: &Request| {
            !(replace || coalesced && item.environment()) && item.mutation() == mutation
        };
        let (retained, requests) = state
            .items
            .iter()
            .filter(|(_, _, item)| kept(item))
            .fold((0usize, 0), |(bytes, count), (_, size, _)| {
                (bytes + size, count + 1)
            });
        let control = matches!(&request, Request::Io(io) if matches!(&**io, Io::Send(ClientMsg::Interrupt { .. })));
        let budget = crate::QueueBudget {
            requests: if mutation {
                crate::session::MUTATION_BUDGET.requests
            } else {
                CONTROL_SLOTS + 1
            },
            ..crate::session::MUTATION_BUDGET
        };
        if !budget.admits(retained.saturating_add(bytes), requests, control) {
            return Err(Error::Busy);
        }
        if replace {
            state.items.clear();
            self.epoch.fetch_add(1, Ordering::AcqRel);
        } else if coalesced {
            state.items.retain(|(_, _, item)| !item.environment());
        }
        if !matches!(&request, Request::Io(io) if matches!(&**io, Io::Hangup)) {
            state
                .items
                .push_back((self.epoch.load(Ordering::Acquire), bytes, request));
        }
        drop(state);
        self.ready.notify();
        if coalesced { Err(Error::Busy) } else { Ok(()) }
    }

    async fn recv(&self) -> Option<(u64, Request)> {
        loop {
            let seen = self.ready.count();
            {
                let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some((epoch, _, item)) = state.items.pop_front() {
                    return Some((epoch, item));
                }
                if state.closed {
                    return None;
                }
            }
            self.ready.until(|| self.ready.count() != seen).await;
        }
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.closed = true;
        state.items.clear();
        drop(state);
        self.ready.notify();
    }
}

struct Handle {
    daemon: Daemon,
    tx: Arc<Outbox>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.tx.epoch.fetch_add(1, Ordering::AcqRel);
        self.tx.stop.notify();
        self.tx.close();
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
            sink: Box::new(move |handle, event| {
                sink(handle, event);
                Box::pin(std::future::ready(true))
            }),
        }
    }

    /// Delivers every event with async backpressure; input keeps its own priority lane.
    pub fn with_inbox<T: Send + 'static>(post: crate::Post<T>, wrap: fn(Event) -> T) -> Self {
        Self {
            origin: Arc::new(crate::capture),
            sink: Box::new(move |_, event| {
                let post = post.clone();
                Box::pin(async move { post.send_async(wrap(event)).await.is_ok() })
            }),
        }
    }

    pub(crate) fn watch(self, rt: Rt, daemon: Daemon) -> ClientHandle {
        let handle = self.start(rt, daemon);
        let _ = handle.0.tx.send(Request::Connect {
            conn: ConnId(1),
            purpose: Purpose::Watch(ClientId::new()),
            restart: false,
        });
        handle
    }

    pub fn start(self, rt: Rt, daemon: Daemon) -> ClientHandle {
        let rx = Arc::new(Outbox::default());
        let handle = ClientHandle(Arc::new(Handle {
            daemon: daemon.clone(),
            tx: rx.clone(),
        }));
        let weak = Arc::downgrade(&handle.0);
        let runtime = rt;
        let spawner = runtime.clone();
        spawner.spawn(Box::pin(async move {
            let mut pending = None;
            while let Some((generation, command)) = match pending.take() {
                Some(command) => Some(command),
                None => rx.recv().await,
            } {
                if generation != rx.epoch.load(Ordering::Acquire) {
                    continue;
                }
                if let Request::Connect {
                    conn,
                    purpose,
                    restart,
                } = command
                {
                    let client = purpose.client();
                    let cancelled = || rx.epoch.load(Ordering::Acquire) != generation;
                    loop {
                        pending = rt::or(
                            async {
                                rx.stop.until(cancelled).await;
                                None
                            },
                            self.connection(
                                &runtime,
                                &daemon,
                                client,
                                conn,
                                purpose.clone(),
                                restart,
                                &rx,
                                generation,
                                &weak,
                            ),
                        )
                        .await;
                        if !matches!(purpose, Purpose::Watch(_)) || cancelled() {
                            break;
                        }
                        rt::or(
                            rx.stop.until(cancelled),
                            runtime.sleep(Duration::from_secs(1)),
                        )
                        .await;
                        if cancelled() {
                            break;
                        }
                    }
                }
            }
        }));
        handle
    }

    async fn emit(
        &self,
        handle: &Weak<Handle>,
        conn: ConnId,
        generation: u64,
        kind: EventKind,
    ) -> bool {
        let sent = {
            let Some(handle) = handle.upgrade() else {
                return false;
            };
            if handle.tx.epoch.load(Ordering::Acquire) != generation {
                return false;
            }
            (self.sink)(&ClientHandle(handle), Event { conn, kind })
        };
        sent.await
            && handle
                .upgrade()
                .is_some_and(|handle| handle.tx.epoch.load(Ordering::Acquire) == generation)
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn connection(
        &self,
        rt: &Rt,
        daemon: &Daemon,
        client: ClientId,
        conn: ConnId,
        purpose: Purpose,
        restart: bool,
        rx: &Outbox,
        generation: u64,
        handle: &Weak<Handle>,
    ) -> Option<(u64, Request)> {
        let made = self
            .open(
                rt, daemon, client, conn, &purpose, restart, generation, handle,
            )
            .await;
        let Connection {
            stream,
            mut buffered,
            welcome,
            remote,
            commands,
            machine,
            upgrade,
            ..
        } = match made {
            Ok(connection) => connection,
            Err(error) => {
                let kind = failure(rt, daemon, error).await;
                self.emit(handle, conn, generation, kind).await;
                return None;
            }
        };
        let read = async {
            if !self
                .emit(
                    handle,
                    conn,
                    generation,
                    EventKind::Connected {
                        welcome,
                        remote: remote.is_some(),
                        commands,
                        machine,
                        upgrade,
                    },
                )
                .await
            {
                return (Error::Closed, None);
            }
            let mut batch = 0;
            loop {
                batch += 1;
                if batch == crate::PUMP_BATCH {
                    batch = 0;
                    rt::yield_now().await;
                }
                match stream.recv(&mut buffered).await {
                    Ok(Some(msg)) => {
                        if !self
                            .emit(handle, conn, generation, EventKind::Msg(msg))
                            .await
                        {
                            return (Error::Closed, None);
                        }
                    }
                    Ok(None) => return (Error::Closed, None),
                    Err(error) => return (error.into(), None),
                }
            }
        };
        let write = async {
            let mut batch = 0;
            while let Some((epoch, command)) = rx.recv().await {
                batch += 1;
                if batch == crate::PUMP_BATCH {
                    batch = 0;
                    rt::yield_now().await;
                }
                if epoch != generation {
                    return (Error::Closed, Some((epoch, command)));
                }
                let command = match command {
                    Request::Flush(done) => {
                        done.send(());
                        continue;
                    }
                    Request::Io(command) => command,
                    command @ Request::Connect { .. } => {
                        return (Error::Closed, Some((epoch, command)));
                    }
                };
                let result = match *command {
                    Io::Send(msg) => stream.send(&msg).await,
                    Io::SendEnvironment => {
                        let origin = self.origin.clone();
                        match rt::unblock_with(
                            &**rt,
                            rt::BlockingClass::Storage,
                            64 << 10,
                            move || origin(),
                        )
                        .await
                        {
                            Ok(env) => stream.send(&ClientMsg::Environment(env)).await,
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
        self.emit(handle, conn, generation, EventKind::Closed(why))
            .await;
        pending
    }

    #[allow(clippy::too_many_arguments)]
    async fn open(
        &self,
        rt: &Rt,
        daemon: &Daemon,
        client: ClientId,
        conn: ConnId,
        purpose: &Purpose,
        restart: bool,
        generation: u64,
        handle: &Weak<Handle>,
    ) -> Result<Connection, Error> {
        let first = |remote: Option<&Environment>| {
            Ok(vec![match purpose {
                Purpose::Watch(_) => ClientMsg::WatchSessions,
                Purpose::Session(attach) => crate::attaching(attach, || {
                    remote.map_or_else(|| (self.origin)(), |env| Ok(env.clone()))
                })?,
            }])
        };
        let on_drain = || async {
            self.emit(handle, conn, generation, EventKind::Waiting)
                .await;
        };
        let start = if restart {
            Start::Replace
        } else {
            Start::Reuse
        };
        crate::link::connect(rt, daemon, Some(client), start, &first, &on_drain).await
    }
}

impl ClientHandle {
    pub fn daemon(&self) -> &Daemon {
        &self.0.daemon
    }

    pub fn pump(&self, renderer: &mut impl Renderer, effects: Vec<Effect>) {
        pump(|io| self.io(io), renderer, effects);
    }

    /// Resolves once preceding, unsuperseded sends reached the transport, or it closed.
    pub async fn flush(&self) -> Result<(), Error> {
        let (done, sent) = rt::oneshot::channel();
        self.0.tx.send(Request::Flush(done))?;
        sent.await.ok_or(Error::Closed)
    }

    /// Busy admits no additional request. A pending environment request still
    /// takes the latest value; mutations are refused without changing their order.
    pub fn io(&self, io: Io) -> Result<(), Error> {
        let revokes = matches!(io, Io::Hangup | Io::Connect { .. });
        let result = self.0.tx.send(match io {
            Io::Connect {
                conn,
                attach,
                restart,
            } => Request::Connect {
                conn,
                purpose: Purpose::Session(attach),
                restart,
            },
            io => Request::Io(Box::new(io)),
        });
        if revokes {
            self.0.tx.stop.notify();
        }
        result
    }
}

/// A daemon that took the connection and never answered is wedged, when
/// its link can say why.
async fn failure(rt: &Rt, daemon: &Daemon, error: Error) -> EventKind {
    if error != Error::NotResponding {
        return EventKind::Failed(error);
    }
    let link = daemon.link.clone();
    let diagnosis = rt::unblock(&**rt, move || {
        link.unanswered();
        link.diagnose()
    });
    match diagnosis.await {
        Some(diagnosis) => EventKind::Wedged(diagnosis),
        None => EventKind::Failed(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Caps, DaemonMsg, Target, read_frame, write_frame};
    use rt::Runtime;
    use std::future::{Future, poll_fn};
    use std::io::{Read, Write};
    use std::pin::pin;
    use std::sync::mpsc;
    use std::task::Poll;

    #[test]
    fn numbered_mutations_always_fit_the_transport_admission() {
        let queue = Outbox::default();
        let env = Environment::new(
            ".".into(),
            [("DATA".into(), "x".repeat(10 << 20).into())],
            false,
        );
        queue
            .send(Request::Io(Box::new(Io::Send(ClientMsg::Environment(env)))))
            .unwrap();
        let mut client = crate::SessionClient::new(crate::Env::Keep);
        client.open(Target::New);
        client.step(Event {
            conn: ConnId(1),
            kind: EventKind::Connected {
                welcome: crate::Welcome {
                    proto: protocol::PROTO,
                    caps: Caps::ALL,
                },
                remote: false,
                commands: Vec::new(),
                machine: protocol::MachineId([0; 16]),
                upgrade: None,
            },
        });
        let effects = client.offer().unwrap().input("x".repeat(8 << 20)).unwrap();
        let io = effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Io(io @ Io::Send(_)) => Some(io),
                _ => None,
            })
            .unwrap();
        assert!(matches!(&io, Io::Send(msg) if msg.client_seq() == Some(1)));
        assert_eq!(
            queue.send(Request::Io(Box::new(io))),
            Ok(()),
            "a numbered mutation must not be stranded by a second admission check"
        );
    }

    #[test]
    fn cancellation_progresses_under_saturation() {
        let outbox = Outbox::default();
        let send = |msg| outbox.send(Request::Io(Box::new(Io::Send(msg))));
        for _ in 0..MUTATION_SLOTS - 1 {
            send(ClientMsg::input("input")).unwrap();
        }
        assert_eq!(send(ClientMsg::input("overflow")), Err(Error::Busy));
        send(ClientMsg::Interrupt {
            client_seq: Some(65),
        })
        .unwrap();
        let outbox = Outbox::default();
        let large = "x".repeat(crate::QueueBudget::default().bytes / 3);
        outbox
            .send(Request::Io(Box::new(Io::Send(ClientMsg::input(
                large.clone(),
            )))))
            .unwrap();
        assert_eq!(
            outbox.send(Request::Io(Box::new(Io::Send(ClientMsg::input(large))))),
            Err(Error::Busy)
        );
        outbox
            .send(Request::Io(Box::new(Io::Send(ClientMsg::Interrupt {
                client_seq: Some(2),
            }))))
            .unwrap();
    }

    struct Fixture {
        home: sys::Home,
        daemon: Daemon,
        listener: sys::Listener,
        _owner: std::fs::File,
        root: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::Builder::new()
                .prefix("a-client-")
                .tempdir()
                .unwrap()
                .keep();
            let home = sys::Home::from(root.join("home"));
            let exe = root.join("a");
            std::fs::write(&exe, "never run").unwrap();
            let staged = home.runtime().stage(&exe).unwrap();
            std::fs::create_dir_all(home.run_dir()).unwrap();
            let owner = std::fs::File::options()
                .read(true)
                .write(true)
                .create_new(true)
                .open(home.daemon_pid_file())
                .unwrap();
            sys::fs::lock(&owner).unwrap();
            write!(&owner, "{}", sys::crash::Record::current("test")).unwrap();
            let listener = home.bind().unwrap();
            Self {
                daemon: Daemon::local(home.clone(), staged),
                home,
                listener,
                _owner: owner,
                root,
            }
        }

        fn connect(&self, rt: Rt) -> (ClientHandle, sys::OwnedConnection) {
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
            let (peer, first) = self.welcome();
            assert!(matches!(first, ClientMsg::Attach { env: None, .. }));
            assert!(matches!(
                received.recv_timeout(Duration::from_secs(2)).unwrap().kind,
                EventKind::Connected { .. }
            ));
            (handle, peer)
        }
        fn welcome(&self) -> (sys::OwnedConnection, ClientMsg) {
            let mut peer = self.listener.accept().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            assert!(matches!(
                read_frame::<ClientMsg>(&mut peer).unwrap(),
                Some(ClientMsg::Hello { .. })
            ));
            let first = read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
            write_frame(
                &mut peer,
                &DaemonMsg::Welcome {
                    proto: protocol::PROTO,
                    build: "test".into(),
                    caps: Caps::ALL,
                    machine: protocol::MachineId([0; 16]),
                    commands: Vec::new(),
                    queries: Vec::new(),
                    upgrade: None,
                },
            )
            .unwrap();
            (peer, first)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.listener.retirement().unwrap().retire().unwrap();
            std::fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    fn replay_exceeding_the_inbox_budget_preserves_every_event_and_outgoing_control() {
        let fixture = Fixture::new();
        let runtime: Rt = Arc::new(rt::Threads::new(1));
        let (post, inbox) = crate::inbox(
            crate::QueueBudget {
                bytes: 8192,
                requests: 2,
                control_reserve: 1,
            },
            Event::retained_bytes,
        );
        let handle =
            Driver::with_inbox(post, std::convert::identity).start(runtime, fixture.daemon.clone());
        handle
            .io(Io::Connect {
                conn: ConnId(1),
                attach: Attach::new(Target::New, crate::Env::Keep),
                restart: false,
            })
            .unwrap();
        let (mut peer, _) = fixture.welcome();
        let mut writer = peer.try_clone().unwrap();
        let replay = std::thread::spawn(move || {
            for seq in 1..=4096 {
                write_frame(&mut writer, &DaemonMsg::Synced { seq }).unwrap();
            }
            writer
        });
        // Flush requires the independent writer task to run while Connected
        // fills the inbox and replay is waiting for the consumer.
        handle
            .io(Io::Send(ClientMsg::Interrupt { client_seq: None }))
            .unwrap();
        rt::block_on(handle.flush()).unwrap();
        assert!(matches!(
            read_frame::<ClientMsg>(&mut peer).unwrap(),
            Some(ClientMsg::Interrupt { .. })
        ));
        assert!(matches!(
            inbox.recv_timeout(Duration::from_secs(2)).unwrap().kind,
            EventKind::Connected { .. }
        ));
        for expected in 1..=4096 {
            assert!(
                matches!(inbox.recv_timeout(Duration::from_secs(2)).unwrap().kind, EventKind::Msg(DaemonMsg::Synced { seq }) if seq == expected)
            );
        }
        let writer = replay.join().unwrap();
        drop(writer);
        drop(peer);
        assert!(matches!(
            inbox.recv_timeout(Duration::from_secs(2)).unwrap().kind,
            EventKind::Closed(Error::Closed)
        ));
        drop(handle);
    }

    #[test]
    fn dropping_the_last_handle_cancels_a_backpressured_reader() {
        let fixture = Fixture::new();
        let runtime: Rt = Arc::new(rt::Threads::new(1));
        let (post, inbox) = crate::inbox(
            crate::QueueBudget {
                bytes: 8192,
                requests: 2,
                control_reserve: 1,
            },
            Event::retained_bytes,
        );
        let handle =
            Driver::with_inbox(post, std::convert::identity).start(runtime, fixture.daemon.clone());
        handle
            .io(Io::Connect {
                conn: ConnId(1),
                attach: Attach::new(Target::New, crate::Env::Keep),
                restart: false,
            })
            .unwrap();
        let (mut peer, _) = fixture.welcome();
        write_frame(&mut peer, &DaemonMsg::Synced { seq: 1 }).unwrap();
        rt::block_on(handle.flush()).unwrap();
        drop(handle);
        // Closing a stream with unread replay may reset the peer.
        match read_frame::<ClientMsg>(&mut peer) {
            Ok(None) => {}
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
            other => panic!("connection stayed open after dropping its handle: {other:?}"),
        }
        assert!(matches!(
            inbox.recv_timeout(Duration::from_secs(2)).unwrap().kind,
            EventKind::Connected { .. }
        ));
        assert!(inbox.recv_timeout(Duration::from_secs(2)).is_none());
        assert!(inbox.closed());
    }

    #[test]
    fn initial_attach_and_refresh_use_the_configured_origin() {
        let fixture = Fixture::new();
        let runtime: Rt = Arc::new(rt::Threads::new(1));
        let (events, received) = mpsc::channel();
        let handle = Driver::with_origin(
            || {
                Ok(Environment::new(
                    "configured-origin".into(),
                    [("FROM_CALLBACK".into(), "yes".into())],
                    false,
                ))
            },
            move |_, event| {
                events.send(event).unwrap();
            },
        )
        .start(runtime, fixture.daemon.clone());
        handle
            .io(Io::Connect {
                conn: ConnId(1),
                attach: Attach::new(Target::New, crate::Env::Origin),
                restart: false,
            })
            .unwrap();
        let (mut peer, first) = fixture.welcome();
        let ClientMsg::Attach {
            env: Some(initial),
            workspace,
            ..
        } = first
        else {
            panic!("attach must carry the configured environment");
        };
        assert_eq!(initial.cwd.as_str(), "configured-origin");
        assert_eq!(workspace.as_ref(), Some(&initial.cwd));
        assert!(matches!(
            received.recv_timeout(Duration::from_secs(2)).unwrap().kind,
            EventKind::Connected { .. }
        ));
        handle.io(Io::SendEnvironment).unwrap();
        let Some(ClientMsg::Environment(refreshed)) = read_frame::<ClientMsg>(&mut peer).unwrap()
        else {
            panic!("expected environment refresh");
        };
        assert_eq!(initial, refreshed);
        drop(handle);
    }

    #[derive(Debug)]
    struct BridgeOrigin {
        link: Arc<dyn crate::link::Link>,
        env: Environment,
    }

    impl crate::link::Link for BridgeOrigin {
        fn open<'a>(
            &'a self,
            rt: &'a Rt,
            build: &'a sys::Staged,
            start: Start,
        ) -> rt::BoxFuture<'a, Result<crate::link::Opened, Error>> {
            Box::pin(async move {
                let mut opened = self.link.open(rt, build, start).await?;
                opened.remote = Some(self.env.clone());
                Ok(opened)
            })
        }
    }

    #[test]
    fn a_remote_attach_keeps_the_bridges_environment() {
        let fixture = Fixture::new();
        let runtime: Rt = Arc::new(rt::Threads::new(1));
        let remote = Environment::new(
            "remote-workspace".into(),
            [("REMOTE".into(), "yes".into())],
            false,
        );
        let mut daemon = fixture.daemon.clone();
        daemon.link = Arc::new(BridgeOrigin {
            link: daemon.link.clone(),
            env: remote.clone(),
        });
        let (events, received) = mpsc::channel();
        let handle = Driver::with_origin(
            || panic!("a remote attach must not capture the local origin"),
            move |_, event| {
                let _ = events.send(event);
            },
        )
        .start(runtime, daemon);
        handle
            .io(Io::Connect {
                conn: ConnId(1),
                attach: Attach::new(Target::New, crate::Env::Origin),
                restart: false,
            })
            .unwrap();
        let (_peer, first) = fixture.welcome();
        assert!(
            matches!(first, ClientMsg::Attach { env: Some(env), workspace: Some(workspace), .. } if env == remote && workspace == remote.cwd)
        );
        assert!(matches!(
            received.recv_timeout(Duration::from_secs(2)).unwrap().kind,
            EventKind::Connected { remote: true, .. }
        ));
        drop(handle);
    }

    fn large_input() -> ClientMsg {
        ClientMsg::Input {
            segments: "x".repeat(8 << 20).into(),
            client_seq: Some(1),
            mode: protocol::Mode::default(),
            force: false,
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
        let io = crate::link::socket(&runtime, stream).unwrap().stream;
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
    fn control_backlog_cannot_consume_the_mutation_window() {
        let queue = Outbox::default();
        for _ in 0..CONTROL_SLOTS {
            queue
                .send(Request::Io(Box::new(Io::Send(ClientMsg::List))))
                .unwrap();
        }
        assert_eq!(
            queue.send(Request::Io(Box::new(Io::Send(ClientMsg::List)))),
            Err(Error::Busy)
        );
        for seq in 1..MUTATION_SLOTS {
            queue
                .send(Request::Io(Box::new(Io::Send(ClientMsg::Input {
                    segments: seq.to_string().into(),
                    mode: protocol::Mode::default(),
                    client_seq: Some(seq as u64),
                    force: false,
                }))))
                .unwrap();
        }
        queue
            .send(Request::Io(Box::new(Io::Send(ClientMsg::Interrupt {
                client_seq: Some(MUTATION_SLOTS as u64),
            }))))
            .unwrap();
        assert_eq!(
            queue.send(Request::Io(Box::new(Io::Send(large_input())))),
            Err(Error::Busy)
        );
        for _ in 0..CONTROL_SLOTS {
            rt::block_on(queue.recv()).unwrap();
        }
        for seq in 1..=MUTATION_SLOTS {
            let (_, Request::Io(io)) = rt::block_on(queue.recv()).unwrap() else {
                panic!("expected mutation");
            };
            assert!(matches!(*io, Io::Send(msg) if msg.client_seq() == Some(seq as u64)));
        }
    }

    #[test]
    fn rejected_replacements_preserve_queued_work_and_the_connection_epoch() {
        let queue = Outbox::default();
        let send = |io| queue.send(Request::Io(Box::new(io)));
        send(Io::Send(ClientMsg::Environment(Environment::new(
            "kept".into(),
            Vec::new(),
            false,
        ))))
        .unwrap();
        send(Io::Send(ClientMsg::input("durable"))).unwrap();
        let oversized = Environment::new(
            "oversized".into(),
            [(
                "DATA".into(),
                "x".repeat(crate::session::MUTATION_BUDGET.bytes).into(),
            )],
            false,
        );
        assert_eq!(
            send(Io::Send(ClientMsg::Environment(oversized.clone()))),
            Err(Error::Busy)
        );
        assert_eq!(
            queue.send(Request::Connect {
                conn: ConnId(2),
                purpose: Purpose::Session(Attach::new(Target::New, crate::Env::Given(oversized))),
                restart: false
            }),
            Err(Error::Busy)
        );
        assert_eq!(queue.epoch.load(Ordering::Acquire), 0);
        let state = queue.state.lock().unwrap();
        assert_eq!(state.items.len(), 2);
        assert!(
            matches!(&state.items[0].2, Request::Io(io) if matches!(&**io, Io::Send(ClientMsg::Environment(env)) if env.cwd.as_str() == "kept"))
        );
        drop(state);
        send(Io::Hangup).unwrap();
        assert_eq!(queue.epoch.load(Ordering::Acquire), 1);
    }

    #[test]
    fn environment_replacement_keeps_only_the_latest_value() {
        let queue = Outbox::default();
        for (dir, result) in [("first", Ok(())), ("latest", Err(Error::Busy))] {
            let environment = Environment::new(dir.into(), Vec::new(), false);
            assert_eq!(
                queue.send(Request::Io(Box::new(Io::Send(ClientMsg::Environment(
                    environment
                ))))),
                result
            );
        }
        let mut state = queue.state.lock().unwrap();
        assert_eq!(state.items.len(), 1);
        let (_, _, Request::Io(io)) = state.items.pop_front().unwrap() else {
            panic!("expected environment");
        };
        assert!(
            matches!(*io, Io::Send(ClientMsg::Environment(env)) if env.cwd.as_str() == "latest")
        );
    }

    #[test]
    fn busy_admission_is_rendered_as_a_client_notice() {
        struct Notices(Vec<Show>);
        impl Renderer for Notices {
            fn show(&mut self, shown: Vec<Show>) -> Vec<Effect> {
                self.0.extend(shown);
                Vec::new()
            }
        }
        let mut notices = Notices(Vec::new());
        pump(
            |_| Err(Error::Busy),
            &mut notices,
            vec![Effect::Io(Io::SendEnvironment)],
        );
        assert!(matches!(
            notices.0.as_slice(),
            [Show::Notice(Notice::NotSent(Error::Busy))]
        ));
    }

    #[test]
    fn stalled_transport_applies_backpressure_to_environment_refreshes() {
        let fixture = Fixture::new();
        let runtime: Rt = Arc::new(rt::Threads::new(1));
        let (handle, mut peer) = fixture.connect(runtime);
        handle.io(Io::Send(large_input())).unwrap();
        peer.read_exact(&mut [0; 4]).unwrap();
        let refused = (0..4096).any(|_| handle.io(Io::SendEnvironment).is_err());
        drop(handle);
        peer.read_to_end(&mut Vec::new()).unwrap();
        assert!(refused, "a stalled transport accepted every queued refresh");
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
        let driver = Driver::new(move |_, _| {
            let _ = &released;
        });
        let handle = driver.watch(runtime.clone(), fixture.daemon.clone());
        let (peer, first) = fixture.welcome();
        assert!(matches!(first, ClientMsg::WatchSessions));
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
                machine: protocol::MachineId([0; 16]),
                commands: Vec::new(),
                queries: Vec::new(),
                upgrade: None,
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
