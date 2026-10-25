//! The daemon connection: staging the bundled `a`, settings, the session
//! list, and sessions, each the client SDK's (a [`client::SessionClient`]
//! on a [`Driver`], published through a [`Feed`]). Transport tasks own I/O;
//! the feed lock orders reducer transitions and their queued effects.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use boltffi::{EventSubscription, StreamProducer, export};
use client::{Attach, Cmd, Daemon, Daemons, Driver, Effect, Env, Member, Notice, SessionClient};
use protocol::{Environment, HomeSettings, HostPath, SessionId, SettingsChange, Target};
use rt::{Runtime, Threads};
use sys::install::CliLink;
use sys::{Home, Staged};

use crate::feed::{Feed, lock};
use crate::{
    AppAction, BlobPiece, ChangeBatch, CliStatus, ClientError, CommandHelp, OpenTarget,
    QueryAnswer, Resync, SessionInfo, SessionListUpdate, SessionSnapshot, Settings, TurnInfo,
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
    rt: &dyn Runtime,
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

/// The per-user daemon, as the CLI reaches it: the native layout or explicit `$A_HOME`.
pub struct Client {
    home: Home,
    /// This machine's daemon alone, for now.
    daemons: Daemons,
    vars: Vec<(OsString, OsString)>,
}

impl Client {
    fn daemon(&self) -> &Daemon {
        &self.daemons.first().daemon
    }
}

#[export]
impl Client {
    /// Stages `helper`, the `a` this app bundles, the way every entry point
    /// does: a daemon this client starts runs the newest build staged.
    pub fn new(helper: &str) -> Result<Self, ClientError> {
        let home = Home::from_env()?;
        let staged: Staged = home.runtime().stage(Path::new(helper))?;
        Ok(Self {
            daemons: Daemons::one(Member::LOCAL, Daemon::local(home.clone(), staged)),
            home,
            vars: session_vars(),
        })
    }

    /// The staged build's id, `<version>+<hash>`.
    pub fn build(&self) -> String {
        self.daemon().staged.id.clone()
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
        let daemon = self.daemon().clone();
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
        let daemon = self.daemon().clone();
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
        let daemon = self.daemon().clone();
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
    daemons: Daemons,
    producer: Arc<StreamProducer<SessionListUpdate>>,
    /// Watches until dropped.
    watch: Mutex<Option<client::SessionWatch>>,
}

#[export]
impl SessionWatch {
    pub fn new(client: &Client) -> Self {
        Self {
            daemons: client.daemons.clone(),
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
        let rt: rt::Rt = runtime().clone();
        *watch = Some(client::SessionWatch::start(
            &rt,
            &self.daemons,
            move |sessions, problems| {
                producer.push(SessionListUpdate {
                    sessions: sessions.iter().map(|l| (&l.summary).into()).collect(),
                    problem: problems.first().map(|p| p.error.to_string()),
                });
            },
        ));
    }
}

/// One query's answers, as the daemon sends them, over a connection of
/// its own: `start` it, then take each answer with `next`. None is ever
/// dropped: the daemon streams no faster than they are taken. Dropping it
/// cancels the query.
pub struct QueryStream {
    asked: Mutex<Option<Asked>>,
    answers: rt::channel::Receiver<QueryAnswer>,
    cancel: Arc<Cancel>,
}

/// A query not yet asked, and where its answers go.
struct Asked {
    daemon: Daemon,
    query: (String, protocol::Scope, protocol::Json),
    answers: rt::channel::AsyncSender<QueryAnswer>,
}

/// A query's cancel, which may come before it is connected.
#[derive(Default)]
struct Cancel(Mutex<(bool, Option<client::Canceller>)>);

impl Cancel {
    fn cancel(&self) {
        let mut cancel = lock(&self.0);
        cancel.0 = true;
        if let Some(canceller) = &cancel.1 {
            let canceller = canceller.clone();
            runtime().spawn(Box::pin(async move {
                let _ = canceller.cancel_async().await;
            }));
        }
    }

    fn connected(&self, canceller: client::Canceller) {
        let mut cancel = lock(&self.0);
        if cancel.0 {
            let canceller = canceller.clone();
            runtime().spawn(Box::pin(async move {
                let _ = canceller.cancel_async().await;
            }));
        }
        cancel.1 = Some(canceller);
    }
}

#[export]
impl QueryStream {
    /// The query `query` asks, as JSON: `{"service": "…", "scope":
    /// "daemon" | {"workspace": "<dir>"} | {"session": "<id>"}, "body":
    /// …}`, the body as the query reads it.
    pub fn new(client: &Client, query: &str) -> Result<Self, ClientError> {
        let invalid = |message: String| ClientError::InvalidArgument { message };
        let mut query: serde_json::Value =
            serde_json::from_str(query).map_err(|e| invalid(e.to_string()))?;
        let service = (query["service"].as_str())
            .ok_or_else(|| invalid("a query names its service".into()))?
            .to_owned();
        let scope = serde_json::from_value(query["scope"].take())
            .map_err(|e| invalid(format!("scope: {e}")))?;
        let body = protocol::Json::encode(&query["body"]).map_err(|e| invalid(e.to_string()))?;
        // A few answers ahead of the app; past them the socket, then the
        // daemon's query, waits.
        let (sender, answers) = rt::channel::bounded_async(16);
        Ok(Self {
            asked: Mutex::new(Some(Asked {
                daemon: client.daemon().clone(),
                query: (service, scope, body),
                answers: sender,
            })),
            answers,
            cancel: Arc::default(),
        })
    }

    /// Asks it, starting the daemon if needed. Starts once.
    pub fn start(&self) {
        let Some(Asked {
            daemon,
            query: (service, scope, body),
            mut answers,
        }) = lock(&self.asked).take()
        else {
            return;
        };
        let cancel = Arc::clone(&self.cancel);
        let rt: rt::Rt = runtime().clone();
        runtime().spawn(Box::pin(async move {
            match daemon.query_async(&rt, &service, scope, body).await {
                Ok(mut read) => {
                    cancel.connected(read.canceller());
                    while let Some(answer) = read.next_async().await {
                        if answers.send(answer.into()).await.is_err() {
                            break;
                        }
                    }
                }
                Err(error) => {
                    let _ = answers
                        .send(QueryAnswer::Failed {
                            error: error.to_string(),
                        })
                        .await;
                }
            }
        }));
    }

    /// The next answer, once there is one; none after the last, which is
    /// never an update.
    pub async fn next(&self) -> Option<QueryAnswer> {
        self.answers.recv().await
    }

    /// Stops it: its last answer says it was cancelled, unless it ended
    /// first.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for QueryStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// One blob's bytes, as the daemon streams them over a connection of its
/// own: `start` it, then take each piece with `next`, in order. The daemon
/// sends no faster than they are taken. Dropping it cancels the fetch.
pub struct BlobFetch {
    asked: Mutex<Option<Fetching>>,
    pieces: rt::channel::Receiver<BlobPiece>,
    cancel: Arc<Cancel>,
}

/// A fetch not yet asked, and where its pieces go.
struct Fetching {
    daemon: Daemon,
    session: SessionId,
    blob: protocol::BlobId,
    offset: u64,
    len: Option<u64>,
    pieces: rt::channel::AsyncSender<BlobPiece>,
}

#[export]
impl BlobFetch {
    /// Blob `blob` (its id, as a `BlobRef` names it), as session `session`
    /// references it: `len` bytes from `offset`, or all to its end without
    /// `len`; clamped to the blob's end.
    pub fn new(
        client: &Client,
        session: &str,
        blob: &str,
        offset: u64,
        len: Option<u64>,
    ) -> Result<Self, ClientError> {
        let invalid = |message: String| ClientError::InvalidArgument { message };
        let session = session
            .parse()
            .map(SessionId)
            .map_err(|e| invalid(format!("session: {e}")))?;
        let blob = blob.parse().map_err(|e| invalid(format!("blob: {e}")))?;
        let (sender, pieces) = rt::channel::bounded_async(16);
        Ok(Self {
            asked: Mutex::new(Some(Fetching {
                daemon: client.daemon().clone(),
                session,
                blob,
                offset,
                len,
                pieces: sender,
            })),
            pieces,
            cancel: Arc::default(),
        })
    }

    /// Asks for it, starting the daemon if needed. Starts once.
    pub fn start(&self) {
        let Some(Fetching {
            daemon,
            session,
            blob,
            offset,
            len,
            mut pieces,
        }) = lock(&self.asked).take()
        else {
            return;
        };
        let cancel = Arc::clone(&self.cancel);
        let range = (
            std::ops::Bound::Included(offset),
            len.map_or(std::ops::Bound::Unbounded, |len| {
                std::ops::Bound::Excluded(offset.saturating_add(len))
            }),
        );
        let rt: rt::Rt = runtime().clone();
        runtime().spawn(Box::pin(async move {
            let ended = match daemon.fetch_async(&rt, session, blob, range).await {
                Ok(mut fetched) => {
                    cancel.connected(fetched.canceller());
                    let mut buf = vec![0; 256 << 10];
                    loop {
                        match fetched.read_async(&mut buf).await {
                            Ok(0) => {
                                let _ = pieces.send(BlobPiece::Done).await;
                                return;
                            }
                            Ok(n) => {
                                let bytes = buf[..n].to_vec();
                                if pieces.send(BlobPiece::Bytes { bytes }).await.is_err() {
                                    return;
                                }
                            }
                            Err(e) => break BlobPiece::from(&e),
                        }
                    }
                }
                Err(e) => BlobPiece::Failed {
                    error: e.to_string(),
                },
            };
            let _ = pieces.send(ended).await;
        }));
    }

    /// The next piece, once there is one; none after the last, which is
    /// never bytes.
    pub async fn next(&self) -> Option<BlobPiece> {
        self.pieces.recv().await
    }

    /// Stops it: its last piece then says it was cancelled, unless it
    /// ended first.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for BlobFetch {
    fn drop(&mut self) {
        self.cancel.cancel();
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
                    std::env::current_dir()?.try_into()?,
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
        .start(runtime().clone(), client.daemon().clone());
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
    #[ffi_stream(item = ChangeBatch)]
    pub fn updates(&self) -> Arc<EventSubscription<ChangeBatch>> {
        lock(&self.feed).subscribe()
    }

    /// A conservative byte charge for all view payloads received since opening.
    pub fn retained_bytes(&self) -> u64 {
        lock(&self.feed).retained_bytes as u64
    }

    pub fn details(&self, key: u64) -> Option<String> {
        lock(&self.feed).details(key)
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
                let dir = HostPath::from(workspace);
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
                workspace: Some(HostPath::from(workspace)),
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

        let (_session, home, root, listener, _owner) = fixture();
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
        assert_eq!(rt::block_on(reading).pid, Some(std::process::id()));
        drop(listener);
        home.endpoint().remove();
        std::fs::remove_dir_all(root).unwrap();
    }

    fn fixture() -> (
        Session,
        Home,
        std::path::PathBuf,
        sys::Listener,
        std::fs::File,
    ) {
        let (client, home, root, listener, owner) = client_fixture();
        (Session::new(&client), home, root, listener, owner)
    }

    /// A client of a daemon that is this test: it holds the lock and
    /// listens, and the test answers for it.
    fn client_fixture() -> (
        Client,
        Home,
        std::path::PathBuf,
        sys::Listener,
        std::fs::File,
    ) {
        let root = tempfile::Builder::new()
            .prefix("a-ffi-order-")
            .tempdir()
            .unwrap()
            .keep();
        let home = Home::from(root.join("home"));
        let exe = root.join("a");
        std::fs::write(&exe, "never run").unwrap();
        let staged = home.runtime().stage(&exe).unwrap();
        std::fs::create_dir_all(home.run_dir()).unwrap();
        let owner = std::fs::File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(home.daemon_pid_file())
            .unwrap();
        sys::fs::lock(&owner).unwrap();
        std::io::Write::write_all(
            &mut &owner,
            sys::crash::Record::current("test").to_string().as_bytes(),
        )
        .unwrap();
        let listener = home.bind().unwrap();
        let client = Client {
            daemons: Daemons::one(Member::LOCAL, Daemon::local(home.clone(), staged)),
            home: home.clone(),
            vars: Vec::new(),
        };
        (client, home, root, listener, owner)
    }

    #[test]
    fn a_query_loses_no_answer_and_a_cancel_reaches_the_daemon() {
        let (client, home, root, listener, _owner) = client_fixture();
        let query = QueryStream::new(
            &client,
            r#"{"service":"echo","scope":{"workspace":"/w"},"body":{"text":"hi"}}"#,
        )
        .unwrap();
        query.start();
        let mut peer = listener.accept().unwrap();
        assert!(matches!(
            read_frame::<ClientMsg>(&mut peer).unwrap(),
            Some(ClientMsg::Hello { .. })
        ));
        let welcome = DaemonMsg::Welcome {
            proto: protocol::PROTO,
            build: "test".into(),
            caps: Caps::ALL,
            machine: protocol::MachineId([0; 16]),
            commands: Vec::new(),
            queries: Vec::new(),
            upgrade: None,
        };
        write_frame(&mut peer, &welcome).unwrap();
        let Some(ClientMsg::Query {
            id, service, scope, ..
        }) = read_frame::<ClientMsg>(&mut peer).unwrap()
        else {
            panic!("no query");
        };
        assert_eq!(service, "echo");
        assert_eq!(scope, protocol::Scope::Workspace("/w".into()));
        // More than the stream holds ahead of the app, which has taken none.
        let body = protocol::Json::encode("hi").unwrap();
        for _ in 0..40 {
            let body = body.clone();
            write_frame(&mut peer, &DaemonMsg::Update { id, body }).unwrap();
        }
        query.cancel();
        assert!(matches!(
            read_frame::<ClientMsg>(&mut peer).unwrap(),
            Some(ClientMsg::Cancel { id: cancelled }) if cancelled == id
        ));
        let outcome = protocol::Outcome::Interrupted { output: None };
        write_frame(&mut peer, &DaemonMsg::Reply { id, outcome }).unwrap();
        let got: Vec<_> = (0..42).map(|_| rt::block_on(query.next())).collect();
        let update = QueryAnswer::Update {
            body: r#""hi""#.into(),
        };
        assert!(
            got[..40]
                .iter()
                .all(|answer| answer.as_ref() == Some(&update))
        );
        assert_eq!(got[40..], [Some(QueryAnswer::Cancelled), None]);
        drop(listener);
        home.endpoint().remove();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_fetch_hands_the_app_each_chunk_in_order_then_done() {
        let (client, home, root, listener, _owner) = client_fixture();
        let (session, blob) = (SessionId::new(), protocol::BlobId([3; 32]));
        let fetch =
            BlobFetch::new(&client, &session.to_string(), &blob.to_string(), 2, Some(5)).unwrap();
        fetch.start();
        let mut peer = listener.accept().unwrap();
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        write_frame(&mut peer, &welcome()).unwrap();
        let asked = read_frame::<ClientMsg>(&mut peer).unwrap();
        let Some(ClientMsg::Fetch {
            id,
            session: of,
            blob: wanted,
            offset: 2,
            len: Some(5),
        }) = asked
        else {
            panic!("{asked:?}");
        };
        assert_eq!((of, wanted), (session, blob));
        for (offset, bytes) in [(2, &b"abc"[..]), (5, b"de")] {
            let chunk = protocol::Chunk {
                blob,
                offset,
                bytes,
            };
            let frame = protocol::Frame::chunk(chunk).unwrap();
            std::io::Write::write_all(&mut peer, frame.as_bytes()).unwrap();
        }
        let output = protocol::Json::encode(&5).unwrap();
        let outcome = protocol::Outcome::Done { output };
        write_frame(&mut peer, &DaemonMsg::Reply { id, outcome }).unwrap();
        let got: Vec<_> = (0..4).map(|_| rt::block_on(fetch.next())).collect();
        let bytes = |b: &[u8]| Some(BlobPiece::Bytes { bytes: b.to_vec() });
        assert_eq!(
            got,
            [bytes(b"abc"), bytes(b"de"), Some(BlobPiece::Done), None]
        );
        drop(listener);
        home.endpoint().remove();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_fetch_takes_no_byte_past_what_it_asked_for() {
        let (client, home, root, listener, _owner) = client_fixture();
        let (session, blob) = (SessionId::new(), protocol::BlobId([3; 32]));
        let fetch =
            BlobFetch::new(&client, &session.to_string(), &blob.to_string(), 0, Some(2)).unwrap();
        fetch.start();
        let mut peer = listener.accept().unwrap();
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        write_frame(&mut peer, &welcome()).unwrap();
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        let chunk = protocol::Chunk {
            blob,
            offset: 0,
            bytes: b"abc",
        };
        let frame = protocol::Frame::chunk(chunk).unwrap();
        std::io::Write::write_all(&mut peer, frame.as_bytes()).unwrap();
        let Some(BlobPiece::Failed { error }) = rt::block_on(fetch.next()) else {
            panic!("bytes past the asked length were taken");
        };
        assert!(error.contains("3 bytes at 0"), "{error}");
        assert_eq!(rt::block_on(fetch.next()), None);
        drop(listener);
        home.endpoint().remove();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_refused_fetch_reads_as_not_held() {
        let (client, home, root, listener, _owner) = client_fixture();
        let fetch = BlobFetch::new(
            &client,
            &SessionId::new().to_string(),
            &protocol::BlobId([3; 32]).to_string(),
            0,
            None,
        )
        .unwrap();
        fetch.start();
        let mut peer = listener.accept().unwrap();
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        write_frame(&mut peer, &welcome()).unwrap();
        let Some(ClientMsg::Fetch { id, .. }) = read_frame::<ClientMsg>(&mut peer).unwrap() else {
            panic!("no fetch");
        };
        write_frame(&mut peer, &DaemonMsg::NotHeld { id }).unwrap();
        assert_eq!(rt::block_on(fetch.next()), Some(BlobPiece::NotHeld));
        assert_eq!(rt::block_on(fetch.next()), None);
        drop(listener);
        home.endpoint().remove();
        std::fs::remove_dir_all(root).unwrap();
    }

    fn welcome() -> DaemonMsg {
        DaemonMsg::Welcome {
            proto: protocol::PROTO,
            build: "test".into(),
            caps: Caps::ALL,
            machine: protocol::MachineId([0; 16]),
            commands: Vec::new(),
            queries: Vec::new(),
            upgrade: None,
        }
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
        let (session, home, root, listener, _owner) = fixture();
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
            machine: protocol::MachineId([0; 16]),
            commands: Vec::new(),
            queries: Vec::new(),
            upgrade: None,
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
                workspace: root.clone().try_into().unwrap(),
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
        write_frame(&mut peer, &DaemonMsg::Draining { held: None }).unwrap();
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
        let (session, home, root, listener, _owner) = fixture();
        let id = SessionId::new();
        session.open(OpenTarget::Id { id: id.to_string() }).unwrap();
        let mut peer = listener.accept().unwrap();
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        read_frame::<ClientMsg>(&mut peer).unwrap().unwrap();
        let welcome = DaemonMsg::Welcome {
            proto: protocol::PROTO,
            build: "test".into(),
            caps: Caps::ALL,
            machine: protocol::MachineId([0; 16]),
            commands: Vec::new(),
            queries: Vec::new(),
            upgrade: None,
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
                workspace: root.clone().try_into().unwrap(),
            },
        )
        .unwrap();
        write_frame(&mut peer, &DaemonMsg::Draining { held: None }).unwrap();
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
