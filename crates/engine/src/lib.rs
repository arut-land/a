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
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Poll;
use std::thread;
use std::time::{Duration, Instant};

use outbox::{Next, Outbox};
use protocol::{
    Activity, Cap, Caps, ClientDecl, ClientMsg, DaemonMsg, Environment, Frame, HomeSettings,
    Origin, PROTO, Rejection, Residency, Role, SessionId, SessionSummary, SettingsChange, Target,
    Versions, read_frame,
};
use session::{Attachment, Session, Shared};
use store::{Header, Jsonl, LogStore};
use sys::Home;
use upgrade::{Lifecycle, Verdict};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

/// How the daemon runs: the `[daemon]` table of `$A_HOME/config.toml`,
/// read as it starts. Never its environment, which is whichever client's
/// started it.
#[derive(Clone, Debug)]
pub struct Config {
    /// Exit after this long with no clients and no work.
    pub idle: Duration,
    /// Unload a session after this long unused.
    pub hibernate: Duration,
    /// How long an upgrade waits for in-flight work.
    pub drain: Duration,
    /// `chat` or `echo` (which needs no model): overrides the config files'
    /// `agent`.
    pub agent: Option<agent::Kind>,
    /// Tools whose calls wait for the user's approval.
    pub ask: Vec<String>,
    /// The system prompt, in place of the one the extensions make.
    pub system_prompt: Option<String>,
    /// Young deaths in a row after which a daemon refuses to serve (0 for
    /// never); see [`sys::crash`].
    pub crash_limit: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(600),
            hibernate: Duration::from_secs(300),
            drain: Duration::from_secs(60),
            agent: None,
            ask: Vec::new(),
            system_prompt: None,
            crash_limit: sys::crash::LIMIT,
        }
    }
}

