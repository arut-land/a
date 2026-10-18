//! The daemon: owns every session, accepts clients, runs the agents.

pub mod config;
mod model;
mod outbox;
mod session;
mod upgrade;

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::future::{Future, poll_fn};
use std::io::{self, BufReader, Write};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};
use std::{mem, thread};

use outbox::{Next, Outbox};
use protocol::{
    Activity, Cap, Caps, ClientDecl, ClientMsg, DaemonMsg, Environment, Frame, Origin, PROTO,
    Rejection, Residency, Role, SessionId, SessionSummary, Target, Versions, read_frame,
};
use session::{Attachment, Session, Shared};
use store::{Header, Jsonl, LogStore};
use sys::Home;
use upgrade::{Lifecycle, Verdict};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

/// How the daemon runs, from `A_*` environment variables.
#[derive(Clone, Debug)]
pub struct Config {
    /// `A_IDLE_SECS`: exit after this long with no clients and no work.
    pub idle: Duration,
    /// `A_HIBERNATE_SECS`: unload a session after this long unused.
    pub hibernate: Duration,
    /// `A_DRAIN_SECS`: how long an upgrade waits for in-flight work.
    pub drain: Duration,
    /// `A_AGENT` (`chat` or `echo`, which needs no model): overrides the
    /// config files.
    pub agent: Option<agent::Kind>,
    /// `A_MODEL` (`host:model`, or a bare model on `OpenAI`): overrides the
    /// config files.
    pub model: Option<String>,
    /// `A_ASK`: comma-separated tools whose calls wait for the user's
    /// approval.
    pub ask: Vec<String>,
    /// `A_SYSTEM_PROMPT`: the system prompt, in place of the one the
    /// extensions make.
    pub system_prompt: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(600),
            hibernate: Duration::from_secs(300),
            drain: Duration::from_secs(60),
            agent: None,
            model: None,
            ask: Vec::new(),
            system_prompt: None,
        }
    }
}

impl Config {
    /// The defaults, overridden by whichever variables are set and valid.
    pub fn from_env() -> Self {
        fn var<T: FromStr>(name: &str) -> Option<T> {
            std::env::var(name).ok()?.parse().ok()
        }
        let defaults = Self::default();
        Self {
            idle: var("A_IDLE_SECS").map_or(defaults.idle, Duration::from_secs),
            hibernate: var("A_HIBERNATE_SECS").map_or(defaults.hibernate, Duration::from_secs),
            drain: var("A_DRAIN_SECS").map_or(defaults.drain, Duration::from_secs),
            agent: var("A_AGENT").or(defaults.agent),
            model: var("A_MODEL").or(defaults.model),
            ask: std::env::var("A_ASK").map_or(defaults.ask, |tools| {
                tools
                    .split(',')
                    .map(str::trim)
                    .filter(|tool| !tool.is_empty())
                    .map(str::to_owned)
                    .collect()
            }),
            system_prompt: std::env::var("A_SYSTEM_PROMPT")
                .ok()
                .filter(|prompt| !prompt.is_empty()),
        }
    }

    /// The built-in extensions, registered as any other. The echo agent's
    /// `sleep` is a test fixture: no model is offered it.
    fn registry(&self) -> Result<agent::Registry, agent::Refused> {
        let registry = agent::builtins(&agent::Policy {
            ask: self.ask.clone(),
        })?;
        match self.agent {
            Some(agent::Kind::Echo) => registry.with(&agent::Fixtures),
            _ => Ok(registry),
        }
    }
}

/// Where the daemon's events go: nowhere unless built with the `events`
/// feature (to its log), so emitting them costs nothing.
#[cfg(not(any(test, feature = "events")))]
static EVENTS: rt::Noop = rt::Noop;
#[cfg(all(feature = "events", not(test)))]
static EVENTS: rt::Stderr = rt::Stderr;
#[cfg(test)]
static EVENTS: tests::Recorder = tests::Recorder::new();

