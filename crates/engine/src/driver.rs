//! A live session: one task, its actor, owns the session's kernel and its
//! log's writer, carries out the effects each step returns and feeds back
//! what they report. Clients, calls, model replies and other sessions
//! reach it only through its mailbox, so a log has one writer, entries
//! commit in the order the kernel numbers them, and no lock is held
//! across an await. What the driver decides is only how: the kernel
//! decides what.

use std::any::Any;
use std::cell::RefCell;
use std::future::{Future, poll_fn};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::task::Poll;
use std::time::Instant;

use crate::catalog::Record;
use crate::store::{Encoded, Header, Loaded, Parent, Snapshot, Writer};
use ext::host::{Binding, Bound, Host, Parts, Registry, Services};
use ext::point::View;
use ext::{BoxFuture, Called, Cancel, Cx, Flag, States};
use kernel::{Core, Durability, Effect, Effects, Event, Load, Note, Owner, Replied, Ticket};
use protocol::ledger::Ledger;
use protocol::view::SessionView;
use protocol::{
    Author, Body, CallId, DaemonMsg, Entry, Environment, Frame, Json, Mode, Outcome, ReadOnly,
    Residency, SessionId, SessionSummary, ToolChoice,
};
use rt::{channel, oneshot, or};
use serde::{Deserialize, Serialize};

use crate::model::{Model, Models};
use crate::outbox::{FrameLog, Outbox};
use crate::transcript::Transcript;
use crate::{Daemon, lock};

/// What every session runs with.
pub struct Shared {
    /// Where actors and effects run, sleep and block.
    pub rt: rt::Rt,
    pub store: Arc<dyn crate::store::LogStore>,
    /// How every session lists, and which are busy.
    pub catalog: crate::catalog::Catalog,
    /// The extensions, as each session takes them up at its next step.
    pub registry: ext::host::Current,
    pub models: Models,
    /// Where every session's commits are barriers.
    pub barriers: kernel::Barriers,
    /// This build: what a snapshot must be of to be restored.
    pub build: String,
    /// Where sessions keep output too large for their logs.
    pub spill: PathBuf,
    /// Sessions loaded since the daemon started.
    pub(crate) loaded: Mutex<std::collections::HashSet<SessionId>>,
    pub(crate) daemon: Weak<Daemon>,
}

impl Shared {
    fn source(&self, workspace: &Path) -> io::Result<Option<Arc<dyn ext::host::Generations>>> {
        self.registry
            .get()
            .service::<dyn ext::host::Sources>()
            .map(|source| {
                let rt = Arc::clone(&self.rt);
                source.open(workspace, Arc::new(move |work| rt.spawn_blocking(work)))
            })
            .transpose()
            .map_err(io::Error::other)
    }
}

/// How [`Live::start`] opens a session's log.
#[derive(Clone, Copy)]
pub enum Open {
    Create,
    Load,
}

/// What a session's calls and decision points read: made before its log
/// is folded, as the kernel's registry binds it.
struct Session {
    id: SessionId,
    workspace: Arc<Path>,
    /// The session's directory for output too large for its log.
    spill: Arc<Path>,
    mail: Mail,
    /// The extensions' state: folded as each entry commits.
    states: Mutex<Arc<Mutex<States>>>,
    /// What commands run in: the last client's, secrets included, or after
    /// a restart the last one logged. One allocation per change, shared by
    /// every call made while it stands.
    env: Mutex<Option<Arc<Environment>>>,
    /// Background process groups the session owns, until each ends.
    jobs: Mutex<Vec<u32>>,
    /// What stops each open call's work, one per call for as long as it
    /// is open: its run, its gate, and whatever a handler deciding about
    /// it took its context along to, all end with it.
    cancels: Mutex<Keyed<CallId, Cancel>>,
    /// Notified as the session changes: a commit, work beginning or
    /// ending, a job ending, the actor ending. What waits on the session
    /// waits here, never by polling.
    changed: rt::Notify,
    /// What the engine provides the session's calls, made when one first
    /// asks.
    services: OnceLock<Arc<Services>>,
    shared: Arc<Shared>,
}

type Mail = Arc<channel::Sender<Msg>>;

/// A loaded session, as clients and the daemon reach it.
pub struct Live {
    session: Arc<Session>,
    pub id: SessionId,
    pub workspace: PathBuf,
    pub parent: Option<Parent>,
    /// The committed entries, encoded once for every client.
    frames: Arc<FrameLog>,
    followers: Mutex<Vec<Outbox>>,
    /// Whether work is in flight, as the kernel marks it.
    busy: AtomicBool,
    /// Whether the log holds any input: a child's first comes from its
    /// parent, once.
    has_input: AtomicBool,
    attached: AtomicUsize,
    last_active: Mutex<Instant>,
    summary: Mutex<SessionSummary>,
    /// The end of the session's actor: set once its writer is closed, so
    /// a reload never makes a second.
    pub(crate) ended: Flag,
}

/// What a side request is answered with: its id and the conversation it
/// asked for, or `None` when it may not be made.
type Opened = Option<(CallId, llm::Messages)>;

/// A client's mutation of a session.
pub enum Request {
    Input {
        text: String,
        mode: Mode,
    },
    Interrupt,
    /// The answer to what call `call` waits on.
    Answer {
        call: CallId,
        value: Json,
    },
}

enum Msg {
    Request {
        request: Request,
        origin: Option<Author>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Step(Event),
    /// A client to follow the session, answered once it does.
    Attach(Outbox, u64, oneshot::Sender<()>),
    Env(Arc<Environment>),
    /// A running call's report, answered once committed.
    Note {
        call: CallId,
        note: Note,
        done: Option<oneshot::Sender<()>>,
    },
    /// Call `call` starts a child on `input` and waits for its result.
    Child {
        call: CallId,
        input: String,
        reply: oneshot::Sender<Json>,
    },
    Replied {
        call: CallId,
        replied: Replied,
        message: Option<llm::Assistant>,
    },
    /// A side model request on behalf of call `of`, answered with its id
    /// once its intent is committed, and if asked, the conversation the
    /// model sees before `of`.
    Side {
        of: CallId,
        purpose: String,
        model: String,
        conversation: bool,
        reply: oneshot::Sender<Opened>,
    },
    Stop,
    /// Make what was only written durable, and show it: sent to itself
    /// [`SYNC_LAG`] after a write that was not synced.
    Sync,
    /// Answered once every message before it is taken.
    #[cfg(test)]
    Barrier(oneshot::Sender<()>),
}

impl Msg {
    /// Whether taking it reads what the session committed: a client's
    /// replay, the conversation a side request asks for.
    const fn reads_the_log(&self) -> bool {
        matches!(self, Self::Attach(..) | Self::Side { .. })
    }
}

/// A client's hold on a session; dropping it detaches.
pub struct Attachment(Arc<Live>);

impl Attachment {
    pub fn session(&self) -> &Arc<Live> {
        &self.0
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.0.attached.fetch_sub(1, Ordering::SeqCst);
        self.0.touch();
    }
}

impl Live {
    /// Opens session `header`'s log, folds it into a kernel and starts the
    /// actor, which resumes it: the only way a session recovers. It blocks
    /// on the store, so it runs on a thread that may.
    pub(crate) fn start(
        header: &Header,
        shared: &Arc<Shared>,
        open: Open,
        owner: Owner,
    ) -> io::Result<Arc<Self>> {
        let base = shared.registry.get();
        let source = shared.source(&header.workspace)?;
        let generation = source.as_ref().map_or_else(
            || shared.registry.generation(),
            |source| source.generation(),
        );
        let registry = source.as_ref().map_or(base, |source| source.registry());
        let (tx, rx) = channel::unbounded();
        let session = Arc::new(Session {
            id: header.id,
            workspace: Arc::from(header.workspace.as_path()),
            spill: Arc::from(shared.spill.join(header.id.to_string())),
            mail: Arc::new(tx),
            states: Mutex::new(Arc::new(Mutex::new(registry.states()))),
            env: Mutex::default(),
            jobs: Mutex::default(),
            cancels: Mutex::default(),
            changed: rt::Notify::new(),
            services: OnceLock::new(),
            shared: Arc::clone(shared),
        });
        let bound = bind(&registry, &session);
        let binding = Arc::clone(&bound) as Arc<dyn Binding>;
        let (loaded, folding) = Folding::open(header, shared, open, &binding, &registry)?;
        let Folding {
            mut core,
            states,
            snapshot: fold_snapshot,
            has_input,
        } = folding;
        *lock(&session.state_view()) = states;
        *lock(&session.env) = core.ledger().env().cloned().map(Arc::new);
        let fresh = lock(&shared.loaded).insert(header.id);
        let listed = (shared.catalog.get(header.id)).unwrap_or_else(|| Record::new(header.clone()));
        let busy = listed.busy;
        let load = Load {
            boot: sys::boot_id(),
            fresh,
            owner,
            busy,
            created: matches!(open, Open::Create),
        };
        // A log kept as it is is shown, never resumed or stepped: nothing
        // would commit.
        let read_only = loaded.writer.read_only();
        let first = match read_only {
            None => core.resumed(&load),
            Some(_) => Effects::new(),
        };
        let live = Arc::new(Self {
            id: header.id,
            workspace: header.workspace.clone(),
            parent: header.parent,
            frames: Arc::new(FrameLog::new(loaded.lines, loaded.seq)),
            followers: Mutex::default(),
            busy: AtomicBool::new(busy && read_only.is_none()),
            has_input: AtomicBool::new(has_input),
            attached: AtomicUsize::new(0),
            last_active: Mutex::new(Instant::now()),
            summary: Mutex::new(SessionSummary {
                read_only,
                ..summary(header, core.ledger())
            }),
            ended: Flag::default(),
            session: Arc::clone(&session),
        });
        registry.hooks().session_start(&bound.point());
        let actor = Actor {
            live: Arc::clone(&live),
            core,
            writer: Some(loaded.writer),
            registry,
            bound,
            source,
            generation,
            pending: None,
            transcript: None,
            replied: None,
            snapshot: None,
            fold_snapshot,
            inflight: Keyed::default(),
            view: None,
            replies: Keyed::default(),
            sides: Keyed::default(),
            tickets: 0,
            phase: Phase::Serving,
            listed,
            unsynced: Vec::new(),
            waiting_sync: Vec::new(),
            sync_armed: false,
            resuming: false,
            since_snapshot: loaded.read,
        };
        shared.rt.spawn(Box::pin(actor.run(rx, first)));
        Ok(live)
    }

