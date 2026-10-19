//! A live session: one task, its actor, owns the session's kernel and its
//! log's writer, carries out the effects each step returns and feeds back
//! what they report. Clients, calls, model replies and other sessions
//! reach it only through its mailbox, so a log has one writer, entries
//! commit in the order the kernel numbers them, and no lock is held
//! across an await. What the driver decides is only how: the kernel
//! decides what.

use std::any::{Any, TypeId};
use std::future::{Future, poll_fn};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::Poll;
use std::time::Instant;

use crate::store::{Header, Parent, Writer};
use ext::host::{Host, Parts, Registry};
use ext::point::{Approving, Failed, View};
use ext::{
    Asked, BoxFuture, Callable, Called, Cancel, Chained, Cx, Facts, Gated, Gating, Name, Parsed,
    PointCx, Recover, Route, States,
};
use kernel::{Answer, Core, Effect, Effects, Event, Load, Note, Owner, Replied, Ticket};
use protocol::view::SessionView;
use protocol::{
    Author, Body, CallId, Card, DaemonMsg, Entry, Environment, Frame, Installed, Json, Mode,
    Outcome, Overrides, Residency, SessionId, SessionSummary, ToolChoice, Verdict,
};
use rt::{channel, oneshot};

use crate::model::{Model, Models};
use crate::outbox::{FrameLog, Outbox};
use crate::transcript::Transcript;
use crate::{Daemon, lock};

/// What every session runs with.
pub struct Shared {
    /// Where actors and effects run, sleep and block.
    pub rt: rt::Rt,
    pub store: Arc<dyn crate::store::LogStore>,
    /// The extensions, as each session takes them up at its next step.
    pub registry: ext::host::Current,
    pub models: Models,
    /// The system prompt, in place of the one the extensions make.
    pub system: Option<String>,
    /// Where sessions keep output too large for their logs.
    pub spill: PathBuf,
    /// Sessions loaded since the daemon started.
    pub(crate) loaded: Mutex<std::collections::HashSet<SessionId>>,
    pub(crate) daemon: std::sync::Weak<Daemon>,
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
    workspace: PathBuf,
    mail: Mail,
    /// The extensions' state: folded as each entry commits.
    states: Mutex<States>,
    /// What commands run in: the last client's, secrets included, or after
    /// a restart the last one logged.
    env: Mutex<Option<Environment>>,
    /// The session's own settings, and the entry that set them.
    overrides: Mutex<(Overrides, Option<u64>)>,
    /// Background process groups the session owns, until each ends.
    jobs: Mutex<Vec<u32>>,
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
    attached: AtomicUsize,
    last_active: Mutex<Instant>,
    summary: Mutex<SessionSummary>,
    pub(crate) ended: Ended,
}

/// A client's mutation of a session.
pub enum Request {
    Input {
        text: String,
        mode: Mode,
    },
    Interrupt,
    /// The answer to what call `call` waits on.
    Answer {
        call: u64,
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
    Env(Environment),
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
    Stop,
    /// Answered once every message before it is taken.
    #[cfg(test)]
    Barrier(oneshot::Sender<()>),
}

/// The end of a session's actor: over once its writer is closed, so a
/// reload never makes a second.
#[derive(Clone, Default)]
pub struct Ended(Arc<(AtomicBool, rt::Notify)>);

impl Ended {
    fn end(&self) {
        self.0.0.store(true, Ordering::SeqCst);
        self.0.1.notify();
    }

    pub fn is_over(&self) -> bool {
        self.0.0.load(Ordering::SeqCst)
    }