impl Config {
    /// The defaults, overridden by what `home`'s file sets; a file that
    /// cannot be read is reported and passed over.
    pub fn load(home: &Home) -> Self {
        let file = sys::Config::read(&home.config_file()).unwrap_or_else(|e| {
            eprintln!("{e}; running with the default [daemon] settings");
            sys::Config::default()
        });
        Self::from(file.daemon)
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

impl From<sys::config::Daemon> for Config {
    fn from(file: sys::config::Daemon) -> Self {
        let defaults = Self::default();
        let secs = |set: Option<u64>, default| set.map_or(default, Duration::from_secs);
        Self {
            idle: secs(file.idle_secs, defaults.idle),
            hibernate: secs(file.hibernate_secs, defaults.hibernate),
            drain: secs(file.drain_secs, defaults.drain),
            agent: file.agent.and_then(|name| {
                name.parse()
                    .map_err(|e| eprintln!("agent `{name}` in [daemon]: {e}"))
                    .ok()
            }),
            ask: file.ask,
            system_prompt: file.system_prompt.filter(|prompt| !prompt.is_empty()),
            crash_limit: file.crash_limit.map_or(defaults.crash_limit, |n| {
                u32::try_from(n).unwrap_or(u32::MAX)
            }),
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
    /// The connections watching the session list, and the list as they
    /// were last sent it.
    watchers: Mutex<(Vec<Outbox>, Vec<SessionSummary>)>,
    /// A client asked the daemon to stop.
    stopping: AtomicBool,
    /// When it started, in seconds since the Unix epoch.
    started: u64,
}

/// Wakes the supervisor. A wake while it is busy is seen when it next
/// waits.
#[derive(Default)]
struct Pulse(rt::Notify);

impl Pulse {
    fn wake(&self) {
        self.0.notify();
    }
}

/// A session the daemon knows about. Its log is loaded, and its actor runs,
/// only while `live` is set; otherwise it is hibernated and costs nothing.
struct Slot {
    header: Header,
    live: Mutex<Option<Arc<Session>>>,
    /// The parent has fired its watch on this child, so reports are moot.
    /// Known only since this daemon started.
    heard: AtomicBool,
    /// How it lists while hibernated: as it was when it hibernated, or
    /// once counted from its log. Only a loaded session appends, so listing
    /// reads each log once.
    last: Mutex<Option<SessionSummary>>,
}

impl Slot {
    fn new(header: Header, live: Option<Arc<Session>>) -> Arc<Self> {
        Arc::new(Self {
            header,
            live: Mutex::new(live),
            heard: AtomicBool::new(false),
            last: Mutex::new(None),
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
            settings: config::Settings::new(home.config_file(), config.agent),
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
            watchers: Mutex::default(),
            stopping: AtomicBool::new(false),
            started: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
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
                *lock(&slot.last) = None;
                empty.insert(Session::start(
                    slot.header.clone(),
                    loaded,
                    Arc::clone(&self.shared),
                ))
            }
        };
        Ok(f(session))
    }

    /// The session `target` names, attached. `workspace` is only worked
    /// out for a target that needs it: an attach by id never looks.
    fn resolve(
        &self,
        target: &Target,
        workspace: impl FnOnce() -> PathBuf,
    ) -> io::Result<Attachment> {
        let found = match target {
            Target::New => return self.create(workspace()),
            Target::Id(id) => self.slot(*id),
            Target::Prefix(prefix) => {
                let slots = lock(&self.slots);
                let mut matching = slots.iter().filter(|s| {
                    let id = s.header.id;
                    id.to_string().starts_with(prefix.as_str())
                        || id.short().to_string().starts_with(prefix.as_str())
                });
                match (matching.next(), matching.next()) {
                    (Some(one), None) => Some(Arc::clone(one)),
                    (Some(_), Some(_)) => {
                        return Err(io::Error::other(format!(
                            "{prefix} matches several sessions"
                        )));
                    }
                    (None, _) => {
                        return Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("no session matches {prefix}"),
                        ));
                    }
                }
            }
            // A child is reached through its parent, not resumed as latest.
            Target::Latest { any_workspace } => {
                let workspace = (!any_workspace).then(workspace);
                lock(&self.slots)
                    .iter()
                    .rev()
                    .find(|s| {
                        s.header.parent.is_none()
                            && workspace.as_ref().is_none_or(|w| s.header.workspace == *w)
                    })
                    .cloned()
            }
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
                None => lock(&slot.last)
                    .get_or_insert_with(|| SessionSummary {
                        id: slot.header.id,
                        workspace: slot.header.workspace.clone(),
                        entries: self.shared.store.count(slot.header.id).unwrap_or(0),
                        clients: 0,
                        // Not loaded since the daemon started, so idle: one
                        // with work or an ask has a busy marker, and loads.
                        activity: Activity::Idle,
                        residency: Residency::Hibernated,
                    })
                    .clone(),
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
                *lock(&slot.last) = Some(SessionSummary {
                    clients: 0,
                    residency: Residency::Hibernated,
                    ..session.summary()
                });
                session_event("hibernate", session.id, String::new);
                session.stop();
            }
        }
    }

    /// Starts sending `outbox` the session list, now and on each change.
    fn watch_list(&self, outbox: &Outbox) {
        let sessions = self.list();
        let mut watchers = lock(&self.watchers);
        reply(
            outbox,
            &DaemonMsg::Sessions {
                sessions: sessions.clone(),
            },
        );
        watchers.0.push(outbox.clone());
        watchers.1 = sessions;
    }

    /// Sends the watchers the session list, if it changed. Whatever changes
    /// a summary (a client coming or going, work beginning or ending, a
    /// session loading or unloading) wakes the supervisor, which calls
    /// this.
    fn publish_list(&self) {
        let mut watchers = lock(&self.watchers);
        if watchers.0.is_empty() {
            return;
        }
        let sessions = self.list();
        if sessions == watchers.1 {
            return;
        }
        if let Ok(frame) = Frame::new(&DaemonMsg::Sessions {
            sessions: sessions.clone(),
        }) {
            watchers.0.retain(|w| w.send(frame.clone()).is_ok());
        }
        watchers.1 = sessions;
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
        let mut pulsed = self.pulse.0.count();
        let mut jobs = agent::job::ENDED.count();
        loop {
            let now = Instant::now();
            event(|| rt::Event {
                name: "wake",
                session: None,
                detail: String::new(),
            });
            #[cfg(unix)]
            if let Some(log) = &exit.log
                && let Err(e) = sys::log::rotate_stderr(log)
            {
                eprintln!("rotating the log: {e}");
            }
            self.hibernate(config.hibernate);
            self.publish_list();
            if self.stopping.load(Ordering::SeqCst) {
                for session in self.live() {
                    session.stop();
                }
                exit.now("asked to stop");
            }
            let draining = life.draining();
            let busy = draining.is_some() || self.has_work();
            if busy != awake.is_some() {
                awake = busy.then(|| sys::Awake::hold("a: an agent is at work"));
            }
            let mut next = self.next_hibernation(config.hibernate, now);
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
            // Whatever woke us counts as activity, and idleness starts
            // again; a background job ending is only looked at.
            let mut sleep = next.map(|at| {
                self.shared
                    .rt
                    .sleep(at.saturating_duration_since(Instant::now()))
            });
            let mut pulse = pin!(self.pulse.0.changed(&mut pulsed));
            let mut ended = pin!(agent::job::ENDED.changed(&mut jobs));
            let woken = poll_fn(|cx| {
                if pulse.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(true);
                }
                let ended = ended.as_mut().poll(cx).is_ready();
                let slept = sleep
                    .as_mut()
                    .is_some_and(|s| s.as_mut().poll(cx).is_ready());
                if ended || slept {
                    Poll::Ready(false)
                } else {
                    Poll::Pending
                }
            });
            if woken.await {
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
    home: Home,
    /// The locked pid file; see [`claim`].
    pid: File,
    /// The log that is our stderr, to rotate.
    log: Option<PathBuf>,
}

impl Exit {
    /// Unlinks the socket first so new clients spawn a fresh daemon (one
    /// that raced into our backlog sees EOF and retries), then empties the
    /// record and clears the crash breaker (a clean exit is no crash, and
    /// ends any run of them) and releases the pid lock before
    /// the process closes its connections: a client reconnecting on EOF
    /// finds the lock free for the daemon it spawns.
    fn now(self, why: &str) -> ! {
        let _ = std::fs::remove_file(self.home.socket());
        if let Err(e) = sys::crash::exited(&self.home, &self.pid) {
            eprintln!("noting a clean exit: {e}");
        }
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

/// How a daemon was started.
#[derive(Debug, Default)]
pub struct Start {
    /// The daemon lock, taken by the client that started it; without one
    /// the daemon takes it.
    pub lock: Option<File>,
    /// Serve even after a crash loop, for `a daemon restart`.
    pub reset_breaker: bool,
    /// Its stderr is `logs/daemon.log`, which it then rotates.
    pub logs: bool,
}

/// Runs the daemon, as staged build `build`, until it has been idle for
/// `config.idle` (no attached clients and no unfinished work) or has drained
/// for a newer build. Sessions live in their logs, so the next client
/// starts a new daemon. After a crash loop it serves no one: it tells the
/// clients waiting why, and exits.
#[cfg(unix)]
pub fn serve(
    listener: &UnixListener,
    home: &Home,
    config: &Config,
    build: &str,
    start: Start,
) -> io::Result<()> {
    let pid = match claim(
        home,
        build,
        start.lock,
        start.reset_breaker,
        config.crash_limit,
    )? {
        Claim::Serve(pid) => pid,
        Claim::Tripped(why) => {
            refuse(listener, &why);
            let _ = std::fs::remove_file(home.socket());
            return Err(io::Error::other(why));
        }
    };
    let daemon = Daemon::load(home, config)?;
    let life = Arc::new(Lifecycle::new(build.to_owned(), home.runtime()));
    let (monitor, watch, config) = (Arc::clone(&daemon), Arc::clone(&life), config.clone());
    let exit = Exit {
        home: home.clone(),
        pid,
        log: start.logs.then(|| home.daemon_log()),
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

/// Whether a daemon may serve.
enum Claim {
    /// Yes, holding the locked pid file.
    Serve(File),
    /// No, for this reason: its predecessors died young too often.
    Tripped(String),
}

/// Keeps the daemon lock (`lock`, handed down by the client that started
/// us, or else taken here) for this process's lifetime and records us in it
/// ([`sys::crash::Record`]), noting first a previous daemon that died
/// holding it. While the lock is held the pid is guaranteed to be ours,
/// which is what lets clients kill a wedged daemon safely. Holding it, we
/// are the only one that counts crashes, so the breaker is ours to check:
/// after `limit` young deaths in a row, unless `reset`, we do not serve.
fn claim(
    home: &Home,
    build: &str,
    lock: Option<File>,
    reset: bool,
    limit: u32,
) -> io::Result<Claim> {
    let mut file = if let Some(file) = lock {
        file
    } else {
        let file = OpenOptions::new()
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
        file
    };
    if reset {
        sys::crash::reset(home)?;
    }
    if let Some(dead) = sys::crash::reap(home, &mut file)? {
        eprintln!(
            "the previous daemon (pid {}) stopped without shutting down",
            dead.pid
        );
    }
    let crashes = sys::crash::Crashes::of(home);
    if crashes.tripped(limit) {
        let report = crashes
            .last
            .map_or_else(String::new, |p| format!("; see {}", p.display()));
        return Ok(Claim::Tripped(format!(
            "the daemon crashed {} times in a row soon after starting{report}; \
             `a daemon restart` tries again",
            crashes.young
        )));
    }
    // Not synced: clients read it while we hold the lock, and after a crash
    // there is no lock to vouch for it.
    write!(file, "{}", sys::crash::Record::current(build))?;
    Ok(Claim::Serve(file))
}

/// Answers every client that connects, until none waits (or, if none
/// came, for [`REFUSED_GRACE`]), with `why` in place of a welcome: for a
/// daemon that must not serve.
#[cfg(unix)]
fn refuse(listener: &UnixListener, why: &str) {
    let Ok(frame) = Frame::new(&DaemonMsg::Error {
        message: why.to_owned(),
    }) else {
        return;
    };
    if listener.set_nonblocking(true).is_err() {
        return;
    }
    let deadline = Instant::now() + REFUSED_GRACE;
    let mut answered = false;
    loop {
        match listener.accept() {
            Ok((mut conn, _)) => {
                answered = true;
                let _ = conn.set_nonblocking(false);
                let _ = conn.set_read_timeout(Some(REFUSED_GRACE));
                let _ = conn.write_all(frame.as_bytes());
                // What it sent is read and dropped until it hangs up.
                let _ = io::copy(&mut conn, &mut io::sink());
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    && !answered
                    && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return,
        }
    }
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

/// The workspace a client in `dir` works in: the git working tree `dir` is
/// in, found with the client's git variables (`GIT_DIR`, ceilings), not the
/// daemon's, so every client, local or bridged, gets the same rule.
fn workspace_of(dir: &std::path::Path, env: Option<&Environment>) -> PathBuf {
    let var = |name: &str| env?.vars.get(name).map(Into::into);
    sys::workspace_in(dir, &sys::GitEnv::from_vars(var))
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
                } => match self
                    .daemon
                    .resolve(&target, || workspace_of(&workspace, env.as_ref()))
                {
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
                ClientMsg::Settings { change } => self.reply(&self.settings(change.as_ref())),
                ClientMsg::WatchSessions => self.daemon.watch_list(self.outbox),
                ClientMsg::Status => self.reply(&DaemonMsg::Status {
                    pid: std::process::id(),
                    build: self.life.build.clone(),
                    started: self.daemon.started,
                    clients: u32::try_from(self.daemon.clients.load(Ordering::SeqCst))
                        .unwrap_or(u32::MAX),
                    known: u32::try_from(lock(&self.daemon.slots).len()).unwrap_or(u32::MAX),
                }),
                ClientMsg::Shutdown if self.decl.role == Role::Viewer => {
                    self.error("this client is a viewer".into());
                }
                ClientMsg::Shutdown => {
                    self.daemon.stopping.store(true, Ordering::SeqCst);
                    self.daemon.pulse.wake();
                }
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

    /// The home settings, after `change` if this client may make it.
    fn settings(&self, change: Option<&SettingsChange>) -> DaemonMsg {
        let home = &self.daemon.shared.models.settings.home;
        let (file, refused) = match change {
            None => (home.get(), None),
            Some(_) if self.decl.role == Role::Viewer => {
                (home.get(), Some("this client is a viewer".to_owned()))
            }
            Some(change) => home
                .change(change)
                .map_or_else(|e| (home.get(), Some(e)), |file| (file, None)),
        };
        let (settings, error) = match &*file {
            Ok(file) => (config::shown(file), refused),
            Err(e) => (HomeSettings::default(), refused.or_else(|| Some(e.clone()))),
        };
        DaemonMsg::Settings { settings, error }
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