    /// Opens a client attachment.
    pub fn open(self: &Arc<Self>) -> Attachment {
        self.attached.fetch_add(1, Ordering::SeqCst);
        self.touch();
        Attachment(Arc::clone(self))
    }

    /// Asks the actor for `request`, and returns once what it logged is
    /// committed: `Err` is why it was refused.
    pub async fn request(&self, request: Request, origin: Option<Author>) -> Result<(), String> {
        self.ask(request, origin).await
    }

    /// Sends the actor `request` now, in order with what was sent before
    /// it; the future is its answer.
    pub fn ask(
        &self,
        request: Request,
        origin: Option<Author>,
    ) -> impl Future<Output = Result<(), String>> + Send + 'static {
        let (reply, answer) = oneshot::channel();
        self.send(Msg::Request {
            request,
            origin,
            reply,
        });
        async move {
            answer
                .await
                .unwrap_or_else(|| Err("the session stopped".to_owned()))
        }
    }

    /// Sends `client`, which has the entries up to `since`, what it missed
    /// and then everything live; it returns once that is queued, ahead of
    /// what is sent the client after.
    pub async fn attach(&self, client: Outbox, since: u64) {
        let (done, attached) = oneshot::channel();
        self.send(Msg::Attach(client, since, done));
        attached.await;
    }

    /// Runs commands from now on in `env`, the newest client's; its
    /// redacted form is logged unless it is the one logged last.
    pub fn set_env(&self, env: impl Into<Arc<Environment>>) {
        self.send(Msg::Env(env.into()));
    }

    /// Hands the session child `child`'s result.
    pub(crate) fn settled(&self, child: SessionId, outcome: Outcome) {
        self.send(Msg::Step(Event::ChildSettled { child, outcome }));
    }

    /// Interrupts the turn, from inside the daemon.
    pub(crate) fn interrupt(&self) {
        let interrupt = Event::Interrupt {
            origin: None,
            ticket: None,
        };
        self.send(Msg::Step(interrupt));
    }

    /// Stops the actor: what runs is cancelled, and what waits stays in
    /// the log for the next load.
    pub(crate) fn stop(&self) {
        self.send(Msg::Stop);
        for &pgid in lock(&self.session.jobs).iter() {
            let _ = sys::Group(pgid).kill();
        }
    }

    fn send(&self, msg: Msg) {
        let _ = self.session.mail.send(msg);
    }

    /// Whether the log holds any input: a child's first comes from its
    /// parent, once.
    pub fn has_input(&self) -> bool {
        self.has_input.load(Ordering::SeqCst)
    }

    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            clients: u32::try_from(self.attached.load(Ordering::SeqCst)).unwrap_or(u32::MAX),
            ..lock(&self.summary).clone()
        }
    }

    pub fn has_work(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    pub fn has_jobs(&self) -> bool {
        !lock(&self.session.jobs).is_empty()
    }

    /// How long the session has had no attachments, no work and no jobs.
    pub fn idle_for(&self) -> std::time::Duration {
        if self.attached.load(Ordering::SeqCst) > 0 || self.has_work() || self.has_jobs() {
            return std::time::Duration::ZERO;
        }
        lock(&self.last_active).elapsed()
    }

    /// Notes activity, and tells the supervisor.
    fn touch(&self) {
        *lock(&self.last_active) = Instant::now();
        if let Some(daemon) = self.session.shared.daemon.upgrade() {
            daemon.pulse.notify();
        }
    }
}

/// A session's log being folded as it loads: the kernel and the
/// extensions' states, from a snapshot or from nothing, then an entry at a
/// time.
struct Folding {
    core: Core,
    states: States,
    snapshot: Option<(u64, Vec<ext::Image>)>,
    /// Whether the log holds any input.
    has_input: bool,
}

impl Folding {
    /// Creates session `header`'s log, or loads it: from the newest
    /// snapshot if it is of this build and these extensions, else from
    /// nothing; then each entry read after it is folded and dropped.
    fn open(
        header: &Header,
        shared: &Shared,
        open: Open,
        binding: &Arc<dyn Binding>,
        registry: &Registry,
    ) -> io::Result<(Loaded, Self)> {
        let folding = RefCell::new(Self {
            core: Core::new(Arc::clone(binding)).with_barriers(shared.barriers),
            states: registry.states(),
            snapshot: None,
            has_input: false,
        });
        let loaded = match open {
            Open::Create => {
                let created = shared.store.create(header)?;
                shared.catalog.record(Record::new(header.clone()));
                created
            }
            Open::Load => shared.store.load(
                header.id,
                &mut |snapshot| {
                    let restored =
                        (folding.borrow_mut()).restore(&snapshot, binding, registry, shared);
                    restored
                        .inspect_err(|e| {
                            crate::session_event("snapshot", header.id, || {
                                format!("not restored: {e}")
                            });
                        })
                        .is_ok()
                },
                &mut |e| folding.borrow_mut().fold(&e),
            )?,
        };
        Ok((loaded, folding.into_inner()))
    }

    /// Starts from `snapshot`, if it is an [`Image`] this build made with
    /// the extensions installed now: else nothing changes, and the log is
    /// folded from its first entry.
    fn restore(
        &mut self,
        snapshot: &Snapshot,
        binding: &Arc<dyn Binding>,
        registry: &Registry,
        shared: &Shared,
    ) -> Result<(), String> {
        let image: Image = snapshot.image.parse().map_err(|e| e.to_string())?;
        if image.build != shared.build {
            return Err(format!("of build {}", image.build));
        }
        if image.extensions != registry.installed() {
            return Err("of other extensions".to_owned());
        }
        self.states.restore(&image.states)?;
        self.snapshot = Some((snapshot.covers, image.states));
        let core = Core::restore(Arc::clone(binding), image.ledger);
        self.core = core.with_barriers(shared.barriers);
        self.has_input = image.has_input;
        Ok(())
    }

    fn fold(&mut self, e: &Entry) {
        self.has_input |= matches!(e.body, Body::Input { .. });
        self.states.apply(e);
        self.core.load(e);
    }
}

/// What a snapshot of a session keeps: the ledger and the extensions'
/// states as they stood at rest, which is all a session at rest is
/// ([`Core::restore`]), under what made them. A load that cannot read
/// one, or one of another build or other extensions, folds the log
/// instead: the snapshot is derived, and only ever spares the fold.
#[derive(Serialize, Deserialize)]
struct Image {
    /// The build that took it: the ledger's shape, and what a fold makes
    /// of an entry, are a build's own.
    build: String,
    extensions: Vec<protocol::Installed>,
    has_input: bool,
    ledger: Ledger,
    states: Vec<ext::Image>,
}

/// How much log, in bytes, a load may read past the newest snapshot: at
/// rest with more, a session takes one. Folding this much costs about as
/// much as taking one.
const SNAPSHOT_EVERY: u64 = 64 << 10;

fn summary(header: &Header, ledger: &Ledger) -> SessionSummary {
    SessionSummary {
        id: header.id,
        workspace: header.workspace.clone(),
        entries: ledger.seq(),
        clients: 0,
        activity: ledger.activity(),
        residency: Residency::Loaded,
        read_only: None,
    }
}

struct Actor {
    live: Arc<Live>,
    core: Core,
    /// `None` only while a commit has it on a blocking thread.
    writer: Option<Writer>,
    /// The registry generation the newest step ran on.
    registry: Arc<Registry>,
    /// The same, bound to the session: what the kernel drives.
    bound: Arc<Bound<Viewed>>,
    source: Option<Arc<dyn ext::host::Generations>>,
    generation: u64,
    pending: Option<Arc<Bound<Viewed>>>,
    /// The conversation the model sees, made at the first model call.
    transcript: Option<Transcript>,
    /// The message of the reply model call `.0` just had, until its
    /// result commits.
    replied: Option<(CallId, llm::Assistant)>,
    /// The last snapshot made, and its seq; `None` for one too large.
    snapshot: Option<(u64, Option<Frame>)>,
    fold_snapshot: Option<(u64, Vec<ext::Image>)>,
    /// What each open call has in flight, one thing at a time.
    inflight: Keyed<CallId, InFlight>,
    /// The view a far attach is sent, kept in step with each commit from
    /// the first time one is asked for: no attach folds the whole log.
    view: Option<SessionView>,
    replies: Keyed<Ticket, oneshot::Sender<Result<(), String>>>,
    /// Side model requests waiting for their call's id, with the
    /// conversation they asked for.
    sides: Keyed<Ticket, (oneshot::Sender<Opened>, Option<llm::Messages>)>,
    tickets: u64,
    phase: Phase,
    /// The session as the catalog last heard of it: busy while work is in
    /// flight, or a call waits on someone. Set before work starts, cleared
    /// after it ends.
    listed: Record,
    /// Entries written and not yet durable, in order, with whoever waits
    /// for each: shown to no one until a sync. A seq a client has seen is
    /// one that survives a power cut, so it is never given out twice.
    unsynced: Vec<(Vec<Entry>, Encoded)>,
    waiting_sync: Vec<oneshot::Sender<()>>,
    /// A [`Msg::Sync`] is on its way.
    sync_armed: bool,
    /// Carrying out what a resume asked for: a commit that fails now does
    /// not resume again, the session stops.
    resuming: bool,
    /// The bytes of log the next load would read past the newest
    /// snapshot: once [`SNAPSHOT_EVERY`], the next rest is snapshotted.
    since_snapshot: u64,
}