    pub async fn over(&self) {
        self.0.1.until(|| self.is_over()).await;
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
        let registry = shared.registry.get();
        let (tx, rx) = channel::unbounded();
        let session = Arc::new(Session {
            id: header.id,
            workspace: header.workspace.clone(),
            mail: Arc::new(tx),
            states: Mutex::new(registry.states()),
            env: Mutex::default(),
            overrides: Mutex::default(),
            jobs: Mutex::default(),
            shared: Arc::clone(shared),
        });
        let mut core = Core::new(Bound::bind(&registry, &session));
        // Each entry is folded as it is read and dropped.
        let mut states = registry.states();
        let loaded = match open {
            Open::Create => shared.store.create(header)?,
            Open::Load => shared.store.load(header.id, &mut |e| {
                states.apply(&e);
                core.load(&e);
            })?,
        };
        *lock(&session.states) = states;
        *lock(&session.env) = core.ledger().env().cloned();
        *lock(&session.overrides) = core.ledger().settings();
        let fresh = lock(&shared.loaded).insert(header.id);
        let busy = shared.store.is_busy(header.id);
        let load = Load {
            boot: sys::boot_id(),
            fresh,
            owner,
            busy,
            created: matches!(open, Open::Create),
        };
        let first = core.resumed(&load);
        let live = Arc::new(Self {
            id: header.id,
            workspace: header.workspace.clone(),
            parent: header.parent,
            frames: Arc::new(FrameLog::new(loaded.lines, loaded.seq)),
            followers: Mutex::default(),
            busy: AtomicBool::new(busy),
            attached: AtomicUsize::new(0),
            last_active: Mutex::new(Instant::now()),
            summary: Mutex::new(summary(header, core.ledger())),
            ended: Ended::default(),
            session: Arc::clone(&session),
        });
        registry.hooks().session_start(&session.point());
        let actor = Actor {
            live: Arc::clone(&live),
            core,
            writer: Some(loaded.writer),
            registry,
            transcript: None,
            replied: None,
            snapshot: None,
            cancels: Keyed::default(),
            waiting: Keyed::default(),
            answers: Keyed::default(),
            replies: Keyed::default(),
            tickets: 0,
            reporting: 0,
            stopping: false,
            marked: busy,
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
        let (reply, answer) = oneshot::channel();
        self.send(Msg::Request {
            request,
            origin,
            reply,
        });
        answer
            .await
            .unwrap_or_else(|| Err("the session stopped".to_owned()))
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
    pub fn set_env(&self, env: Environment) {
        self.send(Msg::Env(env));
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
    /// parent, once. Read from the log's start, where the input of a
    /// child is.
    pub fn has_input(&self) -> bool {
        self.frames
            .entries()
            .any(|e| matches!(e.body, Body::Input { .. }))
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

fn summary(header: &Header, ledger: &protocol::ledger::Ledger) -> SessionSummary {
    SessionSummary {
        id: header.id,
        workspace: header.workspace.clone(),
        entries: ledger.seq(),
        clients: 0,
        activity: ledger.activity(),
        residency: Residency::Loaded,
    }
}

struct Actor {
    live: Arc<Live>,
    core: Core,
    /// `None` only while a commit has it on a blocking thread.
    writer: Option<Writer>,
    /// The registry generation the newest step ran on.
    registry: Arc<Registry>,
    /// The conversation the model sees, made at the first model call.
    transcript: Option<Transcript>,
    /// The message of the reply model call `.0` just had, until its
    /// result commits.
    replied: Option<(u64, llm::Assistant)>,
    /// The last snapshot made, and its seq; `None` for one too large.
    snapshot: Option<(u64, Option<Frame>)>,
    /// What stops each effect in flight.
    cancels: Keyed<CallId, Cancel>,
    /// Calls waiting on a child, for its result.
    waiting: Keyed<CallId, oneshot::Sender<Json>>,
    /// What calls run again are handed: the answer they waited on.
    answers: Keyed<CallId, Json>,
    replies: Keyed<Ticket, oneshot::Sender<Result<(), String>>>,
    tickets: u64,
    /// Tool and model calls in flight, each of which reports once.
    reporting: usize,
    stopping: bool,
    /// Whether the store's busy marker is set: while work is in flight, or
    /// a call waits on someone, so the next daemon loads the session at
    /// once to recover it or to list what it waits on. Set before work
    /// starts, cleared after it ends.
    marked: bool,
}

impl Actor {
    async fn run(mut self, mail: channel::Receiver<Msg>, first: Effects) {
        self.carry(first).await;
        while !(self.stopping && self.reporting == 0)
            && let Some(msg) = mail.recv().await
        {
            if let Some((event, done)) = self.take(msg) {
                // A new registry generation is taken up at the next step.
                let newest = self.live.session.shared.registry.get();
                let generation = (!Arc::ptr_eq(&newest, &self.registry)).then(|| {
                    self.registry = Arc::clone(&newest);
                    Event::Registry(Bound::bind(&newest, &self.live.session))
                });
                self.step(generation.into_iter().chain([event])).await;
                if let Some(done) = done {
                    done.send(());
                }
            }
            // A call may begin to wait without the work marked ending.
            self.mark().await;
            self.publish();
        }
        let registry = &self.registry;
        registry.hooks().session_end(&self.live.session.point());
        // Closed and let go before the end is told: a reload never makes
        // a second writer, and what is told the end holds the last of it.
        let (ended, daemon) = (
            self.live.ended.clone(),
            self.live.session.shared.daemon.clone(),
        );
        drop((self, mail));
        ended.end();
        if let Some(daemon) = daemon.upgrade() {
            daemon.pulse.notify();
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
            } => {
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
                    Request::Answer { call, value } => {
                        let Some(call) = CallId::new(call) else {
                            self.reply(Ticket(self.tickets), Err("no call 0".into()));
                            return None;
                        };
                        Event::Answer {
                            call,
                            value,
                            origin,
                            ticket,
                        }
                    }
                }
            }
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
                if let Some(value) = self.answers.remove(&call) {
                    reply.send(value);
                    return None;
                }
                self.waiting.insert(call, reply);
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
                self.reporting -= 1;
                self.cancels.remove(&call);
                self.replied = message.map(|m| (call.get(), m));
                Event::ModelDone { call, replied }
            }
            #[cfg(test)]
            Msg::Barrier(done) => {
                done.send(());
                return None;
            }
            Msg::Stop => {
                self.stopping = true;
                for cancel in self.cancels.values() {
                    cancel.cancel();
                }
                return None;
            }
        };
        if let Event::ToolDone { call, .. } = &event {
            self.reporting -= 1;
            self.cancels.remove(call);
        }
        Some((event, None))
    }

    /// Steps the kernel through `events` and carries out what follows. A
    /// step that panics is a bug the session survives: what the steps did
    /// is dropped, as a commit that failed drops it, and the session
    /// resumes from its log.
    async fn step(&mut self, events: impl IntoIterator<Item = Event>) {
        let stepped = catch_unwind(AssertUnwindSafe(|| {
            let mut fx = Effects::new();
            for event in events {
                fx.extend(self.core.step(event));
            }
            fx
        }));
        match stepped {
            Ok(fx) => self.carry(fx).await,
            Err(panic) => {
                let why = panicked(&*panic);
                eprintln!("session {}: the kernel {why}", self.live.id.short());
                Box::pin(self.resume()).await;
            }
        }
    }

    /// Carries out `fx` in order, committing each run of appends before
    /// the effect after it. A commit that fails drops the rest and resumes
    /// from what the log holds.
    async fn carry(&mut self, fx: Effects) {
        let mut batch = Vec::new();
        let mut fx = fx.into_iter();
        loop {
            let next = fx.next();
            if let Some(Effect::Append(e)) = next {
                batch.push(e);
                continue;
            }
            if !batch.is_empty()
                && let Err(e) = self.commit(&mut batch).await
            {
                let why = format!("logging: {e}");
                eprintln!("session {}: {why}", self.live.id.short());
                for effect in next.into_iter().chain(fx) {
                    if let Effect::Reply { ticket, .. } = effect {
                        self.reply(ticket, Err(why.clone()));
                    }
                }
                return Box::pin(self.resume()).await;
            }
            match next {
                Some(effect) => self.effect(effect).await,
                None => return,
            }
        }
    }

    /// After a failed commit or a step that panicked: what runs is
    /// cancelled, what was asked is answered that the session stopped, and
    /// the kernel is rebuilt from the committed log, as a load rebuilds it.
    async fn resume(&mut self) {
        for cancel in self.cancels.values() {
            cancel.cancel();
        }
        self.replies.clear();
        self.waiting.clear();
        let bound = Bound::bind(&self.registry, &self.live.session);
        let load = Load {
            boot: sys::boot_id(),
            busy: self.live.has_work(),
            owner: self.owner(),
            ..Load::default()
        };
        let log: Vec<Entry> = self.live.frames.entries().collect();
        match catch_unwind(AssertUnwindSafe(|| Core::resume(bound, &log, &load))) {
            Ok((core, fx)) => {
                self.core = core;
                self.carry(fx).await;
            }
            // Its log is what it is: the session stops, and loads again
            // when next wanted.
            Err(panic) => {
                let why = panicked(&*panic);
                eprintln!("session {}: resuming {why}", self.live.id.short());
                self.stopping = true;
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

    /// Writes `batch` durably, then shows it: the writer stamps its lease.
    async fn commit(&mut self, batch: &mut Vec<Entry>) -> io::Result<()> {
        let mut writer = self
            .writer
            .take()
            .ok_or_else(|| io::Error::other("no writer"))?;
        let mut entries = std::mem::take(batch);
        let rt = Arc::clone(&self.live.session.shared.rt);
        let (writer, entries, written) = rt::unblock(&*rt, move || {
            let written = catch_unwind(AssertUnwindSafe(|| writer.append(&mut entries)))
                .unwrap_or_else(|_| Err(io::Error::other("committing entries panicked")));
            (writer, entries, written)
        })
        .await;
        self.writer = Some(writer);
        written?;
        self.shown(&entries);
        Ok(())
    }

    /// Entries committed: folded where else they are read, and sent.
    fn shown(&mut self, entries: &[Entry]) {
        let session = &self.live.session;
        let hooks = self.registry.hooks();
        self.transcript = self.transcript.take().filter(|t| t.is_on(&self.registry));
        for e in entries {
            self.live.frames.push(e.seq);
            lock(&session.states).apply(e);
            hooks.observe(&session.point(), e);
            let replied = match &e.body {
                Body::ModelResult { call, .. } => self.replied.take_if(|(of, _)| of == call),
                _ => None,
            };
            if let Some(transcript) = &mut self.transcript {
                transcript.apply(e, replied.map(|(_, m)| m));
            }
            if let Body::ToolResult { call, .. } | Body::ModelResult { call, .. } = &e.body
                && let Some(call) = CallId::new(*call)
            {
                // Settled: what still runs of it is let go.
                self.waiting.remove(&call);
            }
        }
        lock(&self.live.followers).retain(|c| c.wake().is_ok());
        *lock(&session.overrides) = self.core.ledger().settings();
    }

    /// The summary as the log now says.
    fn publish(&self) {
        let ledger = self.core.ledger();
        let mut summary = lock(&self.live.summary);
        summary.entries = ledger.seq();
        summary.activity = ledger.activity();
    }

    async fn effect(&mut self, effect: Effect) {
        let shared = Arc::clone(&self.live.session.shared);
        // A stopping session starts nothing: what it would have started is
        // in the log, for its next load.
        let starts = matches!(
            effect,
            Effect::CallModel { .. }
                | Effect::RunTool { .. }
                | Effect::Resume { .. }
                | Effect::Await { .. }
                | Effect::Recover { .. }
                | Effect::Sleep { .. }
                | Effect::StartChild { .. }
        );
        if self.stopping && starts {
            return;
        }
        match effect {
            Effect::Append(_) => unreachable!("appends are committed in runs"),
            Effect::Reply { ticket, result } => self.reply(ticket, result),
            Effect::CallModel {
                call,
                attempt,
                choice,
            } => self.call_model(call, attempt, choice),
            Effect::RunTool {
                call,
                tool,
                input,
                parsed,
            } => self.run_tool(call, tool, input, parsed),
            Effect::Resume {
                call,
                tool,
                input,
                value,
            } => {
                if let Some(waiting) = self.waiting.remove(&call) {
                    waiting.send(value);
                } else {
                    self.answers.insert(call, value);
                    self.run_tool(call, tool, input, None);
                }
            }
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
                if let Some(cancel) = self.cancels.remove(&call) {
                    cancel.cancel();
                }
                self.waiting.remove(&call);
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
                self.mark().await;
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

    /// Sets or clears the busy marker as the session now stands.
    async fn mark(&mut self) {
        let marked = self.live.has_work() || self.core.ledger().activity().requires_action();
        if marked == self.marked {
            return;
        }
        self.marked = marked;
        let shared = &self.live.session.shared;
        let (store, id) = (Arc::clone(&shared.store), self.live.id);
        if let Err(e) = rt::unblock(&*shared.rt, move || store.mark_busy(id, marked)).await {
            eprintln!("session {}: busy marker: {e}", id.short());
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
        let cancel = Cancel::default();
        self.cancels.insert(call, cancel.clone());
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

    /// Asks the model to reply to the transcript before `call`, with the
    /// tools `choice` lets it call, its text streaming to followers under
    /// `attempt`.
    fn call_model(&mut self, call: CallId, attempt: u16, choice: ToolChoice) {
        let work = self.live.session.model().map(|model| {
            let mut request = self.request(call, &model);
            request.tool_choice = match choice {
                ToolChoice::Auto => llm::ToolChoice::Auto,
                ToolChoice::None => llm::ToolChoice::None,
            };
            (model, request)
        });
        let cancel = Cancel::default();
        self.cancels.insert(call, cancel.clone());
        self.reporting += 1;
        let live = Arc::clone(&self.live);
        self.live.session.shared.rt.spawn(Box::pin(async move {
            let (replied, message) = match work {
                Ok((model, request)) => {
                    let deltas = |text: &str| live.delta(call, attempt, text);
                    let replying: BoxFuture<'_, _> =
                        Box::pin(reply(&model, &request, &cancel, deltas));
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
        let shared = &self.live.session.shared;
        let system = shared.system.as_deref();
        let system = system.unwrap_or_else(|| self.registry.system_prompt());
        let frames = &self.live.frames;
        let registry = &self.registry;
        let transcript = self
            .transcript
            .get_or_insert_with(|| Transcript::of(frames.entries(), Arc::clone(registry), system));
        let messages = if let Some(messages) = transcript.before(call.get()) {
            messages.clone()
        } else {
            // A call recovery runs after entries it does not see.
            let before = frames.entries().take_while(|e| e.seq < call.get());
            Transcript::of(before, Arc::clone(registry), system).into_messages()
        };
        model.request(messages, registry.specs().to_vec())
    }

    /// Runs tool call `call`, the model's or a user's command, reporting
    /// its outcome.
    fn run_tool(&mut self, call: CallId, tool: String, input: Json, parsed: Option<Parsed>) {
        let cancel = Cancel::default();
        self.cancels.insert(call, cancel.clone());
        self.reporting += 1;
        let registry = Arc::clone(&self.registry);
        let cx = self.live.session.cx(call, &registry, cancel);
        let mail = Arc::clone(&self.live.session.mail);
        let id = self.live.id;
        self.live.session.shared.rt.spawn(Box::pin(async move {
            let began = crate::stamp();
            let Called { outcome, card } = if registry
                .tools()
                .get(&tool)
                .is_some_and(|t| t.route() != Route::Local)
            {
                Called::failed(format!(
                    "{tool} runs in a client, and this daemon routes none yet"
                ))
            } else {
                contain(registry.tools().run(&tool, &input, parsed, &cx)).await
            };
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
        let view = SessionView::from_log(live.id, live.workspace.clone(), live.frames.entries());
        let frame = Frame::new(&DaemonMsg::Snapshot(Box::new(view))).ok();
        self.snapshot = Some((seq, frame.clone()));
        frame
    }
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

    pub fn ended(&self) -> Ended {
        self.ended.clone()
    }

    /// Returns once the actor has taken every message sent before.
    pub fn barrier(&self) {
        let (done, taken) = oneshot::channel();
        self.send(Msg::Barrier(done));
        rt::block_on(taken);
    }
}

impl Live {
    /// Live text of model call `call`'s attempt `attempt`, to followers.
    fn delta(&self, call: CallId, attempt: u16, text: &str) {
        let delta = DaemonMsg::Delta {
            call: call.get(),
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
    /// What call `call` runs with, on `registry`, stopped by `cancel`.
    fn cx(self: &Arc<Self>, call: CallId, registry: &Arc<Registry>, cancel: Cancel) -> Cx {
        Parts {
            workspace: Arc::from(self.workspace.as_path()),
            env: lock(&self.env).clone().map(Arc::new),
            session: self.id,
            call: call.get(),
            model: self.model_spec().into(),
            spill: Arc::from(self.shared.spill.join(self.id.to_string())),
            cancel,
        }
        .cx(Arc::new(Calling {
            session: Arc::clone(self),
            call,
            registry: Arc::clone(registry),
            own: OnceLock::new(),
        }))
    }

    fn point(&self) -> PointCx<'_> {
        PointCx::new(self.id, &self.workspace, self)
    }

    /// The model the settings and the newest client's environment name
    /// now, or why there is none.
    fn model(&self) -> Result<Model, String> {
        let set = lock(&self.overrides).clone();
        let env = lock(&self.env).clone();
        self.shared.models.model(&self.workspace, env.as_ref(), set)
    }

    /// The model spec the settings name now.
    fn model_spec(&self) -> String {
        let set = lock(&self.overrides).clone();
        let settings = &self.shared.models.settings;
        settings
            .model_in(&self.workspace, set)
            .map_or_else(|_| "none".to_owned(), |(model, _)| model)
    }

    /// The directory the session's commands run in now.
    fn dir(&self, registry: &Registry) -> PathBuf {
        let env = lock(&self.env);
        registry.dir(&self.workspace, env.as_ref(), &lock(&self.states))
    }
}

/// Read only when a handler asks: a point with no handler costs nothing.
impl View for Session {
    fn model(&self) -> String {
        self.model_spec()
    }

    fn states(&self, read: &mut dyn FnMut(&States)) {
        read(&lock(&self.states));
    }
}

/// A registry generation bound to a session: the kernel's view of the
/// extensions.
#[derive(Clone)]
struct Bound {
    registry: Arc<Registry>,
    session: Arc<Session>,
    /// The agent the session runs, by name.
    agent: Option<String>,
}

impl Bound {
    fn bind(registry: &Arc<Registry>, session: &Arc<Session>) -> Arc<dyn kernel::Registry> {
        let settings = &session.shared.models.settings;
        Arc::new(Self {
            registry: Arc::clone(registry),
            session: Arc::clone(session),
            agent: settings.agent(&session.workspace),
        })
    }

    fn point(&self) -> PointCx<'_> {
        self.session.point()
    }

    /// The session, as handlers deciding about call `call` see it.
    fn about(&self, call: CallId) -> PointCx<'_> {
        PointCx::new(self.session.id, &self.session.workspace, self).about(call.get())
    }

    /// The gate of call `call` once the `approve` handler it waits on
    /// answers: with none, the handlers after it are asked, and with no
    /// answer at all the user is. A handler that panics refuses the call.
    async fn approving(self, call: CallId, tool: String, mut asked: Approving) -> Gated {
        loop {
            let answer = contain(async { Ok(asked.answer.0.await) }).await;
            let answer = match answer {
                Ok(Some(approved)) => Some((asked.by, approved)),
                Ok(None) => {
                    let rest = guarded(
                        || {
                            (self.registry.hooks()).approve_from(
                                &self.about(call),
                                &tool,
                                &asked.input,
                                asked.next,
                            )
                        },
                        |_| Asked::Now(Name::from("gate"), false),
                    );
                    match rest {
                        Asked::None => None,
                        Asked::Now(by, approved) => Some((by, approved)),
                        Asked::Later { by, at, answer } => {
                            (asked.by, asked.next, asked.answer) = (by, at + 1, answer);
                            continue;
                        }
                    }
                }
                Err(why) => {
                    let mut decisions = asked.decisions;
                    let reason = format!("{} {why}", asked.by);
                    decisions.push((
                        asked.by,
                        Verdict::Block {
                            reason: reason.clone(),
                        },
                    ));
                    return Gated {
                        input: asked.input,
                        decisions,
                        gate: ext::Gate::Refuse(reason),
                    };
                }
            };
            return Gated::approved(asked.input, asked.decisions, answer);
        }
    }

    /// How to recover model call `call` from `error` once the
    /// `request_error` handler `by`, the `at`th, answers: with none, or if
    /// it panics, the handlers after it are asked.
    async fn recovering(
        self,
        call: CallId,
        attempt: u16,
        error: llm::Error,
        (mut by, mut at, mut answer): (Name, usize, ext::Later<Option<Recover>>),
    ) -> Option<(Name, Recover)> {
        loop {
            if let Ok(Some(recover)) = contain(async { Ok(answer.0.await) }).await {
                return Some((by, recover));
            }
            let failed = Failed {
                error: &error,
                attempt,
            };
            let rest = guarded(
                || (self.registry.hooks()).request_error_from(&self.about(call), failed, at + 1),
                |_| Asked::None,
            );
            match rest {
                Asked::None => return None,
                Asked::Now(by, recover) => return Some((by, recover)),
                Asked::Later {
                    by: next,
                    at: next_at,
                    answer: next_answer,
                } => (by, at, answer) = (next, next_at, next_answer),
            }
        }
    }
}

/// What a decision about a call reads of the session, and the call's own
/// context for work it answers later with.
impl View for Bound {
    fn model(&self) -> String {
        self.session.model_spec()
    }

    fn states(&self, read: &mut dyn FnMut(&States)) {
        read(&lock(&self.session.states));
    }

    fn cx(&self, call: u64) -> Option<Cx> {
        let call = CallId::new(call)?;
        Some(self.session.cx(call, &self.registry, Cancel::default()))
    }
}

impl kernel::Registry for Bound {
    fn agent(&self) -> Box<dyn kernel::Agent> {
        self.registry.agent(self.agent.as_deref())
    }

    fn loop_points(&self) -> &dyn ext::LoopPoints {
        self
    }

    fn installed(&self) -> Vec<Installed> {
        installed(&self.registry)
    }

    fn callable(&self, name: &str) -> Option<Callable> {
        let tool = self.registry.tools().get(name)?;
        let (audience, replay) = (tool.audience(), tool.replay());
        Some(Callable { audience, replay })
    }

    fn model(&self) -> String {
        self.session
            .model()
            .map_or_else(|_| "none".to_owned(), |model| model.name())
    }

    fn context(&self) -> Option<String> {
        let dir = self.session.dir(&self.registry);
        let workspace = &self.session.workspace;
        let facts = Facts {
            workspace,
            dir: &dir,
        };
        guarded(|| self.registry.context(&facts), |_| None)
    }

    fn read(&self, tool: &str, input: &Json) -> (Option<Parsed>, Option<Card>) {
        let Some(tool) = self.registry.tools().get(tool) else {
            return (None, None);
        };
        let Ok(parsed) = tool.parse(input) else {
            return (None, None);
        };
        let card = guarded(|| tool.present(&parsed), |_| None);
        (Some(parsed), card)
    }

    /// Fails closed ([`ext::point::Input::ON_ERROR`]): input a handler
    /// could not decide on is refused, and its client told why.
    fn input(&self, text: String) -> Result<Chained<String>, String> {
        guarded(
            || self.registry.hooks().input(&self.point(), text),
            |why| Err(format!("the input point {why}")),
        )
    }

    /// #43 `a_failing_gate_fails_closed`: a gate that cannot decide denies
    /// the call, and says why.
    fn gate(&self, call: CallId, tool: &str, input: &Json) -> Answer<Gated> {
        let gating = guarded(
            || self.registry.hooks().gate(&self.about(call), tool, input),
            |why| {
                Gating::Now(Gated {
                    input: input.clone(),
                    decisions: vec![(
                        Name::from("gate"),
                        Verdict::Block {
                            reason: why.clone(),
                        },
                    )],
                    gate: ext::Gate::Refuse(format!("the gate {why}")),
                })
            },
        );
        match gating {
            Gating::Now(gated) => Answer::Now(gated),
            Gating::Approving(asked) => {
                let later = self.clone().approving(call, tool.to_owned(), asked);
                Answer::Later(ext::Later::new(later))
            }
        }
    }

    fn request_error(
        &self,
        call: CallId,
        attempt: u16,
        error: &llm::Error,
    ) -> Answer<Option<(Name, Recover)>> {
        let failed = Failed { error, attempt };
        let asked = guarded(
            || {
                self.registry
                    .hooks()
                    .request_error(&self.about(call), failed)
            },
            |_| Asked::None,
        );
        match asked {
            Asked::None => Answer::Now(None),
            Asked::Now(by, recover) => Answer::Now(Some((by, recover))),
            Asked::Later { by, at, answer } => {
                let later =
                    (self.clone()).recovering(call, attempt, error.clone(), (by, at, answer));
                Answer::Later(ext::Later::new(later))
            }
        }
    }

    /// A transform fails open: the outcome goes on as it was.
    fn post_tool(
        &self,
        call: CallId,
        tool: &str,
        input: &Json,
        outcome: Outcome,
    ) -> Chained<Outcome> {
        let original = outcome.clone();
        guarded(
            || (self.registry.hooks()).post_tool(&self.about(call), (tool, input), outcome),
            |_| Chained::new(original),
        )
    }

    fn on_stop(&self) -> Option<(Name, String)> {
        guarded(|| self.registry.hooks().on_stop(&self.point()), |_| None)
    }
}

/// The loop points fail open: a handler that panics leaves what the loop
/// would do by default.
impl ext::LoopPoints for Bound {
    fn turn(&self, input: &kernel::Input, default: kernel::Actions) -> kernel::Actions {
        let points = self.registry.hooks();
        let shown = default.clone();
        guarded(|| points.turn(input, default), |_| shown)
    }

    fn reply(&self, step: u16, reply: &llm::Response, default: kernel::Actions) -> kernel::Actions {
        let points = self.registry.hooks();
        let shown = default.clone();
        guarded(|| points.reply(step, reply, default), |_| shown)
    }

    fn resume(&self, step: u16) -> ext::Resume {
        let points = self.registry.hooks();
        guarded(|| points.resume(step), |_| ext::Resume::Continue)
    }
}

/// What `point` answers, or what `failed` makes of the panic of a handler
/// in it: an extension's failure is never the session's.
fn guarded<T>(point: impl FnOnce() -> T, failed: impl FnOnce(String) -> T) -> T {
    catch_unwind(AssertUnwindSafe(point)).unwrap_or_else(|panic| failed(panicked(&*panic)))
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

/// The registry's extensions as a log records them.
pub fn installed(registry: &Registry) -> Vec<Installed> {
    registry
        .extensions()
        .map(|(name, version)| Installed {
            name: name.to_owned(),
            version,
        })
        .collect()
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
    /// The session's settings and children, made when a call first asks.
    own: OnceLock<(Arc<dyn ext::Settings>, Arc<dyn ext::Sessions>)>,
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
            if let Some(daemon) = session.shared.daemon.upgrade() {
                daemon.jobs.notify();
            }
        }));
    }

    fn progress(&self, text: &str) {
        self.report(Note::Progress(text.to_owned()));
    }

    fn note(&self, ext: &str, state: &str, data: Json) {
        let (ext, state) = (ext.to_owned(), state.to_owned());
        self.report(Note::State { ext, state, data });
    }

    fn states(&self, read: &mut dyn FnMut(&States)) {
        read(&lock(&self.session.states));
    }

    fn registry(&self) -> &Registry {
        &self.registry
    }

    fn service(&self, ty: TypeId) -> Option<&(dyn Any + Send + Sync)> {
        let own = || {
            let own = Arc::new(Own(Arc::clone(&self.session)));
            (
                Arc::clone(&own) as Arc<dyn ext::Settings>,
                own as Arc<dyn ext::Sessions>,
            )
        };
        if ty == TypeId::of::<dyn ext::Settings>() {
            return Some(&self.own.get_or_init(own).0);
        }
        if ty == TypeId::of::<dyn ext::Sessions>() {
            return Some(&self.own.get_or_init(own).1);
        }
        (ty == TypeId::of::<dyn rt::Runtime>()).then_some(&self.session.shared.rt as _)
    }
}

/// A session's settings and children, as its calls reach them.
struct Own(Arc<Session>);

impl ext::Settings for Own {
    fn model(&self, _: &Cx) -> Result<(String, String), String> {
        let session = &self.0;
        let set = lock(&session.overrides).clone();
        let settings = &session.shared.models.settings;
        let (model, from) = settings.model_in(&session.workspace, set)?;
        Ok((model, from.to_string()))
    }

    /// Committed as the settings extension's state, before it returns.
    fn set<'a>(&'a self, cx: &'a Cx, set: Overrides) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let call = CallId::new(cx.call).ok_or_else(|| io::Error::other("no call"))?;
            let (done, committed) = oneshot::channel();
            let note = Note::State {
                ext: "settings".to_owned(),
                state: "model".to_owned(),
                data: set.model.as_deref().into(),
            };
            let note = Msg::Note {
                call,
                note,
                done: Some(done),
            };
            let _ = self.0.mail.send(note);
            committed
                .await
                .ok_or_else(|| io::Error::other("the session stopped"))
        })
    }
}

impl ext::Sessions for Own {
    /// The child the call started, or starts now; an interrupt of the
    /// call's turn reaches it first.
    fn child<'a>(&'a self, cx: &'a Cx, input: String) -> BoxFuture<'a, Outcome> {
        Box::pin(async move {
            let Some(call) = CallId::new(cx.call) else {
                return Outcome::Interrupted { output: None };
            };
            let (reply, result) = oneshot::channel();
            let _ = self.0.mail.send(Msg::Child { call, input, reply });
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
    mut deltas: impl FnMut(&str) + Send,
) -> (Replied, Option<llm::Assistant>) {
    // Sent once polled, as an interrupt meanwhile finds it.
    let mut events = None;
    let reading = async {
        let events = events.insert(model.send(request));
        crate::model::read(events, &mut deltas).await
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
        Ok(reply) => match Json::encode(&reply) {
            Ok(output) => (Replied::Done(output), Some(reply.message)),
            Err(e) => (
                Replied::Failed(llm::Error::new(llm::Kind::Decode, e.to_string())),
                None,
            ),
        },
        Err(error) => (Replied::Failed(error), None),
    }
}

/// `work`'s output, or `None` once `cancel` is cancelled first; `work` is
/// then dropped, and not started at all if cancelled already.
async fn unless<T>(cancel: &Cancel, work: impl Future<Output = T>) -> Option<T> {
    if cancel.is_cancelled() {
        return None;
    }
    let (mut work, mut stop) = (pin!(work), pin!(cancel.cancelled()));
    poll_fn(|cx| {
        if let Poll::Ready(done) = work.as_mut().poll(cx) {
            return Poll::Ready(Some(done));
        }
        stop.as_mut().poll(cx).map(|()| None)
    })
    .await
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
