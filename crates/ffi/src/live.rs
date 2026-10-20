//! The daemon connection: staging the bundled `a`, settings, the session
//! list, and sessions, each the client SDK's (a [`client::SessionClient`]
//! on a [`Driver`], published through a [`Feed`]). The connection is
//! blocking I/O, so it runs on the `rt` runtime's blocking pool and the
//! exported async methods only await it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use boltffi::{EventSubscription, StreamProducer, export};
use client::{Attach, Cmd, Daemon, Driver, Effect, Env, Notice, Renderer, SessionClient, Show};
use protocol::{Environment, HomeSettings, SessionId, SettingsChange, Target};
use rt::Threads;
use sys::install::CliLink;
use sys::{Home, Staged};

use crate::feed::{Feed, lock};
use crate::{
    AppAction, CliStatus, ClientError, CommandHelp, OpenTarget, Resync, SessionInfo,
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

/// The per-user daemon, as the CLI reaches it: `$A_HOME`, else `~/.a`.
pub struct Client {
    home: Home,
    daemon: Daemon,
}

#[export]
impl Client {
    /// Stages `helper`, the `a` this app bundles, the way every entry point
    /// does: a daemon this client starts runs the newest build staged.
    pub fn new(helper: &str) -> Result<Self, ClientError> {
        let home = Home::from_env()?;
        let staged: Staged = home.runtime().stage(Path::new(helper))?;
        Ok(Self {
            daemon: Daemon::local(home.clone(), staged),
            home,
        })
    }

    /// The staged build's id, `<version>+<hash>`.
    pub fn build(&self) -> String {
        self.daemon.staged.id.clone()
    }

    pub fn daemon_log(&self) -> String {
        self.home.daemon_log().display().to_string()
    }

    /// The pid of the running daemon, if one is.
    pub fn daemon_pid(&self) -> Option<u32> {
        client::diagnose(&self.home).pid
    }

    /// `$A_HOME/config.toml`, where the settings are kept.
    pub fn config_file(&self) -> String {
        self.home.config_file().display().to_string()
    }

    /// The model and provider variables sessions fall back on, as the
    /// daemon has them.
    pub async fn settings(&self) -> Result<Settings, ClientError> {
        let daemon = self.daemon.clone();
        blocking(move || Ok(daemon.connect(&|| {})?.settings(None)?.into())).await
    }

    /// Saves the user's edit of the settings `from` into `to`. Only what
    /// changed is sent, so the daemon, the file's one writer, keeps what
    /// another client saved meanwhile. Returns the settings as saved.
    pub async fn save_settings(
        &self,
        from: Settings,
        to: Settings,
    ) -> Result<Settings, ClientError> {
        let change = SettingsChange::between(&HomeSettings::from(from), &HomeSettings::from(to));
        let daemon = self.daemon.clone();
        blocking(move || Ok(daemon.connect(&|| {})?.settings(Some(change))?.into())).await
    }

    /// What `a` resolves to with `dir` in front of `search` (a `PATH`;
    /// `None` is the one sessions get, which may take the login shell a
    /// moment the first time).
    pub async fn cli_status(
        &self,
        dir: String,
        search: Option<String>,
    ) -> Result<CliStatus, ClientError> {
        let runtime = self.home.runtime();
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
        let runtime = self.home.runtime();
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
            daemon.restart()?;
            Ok(())
        })
        .await
    }

    /// Whether variable `name` holds a secret, by the rule the daemon keeps
    /// values out of its logs by: a pane shows such a value masked.
    pub fn is_secret_name(&self, name: &str) -> bool {
        Environment::is_secret(name)
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
            self.daemon.clone(),
            move |list| {
                producer.push(match list {
                    Ok(sessions) => SessionListUpdate {
                        sessions: sessions.iter().map(Into::into).collect(),
                        problem: None,
                    },
                    Err(problem) => SessionListUpdate {
                        sessions: Vec::new(),
                        problem: Some(problem.to_string()),
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
    /// Numbering and pumping share this lock; publishing takes only the feed.
    sending: Arc<Mutex<()>>,
}

/// The feed as the driver's pump renders through it: locked only while it
/// shows, so the driver's I/O never runs under the lock the reader thread
/// takes to step the client.
struct Published<'a>(&'a Mutex<Feed>);

impl Renderer for Published<'_> {
    fn show(&mut self, shown: Vec<Show>) -> Vec<Effect> {
        lock(self.0).show(shown)
    }
}

#[export]
impl Session {
    pub fn new(client: &Client) -> Self {
        let feed = Arc::new(Mutex::new(Feed::new(SessionClient::new(Env::Keep))));
        let shared = Arc::clone(&feed);
        let sending = Arc::new(Mutex::new(()));
        let ordered = Arc::clone(&sending);
        let driver = Driver::with_origin(
            client.daemon.clone(),
            || {
                Ok(Environment::new(
                    std::env::current_dir()?,
                    session_vars(),
                    false,
                ))
            },
            move |driver, event| {
                let _sending = lock(&ordered);
                let effects = lock(&shared).client.step(event);
                driver.pump(&mut Published(&shared), effects);
            },
        );
        Self {
            feed,
            driver,
            sending,
        }
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
            OpenTarget::Id { id } => Attach::new(Target::Id(parse_id(&id)?), Env::Keep),
        };
        let _sending = lock(&self.sending);
        let effects = {
            let mut feed = lock(&self.feed);
            if feed.client.connection().0 > 0 {
                return Err(ClientError::InvalidArgument {
                    message: "the session was opened already".into(),
                });
            }
            feed.client.attach(attach)
        };
        self.driver.pump(&mut Published(&self.feed), effects);
        Ok(())
    }

    /// Sends the user's input as it is.
    pub async fn send(&self, text: String) -> Result<(), ClientError> {
        self.intend(move |client| Ok(client.offer()?.input(text)))
            .await
    }

    /// A line the user typed, parsed as every client parses it: what it
    /// asks of the session is done, and what it asks of the app comes
    /// back. What the session does not take now is told as an error
    /// update, as the other clients show it.
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
        self.intend(move |client| client.offer()?.steer(text)).await
    }

    /// Answers the ask for tool call `call`.
    pub async fn approve(&self, call: u64, approved: bool) -> Result<(), ClientError> {
        let call = protocol::CallId::new(call).ok_or_else(|| ClientError::InvalidArgument {
            message: "call 0 names no call".to_owned(),
        })?;
        self.intend(move |client| client.offer()?.approve(call, approved))
            .await
    }

    /// Stops the running turn: its model or tool call, and the rest of it.
    pub async fn interrupt(&self) -> Result<(), ClientError> {
        self.intend(|client| client.offer()?.interrupt()).await
    }

    /// Detaches; the session keeps running in the daemon.
    pub async fn detach(&self) -> Result<(), ClientError> {
        self.run(Cmd::Detach).await
    }

    /// Replaces a daemon that stopped answering, and attaches again.
    pub async fn restart(&self) -> Result<(), ClientError> {
        let (feed, driver) = (Arc::clone(&self.feed), self.driver.clone());
        let sending = Arc::clone(&self.sending);
        blocking(move || {
            let _sending = lock(&sending);
            let effects = lock(&feed).client.restart();
            driver.pump(&mut Published(&feed), effects);
            Ok(())
        })
        .await
    }
}