/// Emits the event `build` makes, building it only if anyone listens.
#[inline]
fn event(build: impl FnOnce() -> rt::Event) {
    emit(&EVENTS, build);
}

#[inline]
fn emit<E: rt::EventSink>(sink: &E, build: impl FnOnce() -> rt::Event) {
    if E::ENABLED {
        sink.emit(build);
    }
}

/// [`event`], about session `id`.
#[inline]
fn session_event(name: &'static str, id: SessionId, detail: impl FnOnce() -> String) {
    event(|| rt::Event {
        name,
        session: Some(id.to_string()),
        detail: detail(),
    });
}

/// Now, if anyone listens for events: for timing one.
#[inline]
fn stamp() -> Option<Instant> {
    fn enabled<E: rt::EventSink>(_: &E) -> bool {
        E::ENABLED
    }
    enabled(&EVENTS).then(Instant::now)
}

/// Every lock here guards state a panicking holder leaves consistent, so
/// poisoning is ignored.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Daemon {
    shared: Arc<Shared>,
    slots: Mutex<Vec<Arc<Slot>>>,
    /// Open connections; any keeps the daemon alive.
    clients: AtomicUsize,
    /// Wakes the supervisor when what it decides on may have changed.
    pulse: Pulse,
}

/// A wake-up for one waiting task, remembered until it waits.
#[derive(Default)]
struct Pulse(Mutex<(bool, Option<Waker>)>);

impl Pulse {
    fn wake(&self) {
        let mut pulse = lock(&self.0);
        pulse.0 = true;
        if let Some(waker) = pulse.1.take() {
            waker.wake();
        }
    }

    /// Waits for a wake, or until `deadline`; `true` if woken.
    fn wait(&self, rt: &dyn rt::Runtime, deadline: Option<Instant>) -> impl Future<Output = bool> {
        let mut sleep = deadline.map(|at| rt.sleep(at.saturating_duration_since(Instant::now())));
        poll_fn(move |cx| {
            let mut pulse = lock(&self.0);
            if mem::take(&mut pulse.0) {
                return Poll::Ready(true);
            }
            pulse.1 = Some(cx.waker().clone());
            drop(pulse);
            match sleep.as_mut().map(|sleep| sleep.as_mut().poll(cx)) {
                Some(Poll::Ready(())) => Poll::Ready(false),
                _ => Poll::Pending,
            }
        })
    }
}

/// How often the supervisor checks whether background processes have
/// ended, while any run: nothing tells it when they do.
const JOBS_CHECK: Duration = Duration::from_secs(5);

/// A session the daemon knows about. Its log is loaded, and its actor runs,
/// only while `live` is set; otherwise it is hibernated and costs nothing.
struct Slot {
    header: Header,
    live: Mutex<Option<Arc<Session>>>,
    /// The parent has fired its watch on this child, so reports are moot.
    /// Known only since this daemon started.
    heard: AtomicBool,
}

impl Slot {
    fn new(header: Header, live: Option<Arc<Session>>) -> Arc<Self> {
        Arc::new(Self {
            header,
            live: Mutex::new(live),
            heard: AtomicBool::new(false),
        })
    }
}

