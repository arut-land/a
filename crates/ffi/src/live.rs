//! The daemon connection: staging the bundled `a`, settings, the session
//! list, and sessions, each the client SDK's (a [`client::SessionClient`]
//! on a [`Driver`], published through a [`Feed`]). Transport tasks own I/O;
//! the feed lock orders reducer transitions and their queued effects.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use boltffi::{EventSubscription, StreamProducer, export};
use client::{Attach, Cmd, Daemon, Driver, Effect, Env, Notice, SessionClient};
use protocol::{Environment, HomeSettings, SessionId, SettingsChange, Target};
use rt::Threads;
use sys::install::CliLink;
use sys::{Home, Staged};

use crate::feed::{Feed, lock};
use crate::{
    AppAction, CliStatus, ClientError, CommandHelp, OpenTarget, Resync, SessionInfo,
    SessionListUpdate, SessionSnapshot, Settings, TurnInfo, ViewUpdate,
};

fn runtime() -> &'static Arc<Threads> {
    static RUNTIME: OnceLock<Arc<Threads>> = OnceLock::new();
    RUNTIME.get_or_init(|| Arc::new(Threads::default()))
}

/// Runs blocking `work` off the caller's thread.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ClientError> + Send + 'static,
) -> Result<T, ClientError> {
    rt::unblock_with(&**runtime(), rt::BlockingClass::Network, 64 << 10, work).await
}

fn diagnosis(
    rt: &dyn rt::Runtime,
    home: Home,
) -> impl Future<Output = client::Diagnosis> + Send + 'static {
    rt::unblock(rt, move || client::diagnose(&home))
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
    vars: Vec<(OsString, OsString)>,
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
            vars: session_vars(),
        })
    }

    /// The staged build's id, `<version>+<hash>`.
    pub fn build(&self) -> String {
        self.daemon.staged.id.clone()
    }

    pub fn daemon_log(&self) -> String {
        self.home.daemon_log().display().to_string()
    }

    /// Reads the daemon record and crash files off the caller's thread.
    pub async fn daemon_pid(&self) -> Option<u32> {
        diagnosis(&**runtime(), self.home.clone()).await.pid
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
            runtime().clone(),
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
    driver: client::ClientHandle,
    vars: Vec<(OsString, OsString)>,
    #[cfg(test)]
    before_pump: Arc<Mutex<Option<BeforePump>>>,
    #[cfg(test)]
    before_replay: Arc<Mutex<Option<BeforePump>>>,
}

#[cfg(test)]
type BeforePump = Box<dyn FnOnce() + Send>;

#[export]
impl Session {
    pub fn new(client: &Client) -> Self {
        let feed = Arc::new(Mutex::new(Feed::new(SessionClient::new(Env::Keep))));
        let shared = Arc::clone(&feed);
        #[cfg(test)]
        let before_replay = Arc::new(Mutex::new(None::<BeforePump>));
        #[cfg(test)]
        let replaying = Arc::clone(&before_replay);
        let driver = Driver::with_origin(
            || {
                Ok(Environment::new(
                    std::env::current_dir()?,
                    session_vars(),
                    false,
                ))
            },
            move |driver, event| {
                #[cfg(test)]
                let connected = matches!(event.kind, client::EventKind::Connected { .. });
                let mut feed = lock(&shared);
                let effects = feed.client.step(event);
                #[cfg(test)]
                if connected && let Some(pause) = lock(&replaying).take() {
                    pause();
                }
                driver.pump(&mut *feed, effects);
            },
        )
        .start(runtime().clone(), client.daemon.clone());
        Self {
            feed,
            driver,
            vars: client.vars.clone(),
            #[cfg(test)]
            before_pump: Arc::default(),
            #[cfg(test)]
            before_replay,
        }
    }

    /// Every update from here on. Subscribe before `open` to see the
    /// attach; items dropped by a full buffer show as a `rev` gap, for
    /// `resync`.
    #[ffi_stream(item = ViewUpdate)]
    pub fn updates(&self) -> Arc<EventSubscription<ViewUpdate>> {
        lock(&self.feed).subscribe()
    }