/// How long written entries may stay unsynced, and so unseen: a sync is
/// asked for this long after a write that none followed.
const SYNC_LAG: std::time::Duration = std::time::Duration::from_millis(50);

/// What a call has in flight: the one thing it waits on now.
enum InFlight {
    /// A model call's request, prepared and not yet sent: the step point's
    /// decisions about it are logged in between. `Err` is why there is no
    /// model to send it to.
    Prepared(Box<Result<(Model, llm::Request, u16), String>>),
    /// A model call sent; it reports once, with [`Msg::Replied`].
    Model,
    /// A tool call running; it reports once, with [`Event::ToolDone`].
    Tool(Child),
    /// A gate, a recovery or a sleep, answered by an event of its own.
    Later,
}

/// Where a running tool call stands with the child it asks for.
enum Child {
    None,
    /// It asked, and waits for the result.
    Waiting(oneshot::Sender<Json>),
    /// Run again with the answer it waited on, to hand it when it asks.
    Answered(Json),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Serving,
    /// Stopping: what runs is cancelled, nothing starts, and the actor ends
    /// once nothing in flight has to report.
    Draining,
}

impl Actor {
    /// Tool and model calls in flight, each of which reports once.
    fn reporting(&self) -> usize {
        (self.inflight.values())
            .filter(|f| matches!(f, InFlight::Model | InFlight::Tool(_)))
            .count()
    }

    async fn refresh(&mut self) -> Option<Event> {
        let current = &self.live.session.shared.registry;
        let number = self
            .source
            .as_ref()
            .map_or_else(|| current.generation(), |source| source.generation());
        if number == self.generation {
            return None;
        }
        self.generation = number;
        let newest = self
            .source
            .as_ref()
            .map_or_else(|| current.get(), |source| source.registry());
        let frames = Arc::clone(&self.live.frames);
        let pending: Vec<Entry> = self
            .unsynced
            .iter()
            .flat_map(|(entries, _)| entries.iter().cloned())
            .collect();
        let registry = Arc::clone(&newest);
        let snapshot = self.fold_snapshot.clone();
        let boundary = self.core.ledger().seq();
        let prepared = rt::unblock(&*self.live.session.shared.rt, move || {
            let start = Instant::now();
            let mut states = registry.states();
            let mut after = 0;
            if let Some((seq, images)) = snapshot {
                states.restore(&images)?;
                after = seq;
            }
            let mut seen = after;
            for (count, entry) in frames.entries_after(after).chain(pending).enumerate() {
                if count >= 100_000 || start.elapsed() > std::time::Duration::from_secs(1) {
                    return Err("registry state preparation budget exhausted".to_owned());
                }
                if entry.seq.get() != seen + 1 {
                    return Err("registry log boundary is incomplete".to_owned());
                }
                seen = entry.seq.get();
                states.try_apply(&entry)?;
            }
            if seen != boundary || start.elapsed() > std::time::Duration::from_secs(1) {
                return Err("registry log boundary is incomplete or over budget".to_owned());
            }
            Ok::<_, String>(states)
        })
        .await;
        match prepared {
            Ok(states) => {
                let next = bind_states(&newest, &self.live.session, Arc::new(Mutex::new(states)));
                self.pending = Some(Arc::clone(&next));
                Some(Event::Registry(next as Arc<dyn Binding>))
            }
            Err(error) => {
                eprintln!("warning: {error}");
                None
            }
        }
    }

    fn adopted(&mut self) {
        if self
            .pending
            .as_ref()
            .is_some_and(|bound| bound.view().active.load(Ordering::Acquire))
        {
            self.bound = self.pending.take().expect("prepared binding");
            self.registry = Arc::clone(self.bound.registry());
        }
    }

    async fn run(mut self, mail: channel::Receiver<Msg>, first: Effects) {
        let _ = self.carry(first).await;
        let mut next = None;
        while !(self.phase == Phase::Draining && self.reporting() == 0)
            && let Some(msg) = match next.take() {
                Some(msg) => Some(msg),
                None => mail.recv().await,
            }
        {
            // What queued meanwhile is a burst, stepped together and
            // committed once. One that reads what is committed starts a
            // burst of its own, once what came before it is carried out.
            let mut burst = vec![msg];
            while burst.len() < BURST
                && let Some(msg) = mail.try_recv()
            {
                if msg.reads_the_log() {
                    next = Some(msg);
                    break;
                }
                burst.push(msg);
            }
            let (mut events, mut dones) = (Vec::new(), Vec::new());
            let mut sync = false;
            for msg in burst {
                // Answered once what came before it is carried out.
                #[cfg(test)]
                if let Msg::Barrier(done) = msg {
                    dones.push(done);
                    continue;
                }
                if let Msg::Sync = msg {
                    sync = true;
                    continue;
                }
                // Kept as it is, its catalog listing and all.
                if let Some(why) = self.writer.as_ref().and_then(Writer::read_only) {
                    self.refuse(msg, why);
                    continue;
                }
                if let Some((event, done)) = self.take(msg) {
                    events.push(event);
                    dones.extend(done);
                }
            }
            // Recovery does not commit the burst that failed.
            match self.burst(events, sync).await {
                Ok(()) if self.unsynced.is_empty() => {
                    for done in dones {
                        done.send(());
                    }
                }
                Ok(()) => self.waiting_sync.extend(dones),
                Err(()) => drop(dones),
            }
            // A call may begin to wait without the work marked ending.
            self.mark().await;
            self.publish();
            self.live.session.changed.notify();
            if self.since_snapshot >= SNAPSHOT_EVERY {
                self.keep_snapshot().await;
            }
        }
        let _ = self.sync().await;
        if self.since_snapshot >= SNAPSHOT_EVERY {
            self.keep_snapshot().await;
        }
        // Listed as it ends, with all it logged.
        let listing = self.listing(self.live.has_work());
        let shared = Arc::clone(&self.live.session.shared);
        rt::unblock(&*Arc::clone(&shared.rt), move || {
            shared.catalog.record(listing);
        })
        .await;
        self.registry.hooks().session_end(&self.bound.point());
        // Closed and let go before the end is told: a reload never makes
        // a second writer, and what is told the end holds the last of it.
        let (ended, daemon) = (
            self.live.ended.clone(),
            self.live.session.shared.daemon.clone(),
        );
        let session = Arc::clone(&self.live.session);
        drop((self, mail));
        ended.set();
        session.changed.notify();
        if let Some(daemon) = daemon.upgrade() {
            daemon.ended(session.id);
        }
    }

    /// What `msg` asks the kernel, and whom to tell once it is carried
    /// out; `None` for what the actor answers itself.
    fn take(&mut self, msg: Msg) -> Option<(Event, Option<oneshot::Sender<()>>)> {
        let event = match msg {
            Msg::Request {
                request,
                origin,
                reply,
            } => self.requested(request, origin, reply),
            Msg::Step(event) => event,
            Msg::Attach(client, since, done) => {
                self.attach(&client, since);
                done.send(());
                return None;
            }
            Msg::Env(env) => {
                let logged = env.redacted();
                *lock(&self.live.session.env) = Some(env);
                Event::Environment(logged)
            }
            Msg::Note { call, note, done } => return Some((Event::Note { call, note }, done)),
            Msg::Child { call, input, reply } => {
                // Only a running tool call asks; one run again with the
                // answer it waited on is handed it.
                let Some(InFlight::Tool(child)) = self.inflight.get_mut(&call) else {
                    return None;
                };
                match std::mem::replace(child, Child::Waiting(reply)) {
                    Child::Answered(value) => {
                        let Child::Waiting(reply) = std::mem::replace(child, Child::None) else {
                            unreachable!()
                        };
                        reply.send(value);
                        return None;
                    }
                    Child::None | Child::Waiting(_) => {}
                }
                Event::Spawn {
                    call,
                    child: SessionId::new(),
                    input,
                    wait: true,
                }
            }
            Msg::Replied {
                call,
                replied,
                message,
            } => {
                self.inflight.remove(&call);
                self.replied = message.map(|m| (call, m));
                Event::ModelDone { call, replied }
            }
            Msg::Side {
                of,
                purpose,
                model,
                conversation,
                reply,
            } => {
                let conversation = conversation.then(|| self.conversation(of));
                self.tickets += 1;
                let ticket = Ticket(self.tickets);
                self.sides.insert(ticket, (reply, conversation));
                Event::Side {
                    of,
                    purpose,
                    model,
                    ticket,
                }
            }
            #[cfg(test)]
            Msg::Barrier(done) => {
                done.send(());
                return None;
            }
            Msg::Sync => return None,
            Msg::Stop => {
                self.phase = Phase::Draining;
                self.live.session.cancel_all();
                return None;
            }
        };
        // What was in flight reported, or was answered.
        if let Event::ToolDone { call, .. }
        | Event::Gated { call, .. }
        | Event::Recovered { call, .. }
        | Event::Woke { call } = &event
        {
            self.inflight.remove(call);
        }
        Some((event, None))
    }

