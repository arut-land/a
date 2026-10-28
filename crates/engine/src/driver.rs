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
use std::collections::BTreeMap;
use std::future::{Future, poll_fn};
use std::io::{self, Read as _, Write as _};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::Poll;
use std::time::Instant;

use crate::catalog::Record;
use crate::store::{Encoded, Header, Loaded, Parent, Snapshot, Writer};
use ext::host::{Binding, Bound, Host, Parts, Registry, SessionServices};
use ext::point::View;
use ext::{BoxFuture, Called, Cancel, Cx, Flag, States};
use kernel::{
    Admission, Core, Event, Guarded, Load, Note, Owner, Replied, Requirement, StepPlan, Ticket,
    Work as Effect,
};
use protocol::ledger::Ledger;
use protocol::view::SessionView;
use protocol::{
    Activity, Author, Body, CallId, DaemonMsg, Entry, Environment, Frame, Json, Mode, Outcome,
    ReadOnly, Residency, SessionId, SessionSummary, ToolChoice, WaitOn,
};
use rt::BlockingClass::Cpu;
use rt::{channel, oneshot, or};
use serde::{Deserialize, Serialize};

use crate::model::{Model, Models};
use crate::outbox::{FrameLog, Outbox, Subscribers};
use crate::transcript::Transcript;
use crate::{Daemon, lock};
use kernel::SideFact;

#[path = "journal.rs"]
mod journal;
#[path = "projection.rs"]
mod projection;
use journal::{FlushMode, Journal};
use projection::{Invocations, Projection};
#[path = "work.rs"]
mod work;

mod handing;
use work::{WorkHandle, WorkId, WorkSet};
#[path = "services.rs"]
mod services;
use services::{CallPorts, SessionLayers, Viewed, bind};

mod inbox;
mod side;
pub(crate) mod spawn;

pub struct Shared {
    pub http_clients: std::sync::OnceLock<Arc<dyn transport::HttpClients>>,
    pub rt: rt::Rt,
    /// The daemon's one file watcher, which every extension subscribes to.
    pub hub: Arc<rt::watch::Hub>,
    pub store: Arc<dyn crate::store::LogStore>,
    pub catalog: crate::catalog::Catalog,
    pub registry: ext::host::Current,
    pub models: Models,
    pub barriers: kernel::Barriers,
    /// This build: what a snapshot must be of to be restored.
    pub build: String,
    /// The blobs every session's entries reference, one namespace.
    pub blobs: Arc<dyn ext::blob::BlobStore>,
    /// How those blobs appear as files.
    pub tree: Arc<dyn ext::blob::Materialize>,
    pub(crate) loaded: Mutex<std::collections::HashSet<SessionId>>,
    /// What the daemon's previous build handed over and no session has
    /// taken up yet: each takes its own as it loads.
    pub(crate) handed: Mutex<Vec<crate::handover::Handed>>,
    pub(crate) daemon: Weak<Daemon>,
}

impl Shared {
    fn source(&self, workspace: &Path) -> io::Result<Option<Arc<dyn ext::host::Generations>>> {
        self.models.settings.refresh(workspace);
        self.registry
            .get()
            .service::<dyn ext::host::Sources>()
            .map(|source| {
                let rt = Arc::clone(&self.rt);
                source.open(
                    workspace,
                    {
                        let rt = Arc::clone(&self.rt);
                        Arc::new(move |task| rt.spawn(task))
                    },
                    {
                        let rt = Arc::clone(&self.rt);
                        Arc::new(move |duration| rt.sleep(duration))
                    },
                    Arc::new(move |work| {
                        let admitted = rt.spawn_blocking(Cpu, rt::STORAGE_BYTES, work);
                        rt.spawn(Box::pin(async move {
                            let _ = admitted.await;
                        }));
                    }),
                    {
                        let hub = Arc::clone(&self.hub);
                        Arc::new(move |root, changed| {
                            let watch = rt::watch::Watch::tree(root);
                            Box::new(hub.subscribe(watch, move |_| changed()))
                        })
                    },
                )
            })
            .transpose()
            .map_err(io::Error::other)
    }
}

impl Shared {
    fn abandoned(
        &self,
        header: &Header,
        ledger: &Ledger,
    ) -> io::Result<Vec<kernel::CancelledInput>> {
        let mut abandoned = Vec::new();
        let entries = self.store.prefix(header.id, ledger.seq())?;
        let active = entries
            .iter()
            .rev()
            .take_while(|e| !matches!(e.body, Body::Interrupt { .. }))
            .find_map(|e| match e.body {
                Body::Take { input, .. } => Some(input),
                _ => None,
            });
        for entry in &entries {
            let Body::Enqueue { envelope } = &entry.body else {
                continue;
            };
            let seq = entry.seq;
            if Some(seq) != active && !ledger.untaken().iter().any(|q| q.seq == seq) {
                continue;
            }
            let id = envelope.id();
            let entries = if let Some(live) = self
                .daemon
                .upgrade()
                .and_then(|d| d.slot(id.sender))
                .and_then(|s| s.live())
            {
                live.frames.entries()?
            } else if let Some(record) = self.catalog.get(id.sender) {
                self.store.prefix(id.sender, record.entries)?
            } else {
                continue;
            };
            if let Some(cancelled) = entries
                .iter()
                .find_map(|fact| kernel::CancelledInput::interrupted(entry, fact))
            {
                abandoned.push(cancelled);
            }
        }
        Ok(abandoned)
    }

    pub(crate) fn owner(&self, header: &Header) -> io::Result<Owner> {
        let Some(mut parent) = header.parent else {
            return Ok(Owner::None);
        };
        let Some(daemon) = self.daemon.upgrade() else {
            return Ok(Owner::Gone);
        };
        let mut child = header.id;
        loop {
            let Some(slot) = daemon.slot(parent.session) else {
                return Ok(Owner::Gone);
            };
            let cancelled = if let Some(live) = slot.live() {
                cancelled_child(live.frames.entries()?, child)
            } else {
                let seq = self.catalog.get(parent.session).map_or(0, |r| r.entries);
                cancelled_child(self.store.prefix(parent.session, seq)?, child)
            };
            if cancelled {
                return Ok(Owner::Cancelled);
            }
            let Some(ancestor) = slot.header.parent else {
                return Ok(Owner::Present);
            };
            child = parent.session;
            parent = ancestor;
        }
    }
}

