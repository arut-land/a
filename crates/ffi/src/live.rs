//! The daemon connection: staging the bundled `a`, listing sessions, and a
//! session's attachment, folded through a [`Feed`]. The connection is
//! blocking I/O, so it runs on the `rt` runtime's blocking pool and the
//! exported async methods only await it.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use boltffi::{EventSubscription, export};
use client::{Connection, Endpoint, Reader, Writer};
use protocol::view::{Link, SessionList};
use protocol::{ClientMsg, Environment, SessionId, Target};
use rt::{Runtime, Threads};
use sys::{Home, Staged};

use crate::feed::{Feed, lock};
use crate::{
    ClientError, OpenTarget, Resync, SessionInfo, SessionSnapshot, ViewChange, ViewUpdate,
};

fn runtime() -> &'static Threads {
    static RUNTIME: OnceLock<Threads> = OnceLock::new();
    RUNTIME.get_or_init(Threads::default)
}

/// Runs blocking `work` off the caller's thread.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ClientError> + Send + 'static,
) -> Result<T, ClientError> {
    rt::unblock(runtime(), work).await
}

/// This build of `a`, staged in the per-user home, and the daemon it runs.
#[derive(Clone)]
struct Daemon {
    home: Home,
    staged: Staged,
}

impl Daemon {
    fn connect(&self) -> io::Result<Connection> {
        client::connect(&Endpoint::Local(self.home.clone()), &self.staged, &|| {})
    }

    fn attach(
        &self,
        target: Target,
        workspace: PathBuf,
        env: Option<Environment>,
        since: u64,
    ) -> io::Result<Connection> {
        let mut conn = self.connect()?;
        conn.writer.send(&ClientMsg::Attach {
            target,
            workspace,
            since,
            env,
        })?;
        Ok(conn)
    }
}

/// The per-user daemon, as the CLI reaches it: `$A_HOME`, else `~/.a`.
pub struct Client {
    daemon: Daemon,
}

#[export]
impl Client {
    /// Stages `helper`, the `a` this app bundles, the way every entry point
    /// does: a daemon this client starts runs the newest build staged.
    pub fn new(helper: &str) -> Result<Self, ClientError> {
        let home = Home::from_env()?;
        let staged = home.runtime().stage(Path::new(helper))?;
        Ok(Self {
            daemon: Daemon { home, staged },
        })
    }

    /// The staged build's id, `<version>+<hash>`.
    pub fn build(&self) -> String {
        self.daemon.staged.id.clone()
    }

    pub fn daemon_log(&self) -> String {
        self.daemon.home.daemon_log().display().to_string()
    }

    /// The daemon's sessions, starting it if needed.
    pub async fn sessions(&self) -> Result<Vec<SessionInfo>, ClientError> {
        let daemon = self.daemon.clone();
        blocking(move || {
            let Connection {
                mut reader,
                mut writer,
                ..
            } = daemon.connect()?;
            writer.send(&ClientMsg::List)?;
            let mut list = SessionList::default();
            reader.until(|msg| list.fold(&msg).then_some(()))?;
            Ok(list.sessions.iter().map(Into::into).collect())
        })
        .await
    }

    /// Replaces a daemon that stopped answering (`not_responding`).
    pub async fn restart_daemon(&self) -> Result<(), ClientError> {
        let daemon = self.daemon.clone();
        blocking(move || {
            client::restart(&daemon.home, &daemon.staged)?;
            Ok(())
        })
        .await
    }
}

struct Shared {
    feed: Mutex<Feed>,
    /// The open connection's write half; `None` before `open` and once it
    /// ends. Taken before `feed` when both are held.
    writer: Mutex<Option<Writer>>,
    /// A session attaches once: its reader owns the connection from then on.
    opened: AtomicBool,
}

/// One session as this client shows it: open it, then read `updates`.
pub struct Session {
    daemon: Daemon,
    shared: Arc<Shared>,
}

#[export]
impl Session {
    pub fn new(client: &Client) -> Self {
        Self {
            daemon: client.daemon.clone(),
            shared: Arc::new(Shared {
                feed: Mutex::new(Feed::new()),
                writer: Mutex::new(None),
                opened: AtomicBool::new(false),
            }),
        }
    }

    /// Every update from here on. Subscribe before `open` to see the
    /// attach; items dropped by a full buffer show as a `rev` gap, for
    /// `resync`.
    #[ffi_stream(item = ViewUpdate)]
    pub fn updates(&self) -> Arc<EventSubscription<ViewUpdate>> {
        lock(&self.shared.feed).subscribe()
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        lock(&self.shared.feed).snapshot()
    }

    /// What came after update `from`: the updates, or a snapshot when too
    /// many were missed.
    pub fn resync(&self, from: u64) -> Resync {
        lock(&self.shared.feed).resync(from)
    }