    /// What `msg` gets in a session whose log is kept as it is: clients
    /// are shown it, and what would be logged is refused or dropped.
    fn refuse(&mut self, msg: Msg, why: ReadOnly) {
        match msg {
            Msg::Request { reply, .. } => reply.send(Err(why.to_string())),
            #[cfg(test)]
            Msg::Barrier(_) => drop(self.take(msg)),
            Msg::Attach(..) | Msg::Stop => drop(self.take(msg)),
            _ => {}
        }
    }

    /// What a client's `request` asks the kernel, numbered so its reply
    /// finds it.
    fn requested(
        &mut self,
        request: Request,
        origin: Option<Author>,
        reply: oneshot::Sender<Result<(), String>>,
    ) -> Event {
        self.tickets += 1;
        let ticket = Some(Ticket(self.tickets));
        self.replies.insert(Ticket(self.tickets), reply);
        match request {
            Request::Input { text, mode } => Event::Input {
                text,
                mode,
                origin,
                ticket,
            },
            Request::Interrupt => Event::Interrupt { origin, ticket },
            Request::Answer { call, value } => Event::Answer {
                call,
                value,
                origin,
                ticket,
            },
        }
    }

    /// Recovery cannot turn a failed burst into an acknowledgement.
    async fn burst(&mut self, events: Vec<Event>, sync: bool) -> Result<(), ()> {
        if !events.is_empty() {
            let generation = self.refresh().await;
            self.step(generation.into_iter().chain(events)).await?;
        }
        if sync {
            self.sync().await?;
        }
        Ok(())
    }

    /// Steps the kernel through `events` and carries out what follows. A
    /// step that panics is a bug the session survives: what it did is
    /// dropped, as a commit that failed drops it, what the steps before it
    /// did is carried out, and the session resumes from its log.
    async fn step(&mut self, events: impl IntoIterator<Item = Event>) -> Result<(), ()> {
        let mut fx = Effects::new();
        for event in events {
            match catch_unwind(AssertUnwindSafe(|| self.core.step(event))) {
                Ok(more) => {
                    self.adopted();
                    fx.extend(more);
                }
                Err(panic) => {
                    let why = panicked(&*panic);
                    eprintln!("session {}: the kernel {why}", self.live.id.short());
                    if self.carry(fx).await.is_ok() {
                        Box::pin(self.resume()).await;
                    }
                    return Err(());
                }
            }
        }
        self.carry(fx).await
    }

    /// Carries out `fx`: its appends are committed together, as durably
    /// as its commit says, before the effects among them, which then run
    /// in order. Whatever the steps `fx` came from, a burst costs one
    /// commit, as durable as the most durable step asked, and each intent
    /// is still durable before the work it names. Only a cancel, which the
    /// kernel orders before the outcome it then logs, a busy mark set
    /// before the work it marks, and a kill of a process group left over,
    /// which nothing logged waits on, go before what was appended ahead of
    /// them is committed; a cancel of work `fx` has yet to start goes after
    /// that start, as before it there is nothing of the call's to stop. A
    /// commit that fails drops the rest and resumes from what the log
    /// holds.
    async fn carry(&mut self, fx: Effects) -> Result<(), ()> {
        let mut batch = Vec::new();
        let mut durability = None;
        let mut after: Vec<Effect> = Vec::new();
        for effect in fx {
            match effect {
                Effect::Append(e) => batch.push(e),
                Effect::Commit(d) => durability = durability.max(Some(d)),
                Effect::Cancel { call } if after.iter().any(|e| e.starts_call() == Some(call)) => {
                    after.push(effect);
                }
                Effect::MarkBusy(true) | Effect::Cancel { .. } | Effect::Kill { .. } => {
                    self.effect(effect);
                }
                effect => after.push(effect),
            }
        }
        debug_assert_eq!(
            batch.is_empty(),
            durability.is_none(),
            "a step commits once"
        );
        // The session as it stands once `after` is carried out.
        let busy = (after.iter().rev())
            .find_map(|e| match e {
                Effect::MarkBusy(busy) => Some(*busy),
                _ => None,
            })
            .unwrap_or_else(|| self.live.has_work());
        if let Some(durability) = durability
            && let Err(why) = self.flush(&mut batch, durability, busy).await
        {
            self.failed(why, after).await;
            return Err(());
        }
        for effect in after {
            self.effect(effect);
        }
        Ok(())
    }

    /// Commits `batch` as `durability` says, listed `busy`.
    async fn flush(
        &mut self,
        batch: &mut Vec<Entry>,
        durability: Durability,
        busy: bool,
    ) -> Result<(), String> {
        let listing = self.listing(busy);
        let durable = durability == Durability::Barrier;
        self.commit(batch, durable, listing)
            .await
            .map_err(|e| format!("logging: {e}"))
    }

    /// A commit failed for `why`: the effects `rest` after it are dropped,
    /// whoever they would have answered told why, and the session resumes
    /// from what the log holds.
    async fn failed(&mut self, why: String, rest: Vec<Effect>) {
        eprintln!("session {}: {why}", self.live.id.short());
        for effect in rest {
            if let Effect::Reply { ticket, .. } = effect {
                self.reply(ticket, Err(why.clone()));
            }
        }
        // Its log takes nothing: the session stops, and loads again when
        // next wanted.
        if self.resuming {
            self.phase = Phase::Draining;
            self.live.session.cancel_all();
            return;
        }
        Box::pin(self.resume()).await;
    }

    /// After a failed commit or a step that panicked: what runs is
    /// cancelled, what was asked is answered that the session stopped, and
    /// the kernel is rebuilt from the committed log, as a load rebuilds it.
    async fn resume(&mut self) {
        self.live.session.cancel_all();
        lock(&self.live.session.cancels).clear();
        self.replies.clear();
        self.sides.clear();
        self.inflight.clear();
        self.unsynced.clear();
        self.waiting_sync.clear();
        self.sync_armed = false;
        self.writer.take();
        self.bound = bind(&self.registry, &self.live.session);
        let bound = Arc::clone(&self.bound) as Arc<dyn Binding>;
        let header = Header {
            id: self.live.id,
            workspace: self.live.workspace.clone(),
            parent: self.live.parent,
        };
        let shared = Arc::clone(&self.live.session.shared);
        let registry = Arc::clone(&self.registry);
        let reloaded = rt::unblock(&*Arc::clone(&shared.rt), move || {
            let (mut loaded, folding) =
                Folding::open(&header, &shared, Open::Load, &bound, &registry)?;
            // A failed sync may have left complete, readable bytes. They
            // cannot be published until the replacement writer syncs them.
            loaded.writer.sync()?;
            io::Result::Ok((loaded, folding))
        })
        .await;
        let (loaded, folding) = match reloaded {
            Ok(reloaded) => reloaded,
            Err(e) => {
                eprintln!("session {}: reloading: {e}", self.live.id.short());
                self.phase = Phase::Draining;
                return;
            }
        };
        let recovered = FrameLog::new(Arc::clone(&loaded.lines), loaded.seq);
        self.live.frames.reloaded(loaded.lines);
        for entry in recovered.entries_after(self.live.frames.seq()) {
            let lines = Encoded::of(std::slice::from_ref(&entry)).expect("loaded entry encodes");
            self.show(std::slice::from_ref(&entry), &lines);
        }
        self.writer = Some(loaded.writer);
        self.since_snapshot = loaded.read;
        self.core = folding.core;
        *lock(&self.live.session.state_view()) = folding.states;
        self.pending = None;
        self.fold_snapshot = folding.snapshot;
        self.live
            .has_input
            .store(folding.has_input, Ordering::SeqCst);
        self.transcript = None;
        self.replied = None;
        self.snapshot = None;
        self.view = None;
        let load = Load {
            boot: sys::boot_id(),
            busy: self.live.has_work(),
            owner: self.owner(),
            ..Load::default()
        };
        let resumed = catch_unwind(AssertUnwindSafe(|| self.core.resumed(&load)));
        match resumed {
            Ok(fx) => {
                self.resuming = true;
                let _ = self.carry(fx).await;
                self.resuming = false;
            }
            // Its log is what it is: the session stops, and loads again
            // when next wanted.
            Err(panic) => {
                let why = panicked(&*panic);
                eprintln!("session {}: resuming {why}", self.live.id.short());
                self.phase = Phase::Draining;
            }
        }
    }

    fn owner(&self) -> Owner {
        let daemon = self.live.session.shared.daemon.upgrade();
        match (self.live.parent, daemon) {
            (None, _) => Owner::None,
            (Some(parent), Some(daemon)) if daemon.knows(parent.session) => Owner::Present,
            (Some(_), _) => Owner::Gone,
        }
    }

