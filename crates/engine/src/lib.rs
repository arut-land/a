//! The daemon: owns every session, accepts clients, runs the agents.

mod catalog;
pub mod config;
mod driver;
mod model;
mod outbox;
mod server;
mod store;
mod transcript;
mod upgrade;

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::catalog::Catalog;
use crate::store::{Header, Jsonl};
use driver::{Attachment, Live, Open, Request, Shared};
use ext::Flag;
use outbox::Outbox;
use protocol::{DaemonMsg, Environment, Frame, Mode, Outcome, SessionId, SessionSummary, Target};
use rt::oneshot;
use server::reply;
use sys::{Home, Listener};
use upgrade::Lifecycle;

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
    /// The agent sessions run, by name: overrides the config files'
    /// `agent`.
    pub agent: Option<String>,

    /// Young deaths in a row after which a daemon refuses to serve (0 for
    /// never); see [`sys::crash`].
    pub crash_limit: u32,
    /// `[ext]`: the extensions left out, and each one's own table.
    pub ext: sys::config::Exts,
    /// The operator's permission rules (`$A_HOME`'s `policy.toml`), or why
    /// they could not be read: a daemon whose floor is unreadable does not
    /// start.
    pub floor: Result<Vec<sys::config::Rule>, String>,
    /// The registry of the extensions this build links, assembled for this
    /// configuration. The engine itself offers the model nothing.
    pub extensions: fn(&Config) -> Result<ext::host::Registry, ext::Refused>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(600),
            hibernate: Duration::from_secs(300),
            drain: Duration::from_secs(60),
            agent: None,
            crash_limit: sys::crash::LIMIT,
            ext: sys::config::Exts::default(),
            floor: Ok(Vec::new()),
            #[cfg(not(test))]
            extensions: |_| Ok(ext::host::Registry::default()),
            #[cfg(test)]
            extensions: tests::configured,
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
        let floor = match std::fs::read_to_string(home.policy_file()) {
            Ok(text) => sys::config::rules(&text)
                .map_err(|e| format!("{}: {e}", home.policy_file().display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("{}: {e}", home.policy_file().display())),
        };
        Self {
            ext: file.ext,
            floor,
            ..Self::from(file.daemon)
        }
    }

    /// The extensions' registry; an agent configured by name must be one
    /// of theirs.
    fn registry(&self) -> Result<ext::host::Registry, ext::Refused> {
        let registry = (self.extensions)(self)?;
        if let Some(name) = self
            .agent
            .as_deref()
            .filter(|&n| !registry.agents().any(|a| a == n))
        {
            return Err(ext::Refused(format!("no agent named {name} is registered")));
        }
        for warning in registry.warnings() {
            eprintln!("warning: {warning}");
        }
        Ok(registry)
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
            agent: file.agent,
            crash_limit: file.crash_limit.map_or(defaults.crash_limit, |n| {
                u32::try_from(n).unwrap_or(u32::MAX)
            }),
            ext: defaults.ext,
            floor: defaults.floor,
            extensions: defaults.extensions,
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
    pulse: rt::Notify,
    /// Notified each time a process group a session owns ends.
    jobs: rt::Notify,
    /// The connections watching the session list, and the list as they
    /// were last sent it.
    watchers: Mutex<(Vec<Outbox>, Vec<SessionSummary>)>,
    /// A client asked the daemon to stop.
    stopping: AtomicBool,
    /// When it started, in seconds since the Unix epoch.
    started: u64,
}

/// A session the daemon knows about.
struct Slot {
    header: Header,
    residency: Mutex<Residency>,
}

/// Where a session's state is: only in its log, being loaded, live with an
/// actor, or stopping. A log is loaded once its last actor has stopped, so
/// it has one writer, and never on a connection's task.
enum Residency {
    /// Unloaded, costing nothing. `last` is how it lists: as the catalog
    /// had it, or as it was when it was unloaded.
    Hibernated {
        last: SessionSummary,
    },
    /// Loading on the blocking pool; whoever else wants it waits. It lists
    /// as it did before.
    Loading {
        waiters: Vec<oneshot::Sender<Loaded>>,
        last: SessionSummary,
    },
    Live(Arc<Live>),
    /// Unloaded, its actor not yet ended: it lists as the actor last left
    /// it, and loads again only once the actor has.
    Stopping(Arc<Live>),
}

type Loaded = Result<Arc<Live>, (io::ErrorKind, String)>;

impl Slot {
    fn new(header: Header, residency: Residency) -> Arc<Self> {
        Arc::new(Self {
            header,
            residency: Mutex::new(residency),
        })
    }