impl Daemon {
    /// Reads only session headers. Sessions with a busy marker were working
    /// when the last daemon stopped, so they load at once to recover.
    fn load(home: &Home, config: &Config) -> io::Result<Arc<Self>> {
        let store = Arc::new(Jsonl::open(home.sessions_dir())?);
        let slots = store
            .headers()?
            .into_iter()
            .map(|header| Slot::new(header, None))
            .collect();
        let rt: rt::Rt = Arc::new(rt::Threads::default());
        let models = model::Models {
            settings: config::Settings::new(home.config_file(), config.model.clone(), config.agent),
            http: Arc::new(llm::Ureq::new(Arc::clone(&rt))),
        };
        let registry = agent::Current::new(config.registry().map_err(io::Error::other)?);
        let daemon = Arc::new_cyclic(|daemon| Self {
            shared: Arc::new(Shared {
                rt,
                store,
                registry,
                models,
                system: config.system_prompt.clone(),
                spill: home.spill_dir(),
                loaded: Mutex::default(),
                daemon: daemon.clone(),
            }),
            slots: Mutex::new(slots),
            clients: AtomicUsize::new(0),
            pulse: Pulse::default(),
        });
        for slot in daemon.slots() {
            if daemon.shared.store.is_busy(slot.header.id)
                && let Err(e) = daemon.with_live(&slot, |_| ())
            {
                eprintln!(
                    "session {}: loading for recovery: {e}",
                    slot.header.id.short()
                );
            }
        }
        Ok(daemon)
    }

    fn slots(&self) -> Vec<Arc<Slot>> {
        lock(&self.slots).clone()
    }

    fn slot(&self, id: SessionId) -> Option<Arc<Slot>> {
        lock(&self.slots)
            .iter()
            .find(|s| s.header.id == id)
            .cloned()
    }

    /// Runs `f` on the slot's live session, loading it first if hibernated.
    fn with_live<T>(&self, slot: &Slot, f: impl FnOnce(&Arc<Session>) -> T) -> io::Result<T> {
        let mut live = lock(&slot.live);
        let session = match &mut *live {
            Some(session) => session,
            empty @ None => {
                let loaded = self.shared.store.load(slot.header.id)?;
                empty.insert(Session::start(
                    slot.header.clone(),
                    loaded,
                    Arc::clone(&self.shared),
                ))
            }
        };
        Ok(f(session))
    }

    fn resolve(&self, target: &Target, workspace: PathBuf) -> io::Result<Attachment> {
        let found = match target {
            Target::New => return self.create(workspace),
            Target::Id(id) => self.slot(*id),
            // A child is reached through its parent, not resumed as latest.
            Target::Latest { any_workspace } => lock(&self.slots)
                .iter()
                .rev()
                .find(|s| {
                    s.header.parent.is_none() && (*any_workspace || s.header.workspace == workspace)
                })
                .cloned(),
        };
        let slot =
            found.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no matching session"))?;
        self.with_live(&slot, Session::open)
    }

    fn create(&self, workspace: PathBuf) -> io::Result<Attachment> {
        let header = Header {
            id: SessionId::new(),
            workspace,
            parent: None,
            extensions: session::installed(&self.shared.registry.get()),
        };
        let loaded = self.shared.store.create(&header)?;
        let session = Session::start(header.clone(), loaded, Arc::clone(&self.shared));
        let attachment = session.open();
        lock(&self.slots).push(Slot::new(header, Some(session)));
        Ok(attachment)
    }

    fn list(&self) -> Vec<SessionSummary> {
        self.slots()
            .iter()
            .map(|slot| match &*lock(&slot.live) {
                Some(session) => session.summary(),
                None => SessionSummary {
                    id: slot.header.id,
                    workspace: slot.header.workspace.clone(),
                    entries: self.shared.store.count(slot.header.id).unwrap_or(0),
                    clients: 0,
                    // Only a session with no work is unloaded.
                    activity: Activity::Idle,
                    residency: Residency::Hibernated,
                },
            })
            .collect()
    }

    /// Unloads sessions idle for `after`: no attachments, no work. Checked
    /// under the slot lock, which `resolve` also holds while attaching.
    fn hibernate(&self, after: Duration) {
        for slot in self.slots() {
            let mut live = lock(&slot.live);
            if live.as_ref().is_some_and(|s| s.idle_for() >= after)
                && let Some(session) = live.take()
            {
                session_event("hibernate", session.id, String::new);
                session.stop();
            }
        }
    }