    /// Writes `batch`, then shows it: the writer stamps its lease. It is
    /// made durable at once if `durable`, else with the next commit that
    /// is ([`kernel::Durability`]).
    ///
    /// The catalog takes `listing`, if it changed, in the same blocking
    /// hop: before the entries when it is busy, as the mark is set before
    /// the work they are the intent of, and after them when it is not, as
    /// the mark is cleared once the work ends.
    async fn commit(
        &mut self,
        batch: &mut Vec<Entry>,
        durable: bool,
        listing: Record,
    ) -> io::Result<()> {
        let mut writer = self
            .writer
            .take()
            .ok_or_else(|| io::Error::other("no writer"))?;
        let mut entries = std::mem::take(batch);
        let listed = std::mem::replace(&mut self.listed, listing.clone());
        let listing = relisted(&listed, &listing).then_some(listing);
        let shared = Arc::clone(&self.live.session.shared);
        let rt = Arc::clone(&shared.rt);
        let (writer, entries, written, listing) = rt::unblock(&*rt, move || {
            let first = listing.as_ref().is_some_and(|l| l.busy);
            if first && let Some(listing) = &listing {
                shared.catalog.record(listing.clone());
            }
            let written = catch_unwind(AssertUnwindSafe(|| writer.append(&mut entries, durable)))
                .unwrap_or_else(|_| Err(io::Error::other("committing entries panicked")));
            if !first
                && written.is_ok()
                && let Some(listing) = &listing
            {
                shared.catalog.record(listing.clone());
            }
            (writer, entries, written, listing)
        })
        .await;
        // Not cleared after entries that did not commit.
        if written.is_err() && listing.is_some_and(|l| !l.busy) {
            self.listed = listed;
        }
        self.writer = Some(writer);
        let lines = written?;
        crate::session_event("commit", self.live.id, || {
            let how = if durable { "synced" } else { "written" };
            format!("{} entries {how}", entries.len())
        });
        self.folded(&entries);
        self.since_snapshot += lines.lines().map(|l| l.len() as u64 + 1).sum::<u64>();
        if durable {
            // The sync made what was written before durable too.
            self.show_synced();
            self.show(&entries, &lines);
        } else {
            self.unsynced.push((entries, lines));
            self.arm_sync();
        }
        Ok(())
    }

    /// Keeps a snapshot of the session beside its log, if it is at rest
    /// and all it logged is durable: what the next load starts from. A
    /// child's is not kept: what it settled with is the daemon's to read
    /// from its log, not the ledger's to carry. One that cannot be taken
    /// is reported and not tried again until as much log follows.
    async fn keep_snapshot(&mut self) {
        let covers = self.core.ledger().seq();
        let rest =
            self.core.at_rest() && self.unsynced.is_empty() && covers == self.live.frames.seq();
        if !rest || self.live.parent.is_some() || self.writer.is_none() {
            return;
        }
        self.since_snapshot = 0;
        let states = match lock(&self.live.session.state_view()).image() {
            Ok(states) => states,
            Err(e) => {
                eprintln!("session {}: snapshot: {e}", self.live.id.short());
                return;
            }
        };
        self.fold_snapshot = Some((covers, states.clone()));
        let image = Image {
            build: self.live.session.shared.build.clone(),
            extensions: self.registry.installed(),
            has_input: self.live.has_input(),
            ledger: self.core.ledger().clone(),
            states,
        };
        let Some(mut writer) = self.writer.take() else {
            return;
        };
        let rt = Arc::clone(&self.live.session.shared.rt);
        let (writer, taken) = rt::unblock(&*rt, move || {
            let taken = Json::encode(&image)
                .map_err(io::Error::other)
                .and_then(|image| writer.snapshot(&Snapshot { covers, image }));
            (writer, taken)
        })
        .await;
        self.writer = Some(writer);
        match taken {
            Ok(()) => crate::session_event("snapshot", self.live.id, || format!("at {covers}")),
            Err(e) => eprintln!("session {}: snapshot: {e}", self.live.id.short()),
        }
    }

    /// Asks for a sync [`SYNC_LAG`] from now, unless one is on its way.
    fn arm_sync(&mut self) {
        if std::mem::replace(&mut self.sync_armed, true) {
            return;
        }
        let shared = &self.live.session.shared;
        let (mail, lag) = (
            Arc::clone(&self.live.session.mail),
            shared.rt.sleep(SYNC_LAG),
        );
        shared.rt.spawn(Box::pin(async move {
            lag.await;
            let _ = mail.send(Msg::Sync);
        }));
    }

    /// Makes what was only written durable, and shows it. A sync that
    /// fails leaves the session as a failed commit does.
    async fn sync(&mut self) -> Result<(), ()> {
        self.sync_armed = false;
        if self.unsynced.is_empty() {
            return Ok(());
        }
        let Some(mut writer) = self.writer.take() else {
            return Err(());
        };
        let rt = Arc::clone(&self.live.session.shared.rt);
        let (writer, synced) = rt::unblock(&*rt, move || {
            let synced = writer.sync();
            (writer, synced)
        })
        .await;
        self.writer = Some(writer);
        match synced {
            Ok(()) => {
                self.show_synced();
                Ok(())
            }
            Err(e) => {
                self.unsynced.clear();
                self.waiting_sync.clear();
                Box::pin(self.failed(format!("syncing: {e}"), Vec::new())).await;
                Err(())
            }
        }
    }

    /// Shows what was written and is durable now, and answers whoever
    /// waited for it.
    fn show_synced(&mut self) {
        for (entries, lines) in std::mem::take(&mut self.unsynced) {
            self.show(&entries, &lines);
        }
        for done in std::mem::take(&mut self.waiting_sync) {
            done.send(());
        }
    }

    /// Entries durable, as `lines` encodes them: shown to every client.
    fn show(&mut self, entries: &[Entry], lines: &Encoded) {
        if let Some(view) = &mut self.view {
            view.extend(entries);
        }
        for (e, line) in entries.iter().zip(lines.lines()) {
            // One too large for the tail is not framed: followers read it.
            let frame = (line.len() <= crate::outbox::TAIL)
                .then(|| Frame::from_line(e.seq.get(), line).ok())
                .flatten();
            self.live.frames.push(e.seq.get(), frame);
        }
        lock(&self.live.followers).retain(|c| c.wake().is_ok());
    }

    /// Entries committed: folded where else they are read.
    fn folded(&mut self, entries: &[Entry]) {
        let session = &self.live.session;
        let hooks = self.registry.hooks();
        self.transcript = self.transcript.take().filter(|t| t.is_on(&self.registry));
        // An edit changes what came before it: the transcript is made again.
        if entries.iter().any(|e| matches!(e.body, Body::Edit { .. })) {
            self.transcript = None;
        }
        for e in entries {
            lock(&session.state_view()).apply(e);
            if let Some(next) = &self.pending {
                lock(&next.view().states).apply(e);
            }
            if matches!(e.body, Body::Input { .. }) {
                self.live.has_input.store(true, Ordering::SeqCst);
            }

            // What observers do about it runs owned by the session.
            for reaction in hooks.observe(self.bound.point(), e) {
                let ended = self.live.ended.clone();
                session.shared.rt.spawn(Box::pin(async move {
                    unless(&ended, reaction.0).await;
                }));
            }
            let replied = match &e.body {
                Body::ModelResult { call, .. } => self.replied.take_if(|(of, _)| of == call),
                _ => None,
            };
            if let Some(transcript) = &mut self.transcript {
                transcript.apply(e, replied.map(|(_, m)| m));
            }
            if let Body::ToolResult { call, .. } | Body::ModelResult { call, .. } = &e.body {
                // Settled: what still runs of it is let go.
                self.inflight.remove(call);
                session.forget(*call);
                self.bound.settled(*call);
            }
        }
    }

    /// The summary as the log now says: as many entries as are shown.
    fn publish(&self) {
        let ledger = self.core.ledger();
        let mut summary = lock(&self.live.summary);
        summary.entries = self.live.frames.seq();
        summary.activity = ledger.activity();
    }

    fn effect(&mut self, effect: Effect) {
        let shared = Arc::clone(&self.live.session.shared);
        // A stopping session starts nothing: what it would have started is
        // in the log, for its next load.
        if self.phase == Phase::Draining && effect.starts() {
            return;
        }
        match effect {
            Effect::Append(_) | Effect::Commit(_) => unreachable!("appends commit in runs"),
            Effect::Reply { ticket, result } => self.reply(ticket, result),
            Effect::Side { ticket, side } => {
                if let Some((reply, conversation)) = self.sides.remove(&ticket) {
                    reply.send(side.map(|side| (side, conversation.unwrap_or_default())));
                }
            }
            Effect::Prepare {
                call,
                attempt,
                choice,
            } => self.prepare(call, attempt, choice),
            Effect::Send { call } => self.send(call),
            Effect::RunTool { call, tool, input } => self.run_tool(call, tool, input, Child::None),
            Effect::Resume {
                call,
                tool,
                input,
                value,
            } => match self.inflight.get_mut(&call) {
                // Still running, and waiting for it.
                Some(InFlight::Tool(child @ Child::Waiting(_))) => {
                    if let Child::Waiting(reply) = std::mem::replace(child, Child::None) {
                        reply.send(value);
                    }
                }
                // Run again, from a log that holds the answer.
                _ => self.run_tool(call, tool, input, Child::Answered(value)),
            },
            Effect::Await { call, answer } => {
                let gated = self.cancellable(call, answer.0);
                self.later(gated, move |gated| Event::Gated { call, gated });
            }
            Effect::Recover { call, answer } => {
                let recovered = self.cancellable(call, answer.0);
                self.later(recovered, move |recovered| Event::Recovered {
                    call,
                    recovered,
                });
            }
            Effect::Sleep { call, after } => {
                let slept = self.cancellable(call, shared.rt.sleep(after));
                self.later(slept, move |()| Event::Woke { call });
            }
            Effect::Cancel { call } => {
                self.inflight.remove(&call);
                self.live.session.cancelled(call);
            }
            Effect::StartChild { link, child, input } => self.start_child(link, child, input),
            Effect::Interrupt { child } => {
                if let Some(daemon) = shared.daemon.upgrade() {
                    daemon.interrupt(child);
                }
            }
            Effect::Report { outcome } => {
                if let (Some(parent), Some(daemon)) = (self.live.parent, shared.daemon.upgrade()) {
                    let child = self.live.id;
                    shared.rt.spawn(Box::pin(async move {
                        daemon.report(parent.session, child, outcome).await;
                    }));
                }
            }
            Effect::Kill { pgid } => {
                let _ = sys::Group(pgid).kill();
            }
            Effect::MarkBusy(busy) => {
                self.live.busy.store(busy, Ordering::SeqCst);
                let how = if busy { "busy" } else { "settled" };
                crate::session_event(how, self.live.id, String::new);
                self.live.touch();
            }
        }
    }