    fn live(&self) -> Option<Arc<Live>> {
        match &*lock(&self.residency) {
            Residency::Live(live) => Some(Arc::clone(live)),
            _ => None,
        }
    }

    /// Unloads and stops the session; `false` if it was not live.
    fn unload(&self) -> bool {
        let mut residency = lock(&self.residency);
        let Residency::Live(live) = &*residency else {
            return false;
        };
        live.stop();
        *residency = Residency::Stopping(Arc::clone(live));
        true
    }

    /// The actor still stopping, if any; one that has ended leaves the
    /// session hibernated, as it last listed, and nothing of it held. The
    /// actor's end makes the transition ([`Daemon::ended`]); whoever waits
    /// for it only reads it.
    fn ending(&self) -> Option<Flag> {
        let mut residency = lock(&self.residency);
        let Residency::Stopping(live) = &*residency else {
            return None;
        };
        if !live.ended.is_set() {
            return Some(live.ended.clone());
        }
        let last = hibernated(live.summary());
        *residency = Residency::Hibernated { last };
        None
    }

    /// How it lists now.
    fn summary(&self) -> SessionSummary {
        match &*lock(&self.residency) {
            Residency::Live(live) => live.summary(),
            Residency::Stopping(live) => hibernated(live.summary()),
            Residency::Hibernated { last } | Residency::Loading { last, .. } => last.clone(),
        }
    }

    /// Marks it loading, listed as it was; `false` if it is not unloaded.
    fn loading(&self) -> bool {
        let mut residency = lock(&self.residency);
        let last = match &*residency {
            Residency::Hibernated { last } => last.clone(),
            Residency::Stopping(live) => hibernated(live.summary()),
            Residency::Live(_) | Residency::Loading { .. } => return false,
        };
        *residency = Residency::Loading {
            waiters: Vec::new(),
            last,
        };
        true
    }
}

