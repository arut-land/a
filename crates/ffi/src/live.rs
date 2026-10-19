//! The daemon connection: staging the bundled `a`, listing sessions, and a
//! session's attachment, folded through a [`Feed`]. The connection is
//! blocking I/O, so it runs on the `rt` runtime's blocking pool and the
//! exported async methods only await it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use boltffi::{EventSubscription, export};
use client::{Connection, Endpoint, Reader, Writer};
use protocol::view::Link;
use protocol::{ClientDecl, ClientMsg, Environment, SessionId, SettingsChange, Target};
use rt::{Runtime, Threads};
use sys::install::CliLink;
use sys::{Home, Staged};

use crate::feed::{Feed, lock};
use crate::{
    CliStatus, ClientError, EnvVar, OpenTarget, Resync, SessionInfo, SessionSnapshot, Settings,
    ViewChange, ViewUpdate,
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

/// The environment a new session's commands run with: this process's, and
/// over it what the user's login shell exports. An app opened from Finder
/// or the Dock has launchd's environment, whose `PATH` lacks everything the
/// user installed, so their shell is asked once (with a timeout; on failure
/// the process environment stands). Started from a terminal (`TERM` is
/// set), the process has the shell's environment already, and tests stay
/// hermetic.
fn session_vars() -> Vec<(OsString, OsString)> {
    static LOGIN: OnceLock<Vec<(OsString, OsString)>> = OnceLock::new();
    let login = LOGIN.get_or_init(|| {
        if std::env::var_os("TERM").is_some() {
            return Vec::new();
        }
        let shell = std::env::var_os("SHELL")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/zsh".into());
        sys::login_env(&shell, Duration::from_secs(5)).unwrap_or_else(|e| {
            eprintln!("a: the login shell's environment: {e}");
            Vec::new()
        })
    });
    let mut vars: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    vars.extend(login.iter().cloned());
    vars.into_iter().collect()
}

/// The `PATH` a session's commands get.
fn session_path() -> OsString {
    session_vars()
        .into_iter()
        .find_map(|(k, v)| (k == "PATH").then_some(v))
        .unwrap_or_default()
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
        let attach = ClientMsg::Attach {
            target,
            workspace,
            since,
            env,
            decl: ClientDecl::default(),
        };
        client::connect_with(
            &Endpoint::Local(self.home.clone()),
            &self.staged,
            &|| {},
            &|_| Ok(vec![attach.clone()]),
        )
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
            let sessions = daemon.connect()?.sessions()?;
            Ok(sessions.iter().map(Into::into).collect())
        })
        .await
    }

    /// The pid of the running daemon, if one is.
    pub fn daemon_pid(&self) -> Option<u32> {
        client::diagnose(&self.daemon.home).pid
    }

    /// `$A_HOME/config.toml`, where the settings are kept.
    pub fn config_file(&self) -> String {
        self.daemon.home.config_file().display().to_string()
    }

    /// The model and provider variables sessions fall back on, as the
    /// daemon has them.
    pub async fn settings(&self) -> Result<Settings, ClientError> {
        let daemon = self.daemon.clone();
        blocking(move || Ok(daemon.connect()?.settings(None)?.into())).await
    }

    /// Saves the user's edit of the settings `from` into `to`. Only what
    /// changed is sent, so the daemon, the file's one writer, keeps what
    /// another client saved meanwhile. Returns the settings as saved.
    pub async fn save_settings(
        &self,
        from: Settings,
        to: Settings,
    ) -> Result<Settings, ClientError> {
        let change = change(from, to)?;
        let daemon = self.daemon.clone();
        blocking(move || Ok(daemon.connect()?.settings(Some(change))?.into())).await
    }

    /// What `a` resolves to with `dir` in front of `search` (a `PATH`;
    /// `None` is the one sessions get, which may take the login shell a
    /// moment the first time).
    pub async fn cli_status(
        &self,
        dir: String,
        search: Option<String>,
    ) -> Result<CliStatus, ClientError> {
        let runtime = self.daemon.home.runtime();
        blocking(move || {
            let search = search.map_or_else(session_path, OsString::from);
            Ok(CliLink::find(&runtime, Path::new(&dir), &search).into())
        })
        .await
    }

    /// Links `dir/a` to the staged `runtime/current/a`, so the CLI runs the
    /// build the daemon runs: only where no `a` is found on `search`, or
    /// over a link an earlier install made when `replace` is set.
    pub async fn install_cli(
        &self,
        dir: String,
        search: Option<String>,
        replace: bool,
    ) -> Result<CliStatus, ClientError> {
        let runtime = self.daemon.home.runtime();
        blocking(move || {
            let search = search.map_or_else(session_path, OsString::from);
            Ok(CliLink::install(&runtime, Path::new(&dir), &search, replace)?.into())
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
                (Target::New, sys::workspace_of(&dir), Some(dir))
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
            let env = env.map(|dir| Environment::new(dir, session_vars(), false));
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
        self.write(ClientMsg::input(text)).await
    }

    /// Answers the ask for tool call `call`.
    pub async fn approve(&self, call: u64, approved: bool) -> Result<(), ClientError> {
        self.write(ClientMsg::approve(call, approved)).await
    }

    /// Stops the running turn: its model or tool call, and the rest of it.
    pub async fn interrupt(&self) -> Result<(), ClientError> {
        self.write(ClientMsg::INTERRUPT).await
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

/// What the user changed going from `from` to `to`, with names and values
/// trimmed as the pane shows them and blank names skipped.
fn change(from: Settings, to: Settings) -> Result<SettingsChange, ClientError> {
    let vars = |vars: Vec<EnvVar>| -> BTreeMap<String, String> {
        vars.into_iter()
            .map(|EnvVar { name, value }| (name.trim().to_owned(), value))
            .filter(|(name, _)| !name.is_empty())
            .collect()
    };
    let (old, new) = (vars(from.env), vars(to.env));
    if let Some(bad) = new.keys().find(|name| !sys::config::is_var_name(name)) {
        return Err(ClientError::InvalidArgument {
            message: format!("not a variable name: {bad}"),
        });
    }
    let mut env: BTreeMap<String, Option<String>> = old
        .keys()
        .filter(|name| !new.contains_key(*name))
        .map(|name| (name.clone(), None))
        .collect();
    env.extend(
        new.into_iter()
            .filter(|(name, value)| old.get(name) != Some(value))
            .map(|(name, value)| (name, Some(value))),
    );
    let given = |s: Option<String>| s.map_or_else(String::new, |s| s.trim().to_owned());
    let changed = |old: Option<String>, new: Option<String>| {
        let (old, new) = (given(old), given(new));
        (old != new).then_some(new)
    };
    Ok(SettingsChange {
        model: changed(from.model, to.model),
        agent: changed(from.agent, to.agent),
        env,
    })
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
