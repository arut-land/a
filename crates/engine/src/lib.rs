//! The daemon: owns every session, accepts clients, runs the agents.

mod model;
mod session;

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use agent::Toolset;
use protocol::{
    BUILD, ClientMsg, DaemonMsg, PROTO, SessionId, SessionSummary, Target, read_frame, write_frame,
};
use session::{AgentKind, Attachment, Runtime, Session};
use store::{Header, Loaded, Store};
use sys::Home;

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

struct Daemon {
    store: Arc<Store>,
    rt: Arc<Runtime>,
    slots: Mutex<Vec<Arc<Slot>>>,
}

/// A session the daemon knows about. Its log is loaded, and its actor runs,
/// only while `live` is set; otherwise it is hibernated and costs nothing.
struct Slot {
    id: SessionId,
    workspace: PathBuf,
    live: Mutex<Option<Arc<Session>>>,
}

impl Daemon {
    /// Reads only session headers. Sessions with a busy marker were working
    /// when the last daemon stopped, so they load at once to recover.
    fn load(home: &Home) -> io::Result<Self> {
        let store = Arc::new(Store::open(home.sessions_dir())?);
        let rt = Arc::new(runtime_from_env());
        let slots = store
            .headers()?
            .into_iter()
            .map(|h| {
                Arc::new(Slot {
                    id: h.id,
                    workspace: h.workspace,
                    live: Mutex::new(None),
                })
            })
            .collect();
        let daemon = Self {
            store,
            rt,
            slots: Mutex::new(slots),
        };
        let busy: Vec<_> = daemon
            .slots()
            .into_iter()
            .filter(|s| daemon.store.is_busy(s.id))
            .collect();
        for slot in busy {
            if let Err(e) = daemon.with_live(&slot, |_| ()) {
                eprintln!("session {}: loading for recovery: {e}", slot.id.short());
            }
        }
        Ok(daemon)
    }

    fn slots(&self) -> Vec<Arc<Slot>> {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Runs `f` on the slot's live session, loading it first if hibernated.
    fn with_live<T>(&self, slot: &Slot, f: impl FnOnce(&Arc<Session>) -> T) -> io::Result<T> {
        let mut live = slot.live.lock().unwrap_or_else(PoisonError::into_inner);
        if live.is_none() {
            let loaded = self.store.load(slot.id)?;
            *live = Some(Session::start(
                slot.id,
                slot.workspace.clone(),
                loaded,
                self.rt.clone(),
                self.store.clone(),
            ));
        }
        Ok(f(live.as_ref().expect("loaded above")))
    }

    fn resolve(&self, target: &Target, workspace: PathBuf) -> io::Result<Attachment> {
        let slots = self.slots();
        let found = match target {
            Target::New => None,
            Target::Id(id) => slots.iter().find(|s| s.id == *id),
            Target::Latest { any_workspace } => slots
                .iter()
                .rev()
                .find(|s| *any_workspace || s.workspace == workspace),
        };
        if let Some(slot) = found {
            return self.with_live(slot, Session::open);
        }
        if !matches!(target, Target::New) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no matching session",
            ));
        }
        let header = Header {
            id: SessionId::new(),
            workspace,
        };
        let log = self.store.create(&header)?;
        let session = Session::start(
            header.id,
            header.workspace.clone(),
            Loaded {
                entries: Vec::new(),
                log,
            },
            self.rt.clone(),
            self.store.clone(),
        );
        let attachment = session.open();
        let slot = Slot {
            id: header.id,
            workspace: header.workspace,
            live: Mutex::new(Some(session)),
        };
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::new(slot));
        Ok(attachment)
    }

    fn list(&self) -> Vec<SessionSummary> {
        self.slots()
            .iter()
            .map(
                |slot| match &*slot.live.lock().unwrap_or_else(PoisonError::into_inner) {
                    Some(session) => session.summary(),
                    None => SessionSummary {
                        id: slot.id,
                        workspace: slot.workspace.clone(),
                        entries: self.store.count(slot.id).unwrap_or(0),
                        clients: 0,
                    },
                },
            )
            .collect()
    }

    /// Unloads sessions idle for `after`: no attachments, no work. Checked
    /// under the slot lock, which `resolve` also holds while attaching.
    fn hibernate(&self, after: Duration) {
        for slot in self.slots() {
            let mut live = slot.live.lock().unwrap_or_else(PoisonError::into_inner);
            if live.as_ref().is_some_and(|s| s.idle_for() >= after)
                && let Some(session) = live.take()
            {
                session.stop();
            }
        }
    }
}