    /// A conservative byte charge for all view payloads received since opening.
    pub fn retained_bytes(&self) -> u64 {
        lock(&self.feed).retained_bytes as u64
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
                let env = Environment::new(dir.clone(), self.vars.clone(), false);
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
        let mut feed = lock(&self.feed);
        let effects = {
            if feed.client.connection().0 > 0 {
                return Err(ClientError::InvalidArgument {
                    message: "the session was opened already".into(),
                });
            }
            feed.client.attach(attach)
        };
        self.driver.pump(&mut *feed, effects);
        Ok(())
    }

    /// Sends the user's input as it is.
    pub fn send(&self, text: String) -> Result<(), ClientError> {
        self.intend(move |client| client.offer()?.input(text))
    }

    /// A line the user typed, parsed as every client parses it: what it
    /// asks of the session is done, and what it asks of the app comes
    /// back. What the session does not take now is told as an error
    /// update, as the other clients show it.
    pub fn submit(&self, line: &str) -> Result<Option<AppAction>, ClientError> {
        let asking = lock(&self.feed).client.turn().asking;
        let action = match client::parse(line, asking) {
            None => return Ok(None),
            Some(Cmd::Help) => AppAction::Help,
            Some(Cmd::Sessions) => AppAction::Sessions,
            Some(Cmd::Open(_)) => AppAction::NewSession,
            Some(cmd) => return self.run(cmd).map(|()| None),
        };
        Ok(Some(action))
    }

    /// Forks the current conversation at a rest point. Files stay as they
    /// are; the fork keeps its recorded directory and environment.
    pub fn fork(&self, seq: Option<u64>) -> Result<(), ClientError> {
        let seq = seq
            .map(|seq| {
                protocol::Seq::new(seq).ok_or_else(|| ClientError::InvalidArgument {
                    message: "fork sequence must be positive".into(),
                })
            })
            .transpose()?;
        self.run(Cmd::Fork(seq))
    }

    /// Input for the running turn, before its next step.
    pub fn steer(&self, text: String) -> Result<(), ClientError> {
        self.intend(move |client| client.offer()?.steer(text))
    }

    /// Answers the ask for tool call `call`.
    pub fn approve(&self, call: u64, approved: bool) -> Result<(), ClientError> {
        let call = protocol::CallId::new(call).ok_or_else(|| ClientError::InvalidArgument {
            message: "call 0 names no call".to_owned(),
        })?;
        self.intend(move |client| client.offer()?.approve(call, approved))
    }

    /// Stops the running turn: its model or tool call, and the rest of it.
    pub fn interrupt(&self) -> Result<(), ClientError> {
        self.intend(|client| client.offer()?.interrupt())
    }

    /// Detaches; the session keeps running in the daemon.
    pub fn detach(&self) -> Result<(), ClientError> {
        self.run(Cmd::Detach)
    }

    /// Replaces a daemon that stopped answering, and attaches again.
    pub fn restart(&self) -> Result<(), ClientError> {
        let mut feed = lock(&self.feed);
        let effects = feed.client.restart();
        self.driver.pump(&mut *feed, effects);
        Ok(())
    }
}

impl Session {
    fn intend(
        &self,
        intent: impl FnOnce(&mut SessionClient) -> Result<Vec<Effect>, Notice>,
    ) -> Result<(), ClientError> {
        let mut feed = lock(&self.feed);
        let effects = intent(&mut feed.client)?;
        #[cfg(test)]
        if let Some(pause) = lock(&self.before_pump).take() {
            pause();
        }
        self.driver.pump(&mut *feed, effects);
        Ok(())
    }

    fn run(&self, cmd: Cmd) -> Result<(), ClientError> {
        let mut feed = lock(&self.feed);
        if !feed.client.link().attached() {
            return Err(ClientError::NotOpen);
        }
        let effects = feed.client.run(cmd);
        self.driver.pump(&mut *feed, effects);
        Ok(())
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
    fn diagnostic_reads_wait_for_storage_without_blocking_the_caller() {
        use rt::Runtime as _;
        use std::future::{Future, poll_fn};
        use std::pin::pin;
        use std::task::Poll;

        let (_session, home, root, listener) = fixture();
        let runtime = Threads::configured(rt::Config::for_cores(1));
        let (entered, waiting) = mpsc::channel();
        let (release, released) = mpsc::channel();
        rt::block_on(runtime.spawn_blocking(
            rt::BlockingClass::Storage,
            0,
            Box::new(move || {
                entered.send(()).unwrap();
                released.recv().unwrap();
            }),
        ))
        .unwrap();
        waiting.recv().unwrap();
        let mut reading = pin!(diagnosis(&runtime, home.clone()));
        rt::block_on(poll_fn(|cx| {
            assert!(reading.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        }));
        release.send(()).unwrap();
        assert!(rt::block_on(reading).pid.is_none());
        drop(listener);
        home.endpoint().remove();
        std::fs::remove_dir_all(root).unwrap();
    }

    fn fixture() -> (Session, Home, PathBuf, sys::Listener) {
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
            vars: Vec::new(),
        });
        (session, home, root, listener)
    }

