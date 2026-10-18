//! The daemon: owns every session, accepts clients, runs the agents.

mod model;
mod session;
mod upgrade;

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use protocol::{
    ClientMsg, DaemonMsg, PROTO, SessionId, SessionSummary, Target, read_frame, write_frame,
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
    /// `A_AGENT`: `echo` needs no model; anything else chats.
    pub agent: agent::Kind,
    /// `A_MODEL`: `host:model`, or a bare model on `OpenAI`.
    pub model: String,
    /// `A_ASK`: comma-separated tools whose calls wait for the user's
    /// approval.
    pub ask: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(600),
            hibernate: Duration::from_secs(300),
            drain: Duration::from_secs(60),
            agent: agent::Kind::Chat,
            model: "gpt-6-luna".to_owned(),
            ask: Vec::new(),
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
            agent: var("A_AGENT").unwrap_or(defaults.agent),
            model: var("A_MODEL").unwrap_or(defaults.model),
            ask: std::env::var("A_ASK").map_or(defaults.ask, |tools| {
                tools
                    .split(',')
                    .map(str::trim)
                    .filter(|tool| !tool.is_empty())
                    .map(str::to_owned)
                    .collect()
            }),
        }
    }

    /// The built-in extensions, registered as any other.
    fn registry(&self) -> agent::Registry {
        agent::Registry::default()
            .with(&agent::Builtin)
            .with(&agent::AskFor(self.ask.clone()))
    }
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
}

/// A session the daemon knows about. Its log is loaded, and its actor runs,
/// only while `live` is set; otherwise it is hibernated and costs nothing.
struct Slot {
    header: Header,
    live: Mutex<Option<Arc<Session>>>,
}

impl Daemon {
    /// Reads only session headers. Sessions with a busy marker were working
    /// when the last daemon stopped, so they load at once to recover.
    fn load(home: &Home, config: &Config) -> io::Result<Arc<Self>> {
        let store = Arc::new(Jsonl::open(home.sessions_dir())?);
        let slots = store
            .headers()?
            .into_iter()
            .map(|header| {
                Arc::new(Slot {
                    header,
                    live: Mutex::new(None),
                })
            })
            .collect();
        let rt: rt::Rt = Arc::new(rt::Threads::default());
        let model = model::Model::from_spec(&config.model, Arc::clone(&rt));
        let daemon = Arc::new_cyclic(|daemon| Self {
            shared: Arc::new(Shared {
                rt,
                store,
                agent: config.agent,
                registry: config.registry(),
                model,
                daemon: daemon.clone(),
            }),
            slots: Mutex::new(slots),
            clients: AtomicUsize::new(0),
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
        let slots = self.slots();
        let found = match target {
            Target::New => return self.create(workspace),
            Target::Id(id) => slots.iter().find(|s| s.header.id == *id),
            // A child is reached through its parent, not resumed as latest.
            Target::Latest { any_workspace } => slots.iter().rev().find(|s| {
                s.header.parent.is_none() && (*any_workspace || s.header.workspace == workspace)
            }),
        };
        let slot =
            found.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no matching session"))?;
        self.with_live(slot, Session::open)
    }

    fn create(&self, workspace: PathBuf) -> io::Result<Attachment> {
        let header = Header {
            id: SessionId::new(),
            workspace,
            parent: None,
        };
        let loaded = self.shared.store.create(&header)?;
        let session = Session::start(header.clone(), loaded, Arc::clone(&self.shared));
        let attachment = session.open();
        lock(&self.slots).push(Arc::new(Slot {
            header,
            live: Mutex::new(Some(session)),
        }));
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
                session.stop();
            }
        }
    }

    fn has_work(&self) -> bool {
        self.slots()
            .iter()
            .any(|slot| lock(&slot.live).as_ref().is_some_and(|s| s.has_work()))
    }

    /// No connections, and no loaded session has work.
    fn idle(&self) -> bool {
        self.clients.load(Ordering::SeqCst) == 0 && !self.has_work()
    }

    /// Hibernates idle sessions, and exits once the whole daemon has been
    /// idle for `config.idle`, or once a drain has no work left or runs out
    /// of grace. Every entry is durable when written, so exiting is all a
    /// drain needs; unfinished calls recover in the next daemon.
    async fn supervise(&self, life: &Lifecycle, config: &Config, socket: &Path, pid: File) {
        let mut since: Option<Instant> = None;
        loop {
            self.shared.rt.sleep(Duration::from_millis(250)).await;
            self.hibernate(config.hibernate);
            if let Some(began) = life.draining() {
                if !self.has_work() || began.elapsed() >= config.drain {
                    exit(socket, pid, "drained for a newer build");
                }
                continue;
            }
            if !self.idle() {
                since = None;
                continue;
            }
            if since.get_or_insert_with(Instant::now).elapsed() >= config.idle {
                exit(socket, pid, &format!("idle for {}s", config.idle.as_secs()));
            }
        }
    }
}