/// `A_AGENT=echo` selects the echo agent (no model needed); `A_MODEL` picks
/// the model, `gpt-6-luna` by default.
/// The model a session calls when `A_MODEL` names none.
const DEFAULT_MODEL: &str = "gpt-6-luna";

fn runtime_from_env() -> Runtime {
    let agent = match std::env::var("A_AGENT").as_deref() {
        Ok("echo") => AgentKind::Echo,
        _ => AgentKind::Chat,
    };
    let spec = std::env::var("A_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_owned());
    // One worker: model calls block their session's actor, so the runtime
    // only moves response bodies.
    let model = model::Model::from_spec(&spec, Arc::new(rt::Threads::new(1)));
    Runtime {
        agent,
        tools: Toolset::builtin(),
        model,
        pins: session::Pins::default(),
    }
}

/// Runs the daemon until it has been idle for `idle`: no attached clients
/// and no unfinished work. Sessions live in their logs, so the next client
/// simply starts a new daemon.
#[cfg(unix)]
pub fn serve(
    listener: &UnixListener,
    home: &Home,
    idle: Duration,
    hibernate: Duration,
) -> io::Result<()> {
    let _writer = claim(home)?;
    let daemon = Arc::new(Daemon::load(home)?);
    let monitor = daemon.clone();
    let socket = home.socket();
    thread::spawn(move || monitor.supervise(idle, hibernate, &socket));
    for conn in listener.incoming() {
        let conn = conn?;
        let daemon = daemon.clone();
        thread::spawn(move || {
            let _client = daemon.rt.pins.client();
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

impl Daemon {
    fn idle(&self) -> bool {
        let busy = self.slots().iter().any(|slot| {
            let live = slot.live.lock().unwrap_or_else(PoisonError::into_inner);
            live.as_ref().is_some_and(|s| s.has_work())
        });
        self.rt.pins.idle() && !busy
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
            let since = *since.get_or_insert_with(Instant::now);
            if since.elapsed() >= idle {
                // Unlink first so new clients spawn a fresh daemon; one that
                // raced into our backlog sees EOF and retries.
                let _ = std::fs::remove_file(socket);
                eprintln!("idle for {}s, exiting", idle.as_secs());
                std::process::exit(0);
            }
        }
    }
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

    match read_frame(&mut reader)? {
        Some(ClientMsg::Hello { proto, .. }) if proto == PROTO => {
            let _ = tx.send(DaemonMsg::Welcome {
                proto: PROTO,
                build: BUILD.to_owned(),
            });
        }
        _ => {
            let _ = tx.send(DaemonMsg::Mismatch { supported: PROTO });
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
                Err(e) => {
                    let _ = tx.send(DaemonMsg::Error {
                        message: e.to_string(),
                    });
                }
            },
            ClientMsg::Input { text } => match &attached {
                Some(s) => s.session().submit(text),
                None => {
                    let _ = tx.send(DaemonMsg::Error {
                        message: "not attached".into(),
                    });
                }
            },
            ClientMsg::List => {
                let _ = tx.send(DaemonMsg::Sessions {
                    sessions: daemon.list(),
                });
            }
            ClientMsg::Detach => break,
            ClientMsg::Hello { .. } => {}
        }
    }
    Ok(())
}