    /// When the next loaded session is due to hibernate, if one is idle.
    fn next_hibernation(&self, after: Duration, now: Instant) -> Option<Instant> {
        self.live()
            .iter()
            .map(|s| s.idle_for())
            .filter(|idle| !idle.is_zero())
            .map(|idle| now + after.saturating_sub(idle))
            .min()
    }

    fn has_work(&self) -> bool {
        self.live().iter().any(|s| s.has_work())
    }

    fn live(&self) -> Vec<Arc<Session>> {
        self.slots()
            .iter()
            .filter_map(|slot| lock(&slot.live).clone())
            .collect()
    }

    /// No connections, and no loaded session has work or background
    /// processes.
    fn idle(&self) -> bool {
        self.clients.load(Ordering::SeqCst) == 0
            && !self.live().iter().any(|s| s.has_work() || s.has_jobs())
    }

    /// Hibernates idle sessions, and exits once the whole daemon has been
    /// idle for `config.idle`, or once a drain has no work left or runs out
    /// of grace. Every entry is durable when written, so exiting is all a
    /// drain needs; unfinished calls recover in the next daemon.
    ///
    /// It sleeps until the next deadline or until [`Daemon::pulse`] says
    /// something changed (a client came or went, a session's work began or
    /// ended), so an idle daemon does not wake at all. While work runs it
    /// holds off system sleep.
    async fn supervise(&self, life: &Lifecycle, config: &Config, exit: Exit) {
        // When the daemon last became idle.
        let mut since: Option<Instant> = None;
        // When a drain out of grace stopped the calls still running.
        let mut stopped: Option<Instant> = None;
        let mut awake: Option<Option<sys::Awake>> = None;
        loop {
            let now = Instant::now();
            event(|| rt::Event {
                name: "wake",
                session: None,
                detail: String::new(),
            });
            #[cfg(unix)]
            if let Err(e) = sys::log::rotate_stderr(&exit.log) {
                eprintln!("rotating the log: {e}");
            }
            self.hibernate(config.hibernate);
            let draining = life.draining();
            let busy = draining.is_some() || self.has_work();
            if busy != awake.is_some() {
                awake = busy.then(|| sys::Awake::hold("a: an agent is at work"));
            }
            let mut next = self.next_hibernation(config.hibernate, now);
            if self.live().iter().any(|s| s.has_jobs()) {
                next = Some(earliest(next, now + JOBS_CHECK));
            }
            if let Some(began) = draining {
                if now >= began + config.drain && stopped.is_none() {
                    for session in self.live() {
                        session.interrupt();
                    }
                    stopped = Some(now);
                }
                // A stopped call gets the time its processes have to go
                // and its outcome to commit.
                let settled = stopped.map(|at| at + agent::Kill::GRACE * 2);
                if !self.has_work() || settled.is_some_and(|at| now >= at) {
                    for session in self.live() {
                        session.stop();
                    }
                    exit.now("drained for a newer build");
                }
                next = Some(earliest(next, settled.unwrap_or(began + config.drain)));
            } else if self.idle() {
                let at = *since.get_or_insert(now) + config.idle;
                if now >= at {
                    exit.now(&format!("idle for {}s", config.idle.as_secs()));
                }
                next = Some(earliest(next, at));
            } else {
                since = None;
            }
            // Whatever woke us counts as activity: idleness starts again.
            if self.pulse.wait(&*self.shared.rt, next).await {
                since = None;
            }
        }
    }
}

fn earliest(a: Option<Instant>, b: Instant) -> Instant {
    a.map_or(b, |a| a.min(b))
}

/// What the daemon lets go of when it exits.
struct Exit {
    socket: PathBuf,
    /// The locked pid file; see [`claim`].
    pid: File,
    log: PathBuf,
}