impl Session {
    /// Carries out what `intent` asks of the session's offer; what it does
    /// not offer is the error.
    async fn intend(
        &self,
        intent: impl FnOnce(&mut SessionClient) -> Result<Vec<Effect>, Notice> + Send + 'static,
    ) -> Result<(), ClientError> {
        let (feed, driver) = (Arc::clone(&self.feed), self.driver.clone());
        let sending = Arc::clone(&self.sending);
        blocking(move || {
            let _sending = lock(&sending);
            let effects = intent(&mut lock(&feed).client)?;
            driver.pump(&mut Published(&feed), effects);
            Ok(())
        })
        .await
    }

    async fn run(&self, cmd: Cmd) -> Result<(), ClientError> {
        let (feed, driver) = (Arc::clone(&self.feed), self.driver.clone());
        let sending = Arc::clone(&self.sending);
        blocking(move || {
            let _sending = lock(&sending);
            let effects = {
                let mut feed = lock(&feed);
                if !feed.client.link().attached() {
                    return Err(ClientError::NotOpen);
                }
                feed.client.run(cmd)
            };
            driver.pump(&mut Published(&feed), effects);
            Ok(())
        })
        .await
    }
}

/// Released by the app: the daemon is told, and the reader then ends.
impl Drop for Session {
    fn drop(&mut self) {
        let _sending = lock(&self.sending);
        let effects = lock(&self.feed).client.detach();
        self.driver.pump(&mut Published(&self.feed), effects);
    }
}