    fn paused_input(
        session: &Session,
        text: &str,
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    ) {
        *lock(&session.before_pump) = Some(Box::new(move || {
            entered.send(()).unwrap();
            release.recv().unwrap();
        }));
        session.send(text.to_owned()).unwrap();
    }

    #[test]
    fn reconnect_preserves_mutation_order() {
        let (session, home, root, listener) = fixture();
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
        session.send("ready".into()).unwrap();
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
                paused_input(session, "first", numbered, released);
            });
            numbering.recv().unwrap();
            let serialized = session.feed.try_lock().is_err();
            let (started, starting) = mpsc::channel();
            let second = scope.spawn(move || {
                started.send(()).unwrap();
                session.run(Cmd::Input("second".into())).unwrap();
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
                paused_input(session, "during reconnect", queued, released);
            });
            queueing.recv().unwrap();
            // Connected competes with the input while its numbering/pump lock is held.
            write_frame(&mut peer, &welcome).unwrap();
            let serialized = session.feed.try_lock().is_err();
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
        assert!(read_frame::<ClientMsg>(&mut peer).unwrap().is_none());
        drop(peer);
        drop(listener);
        home.endpoint().remove();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reconnect_replay_is_pumped_before_newly_numbered_input() {
        let (session, home, root, listener) = fixture();
        let id = SessionId::new();
        session.open(OpenTarget::Id { id: id.to_string() }).unwrap();
        let mut peer = listener.accept().unwrap();
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        let welcome = DaemonMsg::Welcome {
            proto: protocol::PROTO,
            build: "test".into(),
            caps: Caps::ALL,
            commands: Vec::new(),
        };
        session.send("unanswered".into()).unwrap();
        write_frame(&mut peer, &welcome).unwrap();
        assert_eq!(
            read_frame::<ClientMsg>(&mut peer)
                .unwrap()
                .unwrap()
                .client_seq(),
            Some(1)
        );
        write_frame(
            &mut peer,
            &DaemonMsg::Attached {
                session: id,
                workspace: root.clone(),
            },
        )
        .unwrap();
        write_frame(&mut peer, &DaemonMsg::Draining).unwrap();
        drop(peer);
        let mut peer = listener.accept().unwrap();
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        let (prepared, preparing) = mpsc::channel();
        let (replay, replaying) = mpsc::channel();
        *lock(&session.before_replay) = Some(Box::new(move || {
            prepared.send(()).unwrap();
            replaying.recv().unwrap();
        }));
        write_frame(&mut peer, &welcome).unwrap();
        preparing.recv().unwrap();
        let (numbered, numbering) = mpsc::channel();
        let (send, sending) = mpsc::channel();
        let mut received = Vec::new();
        thread::scope(|scope| {
            let session = &session;
            // Choose the runnable side before input can acquire the lock. With
            // incoming serialization removed, force new input ahead of replay.
            let ordered = session.feed.try_lock().is_err();
            let input = scope.spawn(move || paused_input(session, "new input", numbered, sending));
            if ordered {
                replay.send(()).unwrap();
            }
            numbering.recv().unwrap();
            send.send(()).unwrap();
            if !ordered {
                received.push(
                    read_frame::<ClientMsg>(&mut peer)
                        .unwrap()
                        .unwrap()
                        .client_seq(),
                );
                replay.send(()).unwrap();
            }
            input.join().unwrap();
        });
        while received.len() < 2 {
            received.push(
                read_frame::<ClientMsg>(&mut peer)
                    .unwrap()
                    .unwrap()
                    .client_seq(),
            );
        }
        drop(session);
        assert!(read_frame::<ClientMsg>(&mut peer).unwrap().is_none());
        drop(peer);
        drop(listener);
        home.endpoint().remove();
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(received, vec![Some(1), Some(2)]);
    }
}