    /// Starts child session `child` on `input`, owned by link `link`, in
    /// this session's environment.
    fn start_child(&self, link: protocol::Seq, child: SessionId, input: String) {
        let shared = &self.live.session.shared;
        let header = Header {
            id: child,
            workspace: self.live.workspace.clone(),
            parent: Some(Parent {
                session: self.live.id,
                call: link.get(),
            }),
        };
        let env = lock(&self.live.session.env).clone();
        if let Some(daemon) = shared.daemon.upgrade() {
            shared.rt.spawn(Box::pin(async move {
                if let Err(e) = daemon.start_child(header, input, env).await {
                    eprintln!("starting child {}: {e}", child.short());
                }
            }));
        }
    }

    /// Lists the session in the catalog as it now stands, when no commit
    /// did: a call beginning to wait, the actor ending.
    async fn mark(&mut self) {
        let listing = self.listing(self.live.has_work());
        if !relisted(&self.listed, &listing) {
            return;
        }
        self.listed = listing.clone();
        let shared = Arc::clone(&self.live.session.shared);
        rt::unblock(&*Arc::clone(&shared.rt), move || {
            shared.catalog.record(listing);
        })
        .await;
    }

    /// The session as the catalog lists it, `busy` with work in flight:
    /// marked busy too while a call waits on someone, so the next daemon
    /// loads it at once to show what it asks.
    fn listing(&self, busy: bool) -> Record {
        let ledger = self.core.ledger();
        let activity = ledger.activity();
        Record {
            header: Header {
                id: self.live.id,
                workspace: self.live.workspace.clone(),
                parent: self.live.parent,
            },
            busy: busy || activity.requires_action(),
            entries: ledger.seq(),
            activity,
            read_only: lock(&self.live.summary).read_only,
        }
    }

    fn reply(&mut self, ticket: Ticket, result: Result<(), String>) {
        if let Some(reply) = self.replies.remove(&ticket) {
            reply.send(result);
        }
    }

    /// `work`, which call `call`'s cancel drops.
    fn cancellable<T: Send + 'static>(
        &mut self,
        call: CallId,
        work: impl Future<Output = T> + Send + 'static,
    ) -> impl Future<Output = Option<T>> + Send + 'static {
        let cancel = self.live.session.cancel(call);
        self.inflight.insert(call, InFlight::Later);
        async move { unless(&cancel, work).await }
    }

    /// Steps the event `then` makes of what `work` yields, unless it was
    /// cancelled.
    fn later<T: Send + 'static>(
        &self,
        work: impl Future<Output = Option<T>> + Send + 'static,
        then: impl FnOnce(T) -> Event + Send + 'static,
    ) {
        let mail = Arc::clone(&self.live.session.mail);
        self.live.session.shared.rt.spawn(Box::pin(async move {
            if let Some(done) = work.await {
                let _ = mail.send(Msg::Step(then(done)));
            }
        }));
    }

    /// Prepares the request of model call `call`, attempt `attempt`: the
    /// transcript before it with the tools `choice` lets it call, as the
    /// `step` point leaves it. What the point decided goes to the kernel
    /// to log; the request waits here until the kernel has it sent, unless
    /// a handler answered it, in which case the answer is the reply.
    fn prepare(&mut self, call: CallId, attempt: u16, choice: ToolChoice) {
        let (decisions, answered) = match self.live.session.model() {
            Ok(model) => {
                let mut request = self.request(call, &model);
                request.tool_choice = match choice {
                    ToolChoice::Auto => llm::ToolChoice::Auto,
                    ToolChoice::None => llm::ToolChoice::None,
                };
                let (decisions, answered) = self.bound.step(call, &mut request);
                let answered = answered.map(|(by, response)| {
                    let (replied, message) = replied(response);
                    self.replied = message.map(|m| (call, m));
                    (by, replied)
                });
                if answered.is_none() {
                    let prepared = InFlight::Prepared(Box::new(Ok((model, request, attempt))));
                    self.inflight.insert(call, prepared);
                }
                (decisions, answered)
            }
            Err(error) => {
                self.inflight
                    .insert(call, InFlight::Prepared(Box::new(Err(error))));
                (Vec::new(), None)
            }
        };
        let prepared = Event::Prepared {
            call,
            decisions,
            answered,
        };
        let _ = self.live.session.mail.send(Msg::Step(prepared));
    }

    /// Sends the request prepared for model call `call`, its text
    /// streaming to followers under its attempt.
    fn send(&mut self, call: CallId) {
        let Some(InFlight::Prepared(prepared)) = self.inflight.remove(&call) else {
            return;
        };
        let cancel = self.live.session.cancel(call);
        self.inflight.insert(call, InFlight::Model);
        let live = Arc::clone(&self.live);
        self.live.session.shared.rt.spawn(Box::pin(async move {
            let (replied, message) = match *prepared {
                Ok((model, request, attempt)) => {
                    let mut deltas = |text: &str| live.delta(call, attempt, text);
                    let replying: BoxFuture<'_, _> =
                        Box::pin(reply(&model, &request, &cancel, &mut deltas));
                    contain(replying).await
                }
                // Said so the user can fix it; sending again would not.
                Err(error) => {
                    let error = llm::Error::new(llm::Kind::InvalidRequest, error);
                    (Replied::Failed(error), None)
                }
            };
            let _ = live.session.mail.send(Msg::Replied {
                call,
                replied,
                message,
            });
        }));
    }

    /// The request for model call `call`: the system prompt, the
    /// transcript before it, and the tools.
    fn request(&mut self, call: CallId, model: &Model) -> llm::Request {
        let messages = self.conversation(call);
        model.request(messages, self.registry.specs().clone())
    }

    /// The conversation the model sees before entry `call`.
    fn conversation(&mut self, call: CallId) -> llm::Messages {
        let system = self.registry.system_prompt();
        let frames = &self.live.frames;
        let registry = &self.registry;
        let transcript = self
            .transcript
            .get_or_insert_with(|| Transcript::of(frames.entries(), Arc::clone(registry), system));
        if let Some(messages) = transcript.before(call.get()) {
            messages.clone()
        } else {
            // A call recovery runs after entries it does not see.
            let before = frames.entries().take_while(|e| e.seq < call.0);
            Transcript::of(before, Arc::clone(registry), system).into_messages()
        }
    }

    /// Runs tool call `call`, the model's or a user's command, on its
    /// input as read at its intent if the gate left it as it was,
    /// reporting its outcome.
    fn run_tool(&mut self, call: CallId, tool: String, input: Json, child: Child) {
        let cancel = self.live.session.cancel(call);
        self.inflight.insert(call, InFlight::Tool(child));
        let registry = Arc::clone(&self.registry);
        let parsed = self.bound.take_parsed(call, &input);
        let cx = self.live.session.cx(call, &registry, cancel);
        let mail = Arc::clone(&self.live.session.mail);
        let id = self.live.id;
        self.live.session.shared.rt.spawn(Box::pin(async move {
            let began = crate::stamp();
            let Called { outcome, card } =
                contain(registry.tools().run(&tool, &input, parsed, &cx)).await;
            crate::session_event("effect", id, || {
                let elapsed = began.map_or(std::time::Duration::ZERO, |t| t.elapsed());
                let how = match &outcome {
                    Outcome::Done { .. } => "done",
                    Outcome::Failed { .. } => "failed",
                    Outcome::Interrupted { .. } => "interrupted",
                };
                format!("{tool} call {call}: {how} in {elapsed:?}")
            });
            let done = Event::ToolDone {
                call,
                outcome,
                card,
            };
            let _ = mail.send(Msg::Step(done));
        }));
    }

    /// Sends `client`, which has the entries up to `since`, the attach,
    /// what it missed (or a snapshot, far behind) and its end; then
    /// everything live.
    fn attach(&mut self, client: &Outbox, since: u64) {
        let seq = self.live.frames.seq();
        let far = seq.saturating_sub(since) > SNAPSHOT_GAP;
        let snapshot = far.then(|| self.snapshot(seq)).flatten();
        let cursor = if snapshot.is_some() {
            seq
        } else {
            since.min(seq)
        };
        let live = &self.live;
        let mut frames = Vec::new();
        let attached = DaemonMsg::Attached {
            session: live.id,
            workspace: live.workspace.clone(),
        };
        frames.extend(Frame::new(&attached).ok().map(|f| (cursor, f)));
        frames.extend(snapshot.map(|frame| (seq, frame)));
        frames.extend(
            Frame::new(&DaemonMsg::Synced { seq })
                .ok()
                .map(|f| (seq, f)),
        );
        let follower = client.follow(Arc::clone(&live.frames), cursor, frames);
        lock(&live.followers).push(follower);
    }

    /// The snapshot at entry `seq`, the last committed, made once for every
    /// client attaching then; `None` if too large for a frame.
    fn snapshot(&mut self, seq: u64) -> Option<Frame> {
        if let Some((at, frame)) = &self.snapshot
            && *at == seq
        {
            return frame.clone();
        }
        let live = &self.live;
        let view = self.view.get_or_insert_with(|| {
            SessionView::from_log(live.id, live.workspace.clone(), live.frames.entries())
        });
        let frame = Frame::new(&DaemonMsg::Snapshot(Box::new(view.clone()))).ok();
        self.snapshot = Some((seq, frame.clone()));
        frame
    }
}