impl Exit {
    /// Unlinks the socket first so new clients spawn a fresh daemon (one
    /// that raced into our backlog sees EOF and retries), then empties the
    /// record (a clean exit is no crash) and releases the pid lock before
    /// the process closes its connections: a client reconnecting on EOF
    /// finds the lock free for the daemon it spawns.
    fn now(self, why: &str) -> ! {
        let _ = std::fs::remove_file(&self.socket);
        let _ = self.pid.set_len(0);
        drop(self.pid);
        event(|| rt::Event {
            name: "exit",
            session: None,
            detail: why.to_owned(),
        });
        eprintln!("{why}, exiting");
        std::process::exit(0)
    }
}

/// Counts one open connection for as long as it lives.
struct Client<'a>(&'a Daemon);

impl<'a> Client<'a> {
    fn new(daemon: &'a Daemon) -> Self {
        daemon.clients.fetch_add(1, Ordering::SeqCst);
        daemon.pulse.wake();
        Self(daemon)
    }
}

impl Drop for Client<'_> {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::SeqCst);
        self.0.pulse.wake();
    }
}

/// Runs the daemon, as staged build `build`, until it has been idle for
/// `config.idle` (no attached clients and no unfinished work) or has drained
/// for a newer build. Sessions live in their logs, so the next client simply
/// starts a new daemon.
#[cfg(unix)]
pub fn serve(listener: &UnixListener, home: &Home, config: &Config, build: &str) -> io::Result<()> {
    let pid = claim(home, build)?;
    let daemon = Daemon::load(home, config)?;
    let life = Arc::new(Lifecycle::new(build.to_owned(), home.runtime()));
    let (monitor, watch, config) = (Arc::clone(&daemon), Arc::clone(&life), config.clone());
    let exit = Exit {
        socket: home.socket(),
        pid,
        log: home.daemon_log(),
    };
    daemon.shared.rt.spawn(Box::pin(async move {
        monitor.supervise(&watch, &config, exit).await;
    }));
    // Connections block on their sockets, so each gets a thread of its own
    // and reaches the sessions' tasks through their mailboxes.
    for conn in listener.incoming() {
        let conn = conn?;
        let (daemon, life) = (Arc::clone(&daemon), Arc::clone(&life));
        thread::spawn(move || {
            let _client = Client::new(&daemon);
            if let Err(e) = handle(&daemon, &life, &conn) {
                eprintln!("connection: {e}");
            }
        });
    }
    Ok(())
}

/// Takes the daemon lock for this process's lifetime and records us in it
/// ([`sys::crash::Record`]), noting first a previous daemon that died
/// holding it. While the lock is held the pid is guaranteed to be ours,
/// which is what lets clients kill a wedged daemon safely.
fn claim(home: &Home, build: &str) -> io::Result<File> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(home.daemon_pid_file())?;
    if file.try_lock().is_err() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "another daemon is running",
        ));
    }
    if let Some(dead) = sys::crash::reap(home, &mut file)? {
        eprintln!(
            "the previous daemon (pid {}) stopped without shutting down",
            dead.pid
        );
    }
    // Not synced: clients read it while we hold the lock, and after a crash
    // there is no lock to vouch for it.
    write!(file, "{}", sys::crash::Record::current(build))?;
    Ok(file)
}

/// How long a client refused at its hello has to read the answer and hang
/// up before the daemon does.
const REFUSED_GRACE: Duration = Duration::from_secs(2);

/// How many answers to a connection's numbered mutations are kept for
/// repeats.
const ANSWERS_KEPT: usize = 64;

/// The answer to a mutation: done, or refused and why.
type Answer = Result<(), (Rejection, String)>;

/// A connection's answers to its numbered mutations, so a repeat gets the
/// same answer and is not done again.
#[derive(Default)]
struct Answers {
    last: u64,
    recent: VecDeque<(u64, Answer)>,
}

