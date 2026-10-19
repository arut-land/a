//! The daemon connection: staging the bundled `a`, settings, the session
//! list, and sessions, each the client SDK's (a [`client::SessionClient`]
//! on a [`Driver`], published through a [`Feed`]). The connection is
//! blocking I/O, so it runs on the `rt` runtime's blocking pool and the
//! exported async methods only await it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use boltffi::{EventSubscription, StreamProducer, export};
use client::{Attach, Cmd, Connection, Driver, Effect, Endpoint, Env, SessionClient};
use protocol::{Environment, SessionId, SettingsChange, Target};
use rt::Threads;
use sys::install::CliLink;
use sys::{Home, Staged};

use crate::feed::{Feed, lock};
use crate::{
    AppAction, CliStatus, ClientError, CommandHelp, EnvVar, OpenTarget, Resync, SessionInfo,
    SessionListUpdate, SessionSnapshot, Settings, TurnInfo, ViewUpdate,
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
    fn endpoint(&self) -> Endpoint {
        Endpoint::Local(self.home.clone())
    }

    fn connect(&self) -> io::Result<Connection> {
        client::connect(&self.endpoint(), &self.staged, &|| {})
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

/// The daemon's session list as it changes, pushed by the daemon over one
/// connection: subscribe to `updates`, then `start`.
pub struct SessionWatch {
    daemon: Daemon,
    producer: Arc<StreamProducer<SessionListUpdate>>,
    /// Watches until dropped.
    watch: Mutex<Option<client::SessionWatch>>,
}

#[export]
impl SessionWatch {
    pub fn new(client: &Client) -> Self {
        Self {
            daemon: client.daemon.clone(),
            producer: Arc::new(StreamProducer::new(64)),
            watch: Mutex::new(None),
        }
    }

    /// The list now and after each change, newest first.
    #[ffi_stream(item = SessionListUpdate)]
    pub fn updates(&self) -> Arc<EventSubscription<SessionListUpdate>> {
        self.producer.subscribe()
    }

    /// Watches until dropped, starting the daemon if needed and again
    /// after it restarts (an upgrade, say); while it cannot be reached an
    /// update says why. Starts once.
    pub fn start(&self) {
        let mut watch = lock(&self.watch);
        if watch.is_some() {
            return;
        }
        let producer = Arc::clone(&self.producer);
        *watch = Some(client::SessionWatch::start(
            self.daemon.endpoint(),
            self.daemon.staged.clone(),
            move |list| {
                producer.push(match list {
                    Ok(sessions) => SessionListUpdate {
                        sessions: sessions.iter().rev().map(Into::into).collect(),
                        problem: None,
                    },
                    Err(problem) => SessionListUpdate {
                        sessions: Vec::new(),
                        problem: Some(problem),
                    },
                });
            },
        ));
    }
}

/// The session `wanted` names among `sessions`: `latest` is the first, else
/// one whose id starts with it, in full or short form.
#[export]
pub fn find_session(sessions: Vec<SessionInfo>, wanted: &str) -> Option<String> {
    if wanted == "latest" {
        return sessions.into_iter().next().map(|s| s.id);
    }
    let id = |s: &SessionInfo| s.id.parse().ok().map(SessionId);
    client::matching(&sessions, id, wanted)
        .ok()
        .map(|s| s.id.clone())
}

/// One session as this client shows it: subscribe to `updates`, then
/// `open` it.
pub struct Session {
    feed: Arc<Mutex<Feed>>,
    driver: Driver,
}

/// Has `driver` carry out the client's effects, publishing what comes
/// back for the app.
fn run(driver: &Driver, feed: &Mutex<Feed>, mut effects: Vec<Effect>) {
    while !effects.is_empty() {
        let shown = effects
            .into_iter()
            .filter_map(|effect| driver.apply(effect))
            .collect();
        effects = lock(feed).absorb(shown);
    }
}

#[export]
impl Session {
    pub fn new(client: &Client) -> Self {
        let feed = Arc::new(Mutex::new(Feed::new(SessionClient::new(Env::Keep))));
        let shared = Arc::clone(&feed);
        let daemon = &client.daemon;
        let driver = Driver::with_origin(
            daemon.endpoint(),
            daemon.staged.clone(),
            || {
                Ok(Environment::new(
                    std::env::current_dir()?,
                    session_vars(),
                    false,
                ))
            },
            move |driver, event| {
                let effects = lock(&shared).step(event);
                run(driver, &shared, effects);
            },
        );
        Self { feed, driver }
    }

    /// Every update from here on. Subscribe before `open` to see the
    /// attach; items dropped by a full buffer show as a `rev` gap, for
    /// `resync`.
    #[ffi_stream(item = ViewUpdate)]
    pub fn updates(&self) -> Arc<EventSubscription<ViewUpdate>> {
        lock(&self.feed).subscribe()
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        lock(&self.feed).snapshot()
    }

    /// What came after update `from`: the updates, or a snapshot when too
    /// many were missed.
    pub fn resync(&self, from: u64) -> Resync {
        lock(&self.feed).resync(from)
    }

    /// What the controls offer for the turn now.
    pub fn turn(&self) -> TurnInfo {
        lock(&self.feed).client.turn().into()
    }

    /// Every slash command, for help and completion.
    pub fn commands(&self) -> Vec<CommandHelp> {
        crate::commands(lock(&self.feed).client.commands())
    }

    /// Attaches to `target`. A new session works in the git root of its
    /// directory with this process's environment; an existing one keeps the
    /// environment its last client gave it. When the daemon drains for an
    /// upgrade the session reattaches on its own and replays what it
    /// missed; how the link stands arrives as `link` updates. A session
    /// opens once: make another to attach again.
    pub fn open(&self, target: OpenTarget) -> Result<(), ClientError> {
        let attach = match target {
            OpenTarget::New { workspace } => {
                let dir = PathBuf::from(workspace);
                let env = Environment::new(dir.clone(), session_vars(), false);
                Attach {
                    workspace: Some(dir),
                    ..Attach::new(Target::New, Env::Given(env))
                }
            }
            OpenTarget::Latest {
                workspace,
                any_workspace,
            } => Attach {
                workspace: Some(PathBuf::from(workspace)),
                ..Attach::new(Target::Latest { any_workspace }, Env::Keep)
            },
            OpenTarget::Id { id } => Attach {
                workspace: Some(PathBuf::new()),
                ..Attach::new(Target::Id(parse_id(&id)?), Env::Keep)
            },
        };
        let effects = {
            let mut feed = lock(&self.feed);
            if feed.client.connection() > 0 {
                return Err(ClientError::InvalidArgument {
                    message: "the session was opened already".into(),
                });
            }
            let effects = feed.client.attach(attach);
            feed.absorb(effects)
        };
        run(&self.driver, &self.feed, effects);
        Ok(())
    }

    /// Sends the user's input as it is.
    pub async fn send(&self, text: String) -> Result<(), ClientError> {
        self.run(Cmd::Input(text)).await
    }

    /// A line the user typed, parsed as every client parses it: what it
    /// asks of the session is done, and what it asks of the app comes
    /// back.
    pub async fn submit(&self, line: String) -> Result<Option<AppAction>, ClientError> {
        let asking = lock(&self.feed).client.turn().asking;
        let action = match client::parse(&line, asking) {
            None => return Ok(None),
            Some(Cmd::Help) => AppAction::Help,
            Some(Cmd::Sessions) => AppAction::Sessions,
            Some(Cmd::Open(_)) => AppAction::NewSession,
            Some(cmd) => return self.run(cmd).await.map(|()| None),
        };
        Ok(Some(action))
    }

    /// Input for the running turn, before its next step.
    pub async fn steer(&self, text: String) -> Result<(), ClientError> {
        self.run(Cmd::Steer(text)).await
    }

    /// Answers the ask for tool call `call`.
    pub async fn approve(&self, call: u64, approved: bool) -> Result<(), ClientError> {
        self.run(Cmd::Approve { call, approved }).await
    }

    /// Stops the running turn: its model or tool call, and the rest of it.
    pub async fn interrupt(&self) -> Result<(), ClientError> {
        self.run(Cmd::Interrupt).await
    }

    /// Detaches; the session keeps running in the daemon.
    pub async fn detach(&self) -> Result<(), ClientError> {
        self.run(Cmd::Detach).await
    }

    /// Replaces a daemon that stopped answering, and attaches again.
    pub async fn restart(&self) -> Result<(), ClientError> {
        let (feed, driver) = (Arc::clone(&self.feed), self.driver.clone());
        blocking(move || {
            let effects = {
                let mut feed = lock(&feed);
                let effects = feed.client.restart();
                feed.absorb(effects)
            };
            run(&driver, &feed, effects);
            Ok(())
        })
        .await
    }
}

impl Session {
    async fn run(&self, cmd: Cmd) -> Result<(), ClientError> {
        let (feed, driver) = (Arc::clone(&self.feed), self.driver.clone());
        blocking(move || {
            let effects = {
                let mut feed = lock(&feed);
                if !feed.client.link().attached() {
                    return Err(ClientError::NotOpen);
                }
                let effects = feed.client.run(cmd);
                feed.absorb(effects)
            };
            run(&driver, &feed, effects);
            Ok(())
        })
        .await
    }
}

/// Released by the app: the daemon is told, and the reader then ends.
impl Drop for Session {
    fn drop(&mut self) {
        let effects = {
            let mut feed = lock(&self.feed);
            let effects = feed.client.detach();
            feed.absorb(effects)
        };
        for effect in effects {
            self.driver.apply(effect);
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
