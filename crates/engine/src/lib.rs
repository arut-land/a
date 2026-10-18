//! The daemon: owns every session, accepts clients, runs the agents.

mod model;
mod session;

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use protocol::{
    BUILD, ClientMsg, DaemonMsg, PROTO, SessionId, SessionSummary, Target, read_frame, write_frame,
};
use session::{Attachment, Session, Shared};
use store::{Header, Store};
use sys::Home;

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

/// How the daemon runs, from `A_*` environment variables.
#[derive(Clone, Debug)]
pub struct Config {
    /// `A_IDLE_SECS`: exit after this long with no clients and no work.
    pub idle: Duration,
    /// `A_HIBERNATE_SECS`: unload a session after this long unused.
    pub hibernate: Duration,
    /// `A_AGENT`: `echo` needs no model; anything else chats.
    pub agent: agent::Kind,
    /// `A_MODEL`: `host:model`, or a bare model on `OpenAI`.
    pub model: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(600),
            hibernate: Duration::from_secs(300),
            agent: agent::Kind::Chat,
            model: "gpt-6-luna".to_owned(),
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
            agent: var("A_AGENT").unwrap_or(defaults.agent),
            model: var("A_MODEL").unwrap_or(defaults.model),
        }
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
    fn load(home: &Home, config: &Config) -> io::Result<Self> {
        let store = Store::open(home.sessions_dir())?;
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
        // One worker: model calls block their session's actor, so the runtime
        // only moves response bodies.
        let model = model::Model::from_spec(&config.model, Arc::new(rt::Threads::new(1)));
        let daemon = Self {
            shared: Arc::new(Shared {
                store,
                agent: config.agent,
                tools: agent::Toolset::default(),
                model,
            }),
            slots: Mutex::new(slots),
            clients: AtomicUsize::new(0),
        };
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
            Target::Latest { any_workspace } => slots
                .iter()
                .rev()
                .find(|s| *any_workspace || s.header.workspace == workspace),
        };
        let slot =
            found.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no matching session"))?;
        self.with_live(slot, Session::open)
    }

    fn create(&self, workspace: PathBuf) -> io::Result<Attachment> {
        let header = Header {
            id: SessionId::new(),
            workspace,
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

    /// No connections, and no loaded session has work.
    fn idle(&self) -> bool {
        self.clients.load(Ordering::SeqCst) == 0
            && !self
                .slots()
                .iter()
                .any(|slot| lock(&slot.live).as_ref().is_some_and(|s| s.has_work()))
    }

    /// Hibernates idle sessions, and exits once the whole daemon has been
    /// idle for `idle`.
    fn supervise(&self, idle: Duration, hibernate: Duration, socket: &Path) {
        let mut since: Option<Instant> = None;
        loop {
            thread::sleep(Duration::from_millis(250));
            self.hibernate(hibernate);
            if !self.idle() {
                since = None;
                continue;
            }
            if since.get_or_insert_with(Instant::now).elapsed() >= idle {
                // Unlink first so new clients spawn a fresh daemon; one that
                // raced into our backlog sees EOF and retries.
                let _ = std::fs::remove_file(socket);
                eprintln!("idle for {}s, exiting", idle.as_secs());
                std::process::exit(0);
            }
        }
    }
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

/// Runs the daemon until it has been idle for `config.idle`: no attached
/// clients and no unfinished work. Sessions live in their logs, so the next
/// client simply starts a new daemon.
#[cfg(unix)]
pub fn serve(listener: &UnixListener, home: &Home, config: &Config) -> io::Result<()> {
    let _writer = claim(home)?;
    let daemon = Arc::new(Daemon::load(home, config)?);
    let monitor = Arc::clone(&daemon);
    let (idle, hibernate, socket) = (config.idle, config.hibernate, home.socket());
    thread::spawn(move || monitor.supervise(idle, hibernate, &socket));
    for conn in listener.incoming() {
        let conn = conn?;
        let daemon = Arc::clone(&daemon);
        thread::spawn(move || {
            let _client = Client::new(&daemon.clients);
            if let Err(e) = handle(&daemon, conn) {
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
fn handle(daemon: &Daemon, conn: UnixStream) -> io::Result<()> {
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

    match read_frame(&mut reader)? {
        Some(ClientMsg::Hello { proto, .. }) if proto == PROTO => reply(DaemonMsg::Welcome {
            proto: PROTO,
            build: BUILD.to_owned(),
        }),
        _ => {
            reply(DaemonMsg::Mismatch { supported: PROTO });
            return Ok(());
        }
    }

    let mut attached: Option<Attachment> = None;
    while let Some(msg) = read_frame(&mut reader)? {
        match msg {
            ClientMsg::Attach {
                target,
                workspace,
                since,
            } => match daemon.resolve(&target, workspace) {
                Ok(s) => {
                    s.session().attach(&tx, since);
                    attached = Some(s);
                }
                Err(e) => error(e.to_string()),
            },
            ClientMsg::Input { text } => match &attached {
                Some(s) => s.session().submit(text),
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