fn cancelled_child(entries: impl IntoIterator<Item = Entry>, child: SessionId) -> bool {
    let mut watch = None;
    let mut link = None;
    let mut caller = None;
    for entry in entries {
        match entry.body {
            Body::Link { child: linked, .. } if linked == child => {
                link = Some(entry.seq);
                caller = None;
            }
            Body::Wait {
                call,
                on: WaitOn::Child { link: waiting },
            } if link == Some(waiting) => caller = Some(call),
            // An awaited child still delivers its outcome. Its waiting
            // call's interrupted result proves the owner's cancellation.
            Body::ToolResult {
                call,
                outcome: Outcome::Interrupted { .. },
                ..
            } if caller == Some(call) => return true,
            Body::Watch { child: watched } if watched == child => watch = Some(entry.seq),
            Body::Unwatch { watch: cancelled } if watch == Some(cancelled) => return true,
            _ => {}
        }
    }
    false
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
        (Some(source), None) => Ok(source.snapshot().registry),
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

#[derive(Clone, Copy)]
pub enum Open {
    Create,
    Load,
}

/// What a session's calls and decision points read: made before its log
/// is folded, as the kernel's registry binds it.
struct Session {
    services: Arc<SessionServices>,
    spawn: Mutex<Option<spawn::SpawnIntent>>,
    context_cuts: Mutex<crate::context::Cuts>,
    fence: AtomicU64,
    id: SessionId,
    workspace: Arc<Path>,
    blobs: ext::blob::Blobs,
    mail: Mail,
    states: Mutex<Arc<Mutex<States>>>,
    env: Mutex<Option<Arc<Environment>>>,
    pending_env: Mutex<Option<Arc<Environment>>>,
    /// Background process groups the session owns, until each ends.
    jobs: Mutex<Vec<Job>>,
    /// Calls gone on in the background whose groups can go on in the
    /// daemon's next build ([`ext::Cx::resumable`]).
    resumable: Mutex<Vec<Resumed>>,
    /// Calls whose groups went to the daemon's next build: what they would
    /// still report here is that build's to report.
    handed: Mutex<Vec<CallId>>,
    /// What the daemon's previous build handed this session, until taken
    /// up; what is left once the session runs is killed as it drops.
    adopting: Mutex<Vec<crate::handover::Handed>>,
    work: Mutex<WorkSet>,
    /// Notified as the session changes: a commit, work beginning or
    /// ending, a job ending, the actor ending. What waits on the session
    /// waits here, never by polling.
    changed: rt::Notify,
    shared: Arc<Shared>,
}

type Mail = Arc<channel::Sender<Msg>>;

/// A process group a session owns, left to it by call `call` of `tool`,
/// and what it is to the session.
struct Job {
    call: CallId,
    /// `None` for a call the log no longer names.
    tool: Option<String>,
    role: ext::Role,
    lease: Arc<ext::ProcessLease>,
}

/// A call gone on in the background, running `process`, that goes on in
/// the daemon's next build from `state`.
struct Resumed {
    call: CallId,
    process: Arc<dyn ext::RunningProcess>,
    state: Json,
}

pub struct Live {
    session: Arc<Session>,
    pub id: SessionId,
    pub workspace: PathBuf,
    pub parent: Option<Parent>,
    /// The committed entries, encoded once for every client.
    frames: Arc<FrameLog>,
    followers: Mutex<Subscribers>,
    /// Live text of calls whose `model_call` entry is not shown yet, with
    /// the fence it was streamed under: a stream can start before its call
    /// is durable, and its deltas follow the call once it is shown.
    early: Mutex<Vec<(CallId, u64, Frame)>>,
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

enum Opened {
    Fresh(SideAdmitted),
    Cached {
        outcome: Outcome,
        failure: Option<Json>,
    },
    Closed,
}

struct SideAdmitted {
    id: CallId,
    thread: protocol::publication::ThreadImage,
    limited: bool,
    cancel: Cancel,
    started: Option<Arc<side::Started>>,
}

pub enum Request {
    Input {
        segments: protocol::Segments,
        mode: Mode,
        force: bool,
    },
    Withdraw {
        input: protocol::Seq,
    },
    Interrupt,
    Answer {
        call: CallId,
        value: Json,
    },
}

enum Completion {
    Event(Event),
    Tool {
        call: CallId,
        called: Called,
    },
    DeliveryLost {
        ticket: Option<Ticket>,
    },
    Delivered {
        id: protocol::EnvelopeId,
        recipient: SessionId,
        result: Result<(), protocol::InboxRefusal>,
        ticket: Option<Ticket>,
    },
    Replied {
        call: CallId,
        replied: Replied,
        message: Option<llm::Assistant>,
        execution: Option<Json>,
    },
    SideFact {
        side: CallId,
        fact: SideFact,
        execution: Option<Json>,
        done: oneshot::Sender<Result<(), String>>,
    },
}

enum Msg {
    AwaitEnvelope {
        id: protocol::EnvelopeId,
        reply: oneshot::Sender<Opened>,
    },
    Enqueue {
        envelope: protocol::Envelope,
        reply: oneshot::Sender<Result<(), protocol::InboxRefusal>>,
    },

    Completed {
        fence: u64,
        completion: Completion,
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
    Env,
    Context {
        origin: ContextOrigin,
        request: ContextRequest,
    },

    #[cfg(test)]
    ProjectionCache(bool, oneshot::Sender<[bool; 3]>),
    Reclaim,
    Stop,
    /// Whether the session's work can go to the daemon's next build: see
    /// [`Live::handable`].
    Handable(oneshot::Sender<Result<(), String>>),
    /// Stop the session's groups for a handover, and the session with them:
    /// see [`Live::freeze`].
    Freeze(oneshot::Sender<Result<(), String>>),
    /// Call the handover off: see [`Live::thaw`].
    Thaw,
    /// Give the stopped groups up for good: see [`Live::commit`].
    Commit(oneshot::Sender<Vec<(sys::handover::Given, crate::handover::Owner)>>),
    /// A group the previous build handed over, owned again by `tool`.
    Reheld {
        call: CallId,
        tool: String,
        lease: ext::ProcessLease,
        admission: Admission,
    },
    /// Make what was only written durable, and show it: sent to itself
    /// [`SYNC_LAG`] after a write that was not synced.
    Sync,
    /// Answered once every message before it is taken.
    #[cfg(test)]
    Barrier(oneshot::Sender<()>),
    #[cfg(test)]
    Position(oneshot::Sender<(u64, u64)>),
}

type ContextOrigin = WorkHandle<CallId>;

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
    /// An ephemeral turn's role, which settles its attempt here.
    ephemeral: Option<ext::ModelRole>,
    while_waiting: bool,
    background: bool,
    cancel: Cancel,
    origin: ContextOrigin,
    reply: oneshot::Sender<Opened>,
    thread: protocol::publication::ThreadImage,
    limited: bool,
}

enum ContextRequest {
    Invoke {
        context: ext::point::InvocationContext,
        name: String,
        tool: String,
        input: Json,
        reply: oneshot::Sender<Called>,
    },
    Send {
        id: protocol::EnvelopeId,
        to: ext::Recipient,
        payload: String,
        reply: oneshot::Sender<Result<(), protocol::InboxRefusal>>,
    },
    /// A running call's report, answered once committed.
    Note {
        note: Note,
        done: Option<oneshot::Sender<ext::MutationResult>>,
    },
    Child {
        request: ext::Spawn,
        reply: spawn::SpawnReply,
    },
    Side {
        registry: Arc<Registry>,
        side: ext::ModelTurn,
        reply: oneshot::Sender<Opened>,
    },
    Own {
        process: ext::ProcessLease,
        role: ext::Role,
    },
}

impl Msg {
    /// Whether a session frozen for a handover takes it up: what ends the
    /// handover, or the session, or answers about it; the rest waits.
    const fn passes_freeze(&self) -> bool {
        matches!(
            self,
            Self::Handable(_) | Self::Freeze(_) | Self::Thaw | Self::Commit(_) | Self::Stop
        )
    }

    fn reads_the_log(&self) -> bool {
        if matches!(self, Self::Flushed { .. }) {
            return true;
        }

        #[cfg(test)]
        if matches!(self, Self::Position(_) | Self::ProjectionCache(..)) {
            return true;
        }
        matches!(
            self,
            Self::AwaitEnvelope { .. }
                | Self::Attach(..)
                | Self::Context {
                    request: ContextRequest::Side { .. } | ContextRequest::Child { .. },
                    ..
                }
        )
    }
}

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
    /// Whether the session's log references blob `blob`.
    pub(crate) async fn references(&self, blob: protocol::BlobId) -> io::Result<bool> {
        let frames = Arc::clone(&self.frames);
        let image = lock(&self.session.spawn)
            .as_ref()
            .map(|spawn| spawn.image.clone());
        let places = self.session.blobs.places();
        rt::unblock(&*self.session.shared.rt, move || {
            if frames.references(&blob)? {
                return Ok(true);
            }
            image.map_or(Ok(false), |image| {
                crate::context::references(&image, &places, blob)
            })
        })
        .await
    }

    /// Opens session `header`'s log, folds it into a kernel and starts the
    /// actor, which resumes it: the only way a session recovers. It blocks
    /// on the store, so it runs on a thread that may.
    pub(crate) fn start(
        header: &Header,
        shared: &Arc<Shared>,
        open: Open,
        owner: Owner,
    ) -> io::Result<Arc<Self>> {
        Self::start_in(header, shared, open, owner, None)
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn start_in(
        header: &Header,
        shared: &Arc<Shared>,
        open: Open,
        owner: Owner,
        admitted: Option<Arc<Registry>>,
    ) -> io::Result<Arc<Self>> {
        let source = shared.source(&header.workspace)?;
        let snapshot = source
            .as_ref()
            .map_or_else(|| shared.registry.snapshot(), |source| source.snapshot());
        let generation = snapshot.id;
        let (registry, image) = spawn::restore(
            header,
            shared,
            open,
            source.as_ref(),
            admitted.unwrap_or(snapshot.registry),
        )?;
        let (tx, rx) = channel::unbounded();
        let session = Session::new(header, shared, Arc::new(tx), &registry);
        *lock(&session.spawn) = image;
        let bound = bind(&registry, &session);
        let binding = services::binding(&bound, &session, &registry);
        let (mut loaded, folding) = Folding::open(header, shared, open, &binding, &registry)?;
        // A process restart can read cache-resident entries that no client
        // has seen. Replaying them makes the same promise as a live commit.
        if loaded.seq != 0 {
            loaded
                .writer
                .sync()
                .inspect_err(|e| store_fault(header.id, ext_logging::Operation::Reload, e))?;
            loaded
                .writer
                .flush_inline()
                .inspect_err(|e| store_fault(header.id, ext_logging::Operation::Reload, e))?;
        }
        let Folding {
            mut core,
            states,
            snapshot: fold_snapshot,
            has_input,
            invocations,
        } = folding;
        *lock(&session.state_view()) = states;
        *lock(&session.env) = core.ledger().env().cloned().map(Arc::new);
        let fresh = lock(&shared.loaded).insert(header.id);
        let listed = (shared.catalog.get(header.id)).unwrap_or_else(|| Record::new(header.clone()));
        let busy = listed.busy;
        let abandoned = if header.parent.is_some() && loaded.writer.read_only().is_none() {
            shared.abandoned(header, core.ledger())?
        } else {
            Vec::new()
        };
        let handed = crate::handover::take(&shared.handed, header.id);
        let (adopted, reheld): (Vec<_>, Vec<_>) = (handed.iter())
            .partition(|handed| matches!(handed.owner.kind, crate::handover::Kind::Call { .. }));
        let adopted: Vec<CallId> = adopted.iter().map(|handed| handed.owner.call).collect();
        let reheld: Vec<CallId> = reheld.iter().map(|handed| handed.owner.call).collect();
        *lock(&session.adopting) = handed;
        let load = Load {
            session: Some(header.id),
            abandoned: &abandoned,
            fresh,
            owner,
            busy,
            created: matches!(open, Open::Create),
            adopted: &adopted,
            reheld: &reheld,
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
            early: Mutex::default(),
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
        if !session.model_only() {
            let started = match open {
                Open::Create => ext::point::Started::Created,
                Open::Load => ext::point::Started::Loaded,
            };
            registry.hooks().session_start(&bound.point(), started);
        }
        let actor = Actor {
            live: Arc::clone(&live),
            core,
            registry,
            bound,
            source,
            generation,
            inflight: BTreeMap::default(),
            replies: Replies::default(),
            transport_active: false,
            delivery_queue: std::collections::VecDeque::new(),
            detached: Vec::new(),
            sides: BTreeMap::default(),
            ephemeral: Vec::new(),
            waiting_sides: Vec::new(),
            tickets: 0,
            phase: Phase::Serving,
            listed,
            admissions: Vec::new(),
            resuming: false,
            frozen: None,
            deferred: std::collections::VecDeque::new(),
            rested: true,
            journal: Journal {
                writer: Some(loaded.writer),
                fold_snapshot,
                ordered_seq: loaded.seq,
                durable_seq: loaded.seq,
                since_snapshot: loaded.read,
                ..Journal::default()
            },
            projection: Projection {
                invocations,
                ..Projection::default()
            },
        };
        let run = Box::pin(actor.run(rx, first));
        // The sessions-on-threads measurement (docs/perf/runtime-demand.md)
        // runs each actor on an executor of its own; nothing shipped does.
        #[cfg(test)]
        if let Some(spawn) = crate::tests::runtime_demand::ACTORS.get() {
            spawn(run);
            return Ok(live);
        }
        shared.rt.spawn(run);
        Ok(live)
    }

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
    pub fn set_env(&self, env: impl Into<Arc<Environment>>) -> io::Result<()> {
        let env = env.into();
        if env.retained_bytes() > crate::outbox::BUDGET.bytes {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "environment exceeds the queue budget",
            ));
        }
        let mut pending = lock(&self.session.pending_env);
        if pending.replace(env).is_none() {
            self.send(Msg::Env);
        }
        Ok(())
    }

    pub(crate) fn settled(&self, child: SessionId, outcome: protocol::ToolOutcome) {
        self.send(Msg::Step(Event::ChildSettled { child, outcome }));
    }

    pub(crate) fn cancel_owned(&self) {
        self.send(Msg::Step(Event::OwnerCancelled));
    }

    pub(crate) fn interrupt(&self) {
        let interrupt = Event::Interrupt {
            origin: None,
            ticket: None,
        };
        self.send(Msg::Step(interrupt));
    }

    pub(crate) fn reclaim(&self) {
        self.send(Msg::Reclaim);
    }

    pub(crate) fn stop(&self) {
        self.session
            .services
            .get::<ext::SessionLifetimes>()
            .expect("session lifetimes")
            .close();
        self.send(Msg::Stop);
        for job in lock(&self.session.jobs).iter() {
            let _ = job.lease.signal(ext::ProcessSignal::Kill);
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

    /// Whether it owns process groups of the user's work: helpers it
    /// starts again when needed do not count.
    pub fn has_jobs(&self) -> bool {
        (lock(&self.session.jobs).iter()).any(|job| job.role == ext::Role::Work)
    }

    /// The process groups the session owns.
    pub(crate) fn jobs(&self) -> Vec<Arc<ext::ProcessLease>> {
        (lock(&self.session.jobs).iter())
            .map(|job| Arc::clone(&job.lease))
            .collect()
    }

    /// Whether the session's work can go to the daemon's next build, and
    /// if not why: see [`crate::handover`]. A session that takes no
    /// messages runs nothing to hand.
    pub(crate) async fn handable(&self) -> Result<(), String> {
        let (reply, answer) = oneshot::channel();
        self.send(Msg::Handable(reply));
        answer.await.unwrap_or(Ok(()))
    }

    /// Stops every group the session's work runs, for a handover, and the
    /// session with them: it takes nothing more up until it is thawed
    /// ([`Live::thaw`]) or commits ([`Live::commit`]). Refused, and nothing
    /// stopped, while the session is at work or runs work no other build
    /// takes up, or as a group ends meanwhile.
    pub(crate) async fn freeze(&self) -> Result<(), String> {
        let (reply, answer) = oneshot::channel();
        self.send(Msg::Freeze(reply));
        answer.await.unwrap_or(Ok(()))
    }

    /// Calls a handover off: every group stopped is supervised again, and
    /// the session takes up what came meanwhile.
    pub(crate) fn thaw(&self) {
        self.send(Msg::Thaw);
    }

    /// Gives every group stopped up for good, each with whose it is.
    pub(crate) async fn commit(&self) -> Vec<(sys::handover::Given, crate::handover::Owner)> {
        let (reply, answer) = oneshot::channel();
        self.send(Msg::Commit(reply));
        answer.await.unwrap_or_default()
    }

    /// How long the session has had no attachments, no work and no jobs.
    pub fn idle_for(&self) -> std::time::Duration {
        if self.attached.load(Ordering::SeqCst) > 0 || self.has_work() || self.has_jobs() {
            return std::time::Duration::ZERO;
        }
        lock(&self.last_active).elapsed()
    }

    fn touch(&self) {
        *lock(&self.last_active) = Instant::now();
        if let Some(daemon) = self.session.shared.daemon.upgrade() {
            daemon.pulse.notify();
        }
    }
}

struct Folding {
    core: Core,
    states: States,
    snapshot: Option<u64>,
    has_input: bool,
    invocations: Invocations,
}

impl Folding {
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
            invocations: Invocations::default(),
        }
    }

    fn restore(
        &mut self,
        snapshot: &Snapshot,
        binding: &Arc<dyn Binding>,
        registry: &Registry,
        shared: &Shared,
    ) -> Result<(), String> {
        let image: RecoveryImage = snapshot.image.parse().map_err(|e| e.to_string())?;
        if image.ledger.seq() != snapshot.covers {
            return Err("checkpoint sequence mismatch".into());
        }
        if image.build != shared.build {
            return Err(format!("of build {}", image.build));
        }
        if image.extensions != registry.installed() {
            return Err("of other extensions".to_owned());
        }
        self.states.restore(&image.states)?;
        self.invocations = image.invocations;
        self.snapshot = Some(snapshot.covers);
        let core = Core::restore(Arc::clone(binding), image.ledger);
        self.core = core.with_barriers(shared.barriers);
        self.has_input = image.has_input;
        Ok(())
    }

    fn fold(&mut self, e: &Entry) {
        if let Body::Spawned { intent } = &e.body {
            let image = spawn::SpawnIntent::from_json(intent).expect("admitted spawn intent");
            self.states
                .restore_settings(&image.settings)
                .expect("admitted settings image");
        }
        self.states.apply(e);
        self.has_input |= matches!(e.body, Body::Input { .. });
        self.core.load(e);
        let _ = self.invocations.fold(e);
    }
}

/// Only idle kernels can be checkpointed; transient scheduling state is absent.
#[derive(Serialize, Deserialize)]
struct RecoveryImage {
    build: String,
    extensions: Vec<protocol::Installed>,
    has_input: bool,
    ledger: Ledger,
    states: ext::Checkpoint,
    invocations: Invocations,
}

const SNAPSHOT_EVERY: u64 = 64 << 10;

fn summary(header: &Header, ledger: &Ledger) -> SessionSummary {
    SessionSummary {
        id: header.id,
        workspace: protocol::HostPath::lossy(&header.workspace),
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

    /// The registry generation the newest step ran on.
    registry: Arc<Registry>,
    /// The same, bound to the session: what the kernel drives.
    bound: Arc<Bound<Viewed>>,
    source: Option<Arc<dyn ext::host::Generations>>,
    generation: u64,

    inflight: BTreeMap<CallId, InFlight>,

    replies: Replies,
    detached: Vec<(
        SessionId,
        oneshot::Sender<Result<ext::ConversationHandle, ext::SpawnRefusal>>,
    )>,
    sides: BTreeMap<Ticket, PendingSide>,
    /// Admitted ephemeral turns, by side call: what their attempt started,
    /// for when an interrupt settles them before they report.
    ephemeral: Vec<(CallId, ext::ModelRole, Arc<side::Started>)>,
    transport_active: bool,
    delivery_queue: std::collections::VecDeque<(protocol::Envelope, Option<Ticket>)>,

    waiting_sides: Vec<(CallId, CallId)>,
    tickets: u64,
    phase: Phase,
    /// The session as the catalog last heard of it: busy while work is in
    /// flight, or a call waits on someone. Set before work starts, cleared
    /// after it ends.
    listed: Record,

    admissions: Vec<Guarded<Admitted>>,

    /// Carrying out what a resume asked for: a commit that fails now does
    /// not resume again, the session stops.
    resuming: bool,
    /// Frozen for a handover: what it stopped, and what came meanwhile.
    frozen: Option<handing::Frozen>,
    /// Messages a thaw lets through, before any newer.
    deferred: std::collections::VecDeque<Msg>,
    /// At rest but for what runs in the background, as of the last burst.
    rested: bool,

    journal: Journal,
    projection: Projection,
}

enum Waiter {
    Tool(oneshot::Sender<Called>),
    Model(oneshot::Sender<Opened>),
}

#[derive(Default)]
struct Replies {
    invocations: BTreeMap<Ticket, (oneshot::Sender<Called>, ext::point::InvocationContext)>,
    contexts: BTreeMap<CallId, ext::point::InvocationContext>,
    invoked: BTreeMap<CallId, Vec<Waiter>>,
    clients: BTreeMap<Ticket, oneshot::Sender<Result<(), String>>>,
    deliveries: BTreeMap<Ticket, oneshot::Sender<Result<(), protocol::InboxRefusal>>>,
}

impl Drop for Replies {
    fn drop(&mut self) {
        for (_, reply) in std::mem::take(&mut self.deliveries) {
            reply.send(Err(protocol::InboxRefusal::Interrupted));
        }
    }
}

fn respond<T: Send + 'static>(
    pending: &mut BTreeMap<Ticket, oneshot::Sender<T>>,
    ticket: Ticket,
    result: T,
) {
    if let Some(reply) = pending.remove(&ticket) {
        reply.send(result);
    }
}

enum Admitted {
    Effect(Effect),
    Reaction(ext::Later<()>),
    Attach(Outbox, u64, oneshot::Sender<()>),
}

/// How long written entries may stay unsynced, and so unseen: a sync is
/// asked for this long after a write that none followed.
const SYNC_LAG: std::time::Duration = std::time::Duration::from_millis(50);

struct PreparedModel {
    model: Model,
    request: llm::Prepared,
    attempt: u16,
    // Readers borrow this generation, so extensions retaining a reader cannot pin themselves.
    registry: Arc<Registry>,
}

enum PreparedStep {
    Request(llm::Prepared),
    Answered(ext::Name, ext::point::Stepped),
}

enum InFlight {
    /// A model call's request, prepared and not yet sent: the step point's
    /// decisions about it are logged in between. `Err` is why there is no
    /// model to send it to.
    Prepared(Box<Result<PreparedModel, String>>),
    Thread(Box<(ext::ModelTurn, llm::Messages, Result<Model, llm::Error>)>),
    /// A model call sent; it reports once, with [`Completion::Replied`].
    Model,
    /// Side work must report before the session closes its writer.
    Side,
    /// A tool call running; it reports once, with [`Event::ToolDone`].
    Tool(Child),
    Later,
}

enum Child {
    None,
    Waiting(oneshot::Sender<Json>),
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
            .filter(|f| matches!(f, InFlight::Model | InFlight::Side | InFlight::Tool(_)))
            .count()
    }

    fn can_refresh(&self) -> bool {
        lock(&self.live.session.spawn).is_none()
            && self.phase == Phase::Serving
            && self.core.at_rest()
    }
    fn source_snapshot(&self) -> ext::host::GenerationSnapshot {
        self.source.as_ref().map_or_else(
            || self.live.session.shared.registry.snapshot(),
            |source| source.snapshot(),
        )
    }

    async fn refresh(&mut self) -> Result<(), ()> {
        if !self.can_refresh() {
            return Ok(());
        }
        let snapshot = self.source_snapshot();
        let number = snapshot.id;
        if number == self.generation {
            return Ok(());
        }
        // A generation cannot divide a prefix whose epoch still owns completion.
        if !self.journal.is_durable() {
            self.sync().await?;
            if self.journal.durable_seq < self.journal.ordered_seq
                || !self.journal.flushes.is_empty()
            {
                return Ok(());
            }
        }
        self.generation = number;
        let shared = Arc::clone(&self.live.session.shared);
        let newest = snapshot.registry;
        let bound = bind(&newest, &self.live.session);
        let binding = services::binding(&bound, &self.live.session, &newest);
        let frames = Arc::clone(&self.live.frames);
        let pending: Vec<Entry> = self.pending_entries().collect();
        let registry = Arc::clone(&newest);
        let snapshot = self.journal.fold_snapshot;
        let session = self.live.id;
        let boundary = self.core.ledger().seq();
        let live = self.live.session.state_view();
        let prepared = rt::unblock_with(
            &*Arc::clone(&shared.rt),
            Cpu,
            rt::STORAGE_BYTES,
            move || {
                let start = Instant::now();
                let ((), mut folding) =
                    Folding::rebuild(&binding, &registry, &shared, |restore, each| {
                        let after = snapshot
                            .and_then(|covers| shared.store.read_snapshot(session, covers))
                            .filter(|s| restore(s.clone()))
                            .map_or(0, |s| s.covers);
                        let mut seen = after;
                        for batch in frames
                            .batches_after(after)
                            .chain(std::iter::once(Ok(pending)))
                        {
                            for entry in batch? {
                                if seen - after >= 100_000
                                    || start.elapsed() > std::time::Duration::from_secs(1)
                                {
                                    return Err(io::Error::other(
                                        "registry state preparation budget exhausted",
                                    ));
                                }
                                if entry.seq.get() != seen + 1 {
                                    return Err(io::Error::other(
                                        "registry log boundary is incomplete",
                                    ));
                                }
                                seen = entry.seq.get();
                                each(entry);
                            }
                        }
                        if seen != boundary || start.elapsed() > std::time::Duration::from_secs(1) {
                            return Err(io::Error::other(
                                "registry log boundary is incomplete or over budget",
                            ));
                        }
                        Ok(())
                    })?;
                let live = lock(&live);
                folding.states.adopt(&live).map_err(io::Error::other)?;
                Ok::<_, io::Error>(folding.states)
            },
        )
        .await;
        match prepared {
            Ok(states) => {
                if self.source_snapshot().id != number {
                    return Ok(());
                }
                self.retire_contexts();
                let session = &self.live.session;
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

    async fn refresh_settings(&mut self) {
        let shared = Arc::clone(&self.live.session.shared);
        let workspace = self.live.workspace.clone();
        if shared.models.settings.due(&workspace) {
            rt::unblock(&*Arc::clone(&shared.rt), move || {
                shared.models.settings.refresh(&workspace);
            })
            .await;
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn run(mut self, mail: channel::Receiver<Msg>, first: StepPlan) {
        let _ = self.carry(first).await;
        self.adopt_jobs();
        let mut next = None;
        while !(self.phase == Phase::Draining && self.reporting() == 0)
            && let Some(msg) = match next.take().or_else(|| self.thawed()) {
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
                && let Some(msg) = self.thawed().or_else(|| mail.try_recv())
            {
                if msg.reads_the_log() {
                    next = Some(msg);
                    break;
                }
                burst.push(msg);
            }
            self.refresh_settings().await;
            let (mut events, mut mutations) = (Vec::new(), Vec::new());
            let dones: Vec<oneshot::Sender<()>> = Vec::new();
            #[cfg(test)]
            let mut dones = dones;
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
                // Frozen for a handover, the session takes nothing up.
                if self.frozen.is_some() && !msg.passes_freeze() {
                    self.hold(msg);
                    continue;
                }
                // Kept as it is, its catalog listing and all.
                if let Some(why) = self.journal.writer.as_ref().and_then(Writer::read_only) {
                    self.refuse(msg, why).await;
                    continue;
                }
                if let Some((event, done)) = self.take(msg).await {
                    mutations.push(done);
                    events.push(event);
                }
            }
            // Recovery does not commit the burst that failed.
            match self.burst(events, mutations, sync).await {
                Ok(()) if self.journal.durable_seq >= self.journal.ordered_seq => {
                    for done in dones {
                        done.send(());
                    }
                }
                Ok(()) => self.journal.waiting_sync.extend(
                    dones
                        .into_iter()
                        .map(|done| (self.journal.ordered_seq, done)),
                ),
                Err(()) => drop(dones),
            }
            // A call may begin to wait without the work marked ending.
            self.mark().await;
            self.publish();
            self.live.session.changed.notify();
            // Come to rest but for what runs in the background, the session
            // may be handed over: an upgrade waiting on it hears so.
            let rested = self.core.at_rest_but_background();
            if rested && !self.rested && self.live.has_work() {
                self.live.touch();
            }
            self.rested = rested;
            if self.journal.since_snapshot >= SNAPSHOT_EVERY {
                self.keep_snapshot().await;
            }
        }
        self.drain_flushes(&mail, next).await;
        self.publish();
        if self.journal.since_snapshot >= SNAPSHOT_EVERY {
            self.keep_snapshot().await;
        }
        // Listed as it ends, with all it logged.
        let listing = self.listing(self.live.has_work());
        let shared = Arc::clone(&self.live.session.shared);
        rt::unblock(&*Arc::clone(&shared.rt), move || {
            shared.catalog.record(listing);
        })
        .await;
        if !self.live.session.model_only() {
            self.registry.hooks().session_end(&self.bound.point());
        }
        // Closed and let go before the end is told: a reload never makes
        // a second writer, and what is told the end holds the last of it.
        let ended = self.live.ended.clone();
        let session = Arc::clone(&self.live.session);
        drop((self, mail));
        ended.set();
        session.changed.notify();
        if let Some(daemon) = session.shared.daemon.upgrade() {
            daemon.ended(session.id);
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn take(
        &mut self,
        msg: Msg,
    ) -> Option<(Event, Option<oneshot::Sender<ext::MutationResult>>)> {
        let event = match msg {
            Msg::Flushed { .. } => unreachable!("mailbox admission"),
            Msg::Completed { fence, completion } => {
                if fence != self.live.session.fence.load(Ordering::SeqCst) {
                    return None;
                }
                if let Completion::Tool { call, .. } = &completion
                    && lock(&self.live.session.handed).contains(call)
                {
                    return None;
                }
                match completion {
                    Completion::Event(event) => event,
                    Completion::Tool { call, called } => Event::ToolDone {
                        call,
                        outcome: called.outcome,
                        card: called.card,
                        held: called.held,
                    },
                    Completion::DeliveryLost { ticket } => {
                        self.transport_active = false;
                        if let Some(reply) = ticket.and_then(|t| self.replies.deliveries.remove(&t))
                        {
                            reply.send(Err(protocol::InboxRefusal::Interrupted));
                        }
                        self.pump_delivery();
                        return None;
                    }
                    Completion::Delivered {
                        id,
                        recipient,
                        result,
                        ticket,
                    } => {
                        self.transport_active = false;
                        self.pump_delivery();
                        Event::Delivered {
                            id,
                            recipient,
                            result,
                            ticket,
                        }
                    }
                    Completion::SideFact {
                        side,
                        fact,
                        execution,
                        done,
                    } => {
                        // A side already settled, by an interrupt, keeps no late execution.
                        if self.core.ledger().call(side).is_some() {
                            self.projection
                                .executions
                                .extend(execution.map(|fact| (side, fact)));
                        }
                        self.side_fact(side, fact, done)
                    }
                    Completion::Replied {
                        call,
                        replied,
                        message,
                        execution,
                    } => {
                        self.projection
                            .executions
                            .extend(execution.map(|fact| (call, fact)));
                        if let Replied::Failed(error) = &replied {
                            provider_error(self.live.id, error);
                        }
                        self.inflight.remove(&call);
                        self.projection.replied = message.map(|m| (call, m));
                        Event::ModelDone { call, replied }
                    }
                }
            }
            Msg::Request {
                request,
                origin,
                reply,
            } => self.requested(request, origin, reply),
            Msg::Step(event) => event,
            Msg::Attach(client, since, done) => {
                self.admissions.push(Guarded {
                    admission: Admission {
                        through: self.journal.ordered_seq,
                        requirement: Requirement::Durable,
                    },
                    work: Admitted::Attach(client, since, done),
                });
                self.admit().await;
                return None;
            }
            Msg::Env => {
                let env = lock(&self.live.session.pending_env).take()?;
                let logged = env.redacted();
                *lock(&self.live.session.env) = Some(env);
                Event::Environment(logged)
            }
            Msg::Context { origin, request } => return self.context(&origin, request).await,
            Msg::AwaitEnvelope { id, reply } => {
                self.envelope_reply(id, reply).await;
                return None;
            }
            Msg::Enqueue { envelope, reply } => {
                let ticket = self.delivery_ticket(reply);
                Event::Enqueue { envelope, ticket }
            }
            #[cfg(test)]
            Msg::Barrier(done) => {
                done.send(());
                return None;
            }
            #[cfg(test)]
            Msg::Position(done) => {
                done.send((self.journal.ordered_seq, self.journal.durable_seq));
                return None;
            }
            #[cfg(test)]
            Msg::ProjectionCache(populate, reply) => {
                if populate {
                    self.conversation(CallId(
                        protocol::Seq::new(self.journal.ordered_seq.max(1)).unwrap(),
                    ))
                    .await
                    .unwrap();
                    self.snapshot(self.journal.ordered_seq).await;
                }
                reply.send([
                    self.projection.transcript.is_some(),
                    self.projection.snapshot.is_some(),
                    self.projection.view.is_some(),
                ]);
                return None;
            }
            Msg::Sync => return None,
            Msg::Reclaim => {
                if !self.live.idle_for().is_zero() && self.live.summary().activity == Activity::Idle
                {
                    self.projection.transcript = None;
                    self.projection.snapshot = None;
                    self.projection.view = None;
                }
                return None;
            }
            Msg::Stop => {
                self.phase = Phase::Draining;
                self.live.session.cancel_all();
                return None;
            }
            Msg::Handable(reply) => {
                reply.send(self.handable());
                return None;
            }
            Msg::Freeze(reply) => {
                reply.send(self.freeze_handover().await);
                return None;
            }
            Msg::Thaw => {
                self.thaw_handover();
                return None;
            }
            Msg::Commit(reply) => {
                reply.send(self.commit_handover());
                return None;
            }
            Msg::Reheld {
                call,
                tool,
                lease,
                admission,
            } => {
                self.own_job(call, Some(tool), ext::Role::Work, lease, admission);
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
            lock(&self.live.session.resumable).retain(|resumed| resumed.call != *call);
        }
        Some((event, None))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "every port shares the origin admission check"
    )]
    async fn context(
        &mut self,
        origin: &ContextOrigin,
        request: ContextRequest,
    ) -> Option<(Event, Option<oneshot::Sender<ext::MutationResult>>)> {
        let session = Arc::clone(&self.live.session);
        if !origin.current(&session) {
            let succeeded = matches!(&request, ContextRequest::Side { side, .. } if side.is_background())
                && self.background_owner_succeeded(origin).await;
            if !succeeded {
                if let ContextRequest::Note {
                    done: Some(done), ..
                } = request
                {
                    done.send(Err(ext::MutationError::Revoked));
                }
                return None;
            }
        }
        let call = origin.id;
        match request {
            ContextRequest::Invoke {
                context,
                name,
                tool,
                input,
                reply,
            } => {
                let ticket = self.ticket();
                self.replies.invocations.insert(ticket, (reply, context));
                Some((
                    Event::Invoke {
                        existing: None,
                        parent: call,
                        name,
                        tool,
                        input,
                        ticket,
                    },
                    None,
                ))
            }
            ContextRequest::Send {
                id,
                to,
                payload,
                reply,
            } => {
                let children: Vec<_> = self.core.ledger().links().iter().map(|l| l.child).collect();
                let from = ext::Topology {
                    session: self.live.id,
                    parent: self.live.parent.map(|p| p.session),
                    children: &children,
                };
                // The recipients are resolved once, as the send commits.
                let resolved = if id.sender == self.live.id && id.call == call {
                    (self.registry.service::<dyn ext::Route>())
                        .map_or(Err(protocol::InboxRefusal::Unauthorized), |route| {
                            route.resolve(from, &to)
                        })
                } else {
                    Err(protocol::InboxRefusal::Unauthorized)
                };
                let recipients = match resolved {
                    Ok(recipients) => recipients.into_vec(),
                    Err(refusal) => {
                        reply.send(Err(refusal));
                        return None;
                    }
                };
                let outgoing = protocol::Outgoing {
                    id,
                    recipients,
                    payload,
                };
                let ticket = self.delivery_ticket(reply);
                Some((Event::SendEnvelope { outgoing, ticket }, None))
            }
            ContextRequest::Note { note, done } => {
                if let Note::State { ext, state, .. } = &note
                    && !lock(&session.state_view()).registered(ext, state)
                {
                    if let Some(done) = done {
                        done.send(Err(ext::MutationError::Unregistered));
                    }
                    return None;
                }
                Some((Event::Note { call, note }, done))
            }
            ContextRequest::Child { request, reply } => self
                .open_spawn(call, request, reply)
                .await
                .map(|event| (event, None)),
            ContextRequest::Side {
                registry,
                side,
                reply,
            } => self
                .open_side(origin.clone(), registry, side, reply)
                .await
                .map(|event| (event, None)),
            ContextRequest::Own { process, role } => {
                let ledger = self.core.ledger();
                let tool = match ledger.call(call).map(|open| &open.call) {
                    Some(protocol::ledger::Call::Tool { tool, .. }) => Some(tool.clone()),
                    _ => (ledger.detached().iter())
                        .find(|d| d.call == call)
                        .map(|d| d.tool.clone()),
                };
                self.own_job(call, tool, role, process, origin.admission);
                None
            }
        }
    }

    /// Owns `process`, left by call `call` of `tool`, until its group ends.
    fn own_job(
        &mut self,
        call: CallId,
        tool: Option<String>,
        role: ext::Role,
        process: ext::ProcessLease,
        admission: Admission,
    ) {
        let session = Arc::clone(&self.live.session);
        let process = Arc::new(process);
        lock(&session.jobs).push(Job {
            call,
            tool,
            role,
            lease: Arc::clone(&process),
        });
        let fence = session.fence.load(Ordering::SeqCst);
        self.live.session.spawn_work(
            WorkId::Job(Arc::as_ptr(&process).cast::<()>() as usize),
            admission,
            |cancel| {
                Box::pin(async move {
                    let mut ended = pin!(process.ended());
                    let stopped = unless(&cancel, &mut ended).await.is_none();
                    if stopped {
                        let _ = process.signal(ext::ProcessSignal::Kill);
                        ended.await;
                    }
                    // An end of its own reaches the agent, on the queue,
                    // holding what the group left (its output, sealed now
                    // that nothing writes it); one the session stopped is
                    // its own doing, and one handed to the next build ends
                    // there. The job holds the session until its end is
                    // sent, so no unload or drain comes between.
                    let handed = lock(&session.handed).contains(&call);
                    if !stopped && !handed {
                        let text = job_ended(&***process);
                        let held = match process.holds() {
                            Some(holds) => rt::unblock(&*session.shared.rt, holds).await,
                            None => Vec::new(),
                        };
                        let ended = Event::JobEnded { call, text, held };
                        session.emit(fence, Completion::Event(ended));
                    }
                    lock(&session.jobs).retain(|job| !Arc::ptr_eq(&job.lease, &process));
                    session.changed.notify();
                    if let Some(daemon) = session.shared.daemon.upgrade() {
                        daemon.jobs.notify();
                    }
                })
            },
        );
    }

    async fn refuse(&mut self, msg: Msg, why: ReadOnly) {
        match msg {
            Msg::Env => {
                lock(&self.live.session.pending_env).take();
            }
            Msg::Request { reply, .. } => reply.send(Err(why.to_string())),
            #[cfg(test)]
            Msg::Barrier(_) => drop(self.take(msg).await),
            Msg::Attach(..) | Msg::Stop | Msg::Reclaim => drop(self.take(msg).await),
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
        let ticket = self.ticket();
        self.replies.clients.insert(ticket, reply);
        let ticket = Some(ticket);
        match request {
            Request::Input {
                segments,
                mode,
                force,
            } => Event::Input {
                segments,
                mode,
                force,
                origin,
                ticket,
            },
            Request::Withdraw { input } => Event::Withdraw {
                input,
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
    async fn burst(
        &mut self,
        events: Vec<Event>,
        mutations: Vec<Option<oneshot::Sender<ext::MutationResult>>>,
        sync: bool,
    ) -> Result<(), ()> {
        if !events.is_empty() {
            self.refresh().await?;
            self.step_mutations(events.into_iter().zip(mutations))
                .await?;
            self.refresh().await?;
        }
        if sync {
            self.sync().await?;
        }
        self.show_synced();
        Ok(())
    }

    /// Steps the kernel through `events` and carries out what follows. A
    /// step that panics is a bug the session survives: what it did is
    /// dropped, as a commit that failed drops it, what the steps before it
    /// did is carried out, and the session resumes from its log.
    async fn step(&mut self, events: impl IntoIterator<Item = Event>) -> Result<(), ()> {
        self.step_mutations(events.into_iter().map(|event| (event, None)))
            .await
    }

    async fn prepare_context(
        &mut self,
        invocation: ext::point::InvocationContext,
    ) -> std::thread::Result<()> {
        if self.core.ledger().has_context()
            || self.live.session.model_only()
            || !self.bound.registry().has_context()
        {
            return Ok(());
        }
        let bound = Arc::downgrade(&self.bound);
        rt::unblock(&*self.live.session.shared.rt, move || {
            catch_unwind(AssertUnwindSafe(|| {
                if let Some(bound) = bound.upgrade() {
                    invocation.scope(|| bound.prepare_context());
                }
            }))
        })
        .await
    }

    async fn step_mutations(
        &mut self,
        events: impl IntoIterator<Item = (Event, Option<oneshot::Sender<ext::MutationResult>>)>,
    ) -> Result<(), ()> {
        let mut acknowledgements = Vec::new();
        let mut fx = StepPlan::default();
        for (mut event, done) in events {
            let mut context = ext::point::InvocationContext::default();
            if let Event::Invoke {
                existing,
                parent,
                name,
                tool,
                input,
                ticket,
            } = &mut event
            {
                if let Some((_, inherited)) = self.replies.invocations.get(ticket) {
                    context = inherited.clone();
                }
                match self
                    .projection
                    .invocations
                    .request(*parent, name, tool, input)
                {
                    Ok(found) => {
                        *existing = found;
                        if let Some(call) =
                            found.filter(|call| self.projection.invocations.completed(*call))
                        {
                            self.invocation_reply(*ticket, Ok(call)).await;
                            continue;
                        }
                    }
                    Err(error) => {
                        self.invocation_reply(*ticket, Err(error)).await;
                        continue;
                    }
                }
            }
            let prepared = self.prepare_context(context.clone()).await;
            match prepared.and_then(|()| {
                catch_unwind(AssertUnwindSafe(|| context.scope(|| self.core.step(event))))
            }) {
                Ok(more) => {
                    for entry in &more.appends {
                        if let Some(call) = self.projection.invocations.fold(entry) {
                            self.replies.contexts.insert(call, context.clone());
                        }
                    }
                    if let Some(done) = done {
                        let entry = more.appends.iter().rev().find(|entry| {
                            matches!(
                                entry.body,
                                Body::Ext { .. }
                                    | Body::Fact { .. }
                                    | Body::Context {
                                        publication: Some(_),
                                        ..
                                    }
                            )
                        });
                        if let Some(entry) = entry {
                            acknowledgements.push((entry.clone(), done));
                        } else {
                            done.send(Err(ext::MutationError::Rejected(
                                "kernel rejected mutation".into(),
                            )));
                        }
                    }
                    fx.appends.extend(more.appends);
                    fx.immediate.extend(more.immediate);
                    fx.admitted.extend(more.admitted);
                    if let Some(mut commit) = more.commit {
                        let previous = fx.commit.unwrap_or(commit);
                        commit.durability = commit.durability.max(previous.durability);
                        commit.publish |= previous.publish;
                        fx.commit = Some(commit);
                    }
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
        self.carry(fx).await?;
        for (entry, done) in acknowledgements {
            let result = lock(&self.live.session.state_view()).mutation_result(&entry);
            self.journal
                .waiting_mutations
                .push((entry.seq, result, done));
        }
        Ok(())
    }

    /// An ephemeral turn's result entry ends its attempt's watch; one an
    /// interrupt wrote first, before the turn reported, settles the attempt
    /// it cut off, sent or not.
    fn settle_ephemeral(&mut self, entry: &mut Entry) {
        let Body::ModelResult { call, outcome, .. } = &entry.body else {
            return;
        };
        let Some(i) = self.ephemeral.iter().position(|(id, ..)| id == call) else {
            return;
        };
        let (call, role, started) = self.ephemeral.swap_remove(i);
        if entry.execution.is_none()
            && matches!(outcome, Outcome::Interrupted { .. })
            && let Some((at, attempt, model)) = lock(&started.0).take()
        {
            let observed = Observation {
                attempt,
                model,
                started: Some(at),
                usage: None,
            };
            entry.execution =
                (self.live.session).execution(call, observed, "model_only".into(), Some(&role));
        }
    }

    /// Appends a burst once, then admits each effect against the prefix
    /// and requirement declared by the kernel. Publication and unsafe
    /// effects share an epoch while ordered work keeps making progress.
    async fn carry(&mut self, plan: StepPlan) -> Result<(), ()> {
        let StepPlan {
            mut appends,
            commit,
            immediate,
            admitted: mut after,
        } = plan;
        for entry in &mut appends {
            let call = match &entry.body {
                Body::ModelResult { call, .. }
                | Body::Decision {
                    call: Some(call),
                    d: protocol::Verdict::Retry { .. } | protocol::Verdict::SideRetry { .. },
                    ..
                } => Some(*call),
                _ => None,
            };
            if let Some(i) = self
                .projection
                .executions
                .iter()
                .position(|(id, _)| Some(*id) == call)
            {
                entry.execution = Some(self.projection.executions.remove(i).1);
            }
            self.settle_ephemeral(entry);
        }
        let busy = immediate.iter().rev().find_map(|work| match work {
            Effect::MarkBusy(busy) => Some(*busy),
            _ => None,
        });
        for effect in immediate {
            match effect {
                Effect::Cancel { call } => self.cancel_pending(call, &mut after).await,
                Effect::MarkBusy(true) | Effect::Detach { .. } => self.effect(effect).await,
                Effect::MarkBusy(false) => {}
                _ => unreachable!("external work requires admission"),
            }
        }
        for Guarded { admission, work } in &after {
            if let Effect::Side {
                ticket,
                side: Some(id),
            } = work
                && let Some(pending) = self.sides.get_mut(ticket)
            {
                self.live
                    .session
                    .own_work(WorkId::Call(*id), *admission, pending.cancel.clone());
                if pending.while_waiting {
                    self.waiting_sides.push((pending.origin.id, *id));
                }
            }
        }
        let through = commit.map_or(self.journal.ordered_seq, |commit| commit.through);
        let publish = commit.is_some_and(|commit| commit.publish);
        let needs = after
            .iter()
            .filter(|guarded| guarded.admission.requirement == Requirement::Durable)
            .map(|guarded| guarded.admission.through)
            .max()
            .unwrap_or(0);
        let flush = (publish || needs > self.journal.durable_seq) && through > self.covering();
        let written = if let Some(commit) = commit {
            self.commit(
                &mut appends,
                commit.durability.into(),
                if flush {
                    FlushMode::Immediate
                } else {
                    FlushMode::Deferred
                },
                self.listing(busy.unwrap_or_else(|| self.live.has_work())),
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
                after.into_iter().map(|guarded| guarded.work).collect(),
            )
            .await;
            return Err(());
        }
        self.admissions
            .extend(after.into_iter().map(|guarded| Guarded {
                admission: guarded.admission,
                work: Admitted::Effect(guarded.work),
            }));
        self.admit().await;
        if busy == Some(false) {
            self.effect(Effect::MarkBusy(false)).await;
        }
        Ok(())
    }

    async fn cancel_pending(&mut self, call: CallId, after: &mut Vec<Guarded<Effect>>) {
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
        self.admissions.retain(|guarded| match &guarded.work {
            Admitted::Effect(effect) => retain(effect),
            _ => true,
        });
        after.retain(|guarded| retain(&guarded.work));
        let running = matches!(
            self.inflight.get(&call),
            Some(InFlight::Tool(_) | InFlight::Model)
        );
        self.effect(Effect::Cancel { call }).await;
        if !running && let Some(tool) = cancelled {
            let event = if tool {
                Event::ToolDone {
                    call,
                    outcome: Outcome::Interrupted { output: None },
                    card: None,
                    held: Vec::new(),
                }
            } else {
                Event::ModelDone {
                    call,
                    replied: Replied::Interrupted(None),
                }
            };
            self.live.session.emit(
                self.live.session.fence.load(Ordering::SeqCst),
                Completion::Event(event),
            );
        }
    }

    async fn admit(&mut self) {
        let mut waiting = Vec::new();
        for guarded in std::mem::take(&mut self.admissions) {
            let admission = guarded.admission;
            let watermark = match admission.requirement {
                Requirement::Ordered => self.journal.ordered_seq,
                Requirement::Durable => self.journal.durable_seq,
            };
            if admission.through > watermark {
                waiting.push(guarded);
                continue;
            }
            match guarded.work {
                Admitted::Effect(effect) => {
                    if let Some(call) = effect.starts_call() {
                        self.live.session.call_work(
                            call,
                            self.live.session.fence.load(Ordering::SeqCst),
                            Some(admission),
                        );
                    }
                    self.effect(effect).await;
                }
                Admitted::Attach(client, since, done) => {
                    self.attach(&client, since).await;
                    done.send(());
                }
                Admitted::Reaction(reaction) => {
                    if self.phase == Phase::Draining {
                        continue;
                    }
                    self.tickets += 1;
                    self.live.session.spawn_work(
                        WorkId::Reaction(self.tickets),
                        admission,
                        |cancel| {
                            Box::pin(async move {
                                unless(&cancel, reaction.0).await;
                            })
                        },
                    );
                }
            }
        }
        self.admissions = waiting;
    }

    async fn failed(&mut self, why: String, mut rest: Vec<Effect>) {
        rest.extend(
            std::mem::take(&mut self.admissions)
                .into_iter()
                .filter_map(|guarded| match guarded.work {
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
                respond(&mut self.replies.clients, ticket, Err(why.clone()));
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

    #[allow(clippy::too_many_lines)]
    async fn resume(&mut self) {
        recovery(self.live.id, ext_logging::Rule::ReloadAfterFault);
        self.abandon_generation().await;
        self.bound = bind(&self.registry, &self.live.session);
        let bound = services::binding(&self.bound, &self.live.session, &self.registry);
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
            let abandoned = if header.parent.is_some() && loaded.writer.read_only().is_none() {
                shared.abandoned(&header, folding.core.ledger())?
            } else {
                Vec::new()
            };
            io::Result::Ok((loaded, folding, shared.owner(&header)?, abandoned))
        })
        .await;
        let reloaded = match reloaded {
            Ok((mut loaded, folding, owner, abandoned)) => loaded
                .writer
                .flushed(&*self.live.session.shared.rt)
                .await
                .inspect_err(|e| store_fault(self.live.id, ext_logging::Operation::Reload, e))
                .map(|()| (loaded, folding, owner, abandoned)),
            Err(error) => Err(error),
        };
        let (loaded, folding, owner, abandoned) = match reloaded {
            Ok(reloaded) => reloaded,
            Err(e) => {
                eprintln!("session {}: reloading: {e}", self.live.id.short());
                self.phase = Phase::Draining;
                return;
            }
        };
        self.journal.ordered_seq = loaded.seq;
        self.journal.durable_seq = loaded.seq;
        let recovered = FrameLog::new(Arc::clone(&loaded.lines), loaded.seq);
        self.live.frames.reloaded(loaded.lines);
        self.journal.writer = Some(loaded.writer);
        self.journal.since_snapshot = loaded.read;
        self.core = folding.core;
        *lock(&self.live.session.state_view()) = folding.states;
        self.journal.fold_snapshot = folding.snapshot;
        self.live
            .has_input
            .store(folding.has_input, Ordering::SeqCst);
        self.projection = Projection {
            invocations: folding.invocations,
            published_activity: self.projection.published_activity.take(),
            ..Projection::default()
        };
        let after = self.live.frames.seq();
        let entries = match rt::unblock(&*self.live.session.shared.rt, move || {
            recovered.entries_after(after)
        })
        .await
        {
            Ok(entries) => entries,
            Err(error) => {
                eprintln!("session {}: recovery log: {error}", self.live.id.short());
                self.phase = Phase::Draining;
                return;
            }
        };
        for entry in entries {
            let lines = Encoded::of(std::slice::from_ref(&entry)).expect("loaded entry encodes");
            self.show(std::slice::from_ref(&entry), &lines);
        }
        self.publish();
        let load = Load {
            session: Some(self.live.id),
            busy: self.live.has_work(),
            abandoned: &abandoned,
            owner,
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

    fn retire_contexts(&mut self) {
        self.live
            .session
            .services
            .get::<ext::SessionLifetimes>()
            .expect("session lifetimes")
            .clear();
        self.live.session.fence.fetch_add(1, Ordering::SeqCst);
        self.live.session.cancel_all();
        // Retirement also cancels the deferred sync task; later writes need a new timer.
        self.journal.sync_armed = false;
        self.detached.clear();
        self.ephemeral.clear();
    }

    // Retire the old generation before reopening its log: no waiter or
    // cancellation owner can cross the recovery boundary.
    async fn abandon_generation(&mut self) {
        self.retire_contexts();
        for pending in std::mem::take(&mut self.journal.flushes) {
            pending.ended.wait().await;
        }
        self.admissions.clear();
        self.projection.transcript = None;
        self.replies = Replies::default();
        self.delivery_queue.clear();
        self.transport_active = false;
        self.sides.clear();
        self.inflight.clear();
        self.journal.unsynced.clear();
        self.journal.waiting_sync.clear();
        self.journal.waiting_mutations.clear();
        self.journal.writer.take();
    }

    async fn effect(&mut self, effect: Effect) {
        let shared = Arc::clone(&self.live.session.shared);
        // A stopping session starts nothing: what it would have started is
        // in the log, for its next load.
        if self.phase == Phase::Draining && effect.starts() {
            return;
        }
        match effect {
            Effect::Invocation { ticket, result } => self.invocation_reply(ticket, result).await,
            Effect::Delivery { ticket, result } => {
                respond(&mut self.replies.deliveries, ticket, result);
            }
            Effect::Deliver { envelope, ticket } => self.deliver(envelope, ticket),
            Effect::Reply { ticket, result } => respond(&mut self.replies.clients, ticket, result),
            Effect::Side { ticket, side } => self.admit_side(ticket, side).await,
            Effect::Prepare {
                call,
                attempt,
                choice,
            } => self.prepare(call, attempt, choice).await,
            Effect::Send { call } => self.send(call),
            Effect::RunTool { call, tool, input } => {
                self.run_tool(call, tool, input, Child::None);
            }
            Effect::Adopt { call, tool, input } => self.adopt_tool(call, tool, input),
            Effect::Resume {
                call,
                tool,
                input,
                value,
            } => self.resume_tool(call, tool, input, value),
            Effect::Await { call, answer } => {
                self.later(call, answer.0, move |gated| Event::Gated { call, gated });
            }
            Effect::Recover { call, answer } => {
                self.later(call, answer.0, move |recovered| Event::Recovered {
                    call,
                    recovered,
                });
            }
            Effect::Sleep { call, after } => {
                self.later(call, shared.rt.sleep(after), move |()| Event::Woke { call });
            }
            // The call's handle, made now if it is still held behind
            // admission: the run it admits takes the flag up.
            Effect::Detach { call } => {
                let fence = self.live.session.fence.load(Ordering::SeqCst);
                if let Some(origin) = self.live.session.call_work(call, fence, None) {
                    origin.detach.set();
                }
            }
            Effect::Cancel { call } => {
                self.inflight.remove(&call);
                self.live.session.cancelled(call);
                self.sides.retain(|_, side| side.origin.id != call);
                for (side, open) in self.core.ledger().open_calls() {
                    if open.call == (protocol::ledger::Call::Side { of: call }) {
                        self.live.session.cancelled(side);
                    }
                }
            }
            Effect::StartChild { link, child, input } => self.start_child(link, child, input),
            Effect::Interrupt { child } => {
                lock(&self.live.session.work).cancel(&WorkId::Child(child));
                if let Some(daemon) = shared.daemon.upgrade() {
                    daemon.interrupt(child);
                }
            }
            Effect::Report { outcome } => {
                if let (Some(parent), Some(daemon)) = (self.live.parent, shared.daemon.upgrade()) {
                    let child = self.live.id;
                    self.live.session.spawn_work(
                        WorkId::Report(child),
                        Admission {
                            through: self.journal.durable_seq,
                            requirement: Requirement::Durable,
                        },
                        |cancel| {
                            Box::pin(async move {
                                unless(&cancel, daemon.report(parent.session, child, outcome))
                                    .await;
                            })
                        },
                    );
                }
            }
            Effect::EndStream { stream, cut } => self.end_stream(stream, cut),
            Effect::DiscardStream { stream } => {
                let store = Arc::clone(&shared.blobs);
                // Left behind, it is only a stream no one names.
                let discarded = rt::unblock(&*shared.rt, move || store.discard(stream));
                shared
                    .rt
                    .spawn(Box::pin(async move { drop(discarded.await) }));
            }
            Effect::MarkBusy(busy) => {
                self.live.busy.store(busy, Ordering::SeqCst);
                let how = if busy { "busy" } else { "settled" };
                crate::session_event(how, self.live.id, String::new);
                self.live.touch();
            }
        }
    }

    /// Copies stream `stream`, cut off, on a storage worker; the kernel
    /// hears what it held.
    fn end_stream(&self, stream: protocol::OutputStream, cut: kernel::CutOff) {
        let session = Arc::clone(&self.live.session);
        let fence = session.fence.load(Ordering::SeqCst);
        let shared = &session.shared;
        let store = Arc::clone(&shared.blobs);
        let ended = rt::unblock(&*shared.rt, move || end_cut_off(&*store, stream));
        Arc::clone(&shared.rt).spawn(Box::pin(async move {
            let end = ended.await;
            let ended = Event::StreamEnded { stream, cut, end };
            session.emit(fence, Completion::Event(ended));
        }));
    }

    fn resume_tool(&mut self, call: CallId, tool: String, input: Json, value: Json) {
        let refusal = self
            .core
            .ledger()
            .links()
            .iter()
            .rev()
            .filter_map(|l| spawn::SpawnIntent::from_json(&l.intent).ok())
            .find(|i| i.call == call)
            .and_then(|i| i.refusal);
        let value = refusal.map_or(value, |r| {
            Json::encode(&Result::<Outcome, _>::Err(r)).unwrap()
        });
        match self.inflight.get_mut(&call) {
            // Still running, and waiting for it.
            Some(InFlight::Tool(child @ Child::Waiting(_))) => {
                if let Child::Waiting(reply) = std::mem::replace(child, Child::None) {
                    reply.send(value);
                }
            }
            // Run again, from a log that holds the answer.
            _ => self.run_tool(call, tool, input, Child::None),
        }
    }

    fn start_child(&mut self, link: protocol::Seq, child: SessionId, input: String) {
        let handle = self.live.session.own_work(
            WorkId::Child(child),
            Admission {
                through: link.get(),
                requirement: Requirement::Durable,
            },
            Cancel::default(),
        );
        let cancel = handle.cancel.clone();
        let detached = self
            .detached
            .iter()
            .position(|(id, _)| *id == child)
            .map(|at| self.detached.remove(at).1);
        let shared = &self.live.session.shared;
        let header = Header {
            id: child,
            workspace: self.live.workspace.clone(),
            parent: Some(Parent {
                session: self.live.id,
                call: link.get(),
            }),
        };
        let intent = self
            .core
            .ledger()
            .links()
            .iter()
            .find(|l| l.seq == link)
            .map(|l| l.intent.clone())
            .expect("committed spawn intent");
        let env = lock(&self.live.session.env).clone();
        if let Some(daemon) = shared.daemon.upgrade() {
            let parent = self.live.id;
            handle.spawn(&*shared.rt, async move {
                let refusal = spawn::SpawnIntent::from_json(&intent)
                    .ok()
                    .and_then(|i| i.refusal);
                let result = if cancel.is_set() {
                    Err(io::Error::other("child admission interrupted"))
                } else {
                    match &refusal {
                        Some(refusal) => Err(io::Error::other(format!("{refusal:?}"))),
                        None => daemon.start_child(header, input, env, intent).await,
                    }
                };
                if cancel.is_set() {
                    daemon.interrupt(child);
                }
                if let Some(reply) = detached {
                    let answer = result
                        .as_ref()
                        .map(|()| ext::ConversationHandle { session: child })
                        .map_err(|e| {
                            refusal.unwrap_or_else(|| ext::SpawnRefusal::Unavailable(e.to_string()))
                        });
                    reply.send(answer);
                }
                if let Err(error) = result {
                    daemon
                        .report(
                            parent,
                            child,
                            Outcome::Failed {
                                error: error.to_string(),
                            },
                        )
                        .await;
                }
            });
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

    fn ticket(&mut self) -> Ticket {
        self.tickets += 1;
        Ticket(self.tickets)
    }

    fn later<T: Send + 'static>(
        &mut self,
        call: CallId,
        work: impl Future<Output = T> + Send + 'static,
        then: impl FnOnce(T) -> Event + Send + 'static,
    ) {
        let session = Arc::clone(&self.live.session);
        let handle = session
            .origin(call, session.fence.load(Ordering::SeqCst))
            .expect("actor generation");
        self.inflight.insert(call, InFlight::Later);
        let cancel = handle.cancel.clone();
        let fence = handle.fence;
        handle.spawn(&*Arc::clone(&session.shared.rt), async move {
            if let Some(done) = unless(&cancel, work).await {
                session.emit(fence, Completion::Event(then(done)));
            }
        });
    }

    async fn prepare(&mut self, call: CallId, attempt: u16, choice: ToolChoice) {
        let (mut decisions, mut answered) = (Vec::new(), None);
        let prepared = if self.live.session.model_only() {
            self.prepare_thread(call).await.map(Some)
        } else {
            match self.live.session.model(&self.registry) {
                Ok(model) => match self.request(call, &model).await {
                    Ok(mut request) => {
                        request.tool_choice = match choice {
                            ToolChoice::Auto => llm::ToolChoice::Auto,
                            ToolChoice::None => llm::ToolChoice::None,
                        };
                        let bound = self.bound.clone();
                        let preparing = model.clone();
                        rt::try_unblock_with(
                            &*self.live.session.shared.rt,
                            Cpu,
                            rt::STORAGE_BYTES,
                            move || {
                                let (decided, answer) = bound.step(call, &mut request);
                                let prepared = match answer {
                                    Some((by, response)) => PreparedStep::Answered(by, response),
                                    None => PreparedStep::Request(preparing.prepare(&request)),
                                };
                                (decided, prepared)
                            },
                        )
                        .await
                        .map(|(decided, prepared)| {
                            decisions = decided;
                            match prepared {
                                PreparedStep::Answered(by, response) => {
                                    let (replied, message) = replied(response);
                                    self.projection.replied = message.map(|m| (call, m));
                                    answered = Some((by, replied));
                                    None
                                }
                                PreparedStep::Request(request) => {
                                    Some(InFlight::Prepared(Box::new(Ok(PreparedModel {
                                        model,
                                        request,
                                        attempt,
                                        registry: Arc::clone(&self.registry),
                                    }))))
                                }
                            }
                        })
                        .map_err(|error| error.to_string())
                    }
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            }
        };
        if let Some(prepared) = prepared.transpose() {
            self.inflight.insert(
                call,
                prepared.unwrap_or_else(|error| InFlight::Prepared(Box::new(Err(error)))),
            );
        }
        self.live.session.emit(
            self.live.session.fence.load(Ordering::SeqCst),
            Completion::Event(Event::Prepared {
                call,
                decisions,
                answered,
            }),
        );
    }

    fn send(&mut self, call: CallId) {
        let Some(prepared @ (InFlight::Prepared(_) | InFlight::Thread(_))) =
            self.inflight.remove(&call)
        else {
            return;
        };
        self.inflight.insert(call, InFlight::Model);
        let live = Arc::clone(&self.live);
        let registry = Arc::clone(&self.registry);
        let fence = live.session.fence.load(Ordering::SeqCst);
        let handle = live.session.origin(call, fence).expect("actor generation");
        let cancel = handle.cancel.clone();
        let origin = handle.clone();
        let agent = self.bound.agent_name().to_owned();
        handle.spawn(&*self.live.session.shared.rt, async move {
            let mut execution = None;
            let (replied, message) = match prepared {
                InFlight::Prepared(prepared) => match *prepared {
                    Ok(PreparedModel {
                        model,
                        request,
                        attempt,
                        registry,
                    }) => {
                        let mut deltas = |text: &str| live.delta(call, attempt, text, fence);
                        let cx = live.session.cx(origin.clone(), &registry);
                        let resolved = request.origin();
                        let name = format!("{}:{}", resolved.provider, resolved.model);
                        let mut observed = Observation {
                            attempt,
                            model: name,
                            ..Observation::default()
                        };
                        let result = contain(Box::pin(reply(
                            &model,
                            request,
                            &cancel,
                            &mut deltas,
                            &mut observed,
                        )))
                        .await;
                        execution = live.session.execution(cx.call, observed, agent, None);
                        result
                    }
                    Err(error) => (
                        Replied::Failed(llm::Error::new(llm::Kind::InvalidRequest, error)),
                        None,
                    ),
                },
                InFlight::Thread(prepared) => {
                    let (turn, messages, model) = *prepared;
                    let cx = live.session.cx(origin.clone(), &registry);
                    let mut observed = Observation::default();
                    let attempt = side::Attempt {
                        id: origin.id,
                        cancel: &origin.cancel,
                        fence: origin.fence,
                        started: None,
                    };
                    let running = unless(
                        &cancel,
                        Box::pin(side::run_side(
                            &live.session,
                            &cx,
                            &turn,
                            messages,
                            &model,
                            attempt,
                            &mut observed,
                        )),
                    );
                    let replied = first(running, live.session.shared.rt.sleep(turn.budget.timeout))
                        .await
                        .flatten()
                        .unwrap_or(Replied::Interrupted(None));
                    if matches!(replied, Replied::Interrupted(_)) {
                        observed.usage = None;
                    }
                    execution =
                        live.session
                            .execution(cx.call, observed, "model_only".into(), None);
                    (replied, None)
                }
                _ => unreachable!(),
            };
            live.session.emit(
                fence,
                Completion::Replied {
                    call,
                    replied,
                    message,
                    execution,
                },
            );
        });
    }

    /// The request for model call `call`: the system prompt, the
    /// transcript before it, and the tools.
    async fn request(&mut self, call: CallId, model: &Model) -> Result<llm::Request, String> {
        let messages = self.conversation(call).await.map_err(|e| e.to_string())?;
        let mut request = model.request(messages, self.registry.specs().clone());
        if let Some(spawn) = lock(&self.live.session.spawn).as_ref() {
            request.generation.max_output_tokens = Some(spawn.request.budget.max_tokens);
        }
        Ok(request)
    }

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
        let cx = self
            .live
            .session
            .cx(origin.clone(), &registry)
            .with_invocation(self.replies.contexts.remove(&call).unwrap_or_default());
        // A call another invoked is waited on by it, as long as it runs,
        // and its result is the invoker's to read, unrecorded.
        let invoked = matches!(
            self.core.ledger().call(call).map(|open| &open.call),
            Some(protocol::ledger::Call::Tool {
                invoke: Some(_),
                ..
            })
        );
        let background =
            registry.tools().get(&tool).map(ext::DynTool::detach) == Some(ext::Detach::Background);
        let cx = if background && !invoked {
            cx.detachable(origin.detach.clone())
        } else {
            cx
        };
        let session = Arc::clone(&self.live.session);
        let id = self.live.id;
        origin.spawn(&*self.live.session.shared.rt, async move {
            let began = crate::stamp();
            let tools = registry.tools();
            let run = if invoked {
                tools.call(&tool, &input, parsed, &cx)
            } else {
                tools.run(&tool, &input, parsed, &cx)
            };
            let mut run = pin!(contain(run));
            // A call that can go on in the background is stopped being
            // waited on, not stopped: its outcome is reported all the same.
            let waited = match &cx.detach {
                Some(detach) => first(run.as_mut(), detach.wait()).await,
                None => Some(run.as_mut().await),
            };
            let called = if let Some(called) = waited {
                called
            } else {
                session.emit(fence, Completion::Event(Event::Detached { call }));
                run.await
            };
            crate::session_event("effect", id, || {
                let elapsed = began.map_or(std::time::Duration::ZERO, |t| t.elapsed());
                let how = match &called.outcome {
                    Outcome::Done { .. } => "done",
                    Outcome::Failed { .. } => "failed",
                    Outcome::Interrupted { .. } => "interrupted",
                };
                format!("{tool} call {call}: {how} in {elapsed:?}")
            });
            session.emit(fence, Completion::Tool { call, called });
        });
    }
}

/// Whether the catalog is to hear of `now`: it lists what it waits on and
/// whether it is busy, and its count only as of then, so a commit does not
/// change it.
fn relisted(listed: &Record, now: &Record) -> bool {
    listed.busy != now.busy || listed.activity != now.activity
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
        let input = Request::Input {
            segments: text.into(),
            mode: Mode::Queue,
            force: false,
        };
        rt::block_on(self.request(input, None))
    }

    pub fn ended(&self) -> Flag {
        self.ended.clone()
    }

    pub(crate) fn projection_cache(&self, populate: bool) -> [bool; 3] {
        let (reply, result) = oneshot::channel();
        self.send(Msg::ProjectionCache(populate, reply));
        rt::block_on(result).unwrap()
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
    fn delta(&self, call: CallId, attempt: u16, text: &str, fence: u64) {
        if self.session.fence.load(Ordering::SeqCst) != fence {
            return;
        }
        let delta = DaemonMsg::Delta {
            call,
            text: text.to_owned(),
            attempt,
        };
        let Ok(frame) = Frame::new(&delta) else {
            return;
        };
        // Held while sending, so a call's deltas reach followers in order
        // whether they waited for it to be shown or not.
        let mut early = lock(&self.early);
        if self.frames.seq() < call.get() || early.iter().any(|(waiting, ..)| *waiting == call) {
            early.push((call, fence, frame));
            return;
        }
        self.send_live(&frame);
    }

    /// Sends the live text that waited for its call, now shown, unless the
    /// session has moved past the fence it was streamed under.
    pub(super) fn shown(&self) {
        let seq = self.frames.seq();
        let fence = self.session.fence.load(Ordering::SeqCst);
        let mut early = lock(&self.early);
        for (_, streamed, frame) in early.extract_if(.., |(call, ..)| call.get() <= seq) {
            if streamed == fence {
                self.send_live(&frame);
            }
        }
    }

    fn send_live(&self, frame: &Frame) {
        let after = self.frames.seq();
        lock(&self.followers).retain(|c| c.send_after(after, frame.clone()).is_ok());
    }
}

impl Session {
    fn new(header: &Header, shared: &Arc<Shared>, mail: Mail, registry: &Registry) -> Arc<Self> {
        Arc::new_cyclic(|session| Self {
            services: Arc::new({
                let services = SessionServices::default()
                    .with::<ext::SessionLifetimes>(Arc::new(ext::SessionLifetimes::default()))
                    .with::<dyn ext::Layers>(Arc::new(SessionLayers(session.clone())))
                    .with::<dyn rt::Runtime>(Arc::clone(&shared.rt))
                    .with::<rt::watch::Hub>(Arc::clone(&shared.hub))
                    .with::<dyn ext::Logs>(Arc::new(services::SessionLogs(Arc::clone(shared))))
                    .with::<dyn transport::HttpClients>(Arc::clone(
                        shared.http_clients.get_or_init(|| {
                            Arc::new(transport::Clients::new(Arc::clone(&shared.rt)))
                        }),
                    ));
                match shared
                    .daemon
                    .upgrade()
                    .and_then(|daemon| daemon.queried.get_arc::<ext::Lifetimes>().cloned())
                {
                    Some(lifetimes) => services.with::<ext::Lifetimes>(lifetimes),
                    None => services,
                }
            }),
            spawn: Mutex::default(),
            context_cuts: Mutex::default(),
            fence: AtomicU64::new(0),
            id: header.id,
            workspace: Arc::from(header.workspace.as_path()),
            blobs: ext::blob::Blobs::new(
                Arc::clone(&shared.blobs),
                Arc::clone(&shared.tree),
                header.id,
            ),
            mail,
            states: Mutex::new(Arc::new(Mutex::new(registry.states()))),
            env: Mutex::default(),
            pending_env: Mutex::default(),
            jobs: Mutex::default(),
            resumable: Mutex::default(),
            handed: Mutex::default(),
            adopting: Mutex::default(),
            work: Mutex::default(),
            changed: rt::Notify::new(),
            shared: Arc::clone(shared),
        })
    }

    fn emit(&self, fence: u64, completion: Completion) {
        let _ = self.mail.send(Msg::Completed { fence, completion });
    }

    fn state_view(&self) -> Arc<Mutex<States>> {
        Arc::clone(&lock(&self.states))
    }

    fn cancel(&self, call: CallId) -> Cancel {
        self.origin(call, self.fence.load(Ordering::SeqCst))
            .expect("actor generation")
            .cancel
    }

    fn cancelled(&self, call: CallId) {
        lock(&self.work).cancel(&WorkId::Call(call));
    }

    fn cancel_all(&self) {
        lock(&self.work).revoke();
    }

    fn origin(&self, call: CallId, fence: u64) -> Option<ContextOrigin> {
        self.call_work(call, fence, None)
    }

    fn call_work(
        &self,
        call: CallId,
        fence: u64,
        admission: Option<Admission>,
    ) -> Option<ContextOrigin> {
        let mut work = lock(&self.work);
        if self.fence.load(Ordering::SeqCst) != fence {
            return None;
        }
        let id = WorkId::Call(call);
        if work.get_mut(&id).is_none() {
            work.insert(WorkHandle::new(
                id.clone(),
                fence,
                Admission {
                    through: call.get(),
                    requirement: Requirement::Ordered,
                },
                Cancel::default(),
            ));
        }
        let handle = work.get_mut(&id).unwrap();
        if let Some(admission) = admission {
            handle.admission = admission;
        }
        Some(WorkHandle {
            id: call,
            fence: handle.fence,
            admission: handle.admission,
            cancel: handle.cancel.clone(),
            detach: handle.detach.clone(),
            ended: handle.ended.clone(),
        })
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

#[derive(Default)]
struct Observation {
    attempt: u16,
    model: String,
    started: Option<Instant>,
    usage: Option<Json>,
}

async fn reply(
    model: &Model,
    request: llm::Prepared,
    cancel: &Cancel,
    deltas: &mut (dyn FnMut(&str) + Send),
    observed: &mut Observation,
) -> (Replied, Option<llm::Assistant>) {
    // Sent once polled, as an interrupt meanwhile finds it.
    let origin = request.origin().clone();
    let mut events = None;
    let reading = async {
        let _ = services::attempt_clock();
        let events = events.insert(model.send(request));
        observed.started = events.started();
        crate::model::read(events, deltas).await
    };
    let reading: BoxFuture<'_, _> = Box::pin(reading);
    let read = unless(cancel, reading).await;
    observed.usage = events
        .as_ref()
        .and_then(llm::Events::usage_json)
        .or_else(|| {
            read.as_ref()
                .and_then(|r| r.as_ref().err())
                .and_then(llm::Error::usage_json)
        });
    let Some(read) = read else {
        observed.usage = None;
        let text = events.and_then(llm::Events::interrupted);
        return (Replied::Interrupted(text), None);
    };
    let reply = read.and_then(|()| {
        let events = events.ok_or_else(llm::Error::incomplete)?;
        events.finish(Some(origin))
    });
    match reply {
        Ok(reply) => replied(Ok(reply)),
        Err(error) => (Replied::Failed(error), None),
    }
}

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
pub(crate) async fn contain<T: Failure>(effect: impl Future<Output = T>) -> T {
    let mut effect = pin!(effect);
    poll_fn(|cx| {
        catch_unwind(AssertUnwindSafe(|| effect.as_mut().poll(cx)))
            .unwrap_or_else(|panic| Poll::Ready(T::failed(panicked(&*panic))))
    })
    .await
}

pub(crate) trait Failure {
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

/// What the agent is told of a background process group's end.
fn job_ended(process: &dyn ext::RunningProcess) -> String {
    let how = match process.try_wait() {
        Ok(Some(status)) => status.code().map_or_else(
            || "killed by a signal".to_owned(),
            |code| format!("exit code {code}"),
        ),
        _ => "ended".to_owned(),
    };
    format!("[background job {} ended: {how}]", process.id())
}

/// What stream `of`, which a restart cut off, holds so far, copied into a
/// blob and cut at [`STREAM_CAP`](ext::blob::STREAM_CAP). Copied, never
/// sealed in place: a group the crash left behind may write it still, and
/// would change a blob named by what it holds.
fn end_cut_off(store: &dyn ext::blob::BlobStore, of: protocol::OutputStream) -> kernel::StreamEnd {
    match copied(store, of) {
        Ok(Some(blob)) => kernel::StreamEnd::Copied(blob),
        Ok(None) => kernel::StreamEnd::Empty,
        Err(e) if e.kind() == io::ErrorKind::NotFound => kernel::StreamEnd::Missing,
        Err(_) => kernel::StreamEnd::Unreadable,
    }
}

fn copied(
    store: &dyn ext::blob::BlobStore,
    of: protocol::OutputStream,
) -> io::Result<Option<protocol::BlobRef>> {
    let file = std::fs::File::open(ext::blob::stream_path(store.streams(), of))?;
    let size = file.metadata()?.len().min(ext::blob::STREAM_CAP);
    let mut bytes = (&file).take(size);
    let mut head = Vec::with_capacity(ext::blob::SNIFFED);
    (&mut bytes)
        .take(ext::blob::SNIFFED as u64)
        .read_to_end(&mut head)?;
    if head.is_empty() {
        return Ok(None);
    }
    let mut blob = store.writer(size)?;
    blob.write_all(&head)?;
    io::copy(&mut bytes, &mut blob)?;
    let origin = protocol::BlobOrigin::ToolOutput(of.call);
    blob.finish(ext::blob::media_type(&head), origin).map(Some)
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
) -> StepPlan {
    if read_only.is_some() {
        return StepPlan::default();
    }
    if matches!(open, Open::Load) && load.busy {
        recovery(session, ext_logging::Rule::ResumeLedger);
    }
    core.resumed(load)
}

fn recovery(session: SessionId, rule: ext_logging::Rule) {
    ext_logging::emit(ext_logging::Fact::Recovery { session, rule });
}

fn panicked(panic: &(dyn Any + Send)) -> String {
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("unknown panic");
    format!("panicked: {message}")
}

pub fn commands(registry: &Registry) -> Vec<protocol::CommandInfo> {
    registry
        .commands()
        .map(|c| protocol::CommandInfo {
            name: c.name().to_owned(),
            description: c.description().to_owned(),
        })
        .collect()
}