fn parse_id(id: &str) -> Result<SessionId, ClientError> {
    id.parse::<uuid::Uuid>()
        .map(SessionId)
        .map_err(|_| ClientError::InvalidArgument {
            message: format!("not a session id: {id}"),
        })
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use protocol::{Caps, ClientMsg, DaemonMsg, read_frame, write_frame};

    use super::*;

    #[test]
    fn numbered_input_and_reconnect_replay_cannot_overtake_a_paused_send() {
        let root = std::env::temp_dir().join(format!("a-ffi-order-{}", SessionId::new()));
        std::fs::create_dir_all(&root).unwrap();
        let home = Home::from(root.join("home"));
        let exe = root.join("a");
        std::fs::write(&exe, "never run").unwrap();
        let staged = home.runtime().stage(&exe).unwrap();
        let listener = home.bind().unwrap();
        let session = Session::new(&Client {
            daemon: Daemon::local(home.clone(), staged),
            home: home.clone(),
        });
        session
            .open(OpenTarget::Id {
                id: SessionId::new().to_string(),
            })
            .unwrap();
        let mut peer = listener.accept().unwrap();
        let welcome = DaemonMsg::Welcome {
            proto: protocol::PROTO,
            build: "test".into(),
            caps: Caps::ALL,
            commands: Vec::new(),
        };
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        // Receiving queued input proves Connected was processed, without polling.
        rt::block_on(session.send("ready".into())).unwrap();
        write_frame(&mut peer, &welcome).unwrap();
        assert_eq!(
            read_frame::<ClientMsg>(&mut peer)
                .unwrap()
                .unwrap()
                .client_seq(),
            Some(1)
        );
        let id = SessionId::new();
        write_frame(
            &mut peer,
            &DaemonMsg::Attached {
                session: id,
                workspace: root.clone(),
            },
        )
        .unwrap();
        let (numbered, numbering) = mpsc::channel();
        let (release, released) = mpsc::channel();
        thread::scope(|scope| {
            let session = &session;
            let first = scope.spawn(move || {
                rt::block_on(session.intend(move |client| {
                    let effects = client.offer()?.input("first".into());
                    numbered.send(()).unwrap();
                    released.recv().unwrap();
                    Ok(effects)
                }))
                .unwrap()
            });
            numbering.recv().unwrap();
            let serialized = session.sending.try_lock().is_err();
            let (started, starting) = mpsc::channel();
            let second = scope.spawn(move || {
                started.send(()).unwrap();
                rt::block_on(session.run(Cmd::Input("second".into()))).unwrap();
            });
            starting.recv().unwrap();
            release.send(()).unwrap();
            first.join().unwrap();
            second.join().unwrap();
            assert!(serialized);
        });
        for seq in [2, 3] {
            assert_eq!(
                read_frame::<ClientMsg>(&mut peer)
                    .unwrap()
                    .unwrap()
                    .client_seq(),
                Some(seq)
            );
        }
        write_frame(&mut peer, &DaemonMsg::Draining).unwrap();
        drop(peer);
        let mut peer = listener.accept().unwrap();
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        let (queued, queueing) = mpsc::channel();
        let (release, released) = mpsc::channel();
        thread::scope(|scope| {
            let session = &session;
            let input = scope.spawn(move || {
                rt::block_on(session.intend(move |client| {
                    let effects = client.offer()?.input("during reconnect".into());
                    queued.send(()).unwrap();
                    released.recv().unwrap();
                    Ok(effects)
                }))
                .unwrap()
            });
            queueing.recv().unwrap();
            // Connected competes with the input while its numbering/pump lock is held.
            write_frame(&mut peer, &welcome).unwrap();
            let serialized = session.sending.try_lock().is_err();
            release.send(()).unwrap();
            input.join().unwrap();
            assert!(serialized);
        });
        for seq in [1, 2, 3, 4] {
            assert_eq!(
                read_frame::<ClientMsg>(&mut peer)
                    .unwrap()
                    .unwrap()
                    .client_seq(),
                Some(seq)
            );
        }
        drop(session);
        assert!(matches!(
            read_frame::<ClientMsg>(&mut peer).unwrap(),
            Some(ClientMsg::Detach)
        ));
        drop(peer);
        drop(listener);
        home.endpoint().remove();
        std::fs::remove_dir_all(root).unwrap();
    }
}