impl Daemon {
    /// Reads only the session catalog, opening no log. Sessions it marks
    /// busy were working when the last daemon stopped, so they load at
    /// once, together, to recover.
    fn load(home: &Home, config: &Config) -> io::Result<Owner> {
        let store = Arc::new(Jsonl::open(home.sessions_dir())?);
        let catalog = Catalog::open(home.catalog_file(), &home.sessions_dir(), &*store)?;
        let records = catalog.records();
        let busy: Vec<bool> = records.iter().map(|r| r.busy).collect();
        let slots: Vec<_> = (records.into_iter())
            .map(|record| {
                let last = record.summary();
                Slot::new(record.header, Residency::Hibernated { last })
            })
            .collect();
        let rt: rt::Rt = Arc::new(rt::Threads::default());
        let models = model::Models {
            settings: config::Settings::new(home.config_file(), config.agent.clone()),
            http: Arc::new(llm::Ureq::new(Arc::clone(&rt))),
        };
        let registry = ext::host::Current::new(config.registry().map_err(io::Error::other)?);
        let daemon = Arc::new_cyclic(|daemon| Self {
            shared: Arc::new(Shared {
                rt,
                store,
                catalog,
                registry,
                models,

                spill: home.spill_dir(),
                loaded: Mutex::default(),
                daemon: daemon.clone(),
            }),
            slots: Mutex::new(slots.clone()),
            clients: AtomicUsize::new(0),
            pulse: rt::Notify::new(),
            jobs: rt::Notify::new(),
            watchers: Mutex::default(),
            stopping: AtomicBool::new(false),
            started: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        });
        // Before any client is served; all are loading before any is
        // loaded, so a child that reports at once finds its parent; each
        // loads on the blocking pool, alongside the others.
        let busy: Vec<_> = (slots.into_iter().zip(busy))
            .filter_map(|(slot, busy)| (busy && slot.loading()).then_some(slot))
            .collect();
        let starting: Vec<_> = (busy.iter())
            .map(|slot| {
                let (shared, header) = (Arc::clone(&daemon.shared), slot.header.clone());
                let owner = daemon.owner(slot);
                rt::unblock(&*daemon.shared.rt, move || {
                    Live::start(&header, &shared, Open::Load, owner)
                })
            })
            .collect();
        rt::block_on(async {
            for (slot, started) in busy.iter().zip(starting) {
                if let Err(e) = daemon.started(slot, started.await) {
                    eprintln!(
                        "session {}: loading for recovery: {e}",
                        slot.header.id.short()
                    );
                }
            }
        });
        Ok(Owner(daemon))
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

    /// Whether session `id` exists.
    fn knows(&self, id: SessionId) -> bool {
        self.slot(id).is_some()
    }

    /// Who owns the session `slot` holds.
    fn owner(&self, slot: &Slot) -> kernel::Owner {
        match slot.header.parent {
            None => kernel::Owner::None,
            Some(parent) if self.knows(parent.session) => kernel::Owner::Present,
            Some(_) => kernel::Owner::Gone,
        }
    }

    /// The slot's live session, loaded first if it is not: off the async
    /// workers, once the actor that last had it has ended, and once however
    /// many ask meanwhile.
    async fn live(&self, slot: &Arc<Slot>) -> io::Result<Arc<Live>> {
        loop {
            if let Some(ended) = slot.ending() {
                ended.wait().await;
                continue;
            }
            let wait = {
                let mut residency = lock(&slot.residency);
                match &mut *residency {
                    Residency::Live(live) => return Ok(Arc::clone(live)),
                    Residency::Loading { waiters, .. } => {
                        let (tx, rx) = oneshot::channel();
                        waiters.push(tx);
                        Some(rx)
                    }
                    Residency::Stopping(_) | Residency::Hibernated { .. } => None,
                }
            };
            if wait.is_none() && !slot.loading() {
                continue;
            }
            return match wait {
                Some(loaded) => loaded
                    .await
                    .unwrap_or_else(|| Err((io::ErrorKind::Other, "loading stopped".into())))
                    .map_err(|(kind, e)| io::Error::new(kind, e)),
                None => self.start(slot, Open::Load).await,
            };
        }
    }

    /// Starts the session of `slot`, which is `Loading`, on the blocking
    /// pool, and hands it to whoever waits.
    async fn start(&self, slot: &Arc<Slot>, open: Open) -> io::Result<Arc<Live>> {
        let (shared, header) = (Arc::clone(&self.shared), slot.header.clone());
        let owner = self.owner(slot);
        let rt = Arc::clone(&self.shared.rt);
        let started = rt::unblock(&*rt, move || Live::start(&header, &shared, open, owner)).await;
        self.started(slot, started)
    }

    /// The session of `slot`, which was `Loading`, as `started`: handed to
    /// whoever waits.
    fn started(&self, slot: &Slot, started: io::Result<Arc<Live>>) -> io::Result<Arc<Live>> {
        let mut residency = lock(&slot.residency);
        let next = match &started {
            Ok(live) => Residency::Live(Arc::clone(live)),
            Err(_) => Residency::Hibernated {
                last: match &*residency {
                    Residency::Loading { last, .. } => last.clone(),
                    _ => fresh(&slot.header),
                },
            },
        };
        if let Residency::Loading { waiters, .. } = std::mem::replace(&mut *residency, next) {
            for waiter in waiters {
                waiter.send(match &started {
                    Ok(live) => Ok(Arc::clone(live)),
                    Err(e) => Err((e.kind(), e.to_string())),
                });
            }
        }
        drop(residency);
        if started.is_ok() {
            session_event("load", slot.header.id, String::new);
            self.pulse.notify();
        }
        started
    }

    /// The session `target` names, attached. `workspace` is only worked
    /// out for a target that needs it: an attach by id never looks.
    async fn resolve(
        &self,
        target: &Target,
        workspace: impl FnOnce() -> PathBuf,
    ) -> io::Result<Attachment> {
        let found = match target {
            Target::New => return self.create(workspace()).await,
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
        Ok(self.live(&slot).await?.open())
    }

    async fn create(&self, workspace: PathBuf) -> io::Result<Attachment> {
        let header = Header {
            id: SessionId::new(),
            workspace,
            parent: None,
        };
        let slot = self.new_slot(header);
        Ok(self.start(&slot, Open::Create).await?.open())
    }

    /// A slot for a session to create, `Loading` until it is.
    fn new_slot(&self, header: Header) -> Arc<Slot> {
        let waiters = Vec::new();
        let last = fresh(&header);
        let slot = Slot::new(header, Residency::Loading { waiters, last });
        lock(&self.slots).push(Arc::clone(&slot));
        slot
    }

    /// Starts child session `header.id` on `input`, in `env` (its parent's,
    /// secrets included), unless it runs already: made if missing, given
    /// `input` if its log has none, loaded if hibernated. Doing it again is
    /// harmless, which recovery relies on.
    async fn start_child(
        &self,
        header: Header,
        input: String,
        env: Option<Arc<Environment>>,
    ) -> io::Result<()> {
        let live = if let Some(slot) = self.slot(header.id) {
            self.live(&slot).await?
        } else {
            let slot = self.new_slot(header);
            self.start(&slot, Open::Create).await?
        };
        if !live.has_input() {
            if let Some(env) = env {
                live.set_env(env);
            }
            let input = Request::Input {
                text: input,
                mode: Mode::Queue,
            };
            live.request(input, None).await.map_err(io::Error::other)?;
        }
        Ok(())
    }

    /// Hands child `child`'s result to session `parent`, unless it is
    /// unloaded: a session with a watch armed has work, so it is loaded
    /// (at startup, marked busy in the catalog), and one with none has heard from
    /// every child it watched.
    async fn report(&self, parent: SessionId, child: SessionId, outcome: Outcome) {
        let Some(slot) = self.slot(parent) else {
            return;
        };
        let loading = matches!(
            &*lock(&slot.residency),
            Residency::Live(_) | Residency::Loading { .. }
        );
        if !loading {
            return;
        }
        match self.live(&slot).await {
            Ok(live) => live.settled(child, outcome),
            Err(e) => eprintln!("reporting to {}: {e}", parent.short()),
        }
    }

    /// Interrupts child session `child`'s turn, if it runs.
    fn interrupt(&self, child: SessionId) {
        if let Some(live) = self.slot(child).and_then(|s| s.live()) {
            live.interrupt();
        }
    }

    /// Session `id`'s actor ended: it is hibernated, as it last listed.
    fn ended(&self, id: SessionId) {
        if let Some(slot) = self.slot(id) {
            slot.ending();
        }
        self.pulse.notify();
    }

    /// Every session as it lists, newest first, from memory: no log is
    /// read.
    fn list(&self) -> Vec<SessionSummary> {
        let mut sessions: Vec<SessionSummary> = self.slots().iter().map(|s| s.summary()).collect();
        // Ids are UUIDv7: they order by creation.
        sessions.sort_by_key(|s| std::cmp::Reverse(s.id.0));
        sessions
    }

    /// Unloads sessions idle for `after`: no attachments, work or jobs.
    fn hibernate(&self, after: Duration) {
        for slot in self.slots() {
            let idle = slot.live().is_some_and(|s| s.idle_for() >= after);
            if idle && slot.unload() {
                session_event("hibernate", slot.header.id, String::new);
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
        self.loaded()
            .iter()
            .map(|s| s.idle_for())
            .filter(|idle| !idle.is_zero())
            .map(|idle| now + after.saturating_sub(idle))
            .min()
    }

    fn has_work(&self) -> bool {
        self.loaded().iter().any(|s| s.has_work())
    }

    fn loaded(&self) -> Vec<Arc<Live>> {
        self.slots().iter().filter_map(|slot| slot.live()).collect()
    }

    /// Stops every loaded session and waits, up to [`STOP_GRACE`], for
    /// their actors to end; one loaded meanwhile is stopped in turn.
    async fn shutdown(&self) {
        let mut grace = self.shared.rt.sleep(STOP_GRACE);
        loop {
            let ending: Vec<Flag> = self
                .slots()
                .iter()
                .filter_map(|slot| {
                    slot.unload();
                    slot.ending()
                })
                .collect();
            if ending.is_empty() {
                return;
            }
            let all = async {
                for ended in &ending {
                    ended.wait().await;
                }
                true
            };
            let ended = rt::or(all, async {
                grace.as_mut().await;
                false
            });
            if !ended.await {
                eprintln!("{} sessions still stopping, exiting", ending.len());
                return;
            }
        }
    }

    /// No connections, and no loaded session has work or background
    /// processes.
    fn idle(&self) -> bool {
        self.clients.load(Ordering::SeqCst) == 0
            && !self.loaded().iter().any(|s| s.has_work() || s.has_jobs())
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
        let mut pulsed = self.pulse.count();
        let mut jobs = self.jobs.count();
        loop {
            let now = Instant::now();
            event(|| rt::Event {
                name: "wake",
                session: None,
                detail: String::new(),
            });
            if let Some(log) = &exit.log
                && let Err(e) = sys::log::rotate_stderr(log)
            {
                eprintln!("rotating the log: {e}");
            }
            self.hibernate(config.hibernate);
            self.publish_list();
            if self.stopping.load(Ordering::SeqCst) {
                exit.now(self, "asked to stop").await;
            }
            let draining = life.draining();
            let busy = draining.is_some() || self.has_work();
            if busy != awake.is_some() {
                awake = busy.then(|| sys::Awake::hold("a: an agent is at work"));
            }
            let mut next = self.next_hibernation(config.hibernate, now);
            if let Some(began) = draining {
                if now >= began + config.drain && stopped.is_none() {
                    for session in self.loaded() {
                        session.interrupt();
                    }
                    stopped = Some(now);
                }
                // A stopped call gets the time its processes have to go
                // and its outcome to commit.
                let settled = stopped.map(|at| at + KILL_GRACE * 2);
                if !self.has_work() || settled.is_some_and(|at| now >= at) {
                    exit.now(self, "drained for a newer build").await;
                }
                next = Some(earliest(next, settled.unwrap_or(began + config.drain)));
            } else if self.idle() {
                let at = *since.get_or_insert(now) + config.idle;
                if now >= at {
                    exit.now(self, &format!("idle for {}s", config.idle.as_secs()))
                        .await;
                }
                next = Some(earliest(next, at));
            } else {
                since = None;
            }
            // Whatever woke us counts as activity, and idleness starts
            // again; a background job ending is only looked at.
            let sleep = async {
                match next {
                    Some(at) => {
                        let until = at.saturating_duration_since(Instant::now());
                        self.shared.rt.sleep(until).await;
                    }
                    None => std::future::pending().await,
                }
                false
            };
            let pulse = async {
                self.pulse.changed(&mut pulsed).await;
                true
            };
            let ended = async {
                self.jobs.changed(&mut jobs).await;
                false
            };
            let woken = rt::or(pulse, rt::or(ended, sleep));
            if woken.await {
                since = None;
            }
        }
    }
}

/// How a session unloaded lists: as it last was, with no clients.
/// How a session just made lists.
fn fresh(header: &Header) -> SessionSummary {
    catalog::Record::new(header.clone()).summary()
}

fn hibernated(summary: SessionSummary) -> SessionSummary {
    SessionSummary {
        clients: 0,
        residency: protocol::Residency::Hibernated,
        ..summary
    }
}

fn earliest(a: Option<Instant>, b: Instant) -> Instant {
    a.map_or(b, |a| a.min(b))
}

/// How long stopping the daemon waits for its sessions' actors to end:
/// time for a stopped call's processes to go and its outcome to commit.
const STOP_GRACE: Duration = KILL_GRACE.saturating_mul(2);

/// How long a stopped call's processes have between SIGTERM and SIGKILL,
/// as the shell gives them.
const KILL_GRACE: Duration = Duration::from_secs(3);

/// The daemon, as its owner holds it: dropping it stops every session and
/// waits for their actors, so no writer outlives it. Every other holder
/// (the supervisor, connections, sessions) has a plain `Arc`. Dropped only
/// where blocking is fine: [`serve`]'s thread, or a test's.
struct Owner(Arc<Daemon>);

impl Deref for Owner {
    type Target = Arc<Daemon>;

    fn deref(&self) -> &Arc<Daemon> {
        &self.0
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        rt::block_on(self.0.shutdown());
    }
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
    /// that raced into our backlog sees EOF and retries), then stops
    /// `daemon`'s sessions, so the next daemon, which waits for the pid
    /// lock, never loads a log one of ours still writes. Then it empties the
    /// record and clears the crash breaker (a clean exit is no crash, and
    /// ends any run of them) and releases the pid lock before the process
    /// closes its connections: a client reconnecting on EOF finds the lock
    /// free for the daemon it spawns.
    async fn now(self, daemon: &Daemon, why: &str) -> ! {
        self.home.endpoint().remove();
        daemon.shutdown().await;
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
pub fn serve(
    listener: &Listener,
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
            // The reactor of the runtime the daemon would serve on.
            let reactor = rt::Runtime::reactor(&rt::Threads::default());
            if let Ok(listener) = rt::Async::new(&reactor, listener) {
                rt::block_on(server::refuse(&reactor, &listener, &why));
            }
            home.endpoint().remove();
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
    // Connections are tasks waiting on the reactor, and reach the
    // sessions' actors through their mailboxes.
    let listener = rt::Async::new(&daemon.shared.rt.reactor(), listener)?;
    rt::block_on(server::accept(&listener, &daemon, &life))
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
    if crashes.breaker.tripped(limit) {
        let report = crashes
            .last
            .map_or_else(String::new, |p| format!("; see {}", p.display()));
        return Ok(Claim::Tripped(format!(
            "the daemon crashed {} times in a row soon after starting{report}; \
             `a daemon restart` tries again",
            crashes.breaker.young()
        )));
    }
    // Not synced: clients read it while we hold the lock, and after a crash
    // there is no lock to vouch for it.
    write!(file, "{}", sys::crash::Record::current(build))?;
    Ok(Claim::Serve(file))
}

#[cfg(test)]
mod tests;

#[cfg(all(test, unix))]
mod conn_tests;
