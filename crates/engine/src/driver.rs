//! A live session: one task, its actor, owns the session's kernel and its
//! log's writer, carries out the effects each step returns and feeds back
//! what they report. Clients, calls, model replies and other sessions
//! reach it only through its mailbox, so a log has one writer, entries
//! commit in the order the kernel numbers them, and no lock is held
//! across an await. What the driver decides is only how: the kernel
//! decides what. Making durable entries visible also publishes activity
//! when they cover the current ledger. This boundary is shared by commits,
//! deferred sync and recovery, including the final sync while draining.

use std::any::Any;
use std::cell::RefCell;
use std::future::{Future, poll_fn};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::Poll;
use std::time::Instant;

use crate::catalog::Record;
use crate::store::{Encoded, Header, Loaded, Parent, Snapshot, Writer};
use ext::host::{Binding, Bound, Host, Parts, Registry, Services};
use ext::point::View;
use ext::{BoxFuture, Called, Cancel, Cx, Flag, States};
use kernel::{
    Admission, Core, Durability, Effect, Effects, Event, Load, Note, Owner, Replied, Requirement,
    Ticket,
};
use protocol::ledger::Ledger;
use protocol::view::SessionView;
use protocol::{
    Activity, Author, Body, CallId, DaemonMsg, Entry, Environment, Frame, Json, Mode, Outcome,
    ReadOnly, Residency, SessionId, SessionSummary, ToolChoice,
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

fn recorded_registry(
    shared: &Shared,
    source: Option<&Arc<dyn ext::host::Generations>>,
    installed: Option<&[protocol::Installed]>,
) -> io::Result<Arc<Registry>> {
    match (source, installed) {
        (Some(source), Some(installed)) => Arc::clone(source)
            .restore(installed)
            .map_err(io::Error::other),
        (Some(source), None) => Ok(source.registry()),
        (None, Some(installed)) if installed.iter().any(|i| i.image.is_some()) => Err(
            io::Error::other("recorded dynamic extensions require their loader"),
        ),
        (None, _) => Ok(shared.registry.get()),
    }
}

fn load_registry(
    header: &Header,
    shared: &Shared,
    source: &Arc<dyn ext::host::Generations>,
) -> io::Result<Arc<Registry>> {
    let seq = shared.catalog.get(header.id).map_or(0, |r| r.entries);
    let entries = shared
        .store
        .prefix(header.id, seq)
        .inspect_err(|e| store_fault(header.id, ext_logging::Operation::Reload, e))?;
    let installed = entries.iter().rev().find_map(|entry| match &entry.body {
        Body::Extensions { installed } => Some(installed.as_slice()),
        _ => None,
    });
    recorded_registry(shared, Some(source), installed)
}

pub(crate) fn copy_history(
    header: &Header,
    seq: protocol::Seq,
    id: SessionId,
    shared: &Arc<Shared>,
) -> Result<Header, protocol::ForkError> {
    let entries = shared.store.prefix(header.id, seq.get())?;
    let source = shared.source(&header.workspace)?;
    let installed = entries.iter().rev().find_map(|entry| match &entry.body {
        Body::Extensions { installed } => Some(installed.as_slice()),
        _ => None,
    });
    let registry = recorded_registry(shared, source.as_ref(), installed)
        .map_err(|_| protocol::ForkError::UnavailableGeneration)?;
    let policy = registry
        .service::<dyn ext::host::HistoryPolicy>()
        .ok_or(protocol::ForkError::Disabled)?;
    let (tx, _rx) = channel::unbounded();
    let target = Header {
        id,
        workspace: header.workspace.clone(),
        parent: None,
    };
    let session = Session::new(&target, shared, Arc::new(tx), &registry);
    for entry in &entries {
        lock(&session.state_view()).apply(entry);
        if let Body::Environment(env) = &entry.body {
            *lock(&session.env) = Some(Arc::new(env.clone()));
        }
    }
    let binding = bind(&registry, &session);
    let prefix = ext::host::ValidatedPrefix::prepare(
        policy,
        binding as Arc<dyn Binding>,
        entries,
        header.id,
        seq,
    )?;
    let (mut entries, parent, namespace) = prefix.into_history();
    let fact = Body::Ext {
        ext: namespace.ext.to_string(),
        state: namespace.state.to_string(),
        data: Json::encode(&parent).map_err(io::Error::other)?,
    };
    entries.push(Entry::new(
        protocol::Seq::new(seq.get() + 1).ok_or_else(|| io::Error::other("sequence exhausted"))?,
        protocol::Epoch(
            entries
                .last()
                .map_or(0, |entry| entry.epoch.0)
                .checked_add(1)
                .ok_or_else(|| io::Error::other("epoch exhausted"))?,
        ),
        fact,
    ));
    let copied = Header {
        id: session.id,
        workspace: header.workspace.clone(),
        parent: None,
    };
    shared.catalog.import(&*shared.store, &copied, &entries)?;
    Ok(copied)
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
    fence: AtomicU64,
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

/// An admitted side call keeps its own cancellation scope so a successful
/// parent can revoke its contexts without cancelling work already admitted.
type Opened = Option<(CallId, llm::Messages, Cancel)>;

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
    Fenced {
        fence: u64,
        msg: Box<Msg>,
    },
    Flushed {
        fence: u64,
        through: u64,
        result: io::Result<()>,
    },
    Request {
        request: Request,
        origin: Option<Author>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Step(Event),
    /// A client to follow the session, answered once it does.
    Attach(Outbox, u64, oneshot::Sender<()>),
    Env(Arc<Environment>),
    Context {
        origin: ContextOrigin,
        request: ContextRequest,
    },
    Replied {
        call: CallId,
        replied: Replied,
        message: Option<llm::Assistant>,
    },
    Stop,
    /// Make what was only written durable, and show it: sent to itself
    /// [`SYNC_LAG`] after a write that was not synced.
    Sync,
    /// Answered once every message before it is taken.
    #[cfg(test)]
    Barrier(oneshot::Sender<()>),
    #[cfg(test)]
    Position(oneshot::Sender<(u64, u64)>),
}

/// Context authority stays bound to its call and generation even after
/// settlement removes that call from the session's cancellation map.
#[derive(Clone)]
struct ContextOrigin {
    fence: u64,
    call: CallId,
    cancel: Cancel,
}

impl ContextOrigin {
    fn current(&self, session: &Session) -> bool {
        self.fence == session.fence.load(Ordering::SeqCst) && !self.cancel.is_set()
    }

    fn send(&self, session: &Session, request: ContextRequest) {
        let _ = session.mail.send(Msg::Context {
            origin: self.clone(),
            request,
        });
    }
}

struct PendingSide {
    origin: ContextOrigin,
    reply: oneshot::Sender<Opened>,
    conversation: Option<llm::Messages>,
}

enum ContextRequest {
    /// A running call's report, answered once committed.
    Note {
        note: Note,
        done: Option<oneshot::Sender<()>>,
    },
    /// Child work is admitted under the originating call, even across polling.
    Child {
        input: String,
        reply: oneshot::Sender<Json>,
    },
    /// The actor supplies the side call's authority after committing its intent.
    Side {
        purpose: String,
        model: String,
        conversation: bool,
        reply: oneshot::Sender<Opened>,
    },
    Own {
        pgid: u32,
        ended: BoxFuture<'static, ()>,
    },
}

impl Msg {
    /// Whether taking it reads what the session committed: a client's
    /// replay, the conversation a side request asks for.
    fn reads_the_log(&self) -> bool {
        if let Self::Fenced { msg, .. } = self {
            return msg.reads_the_log();
        }
        if matches!(self, Self::Flushed { .. }) {
            return true;
        }

        #[cfg(test)]
        if matches!(self, Self::Position(_)) {
            return true;
        }
        matches!(
            self,
            Self::Attach(..)
                | Self::Context {
                    request: ContextRequest::Side { .. },
                    ..
                }
        )
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
        let source = shared.source(&header.workspace)?;
        let generation = source.as_ref().map_or_else(
            || shared.registry.generation(),
            |source| source.generation(),
        );
        let registry = match (&source, open) {
            (Some(source), Open::Load) => load_registry(header, shared, source)?,
            (Some(source), Open::Create) => source.registry(),
            (None, _) => shared.registry.get(),
        };
        let (tx, rx) = channel::unbounded();
        let session = Session::new(header, shared, Arc::new(tx), &registry);
        let bound = bind(&registry, &session);
        let binding = Arc::clone(&bound) as Arc<dyn Binding>;
        let (mut loaded, folding) = Folding::open(header, shared, open, &binding, &registry)?;
        // A process restart can read cache-resident entries that no client
        // has seen. Replaying them makes the same promise as a live commit.
        if loaded.seq != 0 {
            loaded
                .writer
                .sync()
                .inspect_err(|e| store_fault(header.id, ext_logging::Operation::Reload, e))?;
            rt::block_on(loaded.writer.flushed(&*shared.rt))
                .inspect_err(|e| store_fault(header.id, ext_logging::Operation::Reload, e))?;
        }
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
        let first = resumed(&mut core, &load, read_only, open, header.id);
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
            ordered_seq: loaded.seq,
            durable_seq: loaded.seq,
            admissions: Vec::new(),
            flushes: Vec::new(),
            reactions_cancel: Cancel::default(),
            published_activity: None,
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
    snapshot: Option<Snapshot>,
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
        let (mut loaded, folding) =
            Self::rebuild(binding, registry, shared, |restore, each| match open {
                Open::Create => {
                    let created = shared.store.create(header).inspect_err(|e| {
                        store_fault(header.id, ext_logging::Operation::Create, e);
                    })?;
                    shared.catalog.record(Record::new(header.clone()));
                    Ok(created)
                }
                Open::Load => shared
                    .store
                    .load(header.id, restore, each)
                    .inspect_err(|e| store_fault(header.id, ext_logging::Operation::Reload, e)),
            })?;
        if loaded.writer.read_only().is_none()
            && folding.core.ledger().extensions().is_some_and(|installed| {
                installed.iter().any(|extension| extension.image.is_some())
                    && installed != registry.installed()
            })
        {
            loaded.writer.keep(ReadOnly::Extensions);
        }
        Ok((loaded, folding))
    }

    fn rebuild<T>(
        binding: &Arc<dyn Binding>,
        registry: &Registry,
        shared: &Shared,
        read: impl FnOnce(&mut dyn FnMut(Snapshot) -> bool, &mut dyn FnMut(Entry)) -> io::Result<T>,
    ) -> io::Result<(T, Self)> {
        let folding = RefCell::new(Self::new(binding, registry, shared));
        let loaded = read(
            &mut |snapshot| {
                folding
                    .borrow_mut()
                    .restore(&snapshot, binding, registry, shared)
                    .is_ok()
            },
            &mut |entry| folding.borrow_mut().fold(&entry),
        )?;
        Ok((loaded, folding.into_inner()))
    }

    fn new(binding: &Arc<dyn Binding>, registry: &Registry, shared: &Shared) -> Self {
        Self {
            core: Core::new(Arc::clone(binding)).with_barriers(shared.barriers),
            states: registry.states(),
            snapshot: None,
            has_input: false,
        }
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
        self.snapshot = Some(snapshot.clone());
        let core = Core::restore(Arc::clone(binding), image.ledger);
        self.core = core.with_barriers(shared.barriers);
        self.has_input = image.has_input;
        Ok(())
    }

    fn fold(&mut self, e: &Entry) {
        self.states.apply(e);
        self.has_input |= matches!(e.body, Body::Input { .. });
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
    states: ext::Checkpoint,
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
    /// The conversation the model sees, made at the first model call.
    transcript: Option<Transcript>,
    /// The message of the reply model call `.0` just had, until its
    /// result commits.
    replied: Option<(CallId, llm::Assistant)>,
    /// The last snapshot made, and its seq; `None` for one too large.
    snapshot: Option<(u64, Option<Frame>)>,
    fold_snapshot: Option<Snapshot>,
    /// What each open call has in flight, one thing at a time.
    inflight: Keyed<CallId, InFlight>,
    /// The view a far attach is sent, kept in step with each commit from
    /// the first time one is asked for: no attach folds the whole log.
    view: Option<SessionView>,
    replies: Keyed<Ticket, oneshot::Sender<Result<(), String>>>,
    /// Side model requests waiting for their call's id, with the
    /// conversation they asked for.
    sides: Keyed<Ticket, PendingSide>,
    tickets: u64,
    phase: Phase,
    /// The session as the catalog last heard of it: busy while work is in
    /// flight, or a call waits on someone. Set before work starts, cleared
    /// after it ends.
    listed: Record,
    /// The last emitted certification, independent of summary changes and
    /// of whether the current ledger is eligible for publication.
    published_activity: Option<(u64, Activity)>,
    /// Entries written and not yet durable, in order, with whoever waits
    /// for each: shown to no one until a sync. A seq a client has seen is
    /// one that survives a power cut, so it is never given out twice.
    unsynced: Vec<(Vec<Entry>, Encoded)>,
    waiting_sync: Vec<(u64, oneshot::Sender<()>)>,
    ordered_seq: u64,
    durable_seq: u64,
    admissions: Vec<(Admission, Admitted)>,
    flushes: Vec<PendingFlush>,
    reactions_cancel: Cancel,
    /// A [`Msg::Sync`] is on its way.
    sync_armed: bool,
    /// Carrying out what a resume asked for: a commit that fails now does
    /// not resume again, the session stops.
    resuming: bool,
    /// The bytes of log the next load would read past the newest
    /// snapshot: once [`SNAPSHOT_EVERY`], the next rest is snapshotted.
    since_snapshot: u64,
}

struct PendingFlush {
    through: u64,
    ended: Flag,
    result: Option<io::Result<()>>,
}

enum Admitted {
    Effect(Effect),
    Reaction(ext::Later<()>),
    Attach(Outbox, u64, oneshot::Sender<()>),
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

    async fn refresh(&mut self) -> Result<(), ()> {
        if self.phase != Phase::Serving || !self.core.at_rest() {
            return Ok(());
        }
        let shared = &self.live.session.shared;
        let number = self.source.as_ref().map_or_else(
            || shared.registry.generation(),
            |source| source.generation(),
        );
        if number == self.generation {
            return Ok(());
        }
        // A generation cannot divide a prefix whose epoch still owns completion.
        if self.durable_seq < self.ordered_seq || !self.flushes.is_empty() {
            self.sync().await?;
            if self.durable_seq < self.ordered_seq || !self.flushes.is_empty() {
                return Ok(());
            }
        }
        self.generation = number;
        let shared = Arc::clone(&self.live.session.shared);
        let newest = self
            .source
            .as_ref()
            .map_or_else(|| shared.registry.get(), |source| source.registry());
        let bound = bind(&newest, &self.live.session);
        let binding = Arc::clone(&bound) as Arc<dyn Binding>;
        let frames = Arc::clone(&self.live.frames);
        let pending: Vec<Entry> = self
            .unsynced
            .iter()
            .flat_map(|(entries, _)| entries.iter().cloned())
            .collect();
        let registry = Arc::clone(&newest);
        let snapshot = self.fold_snapshot.clone();
        let boundary = self.core.ledger().seq();
        let live = self.live.session.state_view();
        let prepared = rt::unblock(&*Arc::clone(&shared.rt), move || {
            let start = Instant::now();
            let ((), folding) = Folding::rebuild(&binding, &registry, &shared, |restore, each| {
                let after = snapshot
                    .filter(|s| restore(s.clone()))
                    .map_or(0, |s| s.covers);
                let mut seen = after;
                for (count, entry) in frames.entries_after(after).chain(pending).enumerate() {
                    if count >= 100_000 || start.elapsed() > std::time::Duration::from_secs(1) {
                        return Err(io::Error::other(
                            "registry state preparation budget exhausted",
                        ));
                    }
                    if entry.seq.get() != seen + 1 {
                        return Err(io::Error::other("registry log boundary is incomplete"));
                    }
                    seen = entry.seq.get();
                    each(entry);
                }
                if seen != boundary || start.elapsed() > std::time::Duration::from_secs(1) {
                    return Err(io::Error::other(
                        "registry log boundary is incomplete or over budget",
                    ));
                }
                Ok(())
            })?;
            folding
                .states
                .validate(&lock(&live))
                .map_err(io::Error::other)?;
            Ok::<_, io::Error>(folding.states)
        })
        .await;
        match prepared {
            Ok(states) => {
                let current = self.source.as_ref().map_or_else(
                    || self.live.session.shared.registry.generation(),
                    |source| source.generation(),
                );
                if current != number {
                    return Ok(());
                }
                let session = &self.live.session;
                session.fence.fetch_add(1, Ordering::SeqCst);
                session.cancel_all();
                lock(&session.cancels).clear();
                self.reactions_cancel.set();
                self.reactions_cancel = Cancel::default();
                *lock(&session.states) = Arc::new(Mutex::new(states));
                self.bound = bind(&newest, session);
                self.registry = newest;
                self.step([Event::Registry(Arc::clone(&self.bound) as Arc<dyn Binding>)])
                    .await
            }
            Err(error) => {
                ext_logging::emit(ext_logging::Fact::AdoptionRejected {
                    session: self.live.id,
                    generation: number,
                });
                eprintln!("warning: {error}");
                Ok(())
            }
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
            if let Msg::Flushed {
                fence,
                through,
                result,
            } = msg
            {
                if fence == self.live.session.fence.load(Ordering::SeqCst) {
                    self.completed(through, result).await;
                    let _ = self.refresh().await;
                    self.publish();
                    self.live.session.changed.notify();
                }
                continue;
            }
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
                Ok(()) if self.durable_seq >= self.ordered_seq => {
                    for done in dones {
                        done.send(());
                    }
                }
                Ok(()) => self
                    .waiting_sync
                    .extend(dones.into_iter().map(|done| (self.ordered_seq, done))),
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
        self.drain_flushes(&mail).await;
        self.publish();
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

    async fn drain_flushes(&mut self, mail: &channel::Receiver<Msg>) {
        let _ = self.sync().await;
        while !self.flushes.is_empty() {
            let Some(msg) = mail.recv().await else { break };
            if let Msg::Flushed {
                fence,
                through,
                result,
            } = msg
                && fence == self.live.session.fence.load(Ordering::SeqCst)
            {
                self.completed(through, result).await;
            }
        }
    }

    fn current(&self, mut msg: Msg) -> Option<Msg> {
        while let Msg::Fenced { fence, msg: inner } = msg {
            if fence != self.live.session.fence.load(Ordering::SeqCst) {
                return None;
            }
            msg = *inner;
        }
        Some(msg)
    }

    /// What `msg` asks the kernel, and whom to tell once it is carried
    /// out; `None` for what the actor answers itself.
    fn take(&mut self, msg: Msg) -> Option<(Event, Option<oneshot::Sender<()>>)> {
        let msg = self.current(msg)?;
        let event = match msg {
            Msg::Fenced { .. } | Msg::Flushed { .. } => unreachable!("mailbox admission"),
            Msg::Request {
                request,
                origin,
                reply,
            } => self.requested(request, origin, reply),
            Msg::Step(event) => event,
            Msg::Attach(client, since, done) => {
                self.admissions.push((
                    Admission {
                        through: self.ordered_seq,
                        requirement: Requirement::Durable,
                    },
                    Admitted::Attach(client, since, done),
                ));
                self.admit();
                return None;
            }
            Msg::Env(env) => {
                let logged = env.redacted();
                *lock(&self.live.session.env) = Some(env);
                Event::Environment(logged)
            }
            Msg::Context { origin, request } => return self.context(&origin, request),
            Msg::Replied {
                call,
                replied,
                message,
            } => {
                if let Replied::Failed(error) = &replied {
                    provider_error(self.live.id, error);
                }
                self.inflight.remove(&call);
                self.replied = message.map(|m| (call, m));
                Event::ModelDone { call, replied }
            }
            #[cfg(test)]
            Msg::Barrier(done) => {
                done.send(());
                return None;
            }
            #[cfg(test)]
            Msg::Position(done) => {
                done.send((self.ordered_seq, self.durable_seq));
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

    fn context(
        &mut self,
        origin: &ContextOrigin,
        request: ContextRequest,
    ) -> Option<(Event, Option<oneshot::Sender<()>>)> {
        let session = &self.live.session;
        if !origin.current(session) {
            return None;
        }
        let call = origin.call;
        let event = match request {
            ContextRequest::Note { note, done } => return Some((Event::Note { call, note }, done)),
            ContextRequest::Child { input, reply } => {
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
            ContextRequest::Side {
                purpose,
                model,
                conversation,
                reply,
            } => {
                let conversation = conversation.then(|| self.conversation(call));
                self.tickets += 1;
                let ticket = Ticket(self.tickets);
                self.sides.insert(
                    ticket,
                    PendingSide {
                        origin: origin.clone(),
                        reply,
                        conversation,
                    },
                );
                Event::Side {
                    of: call,
                    purpose,
                    model,
                    ticket,
                }
            }
            ContextRequest::Own { pgid, ended } => {
                lock(&session.jobs).push(pgid);
                let session = Arc::clone(session);
                self.live.session.shared.rt.spawn(Box::pin(async move {
                    ended.await;
                    lock(&session.jobs).retain(|&p| p != pgid);
                    session.changed.notify();
                    if let Some(daemon) = session.shared.daemon.upgrade() {
                        daemon.jobs.notify();
                    }
                }));
                return None;
            }
        };
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
            self.refresh().await?;
            self.step(events).await?;
            self.refresh().await?;
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
                    fx.extend(more);
                }
                Err(panic) => {
                    let why = panicked(&*panic);
                    ext_logging::emit(ext_logging::Fact::KernelPanic {
                        session: self.live.id,
                    });
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

    /// Appends a burst once, then admits each effect against the prefix
    /// and requirement declared by the kernel. Publication and unsafe
    /// effects share an epoch while ordered work keeps making progress.
    async fn carry(&mut self, fx: Effects) -> Result<(), ()> {
        let mut batch = Vec::new();
        let mut durability = None;
        let mut through = self.ordered_seq;
        let mut publish = false;
        let mut admission = Admission {
            through: 0,
            requirement: Requirement::Ordered,
        };
        let mut after = Vec::new();
        for effect in fx {
            match effect {
                Effect::Append(entry) => batch.push(entry),
                Effect::Commit {
                    through: seq,
                    durability: how,
                    publish: show,
                } => {
                    through = through.max(seq);
                    durability = durability.max(Some(how));
                    publish |= show;
                }
                Effect::Require(required) => admission = required,
                effect => {
                    match &effect {
                        Effect::Cancel { call } => self.cancel_pending(*call, &mut after),
                        Effect::MarkBusy(true) => self.effect(effect),
                        _ => after.push((admission, effect)),
                    }
                    admission = Admission {
                        through: 0,
                        requirement: Requirement::Ordered,
                    };
                }
            }
        }
        debug_assert_eq!(
            batch.is_empty(),
            durability.is_none(),
            "a step commits once"
        );
        let needs = after
            .iter()
            .filter(|(a, _)| a.requirement == Requirement::Durable)
            .map(|(a, _)| a.through)
            .max()
            .unwrap_or(0);
        let flush = (publish || needs > self.durable_seq) && through > self.covering();
        let busy = after
            .iter()
            .rev()
            .find_map(|(_, e)| match e {
                Effect::MarkBusy(busy) => Some(*busy),
                _ => None,
            })
            .unwrap_or_else(|| self.live.has_work());
        let written = if let Some(durability) = durability {
            self.commit(
                &mut batch,
                durability == Durability::Barrier,
                flush,
                self.listing(busy),
            )
            .await
        } else if flush {
            self.order_flush().await
        } else {
            Ok(())
        };
        if let Err(error) = written {
            self.failed(
                format!("logging: {error}"),
                after.into_iter().map(|(_, effect)| effect).collect(),
            )
            .await;
            return Err(());
        }
        self.admissions.extend(
            after
                .into_iter()
                .map(|(a, effect)| (a, Admitted::Effect(effect))),
        );
        self.admit();
        Ok(())
    }

    fn cancel_pending(&mut self, call: CallId, after: &mut Vec<(Admission, Effect)>) {
        let mut cancelled = None;
        let mut retain = |effect: &Effect| {
            if effect.starts_call() != Some(call) {
                return true;
            }
            match effect {
                Effect::RunTool { .. } | Effect::Resume { .. } => {
                    cancelled = Some(true);
                }
                Effect::Prepare { .. } | Effect::Send { .. } => {
                    cancelled = Some(false);
                }
                _ => {}
            }
            false
        };
        self.admissions.retain(|(_, work)| match work {
            Admitted::Effect(effect) => retain(effect),
            _ => true,
        });
        after.retain(|(_, effect)| retain(effect));
        let running = matches!(
            self.inflight.get(&call),
            Some(InFlight::Tool(_) | InFlight::Model)
        );
        self.effect(Effect::Cancel { call });
        if !running && let Some(tool) = cancelled {
            let event = if tool {
                Event::ToolDone {
                    call,
                    outcome: Outcome::Interrupted { output: None },
                    card: None,
                }
            } else {
                Event::ModelDone {
                    call,
                    replied: Replied::Interrupted(None),
                }
            };
            self.live.session.emit(
                self.live.session.fence.load(Ordering::SeqCst),
                Msg::Step(event),
            );
        }
    }

    fn covering(&self) -> u64 {
        self.flushes
            .iter()
            .map(|pending| pending.through)
            .max()
            .unwrap_or(self.durable_seq)
            .max(self.durable_seq)
    }

    fn admit(&mut self) {
        let mut waiting = Vec::new();
        for (admission, work) in std::mem::take(&mut self.admissions) {
            let watermark = match admission.requirement {
                Requirement::Ordered => self.ordered_seq,
                Requirement::Durable => self.durable_seq,
            };
            if admission.through > watermark {
                waiting.push((admission, work));
                continue;
            }
            match work {
                Admitted::Effect(effect) => self.effect(effect),
                Admitted::Attach(client, since, done) => {
                    self.attach(&client, since);
                    done.send(());
                }
                Admitted::Reaction(reaction) => {
                    if self.phase == Phase::Draining {
                        continue;
                    }
                    let cancel = self.reactions_cancel.clone();
                    let ended = self.live.ended.clone();
                    self.live.session.shared.rt.spawn(Box::pin(async move {
                        unless(&ended, unless(&cancel, reaction.0)).await;
                    }));
                }
            }
        }
        self.admissions = waiting;
    }

    fn durable(&mut self, through: u64) {
        self.durable_seq = self.durable_seq.max(through);
        self.show_synced();
        self.publish();
        self.admit();
    }

    fn track(&mut self, through: u64, mut epoch: rt::FlushEpoch) -> io::Result<()> {
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        if let Poll::Ready(result) = std::pin::Pin::new(&mut epoch).poll(&mut cx) {
            result.inspect_err(|e| store_fault(self.live.id, ext_logging::Operation::Commit, e))?;
            if self.flushes.is_empty() {
                self.durable(through);
            } else {
                let ended = Flag::default();
                ended.set();
                self.flushes.push(PendingFlush {
                    through,
                    ended,
                    result: Some(Ok(())),
                });
            }
            return Ok(());
        }
        let ended = Flag::default();
        self.flushes.push(PendingFlush {
            through,
            ended: ended.clone(),
            result: None,
        });
        let session = Arc::clone(&self.live.session);
        let fence = session.fence.load(Ordering::SeqCst);
        self.live.session.shared.rt.spawn(Box::pin(async move {
            let result = epoch.await;
            ended.set();
            let _ = session.mail.send(Msg::Flushed {
                fence,
                through,
                result,
            });
        }));
        Ok(())
    }

    async fn completed(&mut self, through: u64, result: io::Result<()>) {
        let Some(pending) = self
            .flushes
            .iter_mut()
            .find(|pending| pending.through == through)
        else {
            return;
        };
        pending.result = Some(result);
        // Notification tasks may wake out of order. An earlier failure
        // must fence recovery before a later success can publish anything.
        while self
            .flushes
            .first()
            .is_some_and(|pending| pending.result.is_some())
        {
            let pending = self.flushes.remove(0);
            match pending.result.expect("completed epoch") {
                Ok(()) => self.durable(pending.through),
                Err(error) => {
                    store_fault(self.live.id, ext_logging::Operation::Commit, &error);
                    self.failed(format!("flushing: {error}"), Vec::new()).await;
                    break;
                }
            }
        }
    }

    /// A commit failed for `why`: the effects `rest` after it are dropped,
    /// whoever they would have answered told why, and the session resumes
    /// from what the log holds.
    async fn failed(&mut self, why: String, mut rest: Vec<Effect>) {
        rest.extend(
            std::mem::take(&mut self.admissions)
                .into_iter()
                .filter_map(|(_, work)| match work {
                    Admitted::Effect(effect) => Some(effect),
                    Admitted::Reaction(_) => None,
                    Admitted::Attach(client, _, done) => {
                        crate::server::reply(
                            &client,
                            &DaemonMsg::Error {
                                message: why.clone(),
                            },
                        );
                        done.send(());
                        None
                    }
                }),
        );
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
        recovery(self.live.id, ext_logging::Rule::ReloadAfterFault);
        self.live.session.fence.fetch_add(1, Ordering::SeqCst);
        self.reactions_cancel.set();
        self.reactions_cancel = Cancel::default();
        self.live.session.cancel_all();
        for pending in std::mem::take(&mut self.flushes) {
            pending.ended.wait().await;
        }
        self.admissions.clear();
        self.transcript = None;
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
            loaded
                .writer
                .sync()
                .inspect_err(|e| store_fault(header.id, ext_logging::Operation::Reload, e))?;
            io::Result::Ok((loaded, folding))
        })
        .await;
        let reloaded = match reloaded {
            Ok((mut loaded, folding)) => loaded
                .writer
                .flushed(&*self.live.session.shared.rt)
                .await
                .inspect_err(|e| store_fault(self.live.id, ext_logging::Operation::Reload, e))
                .map(|()| (loaded, folding)),
            Err(error) => Err(error),
        };
        let (loaded, folding) = match reloaded {
            Ok(reloaded) => reloaded,
            Err(e) => {
                eprintln!("session {}: reloading: {e}", self.live.id.short());
                self.phase = Phase::Draining;
                return;
            }
        };
        self.ordered_seq = loaded.seq;
        self.durable_seq = loaded.seq;
        let recovered = FrameLog::new(Arc::clone(&loaded.lines), loaded.seq);
        self.live.frames.reloaded(loaded.lines);
        self.writer = Some(loaded.writer);
        self.since_snapshot = loaded.read;
        self.core = folding.core;
        *lock(&self.live.session.state_view()) = folding.states;
        self.fold_snapshot = folding.snapshot;
        self.live
            .has_input
            .store(folding.has_input, Ordering::SeqCst);
        self.transcript = None;
        self.replied = None;
        self.snapshot = None;
        self.view = None;
        for entry in recovered.entries_after(self.live.frames.seq()) {
            let lines = Encoded::of(std::slice::from_ref(&entry)).expect("loaded entry encodes");
            self.show(std::slice::from_ref(&entry), &lines);
        }
        self.publish();
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
                self.publish();
            }
            // Its log is what it is: the session stops, and loads again
            // when next wanted.
            Err(panic) => {
                let why = panicked(&*panic);
                ext_logging::emit(ext_logging::Fact::KernelPanic {
                    session: self.live.id,
                });
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

    /// Registers the epoch after the write barriers, in the appending
    /// job. Ordered completion returns before that thread runs its flush,
    /// so replay-safe work can overlap the syscall.
    async fn commit(
        &mut self,
        batch: &mut Vec<Entry>,
        durable: bool,
        flush: bool,
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
        let (writer, entries, written, listing, epoch, durable_on_order) =
            rt::unblock_flush(&*rt, move || {
                let first = listing.as_ref().is_some_and(|l| l.busy);
                if first && let Some(listing) = &listing {
                    shared.catalog.record(listing.clone());
                }
                let written =
                    catch_unwind(AssertUnwindSafe(|| writer.append(&mut entries, durable)))
                        .unwrap_or_else(|_| Err(io::Error::other("committing entries panicked")));
                if !first
                    && written.is_ok()
                    && let Some(listing) = &listing
                {
                    shared.catalog.record(listing.clone());
                }
                let durable_on_order = durable && writer.durable_on_order();
                let request = (written.is_ok() && flush && !durable_on_order)
                    .then(|| writer.enroll())
                    .flatten();
                let (epoch, leader) = match request {
                    Some(request) => (Some(request.epoch), request.leader),
                    None => (None, None),
                };
                (
                    (writer, entries, written, listing, epoch, durable_on_order),
                    leader,
                )
            })
            .await;
        // Not cleared after entries that did not commit.
        if written.is_err() && listing.is_some_and(|l| !l.busy) {
            self.listed = listed;
        }
        self.writer = Some(writer);
        let lines = written
            .inspect_err(|e| store_fault(self.live.id, ext_logging::Operation::Commit, e))?;
        crate::session_event("commit", self.live.id, || {
            let how = if durable { "synced" } else { "written" };
            format!("{} entries {how}", entries.len())
        });
        self.ordered_seq = entries
            .last()
            .map_or(self.ordered_seq, |entry| entry.seq.get());
        self.since_snapshot += lines.lines().map(|line| line.len() as u64 + 1).sum::<u64>();
        self.unsynced.push((entries.clone(), lines));
        self.folded(&entries);
        if let Some(epoch) = epoch {
            self.track(self.ordered_seq, epoch)?;
        } else if durable_on_order || flush {
            self.durable(self.ordered_seq);
        }
        if !self.unsynced.is_empty() {
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
                ext_logging::emit(ext_logging::Fact::Fault {
                    session: Some(self.live.id),
                    operation: ext_logging::Operation::Snapshot,
                    kind: ext_logging::Fault(io::ErrorKind::InvalidData),
                });
                eprintln!("session {}: snapshot: {e}", self.live.id.short());
                return;
            }
        };
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
                .and_then(|image| {
                    let snapshot = Snapshot { covers, image };
                    writer.snapshot(&snapshot)?;
                    Ok(snapshot)
                });
            (writer, taken)
        })
        .await;
        self.writer = Some(writer);
        match taken {
            Ok(snapshot) => {
                self.fold_snapshot = Some(snapshot);
                crate::session_event("snapshot", self.live.id, || format!("at {covers}"));
            }
            Err(e) => {
                store_fault(self.live.id, ext_logging::Operation::Snapshot, &e);
                eprintln!("session {}: snapshot: {e}", self.live.id.short());
            }
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
        if self.ordered_seq <= self.covering() {
            return Ok(());
        }
        if let Err(error) = self.order_flush().await {
            self.unsynced.clear();
            self.waiting_sync.clear();
            Box::pin(self.failed(format!("syncing: {error}"), Vec::new())).await;
            return Err(());
        }
        Ok(())
    }

    async fn order_flush(&mut self) -> io::Result<()> {
        let Some(mut writer) = self.writer.take() else {
            return Err(io::Error::other("no writer"));
        };
        let rt = Arc::clone(&self.live.session.shared.rt);
        let (writer, result, epoch) = rt::unblock_flush(&*rt, move || {
            let result = writer.sync();
            let request = result.is_ok().then(|| writer.enroll()).flatten();
            let (epoch, leader) = match request {
                Some(request) => (Some(request.epoch), request.leader),
                None => (None, None),
            };
            ((writer, result, epoch), leader)
        })
        .await;
        self.writer = Some(writer);
        result.inspect_err(|e| store_fault(self.live.id, ext_logging::Operation::Commit, e))?;
        if let Some(epoch) = epoch {
            self.track(self.ordered_seq, epoch)?;
        } else {
            self.durable(self.ordered_seq);
        }
        Ok(())
    }

    /// Shows what was written and is durable now, and answers whoever
    /// waited for it.
    fn show_synced(&mut self) {
        let mut pending = Vec::new();
        for (entries, lines) in std::mem::take(&mut self.unsynced) {
            if entries
                .last()
                .is_some_and(|entry| entry.seq.get() <= self.durable_seq)
            {
                self.show(&entries, &lines);
            } else {
                pending.push((entries, lines));
            }
        }
        self.unsynced = pending;
        let mut waiting = Vec::new();
        for (through, done) in std::mem::take(&mut self.waiting_sync) {
            if through <= self.durable_seq {
                done.send(());
            } else {
                waiting.push((through, done));
            }
        }
        self.waiting_sync = waiting;
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

    /// Ordered entries also feed speculative transcripts and observers.
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
            if matches!(e.body, Body::Input { .. }) {
                self.live.has_input.store(true, Ordering::SeqCst);
            }

            // What observers do about it runs owned by the session.
            for reaction in hooks.observe(self.bound.point(), e) {
                if let Some(work) = reaction.work {
                    self.admissions.push((
                        Admission {
                            through: e.seq.get(),
                            requirement: reaction.requirement,
                        },
                        Admitted::Reaction(work),
                    ));
                }
            }
            let replied = match &e.body {
                Body::ModelResult { call, .. } => self.replied.take_if(|(of, _)| of == call),
                _ => None,
            };
            if let Some(transcript) = &mut self.transcript {
                transcript.apply(e, replied.map(|(_, m)| m));
            }
            if let Body::ToolResult { call, .. } | Body::ModelResult { call, .. } = &e.body {
                self.inflight.remove(call);
                session.forget(*call);
                self.bound.settled(*call);
            }
        }
    }

    /// Lists the visible prefix and certifies its activity only when the
    /// whole ledger is visible. Eligibility can change without the summary.
    fn publish(&mut self) {
        let seq = self.live.frames.seq();
        let activity = self.core.ledger().activity();
        {
            let mut summary = lock(&self.live.summary);
            summary.entries = seq;
            summary.activity = activity;
        }
        let current = self.current_activity();
        if self.published_activity == current {
            return;
        }
        if let Some((seq, activity)) = current
            && let Ok(frame) = Frame::new(&DaemonMsg::Activity { seq, activity })
        {
            lock(&self.live.followers).retain(|c| c.send_after(seq, frame.clone()).is_ok());
            self.published_activity = current;
        }
    }

    fn current_activity(&self) -> Option<(u64, Activity)> {
        let seq = self.core.ledger().seq();
        (self.live.frames.seq() == seq && self.durable_seq >= seq)
            .then(|| (seq, self.core.ledger().activity()))
    }

    fn effect(&mut self, effect: Effect) {
        let shared = Arc::clone(&self.live.session.shared);
        // A stopping session starts nothing: what it would have started is
        // in the log, for its next load.
        if self.phase == Phase::Draining && effect.starts() {
            return;
        }
        match effect {
            Effect::Append(_) | Effect::Commit { .. } | Effect::Require(_) => {
                unreachable!("appends commit in runs")
            }
            Effect::Reply { ticket, result } => self.reply(ticket, result),
            Effect::Side { ticket, side } => {
                if let Some(PendingSide {
                    reply,
                    conversation,
                    ..
                }) = self.sides.remove(&ticket)
                {
                    let side = side.filter(|side| self.core.ledger().call(*side).is_some());
                    reply.send(side.map(|side| {
                        let cancel = self.live.session.cancel(side);
                        (side, conversation.unwrap_or_default(), cancel)
                    }));
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
                self.sides.0.retain(|(_, side)| side.origin.call != call);
                for (side, open) in self.core.ledger().open_calls() {
                    if open.call == (protocol::ledger::Call::Side { of: call }) {
                        self.live.session.cancelled(side);
                    }
                }
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
        let fence = self.live.session.fence.load(Ordering::SeqCst);
        self.live.session.shared.rt.spawn(Box::pin(async move {
            if let Some(done) = work.await {
                let _ = mail.send(Msg::Fenced {
                    fence,
                    msg: Box::new(Msg::Step(then(done))),
                });
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
        self.live.session.emit(
            self.live.session.fence.load(Ordering::SeqCst),
            Msg::Step(prepared),
        );
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
        let fence = live.session.fence.load(Ordering::SeqCst);
        self.live.session.shared.rt.spawn(Box::pin(async move {
            let (replied, message) = match *prepared {
                Ok((model, request, attempt)) => {
                    let mut deltas = |text: &str| live.delta(call, attempt, text, fence);
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
            live.session.emit(
                fence,
                Msg::Replied {
                    call,
                    replied,
                    message,
                },
            );
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
        let transcript = self.transcript.get_or_insert_with(|| {
            Transcript::of(
                frames.entries().chain(
                    self.unsynced
                        .iter()
                        .flat_map(|(entries, _)| entries.iter().cloned()),
                ),
                Arc::clone(registry),
                system,
            )
        });
        if let Some(messages) = transcript.before(call.get()) {
            messages.clone()
        } else {
            // Recovery keeps the call's conversation boundary, including later
            // transcript edits and context republished while preparing it.
            let before = frames
                .entries()
                .chain(
                    self.unsynced
                        .iter()
                        .flat_map(|(entries, _)| entries.iter().cloned()),
                )
                .filter(|e| {
                    e.seq < call.0 || matches!(e.body, Body::Context { .. } | Body::Edit { .. })
                });
            Transcript::of(before, Arc::clone(registry), system).into_messages()
        }
    }

    /// Runs tool call `call`, the model's or a user's command, on its
    /// input as read at its intent if the gate left it as it was,
    /// reporting its outcome.
    fn run_tool(&mut self, call: CallId, tool: String, input: Json, child: Child) {
        self.inflight.insert(call, InFlight::Tool(child));
        let registry = Arc::clone(&self.registry);
        let parsed = self.bound.take_parsed(call, &input);
        let fence = self.live.session.fence.load(Ordering::SeqCst);
        let origin = self
            .live
            .session
            .origin(call, fence)
            .expect("actor generation");
        let cx = self.live.session.cx(origin, &registry);
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
            let _ = mail.send(Msg::Fenced {
                fence,
                msg: Box::new(Msg::Step(done)),
            });
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
        if let Some((seq, activity)) = self.current_activity() {
            frames.extend(
                Frame::new(&DaemonMsg::Activity { seq, activity })
                    .ok()
                    .map(|f| (seq, f)),
            );
        }
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
    fn delta(&self, call: CallId, attempt: u16, text: &str, fence: u64) {
        if self.frames.seq() < call.get() || self.session.fence.load(Ordering::SeqCst) != fence {
            return;
        }
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
    fn new(header: &Header, shared: &Arc<Shared>, mail: Mail, registry: &Registry) -> Arc<Self> {
        Arc::new(Self {
            fence: AtomicU64::new(0),
            id: header.id,
            workspace: Arc::from(header.workspace.as_path()),
            spill: Arc::from(shared.spill.join(header.id.to_string())),
            mail,
            states: Mutex::new(Arc::new(Mutex::new(registry.states()))),
            env: Mutex::default(),
            jobs: Mutex::default(),
            cancels: Mutex::default(),
            changed: rt::Notify::new(),
            shared: Arc::clone(shared),
        })
    }

    fn emit(&self, fence: u64, msg: Msg) {
        let _ = self.mail.send(Msg::Fenced {
            fence,
            msg: Box::new(msg),
        });
    }

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

    /// Retained contexts must lose authority before this call can be forgotten.
    fn forget(&self, call: CallId) {
        self.cancelled(call);
    }

    /// Stops every call's work.
    fn cancel_all(&self) {
        for cancel in lock(&self.cancels).values() {
            cancel.set();
        }
    }

    fn origin(&self, call: CallId, fence: u64) -> Option<ContextOrigin> {
        let mut cancels = lock(&self.cancels);
        // A retained view cannot acquire a replacement call's cancellation token.
        if self.fence.load(Ordering::SeqCst) != fence {
            return None;
        }
        let cancel = if let Some(cancel) = cancels.get(&call) {
            cancel.clone()
        } else {
            let cancel = Cancel::default();
            cancels.insert(call, cancel.clone());
            cancel
        };
        Some(ContextOrigin {
            fence,
            call,
            cancel,
        })
    }

    fn cx(self: &Arc<Self>, origin: ContextOrigin, registry: &Arc<Registry>) -> Cx {
        Parts {
            workspace: Arc::clone(&self.workspace),
            env: lock(&self.env).clone(),
            session: self.id,
            call: origin.call,
            model: self.model_spec().into(),
            spill: Arc::clone(&self.spill),
            cancel: origin.cancel.clone(),
            services: self.services(origin.clone()),
        }
        .cx(Arc::new(Calling {
            origin,
            session: Arc::clone(self),
            registry: Arc::clone(registry),
        }))
    }

    /// What the engine provides the session's calls: the config layers,
    /// child sessions, side model requests and the runtime.
    fn services(self: &Arc<Self>, origin: ContextOrigin) -> Arc<Services> {
        let own = Arc::new(Own(Arc::downgrade(self), origin));
        Arc::new(
            Services::default()
                .with::<dyn ext::Layers>(own.clone())
                .with::<dyn ext::Sessions>(own.clone())
                .with::<dyn ext::Models>(own)
                .with::<dyn rt::Runtime>(Arc::clone(&self.shared.rt)),
        )
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
    fence: u64,
}

impl View for Viewed {
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
        read(&lock(&self.session.state_view()));
    }

    fn env(&self) -> Option<Arc<Environment>> {
        lock(&self.session.env).clone()
    }

    /// With the call's own cancel: work a handler takes it along to ends
    /// with the call.
    fn cx(&self, call: CallId) -> Option<Cx> {
        let origin = self.session.origin(call, self.fence)?;
        Some(self.session.cx(origin, &self.registry))
    }
}

/// `registry` bound to `session`: what the kernel drives.
fn bind(registry: &Arc<Registry>, session: &Arc<Session>) -> Arc<Bound<Viewed>> {
    let settings = &session.shared.models.settings;
    let agent = settings.agent(&session.workspace);
    let viewed = Arc::new(Viewed {
        session: Arc::clone(session),
        registry: Arc::clone(registry),
        fence: session.fence.load(Ordering::SeqCst),
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

/// The engine as a running call reaches it: reports go through the actor's mailbox.
struct Calling {
    origin: ContextOrigin,
    session: Arc<Session>,
    registry: Arc<Registry>,
}

impl Calling {
    fn report(&self, note: Note) {
        self.origin
            .send(&self.session, ContextRequest::Note { note, done: None });
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
        self.origin
            .send(&self.session, ContextRequest::Own { pgid, ended });
    }

    fn progress(&self, text: &str) {
        self.report(Note::Progress(text.to_owned()));
    }

    /// Resolves once the change is committed, so a read after it sees it.
    fn note(&self, ext: &str, state: &str, data: Json) -> BoxFuture<'static, ()> {
        let (ext, state) = (ext.to_owned(), state.to_owned());
        let (done, committed) = oneshot::channel();
        let note = ContextRequest::Note {
            note: Note::State { ext, state, data },
            done: Some(done),
        };
        self.origin.send(&self.session, note);
        Box::pin(async move {
            let _ = committed.await;
        })
    }

    fn publish(&self, namespace: &str, key: Json, text: String) -> BoxFuture<'static, ()> {
        let (done, committed) = oneshot::channel();
        self.origin.send(
            &self.session,
            ContextRequest::Note {
                note: Note::Publish {
                    namespace: namespace.to_owned(),
                    key,
                    text,
                },
                done: Some(done),
            },
        );
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
        read(&lock(&self.session.state_view()));
    }

    fn registry(&self) -> &Registry {
        &self.registry
    }
}

impl ext::Models for Own {
    /// Logged under the call as a model call of its own, then sent once:
    /// its outcome is logged even if its successful parent has already settled.
    fn call<'a>(
        &'a self,
        _cx: &'a Cx,
        side: ext::Side,
    ) -> BoxFuture<'a, Result<llm::Response, llm::Error>> {
        Box::pin(async move {
            let invalid = |e: String| llm::Error::new(llm::Kind::InvalidRequest, e);
            let session = self
                .0
                .upgrade()
                .ok_or_else(|| invalid("the session is gone".into()))?;
            let fence = self.1.fence;
            if !self.1.current(&session) {
                return Err(invalid("the call was cancelled".into()));
            }
            let model = match &side.model {
                Some(spec) => {
                    let env = lock(&session.env).clone();
                    (session.shared.models).model(&session.workspace, env.as_deref(), spec)
                }
                None => session.model(),
            }
            .map_err(invalid)?;
            let of = self.1.call;
            let (asking, opened) = oneshot::channel();
            let purpose = side.purpose.to_string();
            self.1.send(
                &session,
                ContextRequest::Side {
                    purpose,
                    model: model.name(),
                    conversation: side.conversation,
                    reply: asking,
                },
            );
            let Some(Some((id, conversation, cancel))) = opened.await else {
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
            // of its own, stopped by its own cancel: a handler that
            // drops its answer does not leave the side call open.
            let (answered, answer) = oneshot::channel();
            let timeout = session.shared.rt.sleep(side.budget.timeout);
            let mail = Arc::clone(&session.mail);
            let session_id = session.id;
            session.shared.rt.spawn(Box::pin(async move {
                // Read as the call's own replies are, with nothing streamed.
                let mut quiet = |_: &str| {};
                let asked = reply(&model, &request, &cancel, &mut quiet);
                let (outcome, answer) =
                    sided(session_id, first(asked, timeout).await.map(|(r, _)| r));
                let _ = mail.send(Msg::Fenced {
                    fence,
                    msg: Box::new(Msg::Step(Event::SideDone { side: id, outcome })),
                });
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
fn sided(
    session: SessionId,
    replied: Option<Replied>,
) -> (Outcome, Result<llm::Response, llm::Error>) {
    let (outcome, answer) = match replied {
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
            return (Outcome::Interrupted { output }, Err(error));
        }
        None => {
            let error = llm::Error::new(llm::Kind::Transport, "the side request timed out");
            let failed = Outcome::Failed {
                error: error.to_string(),
            };
            (failed, Err(error))
        }
    };
    let answer = answer.inspect_err(|error| provider_error(session, error));
    (outcome, answer)
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
/// config layers, child sessions and side model requests. Its authority
/// belongs to the context that obtained the service, never a later call.
struct Own(Weak<Session>, ContextOrigin);

impl ext::Layers for Own {
    fn model(&self, workspace: &Path) -> Result<(String, String), String> {
        let session = self.0.upgrade().ok_or("the session is gone")?;
        ext::Layers::model(&session.shared.models.settings, workspace)
    }
}

impl ext::Sessions for Own {
    /// The child the call started, or starts now; an interrupt of the
    /// call's turn reaches it first.
    fn child<'a>(&'a self, _cx: &'a Cx, input: String) -> BoxFuture<'a, Outcome> {
        Box::pin(async move {
            let Some(session) = self.0.upgrade() else {
                return Outcome::Interrupted { output: None };
            };
            let (reply, result) = oneshot::channel();
            if !self.1.current(&session) {
                return Outcome::Interrupted { output: None };
            }
            self.1
                .send(&session, ContextRequest::Child { input, reply });
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

fn provider_error(session: SessionId, error: &llm::Error) {
    use ext_logging::Provider;
    let kind = match error.kind {
        llm::Kind::Transport => Provider::Transport,
        llm::Kind::Auth => Provider::Auth,
        llm::Kind::RateLimit => Provider::RateLimit,
        llm::Kind::Quota => Provider::Quota,
        llm::Kind::ContextOverflow => Provider::ContextOverflow,
        llm::Kind::InvalidRequest => Provider::InvalidRequest,
        llm::Kind::ContentFilter => Provider::ContentFilter,
        llm::Kind::Server => Provider::Server,
        llm::Kind::Decode => Provider::Decode,
        llm::Kind::Incomplete => Provider::Incomplete,
        llm::Kind::Unsupported => Provider::Unsupported,
        llm::Kind::Other => Provider::Other,
    };
    ext_logging::emit(ext_logging::Fact::ProviderError {
        session,
        kind,
        status: error.status,
        retryable: error.is_retryable(),
    });
}

fn store_fault(session: SessionId, operation: ext_logging::Operation, error: &io::Error) {
    ext_logging::emit(ext_logging::Fact::Fault {
        session: Some(session),
        operation,
        kind: ext_logging::Fault(error.kind()),
    });
}

fn resumed(
    core: &mut Core,
    load: &Load<'_>,
    read_only: Option<ReadOnly>,
    open: Open,
    session: SessionId,
) -> Effects {
    if read_only.is_some() {
        return Effects::new();
    }
    if matches!(open, Open::Load) && load.busy {
        recovery(session, ext_logging::Rule::ResumeLedger);
    }
    core.resumed(load)
}

fn recovery(session: SessionId, rule: ext_logging::Rule) {
    ext_logging::emit(ext_logging::Fact::Recovery { session, rule });
}