    /// Attaches to `target`. A new session works in the git root of its
    /// directory with this process's environment; an existing one keeps the
    /// environment its last client gave it. When the daemon drains for an
    /// upgrade the session reattaches on its own and replays what it
    /// missed; any other end of the connection is a `closed` update. A
    /// session opens once (until an attempt succeeds): make another to
    /// attach again.
    pub async fn open(&self, target: OpenTarget) -> Result<(), ClientError> {
        let (target, workspace, env) = match target {
            OpenTarget::New { workspace } => {
                let dir = PathBuf::from(workspace);
                let env = Environment::new(dir.clone(), std::env::vars_os(), false);
                (Target::New, sys::workspace_of(&dir), Some(env))
            }
            OpenTarget::Latest {
                workspace,
                any_workspace,
            } => (
                Target::Latest { any_workspace },
                sys::workspace_of(Path::new(&workspace)),
                None,
            ),
            OpenTarget::Id { id } => (Target::Id(parse_id(&id)?), PathBuf::new(), None),
        };
        let (daemon, shared) = (self.daemon.clone(), Arc::clone(&self.shared));
        blocking(move || {
            if shared.opened.swap(true, Ordering::AcqRel) {
                return Err(ClientError::InvalidArgument {
                    message: "the session was opened already".into(),
                });
            }
            let conn = daemon
                .attach(target, workspace, env, 0)
                .inspect_err(|_| shared.opened.store(false, Ordering::Release))?;
            *lock(&shared.writer) = Some(conn.writer);
            let reader = conn.reader;
            runtime().spawn_blocking(Box::new(move || read(&daemon, &shared, reader)));
            Ok(())
        })
        .await
    }

    /// Sends the user's input.
    pub async fn send(&self, text: String) -> Result<(), ClientError> {
        self.write(ClientMsg::Input { text }).await
    }

    /// Answers the ask for tool call `call`.
    pub async fn approve(&self, call: u64, approved: bool) -> Result<(), ClientError> {
        self.write(ClientMsg::Approve { call, approved }).await
    }

    /// Detaches; the session keeps running in the daemon.
    pub async fn detach(&self) -> Result<(), ClientError> {
        let shared = Arc::clone(&self.shared);
        blocking(move || {
            let writer = lock(&shared.writer).take();
            writer
                .ok_or(ClientError::NotOpen)?
                .send(&ClientMsg::Detach)?;
            Ok(())
        })
        .await
    }
}

impl Session {
    async fn write(&self, msg: ClientMsg) -> Result<(), ClientError> {
        let shared = Arc::clone(&self.shared);
        blocking(move || {
            let mut writer = lock(&shared.writer);
            writer.as_mut().ok_or(ClientError::NotOpen)?.send(&msg)?;
            Ok(())
        })
        .await
    }
}

/// Released by the app: the daemon is told, and the reader then ends.
impl Drop for Session {
    fn drop(&mut self) {
        if let Some(mut writer) = lock(&self.shared.writer).take() {
            let _ = writer.send(&ClientMsg::Detach);
        }
    }
}

fn parse_id(id: &str) -> Result<SessionId, ClientError> {
    id.parse::<uuid::Uuid>()
        .map(SessionId)
        .map_err(|_| ClientError::InvalidArgument {
            message: format!("not a session id: {id}"),
        })
}

/// Folds what the daemon sends until the connection ends, reattaching
/// across a drain.
fn read(daemon: &Daemon, shared: &Shared, mut reader: Reader) {
    loop {
        let ended = loop {
            match reader.recv() {
                Ok(Some(msg)) => {
                    lock(&shared.feed).fold(&msg);
                }
                Ok(None) => break "the daemon closed the connection".to_owned(),
                Err(e) => break e.to_string(),
            }
        };
        let mut writer = lock(&shared.writer);
        let (draining, since, workspace) = {
            let feed = lock(&shared.feed);
            let view = &feed.view;
            let draining = view.session.filter(|_| view.link == Link::Draining);
            (draining, view.seq, view.workspace.clone())
        };
        // Detached on purpose, or reconnected for the new build.
        let end = match (writer.is_some(), draining) {
            (false, _) => ViewChange::Detached,
            (true, Some(session)) => {
                match daemon.attach(Target::Id(session), workspace, None, since) {
                    Ok(conn) => {
                        *writer = Some(conn.writer);
                        reader = conn.reader;
                        continue;
                    }
                    Err(e) => ViewChange::Closed {
                        message: e.to_string(),
                    },
                }
            }
            (true, None) => ViewChange::Closed { message: ended },
        };
        *writer = None;
        let mut feed = lock(&shared.feed);
        feed.closed = true;
        feed.local(end);
        return;
    }
}