/// Whether the catalog is to hear of `now`: it lists what it waits on and
/// whether it is busy, and its count only as of then, so a commit does not
/// change it.
fn relisted(listed: &Record, now: &Record) -> bool {
    listed.busy != now.busy || listed.activity != now.activity
}

/// Values by key, in a vector: a session has few of each in flight.
struct Keyed<K, V>(Vec<(K, V)>);

impl<K, V> Default for Keyed<K, V> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<K: PartialEq, V> Keyed<K, V> {
    fn insert(&mut self, key: K, value: V) {
        self.remove(&key);
        self.0.push((key, value));
    }

    fn remove(&mut self, key: &K) -> Option<V> {
        let at = self.0.iter().position(|(k, _)| k == key)?;
        Some(self.0.swap_remove(at).1)
    }

    fn get(&self, key: &K) -> Option<&V> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.0.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn values(&self) -> impl Iterator<Item = &V> {
        self.0.iter().map(|(_, v)| v)
    }

    fn clear(&mut self) {
        self.0.clear();
    }
}

/// How many entries behind a client must be to get a snapshot rather than
/// a replay.
const SNAPSHOT_GAP: u64 = 256;

/// The most messages an actor steps together, so that a flood of them
/// still commits now and then.
const BURST: usize = 64;

#[cfg(test)]
impl Live {
    /// Takes `text` in as a client's input, once committed.
    pub fn submit(&self, text: &str) -> Result<(), String> {
        let text = text.to_owned();
        let input = Request::Input {
            text,
            mode: Mode::Queue,
        };
        rt::block_on(self.request(input, None))
    }

    pub fn ended(&self) -> Flag {
        self.ended.clone()
    }

    /// Returns once the actor has taken every message sent before.
    pub fn barrier(&self) {
        let (done, taken) = oneshot::channel();
        self.send(Msg::Barrier(done));
        rt::block_on(taken);
    }

    /// Returns once `ready` holds, checked as the session changes; panics
    /// with `what` if it does not within `within`.
    pub fn wait(
        &self,
        what: &str,
        within: std::time::Duration,
        ready: impl FnMut() -> bool + Send,
    ) {
        let changed = &self.session.changed;
        let timeout = rt::Reactor::AsyncIo.sleep(within);
        let held = rt::block_on(or(
            async {
                changed.until(ready).await;
                true
            },
            async {
                timeout.await;
                false
            },
        ));
        assert!(held, "never {what}");
    }
}

impl Live {
    /// Live text of model call `call`'s attempt `attempt`, to followers.
    fn delta(&self, call: CallId, attempt: u16, text: &str) {
        let delta = DaemonMsg::Delta {
            call,
            text: text.to_owned(),
            attempt,
        };
        if let Ok(frame) = Frame::new(&delta) {
            let after = self.frames.seq();
            lock(&self.followers).retain(|c| c.send_after(after, frame.clone()).is_ok());
        }
    }
}

impl Session {
    fn state_view(&self) -> Arc<Mutex<States>> {
        Arc::clone(&lock(&self.states))
    }

    /// What stops call `call`'s work: the one cancel it has while open,
    /// made as the first of its work asks for it.
    fn cancel(&self, call: CallId) -> Cancel {
        let mut cancels = lock(&self.cancels);
        if let Some(cancel) = cancels.get(&call) {
            return cancel.clone();
        }
        let cancel = Cancel::default();
        cancels.insert(call, cancel.clone());
        cancel
    }

    /// Stops everything in flight for call `call`; work it starts after
    /// this gets a cancel of its own.
    fn cancelled(&self, call: CallId) {
        if let Some(cancel) = lock(&self.cancels).remove(&call) {
            cancel.set();
        }
    }

    /// Call `call` settled: what it still has running is on its own (a
    /// side request asked beside it finishes), and nothing more of it is
    /// stopped.
    fn forget(&self, call: CallId) {
        lock(&self.cancels).remove(&call);
    }

    /// Stops every call's work.
    fn cancel_all(&self) {
        for cancel in lock(&self.cancels).values() {
            cancel.set();
        }
    }

    /// What call `call` runs with, on `registry`, stopped by `cancel`.
    fn cx(self: &Arc<Self>, call: CallId, registry: &Arc<Registry>, cancel: Cancel) -> Cx {
        self.cx_on(call, registry, cancel, self.state_view())
    }

    fn cx_on(
        self: &Arc<Self>,
        call: CallId,
        registry: &Arc<Registry>,
        cancel: Cancel,
        states: Arc<Mutex<States>>,
    ) -> Cx {
        Parts {
            workspace: Arc::clone(&self.workspace),
            env: lock(&self.env).clone(),
            session: self.id,
            call,
            model: self.model_spec().into(),
            spill: Arc::clone(&self.spill),
            cancel,
            services: self.services(),
        }
        .cx(Arc::new(Calling {
            session: Arc::clone(self),
            call,
            registry: Arc::clone(registry),
            states,
        }))
    }

    /// What the engine provides the session's calls: the config layers,
    /// child sessions, side model requests and the runtime.
    fn services(self: &Arc<Self>) -> Arc<Services> {
        Arc::clone(self.services.get_or_init(|| {
            let own = Arc::new(Own(Arc::downgrade(self)));
            Arc::new(
                Services::default()
                    .with::<dyn ext::Layers>(own.clone())
                    .with::<dyn ext::Sessions>(own.clone())
                    .with::<dyn ext::Models>(own)
                    .with::<dyn rt::Runtime>(Arc::clone(&self.shared.rt)),
            )
        }))
    }

    /// The model the newest client's environment and the settings name
    /// now, or why there is none.
    fn model(&self) -> Result<Model, String> {
        let (spec, _) = self.model_named()?;
        let env = lock(&self.env).clone();
        self.shared
            .models
            .model(&self.workspace, env.as_deref(), &spec)
    }

    /// The model spec the settings name now, and what names it: the
    /// `ModelSpec` service's word (the session's own choice over the
    /// config layers) if an extension provides it, else the layers'.
    fn model_named(&self) -> Result<(String, String), String> {
        let layers = &self.shared.models.settings;
        let registry = self.shared.registry.get();
        match registry.service::<dyn ext::ModelSpec>() {
            Some(spec) => spec.model(&lock(&self.state_view()), layers, &self.workspace),
            None => ext::Layers::model(layers, &self.workspace),
        }
    }

    /// The model spec the settings name now.
    fn model_spec(&self) -> String {
        self.model_named()
            .map_or_else(|_| "none".to_owned(), |(model, _)| model)
    }
}

/// The config files, as the layers a model is named in.
impl ext::Layers for crate::config::Settings {
    fn model(&self, workspace: &Path) -> Result<(String, String), String> {
        let (model, source) = self.model(workspace)?;
        Ok((model, source.to_string()))
    }
}

/// The session as handlers at decision points read it: only when one
/// asks, so a point with no handler costs nothing. A handler may take a
/// call's context along, to work it answers later with.
pub(crate) struct Viewed {
    session: Arc<Session>,
    registry: Arc<Registry>,
    states: Arc<Mutex<States>>,
    active: AtomicBool,
}

impl View for Viewed {
    fn activate(&self) {
        *lock(&self.session.states) = Arc::clone(&self.states);
        self.active.store(true, Ordering::Release);
    }

    fn session(&self) -> SessionId {
        self.session.id
    }

    fn workspace(&self) -> &Path {
        &self.session.workspace
    }

    /// As a call is recorded: the model's name once it is made, else
    /// what the settings asked for.
    fn model(&self) -> String {
        (self.session.model()).map_or_else(|_| self.session.model_spec(), |model| model.name())
    }

    fn states(&self, read: &mut dyn FnMut(&States)) {
        read(&lock(&self.states));
    }

    fn env(&self) -> Option<Arc<Environment>> {
        lock(&self.session.env).clone()
    }

    /// With the call's own cancel: work a handler takes it along to ends
    /// with the call.
    fn cx(&self, call: CallId) -> Option<Cx> {
        let registry = Arc::clone(&self.registry);
        Some(self.session.cx_on(
            call,
            &registry,
            self.session.cancel(call),
            Arc::clone(&self.states),
        ))
    }
}

/// `registry` bound to `session`: what the kernel drives.
fn bind(registry: &Arc<Registry>, session: &Arc<Session>) -> Arc<Bound<Viewed>> {
    bind_states(registry, session, session.state_view())
}

fn bind_states(
    registry: &Arc<Registry>,
    session: &Arc<Session>,
    states: Arc<Mutex<States>>,
) -> Arc<Bound<Viewed>> {
    let settings = &session.shared.models.settings;
    let agent = settings.agent(&session.workspace);
    let viewed = Arc::new(Viewed {
        session: Arc::clone(session),
        registry: Arc::clone(registry),
        states,
        active: AtomicBool::new(false),
    });
    Arc::new(Bound::new(Arc::clone(registry), viewed, agent))
}

/// What a panic says.
fn panicked(panic: &(dyn Any + Send)) -> String {
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("unknown panic");
    format!("panicked: {message}")
}

/// The slash commands a session runs, as a welcome advertises them.
pub fn commands(registry: &Registry) -> Vec<protocol::CommandInfo> {
    registry
        .commands()
        .map(|c| protocol::CommandInfo {
            name: c.name().to_owned(),
            description: c.description().to_owned(),
        })
        .collect()
}

/// The engine as a running call reaches it, on the registry generation it
/// started with: what it reports goes through the actor's mailbox.
struct Calling {
    session: Arc<Session>,
    call: CallId,
    registry: Arc<Registry>,
    states: Arc<Mutex<States>>,
}