impl Answers {
    /// The answer to mutation `seq`: `act`'s if it is new, else the one
    /// given before.
    fn answer(&mut self, seq: u64, act: impl FnOnce() -> Answer) -> Answer {
        if seq <= self.last {
            return self.recent.iter().find(|(s, _)| *s == seq).map_or_else(
                || Err((Rejection::Stale, "a repeat too old to answer again".into())),
                |(_, a)| a.clone(),
            );
        }
        let answer = act();
        self.last = seq;
        if self.recent.len() == ANSWERS_KEPT {
            self.recent.pop_front();
        }
        self.recent.push_back((seq, answer.clone()));
        answer
    }
}

/// Writes what `outbox` hands out until it closes, catching the client up
/// when it falls behind.
#[cfg(unix)]
fn write_out(mut conn: UnixStream, outbox: &Outbox) {
    while let Some(next) = outbox.next() {
        match next {
            Next::Write(frame) => {
                if conn.write_all(frame.as_bytes()).is_err() {
                    outbox.abandon();
                    return;
                }
            }
            Next::CatchUp {
                session,
                following,
                since,
            } => match session.upgrade() {
                Some(session) => session.catch_up(outbox, following, since),
                None => outbox.unfollow(),
            },
        }
    }
}

#[cfg(unix)]
fn handle(daemon: &Daemon, life: &Lifecycle, conn: &UnixStream) -> io::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let (verdict, theirs, caps) = match read_frame(&mut reader)? {
        Some(ClientMsg::Hello {
            proto,
            build,
            min,
            caps,
        }) => {
            let theirs = Versions::new(min, proto);
            (life.verdict(theirs, &build), theirs, caps & Caps::ALL)
        }
        _ => (Verdict::Mismatch, Versions::from(0), Caps::NONE),
    };
    let outbox = Outbox::new(caps);
    let writer = outbox.clone();
    let stream = conn.try_clone()?;
    thread::spawn(move || write_out(stream, &writer));
    // Whatever the client sent behind a hello that is not welcomed is read
    // and dropped, never acted on.
    let refuse = |reader: &mut BufReader<UnixStream>| {
        let _ = io::copy(reader, &mut io::sink());
    };
    let result = match (verdict, Versions::OURS.common(theirs)) {
        (Verdict::Welcome, Some(proto)) => {
            let registered = life.register(outbox.clone());
            let mut conn = Conn {
                daemon,
                life,
                outbox: &outbox,
                client: registered.client(),
                attached: None,
                decl: ClientDecl::default(),
            };
            conn.reply(&DaemonMsg::Welcome {
                proto,
                build: life.build.clone(),
                caps,
                client: conn.client,
            });
            conn.serve(&mut reader)
        }
        (Verdict::Drain, _) => {
            life.drain();
            daemon.pulse.wake();
            reply(&outbox, &DaemonMsg::Draining);
            // Held open until we exit: the client's cue to reconnect.
            refuse(&mut reader);
            Ok(())
        }
        _ => {
            reply(
                &outbox,
                &DaemonMsg::Mismatch {
                    supported: PROTO,
                    min: Versions::OURS.min_field(),
                },
            );
            conn.set_read_timeout(Some(REFUSED_GRACE))?;
            refuse(&mut reader);
            Ok(())
        }
    };
    outbox.close();
    result
}

fn reply(outbox: &Outbox, msg: &DaemonMsg) {
    // The writer ends with the connection; a lost reply needs no handling.
    if let Ok(frame) = Frame::new(msg) {
        let _ = outbox.send(frame);
    }
}

/// A welcomed connection.
struct Conn<'a> {
    daemon: &'a Daemon,
    life: &'a Lifecycle,
    outbox: &'a Outbox,
    /// Its id, as origins name it.
    client: u32,
    attached: Option<Attachment>,
    decl: ClientDecl,
}