/// Unlinks the socket first so new clients spawn a fresh daemon (one that
/// raced into our backlog sees EOF and retries), then releases the pid lock
/// before the process closes its connections: a client reconnecting on EOF
/// finds the lock free for the daemon it spawns.
fn exit(socket: &Path, pid: File, why: &str) -> ! {
    let _ = std::fs::remove_file(socket);
    drop(pid);
    eprintln!("{why}, exiting");
    std::process::exit(0)
}

/// Counts one open connection for as long as it lives.
struct Client<'a>(&'a AtomicUsize);

impl<'a> Client<'a> {
    fn new(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for Client<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Runs the daemon, as staged build `build`, until it has been idle for
/// `config.idle` (no attached clients and no unfinished work) or has drained
/// for a newer build. Sessions live in their logs, so the next client simply
/// starts a new daemon.
#[cfg(unix)]
pub fn serve(listener: &UnixListener, home: &Home, config: &Config, build: &str) -> io::Result<()> {
    let pid = claim(home)?;
    let daemon = Daemon::load(home, config)?;
    let life = Arc::new(Lifecycle::new(build.to_owned(), home.runtime()));
    let (monitor, watch, config, socket) = (
        Arc::clone(&daemon),
        Arc::clone(&life),
        config.clone(),
        home.socket(),
    );
    daemon.shared.rt.spawn(Box::pin(async move {
        monitor.supervise(&watch, &config, &socket, pid).await;
    }));
    // Connections block on their sockets, so each gets a thread of its own
    // and reaches the sessions' tasks through their mailboxes.
    for conn in listener.incoming() {
        let conn = conn?;
        let (daemon, life) = (Arc::clone(&daemon), Arc::clone(&life));
        thread::spawn(move || {
            let _client = Client::new(&daemon.clients);
            if let Err(e) = handle(&daemon, &life, conn) {
                eprintln!("connection: {e}");
            }
        });
    }
    Ok(())
}

/// Takes the daemon lock for this process's lifetime and records our pid in
/// it. While the lock is held the pid is guaranteed to be ours, which is what
/// lets clients kill a wedged daemon safely.
fn claim(home: &Home) -> io::Result<File> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.daemon_pid_file())?;
    if file.try_lock().is_err() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "another daemon is running",
        ));
    }
    file.set_len(0)?;
    write!(file, "{}", std::process::id())?;
    file.sync_data()?;
    Ok(file)
}

#[cfg(unix)]
fn handle(daemon: &Daemon, life: &Lifecycle, conn: UnixStream) -> io::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let (tx, rx) = mpsc::channel::<DaemonMsg>();
    let mut writer = conn;
    thread::spawn(move || {
        for msg in rx {
            if write_frame(&mut writer, &msg).is_err() {
                break;
            }
        }
    });
    // The writer thread ends with the connection; a lost reply needs no
    // handling here.
    let reply = |msg| drop(tx.send(msg));
    let error = |message: String| reply(DaemonMsg::Error { message });

    let verdict = match read_frame(&mut reader)? {
        Some(ClientMsg::Hello { proto, build }) => life.verdict(proto, &build),
        _ => Verdict::Mismatch,
    };
    match verdict {
        Verdict::Welcome => reply(DaemonMsg::Welcome {
            proto: PROTO,
            build: life.build.clone(),
        }),
        Verdict::Mismatch => {
            reply(DaemonMsg::Mismatch { supported: PROTO });
            return Ok(());
        }
        Verdict::Drain => {
            life.drain();
            reply(DaemonMsg::Draining);
            // Held open until we exit: the client's cue to reconnect.
            while read_frame::<ClientMsg>(&mut reader)?.is_some() {}
            return Ok(());
        }
    }
    let _registered = life.register(tx.clone());

    let set_env = |s: &Attachment, env| {
        if let Err(e) = s.session().set_env(env) {
            error(format!("recording the environment: {e}"));
        }
    };
    let mut attached: Option<Attachment> = None;
    while let Some(msg) = read_frame(&mut reader)? {
        match msg {
            ClientMsg::Attach {
                target,
                workspace,
                since,
                env,
            } => match daemon.resolve(&target, workspace) {
                Ok(s) => {
                    // Before the replay, so a client that saw itself
                    // attached knows its environment is recorded.
                    if let Some(env) = env {
                        set_env(&s, env);
                    }
                    s.session().attach(&tx, since);
                    attached = Some(s);
                }
                Err(e) => error(e.to_string()),
            },
            ClientMsg::Environment(env) => match &attached {
                Some(s) => set_env(s, env),
                None => error("not attached".into()),
            },
            ClientMsg::Input { .. } | ClientMsg::Approve { .. } if life.draining().is_some() => {
                error(
                    "the daemon is restarting for an upgrade; send that again in a moment".into(),
                );
            }
            ClientMsg::Input { text } => match &attached {
                Some(s) => s.session().submit(text),
                None => error("not attached".into()),
            },
            ClientMsg::Approve { call, approved } => match &attached {
                Some(s) => {
                    if let Err(message) = s.session().answer(call, approved) {
                        error(message);
                    }
                }
                None => error("not attached".into()),
            },
            ClientMsg::List => reply(DaemonMsg::Sessions {
                sessions: daemon.list(),
            }),
            ClientMsg::Detach => break,
            ClientMsg::Hello { .. } => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