impl Calling {
    fn report(&self, note: Note) {
        let call = self.call;
        let _ = (self.session.mail).send(Msg::Note {
            call,
            note,
            done: None,
        });
    }
}

impl Host for Calling {
    fn spawned(&self, pgid: u32, background: bool) {
        let Some(boot) = sys::boot_id() else { return };
        let boot = boot.to_owned();
        self.report(Note::Process {
            pgid,
            boot,
            background,
        });
    }

    fn own(&self, pgid: u32, ended: BoxFuture<'static, ()>) {
        self.spawned(pgid, true);
        lock(&self.session.jobs).push(pgid);
        let session = Arc::clone(&self.session);
        self.session.shared.rt.spawn(Box::pin(async move {
            ended.await;
            lock(&session.jobs).retain(|&p| p != pgid);
            session.changed.notify();
            if let Some(daemon) = session.shared.daemon.upgrade() {
                daemon.jobs.notify();
            }
        }));
    }

    fn progress(&self, text: &str) {
        self.report(Note::Progress(text.to_owned()));
    }

    /// Resolves once the change is committed, so a read after it sees it.
    fn note(&self, ext: &str, state: &str, data: Json) -> BoxFuture<'static, ()> {
        let (ext, state) = (ext.to_owned(), state.to_owned());
        let (done, committed) = oneshot::channel();
        let note = Msg::Note {
            call: self.call,
            note: Note::State { ext, state, data },
            done: Some(done),
        };
        let _ = self.session.mail.send(note);
        Box::pin(async move {
            let _ = committed.await;
        })
    }

    fn edit(&self, target: protocol::Seq, upto: Option<protocol::Seq>, replace: Option<Json>) {
        self.report(Note::Edit {
            target,
            upto,
            replace,
        });
    }

    fn states(&self, read: &mut dyn FnMut(&States)) {
        read(&lock(&self.states));
    }

    fn registry(&self) -> &Registry {
        &self.registry
    }
}

impl ext::Models for Own {
    /// Logged under the call as a model call of its own, then sent once:
    /// its outcome is logged as it ends, and the call's end ends it.
    fn call<'a>(
        &'a self,
        cx: &'a Cx,
        side: ext::Side,
    ) -> BoxFuture<'a, Result<llm::Response, llm::Error>> {
        Box::pin(async move {
            let invalid = |e: String| llm::Error::new(llm::Kind::InvalidRequest, e);
            let session = self
                .0
                .upgrade()
                .ok_or_else(|| invalid("the session is gone".into()))?;
            let model = match &side.model {
                Some(spec) => {
                    let env = lock(&session.env).clone();
                    (session.shared.models).model(&session.workspace, env.as_deref(), spec)
                }
                None => session.model(),
            }
            .map_err(invalid)?;
            let of = cx.call;
            let (asking, opened) = oneshot::channel();
            let purpose = side.purpose.to_string();
            let _ = session.mail.send(Msg::Side {
                of,
                purpose,
                model: model.name(),
                conversation: side.conversation,
                reply: asking,
            });
            let Some(Some((id, conversation))) = opened.await else {
                return Err(invalid(format!("call {of} is not open to ask for")));
            };
            let mut request = side.request;
            if side.conversation {
                // Pushed onto the conversation, whose encodings it keeps.
                let mut messages = conversation;
                for message in request.messages.iter() {
                    messages.push(message.clone());
                }
                request.messages = messages;
            }
            request.model = model.id();
            request.generation.max_output_tokens =
                std::num::NonZeroU32::new(side.budget.max_tokens);
            // Logged as intent, it is seen through to its outcome by a task
            // of its own, stopped by the call's cancel: a handler that
            // drops its answer does not leave the side call open.
            let (answered, answer) = oneshot::channel();
            let timeout = session.shared.rt.sleep(side.budget.timeout);
            let (cancel, mail) = (cx.cancel.clone(), Arc::clone(&session.mail));
            session.shared.rt.spawn(Box::pin(async move {
                // Read as the call's own replies are, with nothing streamed.
                let mut quiet = |_: &str| {};
                let asked = reply(&model, &request, &cancel, &mut quiet);
                let (outcome, answer) = sided(first(asked, timeout).await.map(|(r, _)| r));
                let _ = mail.send(Msg::Step(Event::SideDone { side: id, outcome }));
                answered.send(answer);
            }));
            answer
                .await
                .unwrap_or_else(|| Err(llm::Error::new(llm::Kind::Incomplete, "not answered")))
        })
    }
}

/// What a side request's reply, or its timing out, is logged as and
/// answered with.
fn sided(replied: Option<Replied>) -> (Outcome, Result<llm::Response, llm::Error>) {
    match replied {
        Some(Replied::Done(output)) => {
            let answer = llm::Response::from_json(output.raw())
                .map_err(|e| llm::Error::new(llm::Kind::Decode, e.to_string()));
            (Outcome::Done { output }, answer)
        }
        Some(Replied::Failed(error)) => {
            let failed = Outcome::Failed {
                error: error.to_string(),
            };
            (failed, Err(error))
        }
        Some(Replied::Interrupted(output)) => {
            let error = llm::Error::new(llm::Kind::Incomplete, "the call ended first");
            (Outcome::Interrupted { output }, Err(error))
        }
        None => {
            let error = llm::Error::new(llm::Kind::Transport, "the side request timed out");
            let failed = Outcome::Failed {
                error: error.to_string(),
            };
            (failed, Err(error))
        }
    }
}

/// What `work` yields, unless `stop` resolves first.
async fn first<T>(work: impl Future<Output = T>, stop: impl Future<Output = ()>) -> Option<T> {
    or(async { Some(work.await) }, async {
        stop.await;
        None
    })
    .await
}

/// The session, as its calls reach it for what only the engine has: the
/// config layers, child sessions and side model requests. Weak, as the
/// session holds these.
struct Own(Weak<Session>);

impl ext::Layers for Own {
    fn model(&self, workspace: &Path) -> Result<(String, String), String> {
        let session = self.0.upgrade().ok_or("the session is gone")?;
        ext::Layers::model(&session.shared.models.settings, workspace)
    }
}

impl ext::Sessions for Own {
    /// The child the call started, or starts now; an interrupt of the
    /// call's turn reaches it first.
    fn child<'a>(&'a self, cx: &'a Cx, input: String) -> BoxFuture<'a, Outcome> {
        Box::pin(async move {
            let Some(session) = self.0.upgrade() else {
                return Outcome::Interrupted { output: None };
            };
            let call = cx.call;
            let (reply, result) = oneshot::channel();
            let _ = session.mail.send(Msg::Child { call, input, reply });
            match result.await {
                Some(value) => value
                    .parse()
                    .unwrap_or(Outcome::Interrupted { output: None }),
                None => Outcome::Interrupted { output: None },
            }
        })
    }
}

/// Attempt `attempt` at `model`'s reply to `request`, its text streaming
/// to `deltas`: what the kernel is told, and the message of a reply.
async fn reply(
    model: &Model,
    request: &llm::Request,
    cancel: &Cancel,
    deltas: &mut (dyn FnMut(&str) + Send),
) -> (Replied, Option<llm::Assistant>) {
    // Sent once polled, as an interrupt meanwhile finds it.
    let mut events = None;
    let reading = async {
        let events = events.insert(model.send(request));
        crate::model::read(events, deltas).await
    };
    let reading: BoxFuture<'_, _> = Box::pin(reading);
    let Some(read) = unless(cancel, reading).await else {
        let text = events.and_then(llm::Events::interrupted);
        return (Replied::Interrupted(text), None);
    };
    let reply = read.and_then(|()| {
        let events = events.ok_or_else(llm::Error::incomplete)?;
        events.finish(Some(model.origin(request)))
    });
    match reply {
        Ok(reply) => replied(Ok(reply)),
        Err(error) => (Replied::Failed(error), None),
    }
}

/// What the kernel is told of a reply, or a failure, and the message of
/// the reply for the transcript.
fn replied(response: Result<llm::Response, llm::Error>) -> (Replied, Option<llm::Assistant>) {
    match response {
        Ok(response) => match Json::encode(&response) {
            Ok(output) => (Replied::Done(output), Some(response.message)),
            Err(e) => (
                Replied::Failed(llm::Error::new(llm::Kind::Decode, e.to_string())),
                None,
            ),
        },
        Err(error) => (Replied::Failed(error), None),
    }
}

/// `work`'s output, or `None` once `cancel` is set first; `work` is then
/// dropped, and not started at all if set already.
async fn unless<T>(cancel: &Flag, work: impl Future<Output = T>) -> Option<T> {
    if cancel.is_set() {
        return None;
    }
    first(work, cancel.wait()).await
}

/// Runs `effect` so that a panic inside it fails the call instead of
/// taking the actor down.
async fn contain<T: Failure>(effect: impl Future<Output = T>) -> T {
    let mut effect = pin!(effect);
    poll_fn(|cx| {
        catch_unwind(AssertUnwindSafe(|| effect.as_mut().poll(cx)))
            .unwrap_or_else(|panic| Poll::Ready(T::failed(panicked(&*panic))))
    })
    .await
}

/// What an effect that panicked reports.
trait Failure {
    fn failed(error: String) -> Self;
}

impl<T> Failure for Result<T, String> {
    fn failed(error: String) -> Self {
        Err(error)
    }
}

impl Failure for Called {
    fn failed(error: String) -> Self {
        Self::failed(error)
    }
}

impl Failure for (Replied, Option<llm::Assistant>) {
    fn failed(error: String) -> Self {
        (
            Replied::Failed(llm::Error::new(llm::Kind::Decode, error)),
            None,
        )
    }
}

#[cfg(test)]
mod tests;