impl Conn<'_> {
    fn reply(&self, msg: &DaemonMsg) {
        reply(self.outbox, msg);
    }

    fn error(&self, message: String) {
        self.reply(&DaemonMsg::Error { message });
    }

    #[cfg(unix)]
    fn serve(&mut self, reader: &mut BufReader<UnixStream>) -> io::Result<()> {
        let mut answers = Answers::default();
        while let Some(mut msg) = read_frame::<ClientMsg>(reader)? {
            if let Some(&mut client_seq) = msg.client_seq_mut() {
                let answer = match client_seq {
                    Some(seq) => answers.answer(seq, || self.mutate(msg)),
                    None => self.mutate(msg),
                };
                self.answered(client_seq, answer);
                continue;
            }
            match msg {
                ClientMsg::Attach {
                    target,
                    workspace,
                    since,
                    env,
                    decl,
                } => match self.daemon.resolve(&target, workspace) {
                    Ok(s) => {
                        self.decl = decl;
                        // Before the replay, so a client that saw itself
                        // attached knows its environment is recorded.
                        if let Some(env) = env {
                            self.set_env(&s, env);
                        }
                        s.session().attach(self.outbox, since);
                        session_event("attach", s.session().id, || format!("since {since}"));
                        self.attached = Some(s);
                    }
                    Err(e) => self.error(e.to_string()),
                },
                ClientMsg::Declare(decl) => self.decl = decl,
                ClientMsg::Environment(env) => match &self.attached {
                    Some(s) => self.set_env(s, env),
                    None => self.error("not attached".into()),
                },
                ClientMsg::List => self.reply(&DaemonMsg::Sessions {
                    sessions: self.daemon.list(),
                }),
                ClientMsg::Detach => break,
                ClientMsg::Hello { .. }
                | ClientMsg::Input { .. }
                | ClientMsg::Steer { .. }
                | ClientMsg::Approve { .. }
                | ClientMsg::Interrupt { .. } => {}
            }
        }
        self.outbox.unfollow();
        Ok(())
    }

    fn set_env(&self, s: &Attachment, env: Environment) {
        if let Err(e) = s.session().set_env(env) {
            self.error(format!("recording the environment: {e}"));
        }
    }

    /// Does mutation `msg`, if this client may.
    fn mutate(&self, msg: ClientMsg) -> Answer {
        let session = match &self.attached {
            _ if self.decl.role == Role::Viewer => {
                return Err((Rejection::ReadOnly, "this client is a viewer".into()));
            }
            Some(s) => s.session(),
            None => return Err((Rejection::NotAttached, "not attached".into())),
        };
        match msg {
            ClientMsg::Interrupt { .. } => {
                session.interrupt();
                Ok(())
            }
            _ if self.life.draining().is_some() => Err((
                Rejection::Draining,
                "the daemon is restarting for an upgrade; send that again in a moment".into(),
            )),
            ClientMsg::Input { text, .. } => session
                .submit(text)
                .map_err(|message| (Rejection::Refused, message)),
            ClientMsg::Steer { text, .. } => session
                .steer(text)
                .map_err(|message| (Rejection::Refused, message)),
            ClientMsg::Approve { call, approved, .. } => session
                .answer(call, approved)
                .map_err(|message| (Rejection::NotWaiting, message)),
            _ => Err((Rejection::Other, "not a mutation".into())),
        }
    }

    /// Tells the client the answer to its mutation `client_seq`: an ack
    /// or rejection where it takes them, else an error for a refusal.
    fn answered(&self, client_seq: Option<u64>, answer: Answer) {
        match (client_seq, answer) {
            (Some(client_seq), answer) if self.outbox.caps().has(Cap::Ack) => {
                let origin = Origin {
                    client: self.client,
                    client_seq,
                };
                self.reply(&match answer {
                    Ok(()) => DaemonMsg::Ack { origin },
                    Err((reason, message)) => DaemonMsg::Rejected {
                        origin,
                        reason,
                        message,
                    },
                });
            }
            (_, Err((_, message))) => self.error(message),
            (_, Ok(())) => {}
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, unix))]
mod conn_tests;
