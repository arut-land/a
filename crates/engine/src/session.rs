use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Display;
use std::future::{Future, poll_fn};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

use agent::{
    Action, Agent, Cancel, Cx, Decided, DynTool, Facts, Gate, Hooks, Host, Input, Registry, Replay,
    Route, States,
};
use protocol::ledger::{self, Ledger, Phase};
use protocol::view::SessionView;
use protocol::{
    Activity, Body, Cap, Card, DaemonMsg, Decision, Entry, Environment, Frame, Installed, Json,
    Outcome, Overrides, Residency, SessionId, SessionSummary,
};
use rt::BoxFuture;
use serde_json::{Value, json};
use store::{Header, Loaded, Log, LogStore, Parent};

use crate::model::{Model, Models};
use crate::outbox::{FrameLog, Outbox};
use crate::{Daemon, lock};

mod facts;
mod links;
#[cfg(test)]
mod oracle;
#[cfg(test)]
mod tests;

use facts::{Replayer, Transcript, ask, recap, replay};

/// What every session runs with.
pub struct Shared {
    /// Where session actors run as tasks, and effects sleep and block.
    pub rt: rt::Rt,
    pub store: Arc<dyn LogStore>,
    /// The extensions' tools and hooks, as each session resolves them at
    /// its step boundaries.
    pub registry: agent::Current,
    /// The settings, and the model they name.
    pub models: Models,
    /// The system prompt, in place of the one the extensions make.
    pub system: Option<String>,
    /// Where sessions keep output too large for their logs, a directory
    /// each.
    pub spill: PathBuf,
    /// Sessions loaded since the daemon started.
    pub(crate) loaded: Mutex<HashSet<SessionId>>,
    /// Where linked sessions are started and found.
    pub(crate) daemon: Weak<Daemon>,
}

pub struct Session {
    /// This session, for the jobs that commit its entries.
    this: Weak<Session>,
    pub id: SessionId,
    pub workspace: PathBuf,
    /// The call that owns this session, if it is a child.
    parent: Option<Parent>,
    /// The epoch of the lease its log is appended under.
    epoch: u32,
    /// The registry generation the current step runs on.
    step: Mutex<Arc<Registry>>,
    state: Mutex<State>,
    /// The committed entries, encoded once for every client; they commit
    /// under `state`'s lock.
    frames: Arc<FrameLog>,
    /// The appender; only the job draining `queue` writes to it.
    log: Mutex<Box<dyn Log>>,
    /// Entries waiting to be committed, in order.
    queue: Mutex<Queue>,
    /// The actor's messages. The session's inbox, what waits for the user,
    /// is in its log: see [`facts::inbox`].
    mailbox: rt::channel::Sender<Msg>,
    answers: Mutex<Answers>,
    /// Input taken in and not yet taken up, in order; its log entries are
    /// what lasts. Taking input in holds this lock until its entry commits,
    /// so the order here is the log's.
    waiting: Mutex<VecDeque<Waiting>>,
    /// Inputs submitted but not yet fully handled. They pin the daemon and
    /// keep the session loaded, as do watches not yet fired.
    queued: AtomicUsize,
    /// Whether the actor is in a unit of work, even one that waits for an
    /// answer and so is not counted in `queued`. Recovery is from the start.
    running: AtomicBool,
    /// Open client attachments; a session with any stays loaded.
    attached: AtomicUsize,
    last_active: Mutex<Instant>,
    /// What commands run in: the last client's capture, secrets included,
    /// or after a restart the last one logged, without them.
    env: Mutex<Option<Environment>>,
    /// The extensions' state, folded from the log.
    states: Mutex<States>,
    /// Stops the turn running now.
    cancel: Mutex<Cancel>,
    /// The message of the reply model call `.0` just had, until its result
    /// commits.
    replied: Mutex<Option<(u64, llm::Assistant)>>,
    /// Background process groups the session owns, until it ends.
    jobs: Mutex<Vec<u32>>,
    shared: Arc<Shared>,
}

/// How [`Session::start`] opens a session's log in the store.
#[derive(Clone, Copy)]
pub enum Open {
    /// Made new, for a session its header describes.
    Create,
    /// Loaded, and recovered if it holds any entries.
    Load,
}

enum Msg {
    /// Input was taken in: the next of [`Session::waiting`] is the actor's
    /// to take up, unless a turn took it already.
    Queued,
    /// Child `child` settled with `outcome`.
    Settled {
        child: SessionId,
        outcome: Outcome,
    },
    Stop,
}

struct State {
    /// What the entries say, folded as each commits.
    ledger: Ledger,
    /// The conversation the model sees, made at the first model call and
    /// folded as each entry commits from then on.
    transcript: Option<Transcript>,
    /// The clients following the session.
    clients: Vec<Outbox>,
    /// The last snapshot made, and the seq it is at; `None` for one too
    /// large for a frame.
    snapshot: Option<(u64, Option<Frame>)>,
}

/// How many entries behind a client must be to get a snapshot rather than
/// a replay, when it takes them.
const SNAPSHOT_GAP: u64 = 256;

/// Entries recorded and not yet committed. While `draining`, one blocking
/// job commits them in batches, so log writes and their fsyncs stay off the
/// async workers and an entry is shown only after every earlier one.
#[derive(Default)]
struct Queue {
    /// The seq of the last entry recorded.
    next: u64,
    pending: Vec<Entry>,
    /// Flushes waiting for every entry up to a seq to commit.
    flushes: Vec<(u64, rt::channel::Sender<io::Result<()>>)>,
    draining: bool,
}

/// Input waiting for the actor.
struct Waiting {
    /// The entry that took it in.
    seq: u64,
    text: String,
    steer: bool,
    /// Whether it is a slash command.
    command: bool,
    /// Whether that entry is its `user` or `command` entry already: it
    /// arrived with nothing running or waiting, so it was not queued.
    started: bool,
}

/// Answers to the inbox's asks, until the actor takes them. `stopped` wakes
/// an actor waiting for one so it can stop.
#[derive(Default)]
struct Answers {
    given: HashMap<u64, bool>,
    stopped: bool,
    /// The actor, while it waits for an answer.
    waiting: Option<Waker>,
}

impl Answers {
    fn wake(&mut self) {
        if let Some(waker) = self.waiting.take() {
            waker.wake();
        }
    }
}

/// A client's hold on a session; dropping it detaches.
pub struct Attachment(Arc<Session>);

impl Attachment {
    pub fn session(&self) -> &Session {
        &self.0
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.0.attached.fetch_sub(1, Ordering::SeqCst);
        self.0.touch();
    }
}

impl Session {
    /// Starts a loaded session's actor: the single writer that steps the
    /// agent and runs its effects. Calls left dangling by a crash are
    /// recovered first.
    pub fn start(header: Header, shared: Arc<Shared>, open: Open) -> io::Result<Arc<Self>> {
        Self::start_in(header, shared, open, None, Vec::new())
    }

    /// [`Session::start`], running commands in `env` rather than the last
    /// one logged: a child runs in its parent's, secrets included. A log
    /// with no entries is first given `opening`.
    fn start_in(
        header: Header,
        shared: Arc<Shared>,
        open: Open,
        env: Option<Environment>,
        opening: Vec<Body>,
    ) -> io::Result<Arc<Self>> {
        let registry = shared.registry.get();
        let kind = shared.models.settings.agent(&header.workspace);
        let mut load = Load {
            ledger: Ledger::default(),
            states: registry.states(),
            replay: Replayer::default(),
            agent: kind.agent(),
        };
        // Each entry is folded as it is read and dropped: what loading
        // keeps is what the session keeps, never the entries.
        let mut loaded = match open {
            Open::Create => shared.store.create(&header)?,
            Open::Load => shared.store.load(header.id, &mut |e| load.fold(&e))?,
        };
        if loaded.seq == 0 {
            for body in opening {
                load.fold(&loaded.append(body)?);
            }
        }
        if let Some(body) = note_extensions(&header, &load.ledger, &registry) {
            match loaded.append(body) {
                Ok(e) => load.fold(&e),
                Err(e) => eprintln!("session {}: recording extensions: {e}", header.id.short()),
            }
        }
        let Load {
            ledger,
            states,
            replay,
            agent,
        } = load;
        let Loaded {
            log,
            lines,
            seq,
            epoch,
        } = loaded;
        let frames = FrameLog::new(lines, seq);
        // A new session has nothing to recover.
        let recover = (seq > 0).then(|| replay.into_actions());
        let fresh = recover.is_none();
        let (mailbox, rx) = rt::channel::unbounded();
        let armed = ledger.armed().count();
        let env = env.or_else(|| ledger.env().cloned());
        let session = Arc::new_cyclic(|this| Self {
            this: this.clone(),
            id: header.id,
            workspace: header.workspace,
            parent: header.parent,
            epoch,
            step: Mutex::new(Arc::clone(&registry)),
            state: Mutex::new(State {
                ledger,
                transcript: None,
                clients: Vec::new(),
                snapshot: None,
            }),
            frames: Arc::new(frames),
            log: Mutex::new(log),
            queue: Mutex::new(Queue {
                next: seq,
                ..Queue::default()
            }),
            mailbox,
            answers: Mutex::default(),
            waiting: Mutex::default(),
            // Recovery counts as queued work until it has run, and every
            // armed watch until it fires.
            queued: AtomicUsize::new(usize::from(!fresh) + armed),
            running: AtomicBool::new(!fresh),
            attached: AtomicUsize::new(0),
            last_active: Mutex::new(Instant::now()),
            env: Mutex::new(env),
            states: Mutex::new(states),
            cancel: Mutex::default(),
            replied: Mutex::default(),
            jobs: Mutex::default(),
            shared,
        });
        registry.hooks().session_start(session.id);
        let actor = Arc::clone(&session);
        session.shared.rt.spawn(Box::pin(async move {
            actor.run(&rx, kind, agent, recover).await;
        }));
        Ok(session)
    }

    /// Opens a client attachment. Callers hold the session's slot lock, so a
    /// session cannot be unloaded between being found and being attached.
    pub fn open(self: &Arc<Self>) -> Attachment {
        self.attached.fetch_add(1, Ordering::SeqCst);
        self.touch();
        Attachment(Arc::clone(self))
    }

    /// Takes the user's input in as the `input` hooks leave it, durably,
    /// before returning: `/name args` runs command `name` if there is one,
    /// anything else goes to the agent, after the turn running now and the
    /// input already waiting. `Err` is why a hook refused it, or why it
    /// could not be logged.
    pub fn submit(&self, text: String) -> Result<(), String> {
        self.take_in(text, false)
    }

    /// [`Session::submit`] for input the running turn takes at its next
    /// step boundary, ahead of what waits; with no turn running it is plain
    /// input.
    pub fn steer(&self, text: String) -> Result<(), String> {
        self.take_in(text, true)
    }

    fn take_in(&self, mut text: String, steer: bool) -> Result<(), String> {
        let registry = self.shared.registry.get();
        let mut decided = Decided::new();
        registry.hooks().input(&mut text, &mut decided)?;
        let parsed = as_command(&registry, &text);
        let command = parsed.is_some();
        let mut waiting = lock(&self.waiting);
        for (hook, decision) in decided {
            self.record(Body::Hook {
                hook: hook.to_owned(),
                decision,
            });
        }
        // Nothing runs or waits, so nothing can come between: the input's
        // own entry starts its work.
        let started =
            waiting.is_empty() && !self.has_work() && !self.running.load(Ordering::SeqCst);
        let body = match parsed {
            _ if !started => Body::Queued {
                text: text.clone(),
                steer,
            },
            Some((name, args)) => Body::Command {
                name: name.to_owned(),
                args: args.to_owned(),
            },
            None => Body::User { text: text.clone() },
        };
        let seq = self.record(body);
        rt::block_on(self.flush()).map_err(|e| format!("logging the input: {e}"))?;
        waiting.push_back(Waiting {
            seq,
            text,
            steer,
            command,
            started,
        });
        self.queued.fetch_add(1, Ordering::SeqCst);
        drop(waiting);
        let _ = self.mailbox.send(Msg::Queued);
        Ok(())
    }

    /// Stops the running turn: its model or tool call, and what the agent
    /// would have done next. The turns of the sessions it started stop
    /// first, bottom-up. With no unit of work running there is nothing to
    /// stop.
    pub fn interrupt(&self) {
        for child in self.children() {
            child.interrupt();
        }
        let cancel = lock(&self.cancel);
        if self.running.load(Ordering::SeqCst) {
            cancel.cancel();
        }
    }

    /// Answers the ask for tool call `call`; the actor logs the answer and
    /// runs or refuses the call. The first answer wins.
    pub fn answer(&self, call: u64, approved: bool) -> Result<(), String> {
        if !self.lock().ledger.inbox().contains(&call) {
            return Err(format!("call {call} is not waiting for approval"));
        }
        let mut answers = lock(&self.answers);
        answers.given.entry(call).or_insert(approved);
        answers.wake();
        Ok(())
    }

    pub fn has_work(&self) -> bool {
        self.queued.load(Ordering::SeqCst) > 0
    }

    /// Whether the session or a child it owns has work or processes: an
    /// owner is idle only once what it owns is.
    fn busy(&self) -> bool {
        self.has_work() || self.has_jobs() || self.children().iter().any(|c| c.busy())
    }

    /// The child sessions this one owns that are loaded.
    fn children(&self) -> Vec<Arc<Self>> {
        self.shared
            .daemon
            .upgrade()
            .map_or_else(Vec::new, |daemon| daemon.children(self.id))
    }

    /// Whether any background process the session owns still runs.
    pub fn has_jobs(&self) -> bool {
        let mut jobs = lock(&self.jobs);
        jobs.retain(|&pgid| sys::group_alive(pgid));
        !jobs.is_empty()
    }

    /// How long the session has had no attachments and nothing it owns has
    /// had work or jobs.
    pub fn idle_for(&self) -> Duration {
        if self.attached.load(Ordering::SeqCst) > 0 || self.busy() {
            return Duration::ZERO;
        }
        lock(&self.last_active).elapsed()
    }

    /// Stops the actor, the turn it runs and the processes it owns, after
    /// unloading the children it owns; the session lives on in its log.
    /// Callers check `idle_for` under the slot lock first, or are draining
    /// the daemon. Stopping again does nothing.
    pub fn stop(&self) {
        if std::mem::replace(&mut lock(&self.answers).stopped, true) {
            return;
        }
        if let Some(daemon) = self.shared.daemon.upgrade() {
            daemon.unload_children(self.id);
        }
        lock(&self.cancel).cancel();
        lock(&self.answers).wake();
        let _ = self.mailbox.send(Msg::Stop);
        for pgid in lock(&self.jobs).drain(..) {
            self.kill(pgid);
        }
        self.registry().hooks().session_end(self.id);
    }

    /// Attaches `client`, which has the entries up to `since`: it is sent
    /// the attach, what it missed (or a snapshot, if it is far behind and
    /// takes them) and its end, a recap and the asks still waiting, then
    /// everything live. What it missed is read from the shared frame log as
    /// it writes, so attaching copies nothing.
    pub fn attach(&self, client: &Outbox, since: u64) {
        let mut st = self.lock();
        let seq = self.frames.seq();
        let far = client.caps().has(Cap::Snapshot) && seq.saturating_sub(since) > SNAPSHOT_GAP;
        let snapshot = far.then(|| self.snapshot(&mut st, seq)).flatten();
        let cursor = if snapshot.is_some() {
            seq
        } else {
            since.min(seq)
        };
        let mut frames = Vec::new();
        let mut push = |after: u64, msg: &DaemonMsg| {
            frames.extend(Frame::new(msg).ok().map(|frame| (after, frame)));
        };
        push(
            cursor,
            &DaemonMsg::Attached {
                session: self.id,
                workspace: self.workspace.clone(),
            },
        );
        frames.extend(snapshot.map(|frame| (seq, frame)));
        let mut push = |msg: &DaemonMsg| frames.extend(Frame::new(msg).ok().map(|f| (seq, f)));
        push(&DaemonMsg::Synced { seq });
        let reply = facts::reply(self.frames.entries_after(st.ledger.turn_start()));
        if let Some(recap) = recap(&st.ledger, reply.as_deref(), since) {
            push(&DaemonMsg::Recap(recap));
        }
        for &call in st.ledger.inbox() {
            if let Some(msg) = ask(&st.ledger, call) {
                push(&msg);
            }
        }
        let follower = client.follow(Arc::clone(&self.frames), cursor, frames);
        st.clients.push(follower);
    }

    /// The snapshot of the session at entry `seq`, the last committed,
    /// made once for every client attaching then; `None` if too large for
    /// a frame, so that it is replayed instead.
    fn snapshot(&self, st: &mut State, seq: u64) -> Option<Frame> {
        if let Some((at, frame)) = &st.snapshot
            && *at == seq
        {
            return frame.clone();
        }
        let view = SessionView::from_log(self.id, self.workspace.clone(), self.frames.entries());
        let frame = Frame::new(&DaemonMsg::Snapshot(Box::new(view))).ok();
        st.snapshot = Some((seq, frame.clone()));
        frame
    }

    /// Runs commands from now on in `env`, the newest client's. Its redacted
    /// form is logged when that differs from the last one logged, so
    /// re-attaching from the same shell adds nothing.
    pub fn set_env(&self, env: Environment) -> io::Result<()> {
        let logged = env.redacted();
        *lock(&self.env) = Some(env);
        if self.lock().ledger.env() == Some(&logged) {
            return Ok(());
        }
        rt::block_on(self.append(Body::Environment(logged))).map(drop)
    }

    pub fn summary(&self) -> SessionSummary {
        let st = self.lock();
        let activity = if !st.ledger.inbox().is_empty() {
            Activity::Approval
        } else if self.has_work() {
            Activity::Running
        } else {
            Activity::Idle
        };
        SessionSummary {
            id: self.id,
            workspace: self.workspace.clone(),
            entries: st.ledger.seq(),
            clients: u32::try_from(self.attached.load(Ordering::SeqCst)).unwrap_or(u32::MAX),
            activity,
            residency: Residency::Loaded,
        }
    }

    /// Notes activity, and tells the daemon's supervisor, which decides
    /// on hibernation and idle exit.
    fn touch(&self) {
        *lock(&self.last_active) = Instant::now();
        if let Some(daemon) = self.shared.daemon.upgrade() {
            daemon.pulse.wake();
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn warn(&self, problem: impl Display) {
        eprintln!("session {}: {problem}", self.id.short());
    }

    fn kill(&self, pgid: u32) {
        if let Err(e) = sys::kill_process_group(pgid) {
            self.warn(format_args!("killing process group {pgid}: {e}"));
        }
    }

    /// Commits an entry and every one recorded before it, returning its seq.
    async fn append(&self, body: Body) -> io::Result<u64> {
        let seq = self.record(body);
        self.flush().await?;
        Ok(seq)
    }

    /// Records an entry to commit after every one recorded before it, and
    /// returns its seq. It is committed, and shown, at the next flush: an
    /// actor flushes before every effect, before reading its log and at the
    /// end of each unit of work, so the entries of one step share an fsync.
    fn record(&self, body: Body) -> u64 {
        let mut queue = lock(&self.queue);
        queue.next += 1;
        let seq = queue.next;
        queue.pending.push(Entry::new(seq, self.epoch, body));
        seq
    }

    /// Commits every entry recorded so far, off the async workers.
    async fn flush(&self) -> io::Result<()> {
        let rx = {
            let mut queue = lock(&self.queue);
            if queue.pending.is_empty() && !queue.draining {
                return Ok(());
            }
            let (tx, rx) = rt::channel::bounded(1);
            let upto = queue.next;
            queue.flushes.push((upto, tx));
            self.drain_in(&mut queue);
            rx
        };
        rx.recv()
            .await
            .unwrap_or_else(|| Err(io::Error::other("the log writer stopped")))
    }

    /// Starts committing what is recorded without waiting for it.
    fn kick(&self) {
        self.drain_in(&mut lock(&self.queue));
    }

    /// Starts the job that commits `queue` unless it runs already.
    fn drain_in(&self, queue: &mut Queue) {
        if !std::mem::replace(&mut queue.draining, true)
            && let Some(this) = self.this.upgrade()
        {
            self.shared
                .rt
                .spawn_blocking(Box::new(move || this.drain()));
        }
    }

    /// Commits recorded entries, a batch at a time, until none are left.
    /// A batch that fails is dropped with everything recorded after it,
    /// so the seqs of the next entries recorded follow the committed ones.
    fn drain(&self) {
        let mut batch = Vec::new();
        loop {
            let flushes = {
                let mut queue = lock(&self.queue);
                if queue.pending.is_empty() && queue.flushes.is_empty() {
                    queue.draining = false;
                    return;
                }
                std::mem::swap(&mut batch, &mut queue.pending);
                std::mem::take(&mut queue.flushes)
            };
            let result = catch_unwind(AssertUnwindSafe(|| self.commit(&mut batch)))
                .unwrap_or_else(|_| Err(io::Error::other("committing entries panicked")));
            batch.clear();
            let committed = self.frames.seq();
            if let Err(e) = &result {
                let mut queue = lock(&self.queue);
                queue.pending.clear();
                queue.next = committed;
                self.warn(e);
            }
            for (upto, tx) in flushes {
                let done = match &result {
                    _ if committed >= upto => Ok(()),
                    Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
                    Ok(()) => Err(io::Error::other("an earlier entry failed to commit")),
                };
                drop(tx.send(done));
            }
        }
    }

    /// Writes `batch` durably, then shows it. Only [`Session::drain`] calls
    /// it, so entries commit in order.
    fn commit(&self, batch: &mut Vec<Entry>) -> io::Result<()> {
        lock(&self.log).append_all(batch)?;
        let registry = self.registry();
        let hooks = registry.hooks();
        let mut st = self.lock();
        // Made again, when next asked for, on other tools.
        st.transcript = st.transcript.take().filter(|t| t.is_on(&registry));
        for entry in batch.drain(..) {
            self.frames.push(entry.seq);
            hooks.observe(&entry);
            st.ledger.apply(&entry);
            let replied = match &entry.body {
                Body::ModelResult { call, .. } => lock(&self.replied)
                    .take_if(|(of, _)| of == call)
                    .map(|(_, message)| message),
                _ => None,
            };
            if let Some(transcript) = &mut st.transcript {
                transcript.apply(&entry, replied);
            }
            // Under the same lock as the entry, so a client attaching
            // meanwhile gets the ask exactly once.
            if let Body::Ask { call } = entry.body
                && let Some(Ok(frame)) = ask(&st.ledger, call).as_ref().map(Frame::new)
            {
                st.clients
                    .retain(|c| c.send_after(entry.seq, frame.clone()).is_ok());
            }
        }
        st.clients.retain(|c| c.wake().is_ok());
        #[cfg(test)]
        oracle::check(&self.frames.entries().collect::<Vec<_>>(), &st.ledger);
        Ok(())
    }

    /// Commits what the hooks decided. Decisions that repeat the hook
    /// entries ending the log are a gate recovery runs again because a crash
    /// cut it off before its effect was committed: they are logged once.
    async fn decide(&self, decided: Decided) -> io::Result<()> {
        if decided.is_empty() {
            return Ok(());
        }
        self.flush().await?;
        {
            let st = self.lock();
            // What a restart logs before recovery runs is not part of it.
            let logged = st.ledger.trailing_hooks();
            let same = logged
                .iter()
                .map(|(hook, decision)| (hook.as_str(), decision))
                .eq(decided.iter().map(|(hook, decision)| (*hook, decision)));
            if !logged.is_empty() && same {
                return Ok(());
            }
        }
        for (hook, decision) in decided {
            self.record(Body::Hook {
                hook: hook.to_owned(),
                decision,
            });
        }
        self.flush().await
    }

    /// Sends a message that is not part of the log to every attached client.
    fn broadcast(&self, msg: &DaemonMsg) {
        if let Ok(frame) = Frame::new(msg) {
            let mut st = self.lock();
            let after = self.frames.seq();
            st.clients
                .retain(|c| c.send_after(after, frame.clone()).is_ok());
        }
    }

    /// An agent may step past entries that then fail to commit; after a
    /// failed unit of work it is rebuilt from the log. Each unit's future is
    /// boxed, so an idle session's actor holds only what waiting takes.
    /// `recover` is what `agent`, stepped through the log, asked for that
    /// the log does not show; `None` for a new session.
    async fn run(
        &self,
        mailbox: &rt::channel::Receiver<Msg>,
        kind: agent::Kind,
        mut agent: Box<dyn Agent>,
        recover: Option<VecDeque<Action>>,
    ) {
        let mut ok = match recover {
            Some(asked) => Box::pin(self.work(Box::pin(self.recover(agent.as_mut(), asked)))).await,
            None => true,
        };
        loop {
            if !ok {
                agent = kind.agent();
                replay(agent.as_mut(), self.frames.entries());
                let mut states = self.registry().states();
                states.replay(self.frames.entries());
                *lock(&self.states) = states;
            }
            ok = match mailbox.recv().await {
                Some(Msg::Queued) => {
                    let next = lock(&self.waiting).pop_front();
                    let take = Box::pin(async {
                        match next {
                            Some(input) => self.take(agent.as_mut(), input).await,
                            // A turn took it up already, as a steer.
                            None => Ok(()),
                        }
                    });
                    Box::pin(self.work(take)).await
                }
                Some(Msg::Settled { child, outcome }) => {
                    Box::pin(self.fire(agent.as_mut(), child, outcome)).await
                }
                Some(Msg::Stop) | None => return,
            };
        }
    }

    /// Takes up input that was waiting: runs it as a command, or starts a
    /// turn with it.
    async fn take(&self, agent: &mut dyn Agent, input: Waiting) -> io::Result<()> {
        self.resolve();
        let Waiting {
            seq,
            text,
            command,
            started,
            ..
        } = input;
        if let (true, Some((name, args))) = (command, slash(&text)) {
            let call = if started {
                seq
            } else {
                self.append(Body::Command {
                    name: name.to_owned(),
                    args: args.to_owned(),
                })
                .await?
            };
            return self.command(call, name, args).await;
        }
        if !started {
            self.record(Body::User { text: text.clone() });
        }
        let inputs = VecDeque::from([Input::User { text }]);
        self.drive(agent, VecDeque::new(), inputs).await
    }

    /// Runs one queued unit of work, with a busy marker on disk for as long
    /// as anything is in flight, and commits what it recorded. An interrupt
    /// from here on stops it. Returns whether it all went well.
    /// The job is boxed so that every kind of work shares one copy of this.
    async fn work(&self, job: BoxFuture<'_, io::Result<()>>) -> bool {
        let began = crate::stamp();
        self.touch();
        self.running.store(true, Ordering::SeqCst);
        if let Err(e) = self.mark_busy(true).await {
            self.warn(format_args!("busy marker: {e}"));
        }
        let done = match job.await {
            Ok(()) => self.flush().await,
            Err(e) => self.flush().await.and(Err(e)),
        };
        if let Err(e) = &done {
            self.warn(e);
        }
        crate::session_event("step", self.id, || {
            let elapsed = began.map_or(Duration::ZERO, |t| t.elapsed());
            let how = if done.is_ok() { "done" } else { "failed" };
            format!("{how} in {elapsed:?}")
        });
        {
            // Under the lock an interrupt takes: it stops this unit, or
            // finds nothing running and stops nothing, never the next.
            let mut cancel = lock(&self.cancel);
            *cancel = Cancel::default();
            self.running.store(false, Ordering::SeqCst);
        }
        let settled = self.queued.fetch_sub(1, Ordering::SeqCst) == 1;
        // Stopped while it waits for approval, it stays marked: the next
        // daemon loads it at once and asks again.
        let stopped = lock(&self.answers).stopped;
        let asking = stopped && !self.lock().ledger.inbox().is_empty();
        if settled
            && !asking
            && let Err(e) = self.mark_busy(false).await
        {
            self.warn(format_args!("busy marker: {e}"));
        }
        self.touch();
        if settled {
            self.report().await;
        }
        done.is_ok()
    }

    async fn mark_busy(&self, busy: bool) -> io::Result<()> {
        let (store, id) = (Arc::clone(&self.shared.store), self.id);
        rt::unblock(&*self.shared.rt, move || store.mark_busy(id, busy)).await
    }

    /// Runs `actions`, then steps the agent with `inputs` and every input
    /// the effects produce, in order, each step on the registry generation
    /// newest when it began. Once an agent that was stepped stops, the
    /// `on_stop` hooks may hand it more to do. An interrupt stops it all.
    async fn drive(
        &self,
        agent: &mut dyn Agent,
        mut actions: VecDeque<Action>,
        mut inputs: VecDeque<Input>,
    ) -> io::Result<()> {
        let mut stepped = false;
        loop {
            if self.turn().is_cancelled() {
                return self.halt(agent, actions, inputs).await;
            }
            if let Some(action) = actions.pop_front() {
                inputs.extend(self.act(action).await?);
                continue;
            }
            if let Some(input) = inputs.pop_front() {
                self.resolve();
                actions.extend(agent.step(input));
                stepped = true;
                continue;
            }
            if !std::mem::take(&mut stepped) {
                return Ok(());
            }
            self.flush().await?;
            let mut decided = Decided::new();
            let next = self.registry().hooks().on_stop(&*self.frames, &mut decided);
            self.decide(decided).await?;
            match next {
                Some(text) => inputs.push_back(Input::User { text }),
                None => return Ok(()),
            }
        }
    }

    /// Ends an interrupted turn. The agent still hears what happened, as a
    /// replay of the log would tell it, but what it asks for is dropped: a
    /// tool call is logged as interrupted without running, so every call
    /// the model made is answered, and the rest is not done.
    async fn halt(
        &self,
        agent: &mut dyn Agent,
        mut actions: VecDeque<Action>,
        inputs: VecDeque<Input>,
    ) -> io::Result<()> {
        for input in inputs {
            actions.extend(agent.step(input));
        }
        if actions.is_empty() {
            return Ok(());
        }
        let registry = self.registry();
        for action in actions {
            if let Action::RunTool {
                tool, input, id, ..
            } = action
            {
                let call = self.record(Body::ToolCall {
                    card: card(&registry, &tool, &input),
                    tool,
                    input: input.into(),
                    id,
                });
                let outcome = Outcome::Interrupted { output: None };
                self.record(Body::ToolResult {
                    call,
                    outcome: outcome.clone(),
                    card: None,
                });
                drop(agent.step(Input::ToolDone { call, outcome }));
            }
        }
        self.record(Body::Interrupt);
        self.flush().await
    }

    /// Carries out one action; an effect returns the input reporting its
    /// outcome.
    async fn act(&self, action: Action) -> io::Result<Option<Input>> {
        let (call, seq, outcome) = match action {
            Action::Emit(body) => {
                self.record(body);
                return Ok(None);
            }
            Action::Spawn { input } => {
                self.spawn(input).await?;
                return Ok(None);
            }
            Action::CallModel => {
                self.steer_in();
                self.note_context();
                let model = self.model();
                let name = model
                    .as_ref()
                    .map_or_else(|_| "none".to_owned(), Model::name);
                let seq = self.append(Body::ModelCall { model: name }).await?;
                (Call::Model, seq, self.run_model(seq, model).await)
            }
            Action::RunTool {
                tool,
                mut input,
                id,
                refuse,
            } => {
                let registry = self.registry();
                let gate = if let Some(error) = refuse {
                    Err(error)
                } else {
                    let mut decided = Decided::new();
                    let verdict = registry.hooks().gate(&tool, &mut input, &mut decided);
                    self.decide(decided).await?;
                    verdict
                };
                let seq = self
                    .append(Body::ToolCall {
                        card: card(&registry, &tool, &input),
                        tool: tool.clone(),
                        input: Json::new(&input),
                        id,
                    })
                    .await?;
                let outcome = match gate {
                    Ok(Gate::Run) => self.run_tool(seq, &tool, &input).await,
                    Ok(Gate::Ask) => {
                        self.append(Body::Ask { call: seq }).await?;
                        self.ask(seq, &tool, &input).await?
                    }
                    Err(error) => Outcome::Failed { error },
                };
                (Call::Tool { tool, input }, seq, outcome)
            }
        };
        self.finish(&call, seq, outcome).await.map(Some)
    }

    /// Waits for the user's answer to the ask for call `seq`, logs it, and
    /// runs the call if approved. An interrupt answers it: the call is
    /// interrupted without running, whatever the user says.
    async fn ask(&self, seq: u64, tool: &str, input: &Value) -> io::Result<Outcome> {
        self.flush().await?;
        let turn = self.turn();
        let (hook, approved) = match self.answer_to(seq, &turn).await? {
            Some(approved) => ("user", approved),
            None => ("interrupt", false),
        };
        self.append(Body::Hook {
            hook: hook.to_owned(),
            decision: Decision::Approval {
                tool: tool.to_owned(),
                approved,
                call: Some(seq),
            },
        })
        .await?;
        Ok(if turn.is_cancelled() {
            Outcome::Interrupted { output: None }
        } else if approved {
            self.run_tool(seq, tool, input).await
        } else {
            not_approved()
        })
    }

    /// Waits until call `call` is answered, or `None` once `turn` is
    /// interrupted first. Waiting is not work: the session may hibernate,
    /// and the daemon exit, with the ask in its inbox; recovery asks again.
    /// A stop while waiting is an `Interrupted` error.
    async fn answer_to(&self, call: u64, turn: &Cancel) -> io::Result<Option<bool>> {
        self.queued.fetch_sub(1, Ordering::SeqCst);
        self.touch();
        let mut interrupted = pin!(turn.cancelled());
        let answer = poll_fn(|cx| {
            let mut answers = lock(&self.answers);
            if let Some(approved) = answers.given.remove(&call) {
                return Poll::Ready(Some(Some(approved)));
            }
            // A stop interrupts the turn too: it is checked first.
            if answers.stopped {
                return Poll::Ready(None);
            }
            if interrupted.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Some(None));
            }
            answers.waiting = Some(cx.waker().clone());
            Poll::Pending
        })
        .await;
        self.queued.fetch_add(1, Ordering::SeqCst);
        answer.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Interrupted,
                format!("stopped while call {call} waits for approval"),
            )
        })
    }

    async fn run_call(&self, call: &Call, seq: u64) -> Outcome {
        match call {
            Call::Model => self.run_model(seq, self.model()).await,
            Call::Tool { tool, input } => self.run_tool(seq, tool, input).await,
        }
    }

    /// Commits the outcome of call `seq`, as the `post_tool` hooks leave a
    /// tool's; returns the input that reports it to the agent.
    async fn finish(&self, call: &Call, seq: u64, mut outcome: Outcome) -> io::Result<Input> {
        let (body, input) = match call {
            Call::Model => (
                Body::ModelResult {
                    call: seq,
                    outcome: outcome.clone(),
                },
                Input::ModelDone { call: seq, outcome },
            ),
            Call::Tool { tool, input } => {
                let mut decided = Decided::new();
                let registry = self.registry();
                let hooks = registry.hooks();
                hooks.post_tool(seq, (tool, input), &mut outcome, &mut decided);
                self.decide(decided).await?;
                let card = match &outcome {
                    Outcome::Done { output } => registry
                        .tools()
                        .get(tool)
                        .and_then(|t| t.present_result(input, output)),
                    _ => None,
                };
                (
                    Body::ToolResult {
                        call: seq,
                        outcome: outcome.clone(),
                        card,
                    },
                    Input::ToolDone { call: seq, outcome },
                )
            }
        };
        self.record(body);
        Ok(input)
    }

    /// Runs tool call `call`, unless the turn was interrupted already, as
    /// it may be while the call's gate and intent commit.
    async fn run_tool(&self, call: u64, tool: &str, input: &Value) -> Outcome {
        if self.turn().is_cancelled() {
            return Outcome::Interrupted { output: None };
        }
        let registry = self.registry();
        let route = registry.tools().get(tool).map(DynTool::route);
        if route.is_some_and(|route| route != Route::Local) {
            return Outcome::Failed {
                error: format!("{tool} runs in a client, and this daemon routes none yet"),
            };
        }
        let cx = self.cx(call, &registry);
        let began = crate::stamp();
        let outcome = contain(registry.tools().run(tool, input, &cx)).await;
        crate::session_event("effect", self.id, || {
            let elapsed = began.map_or(Duration::ZERO, |t| t.elapsed());
            let how = match &outcome {
                Outcome::Done { .. } => "done",
                Outcome::Failed { .. } => "failed",
                Outcome::Interrupted { .. } => "interrupted",
            };
            format!("{tool} call {call}: {how} in {elapsed:?}")
        });
        outcome
    }

    /// Runs slash command `/name args`, logged as call `call`: no turn for
    /// the agent.
    async fn command(&self, call: u64, name: &str, args: &str) -> io::Result<()> {
        let registry = self.registry();
        let outcome = match registry.command(name) {
            Some(command) => {
                let cx = self.cx(call, &registry);
                contain(command.run(args, &cx)).await
            }
            None if name == MODEL => self.set_model(args),
            None => Outcome::Failed {
                error: format!("no command /{name}"),
            },
        };
        self.record(Body::CommandResult { call, outcome });
        Ok(())
    }

    /// `/model`: reports the model the session calls and the layer that
    /// names it; `/model <host:model>` sets it for this session, over every
    /// config layer, and `/model reset` leaves it to them again.
    fn set_model(&self, args: &str) -> Outcome {
        let set = if args.is_empty() {
            self.overrides()
        } else {
            let model = (args != "reset").then(|| args.to_owned());
            let set = Overrides { model };
            let seq = self.record(Body::Settings(set.clone()));
            (set, Some(seq))
        };
        match self.shared.models.settings.model_in(&self.workspace, set) {
            Ok((model, from)) => Outcome::Done {
                output: json!({ "model": model, "from": from.to_string() }).into(),
            },
            Err(error) => Outcome::Failed { error },
        }
    }

    /// The session's own settings, and the seq of the entry that set them.
    fn overrides(&self) -> (Overrides, Option<u64>) {
        self.lock().ledger.settings()
    }

    /// The model spec the settings name now.
    fn model_spec(&self) -> String {
        let set = self.overrides();
        self.shared
            .models
            .settings
            .model_in(&self.workspace, set)
            .map_or_else(|_| "none".to_owned(), |(model, _)| model)
    }

    /// What call `call` runs with, on `registry`. An interrupt of the turn
    /// stops it.
    fn cx(&self, call: u64, registry: &Arc<Registry>) -> Cx {
        let session = self
            .this
            .upgrade()
            .expect("a session that runs a call is alive");
        Cx {
            workspace: Arc::from(self.workspace.as_path()),
            env: lock(&self.env).clone().map(Arc::new),
            session: self.id,
            call,
            model: self.model_spec().into(),
            spill: Arc::from(self.shared.spill.join(self.id.to_string())),
            cancel: self.turn(),
            rt: Arc::clone(&self.shared.rt),
            host: Arc::new(Effect {
                session,
                call,
                registry: Arc::clone(registry),
            }),
        }
    }

    /// What stops the running turn.
    fn turn(&self) -> Cancel {
        lock(&self.cancel).clone()
    }

    /// The registry generation the current step runs on.
    fn registry(&self) -> Arc<Registry> {
        Arc::clone(&lock(&self.step))
    }

    /// Moves to the newest registry generation, at a step boundary,
    /// logging the extensions it runs when they changed.
    fn resolve(&self) {
        let newest = self.shared.registry.get();
        let mut step = lock(&self.step);
        if Arc::ptr_eq(&step, &newest) {
            return;
        }
        let changed = !step.extensions().eq(newest.extensions());
        *step = newest;
        if changed {
            self.record(Body::Extensions {
                installed: installed(&step),
            });
        }
    }

    /// Gives the running turn the steering input waiting, before the model
    /// is next asked.
    fn steer_in(&self) {
        lock(&self.waiting).retain(|w| {
            if !w.steer || w.started {
                return true;
            }
            self.record(Body::Steer {
                text: w.text.clone(),
                queued: w.seq,
            });
            false
        });
    }

    /// The directory the session's commands run in now.
    fn dir(&self) -> PathBuf {
        let env = lock(&self.env);
        agent::session_dir(&self.workspace, env.as_ref(), &lock(&self.states))
    }

    /// Logs the context the extensions give, before the session's first
    /// model call.
    fn note_context(&self) {
        if self.lock().ledger.has_context() {
            return;
        }
        let dir = self.dir();
        let facts = Facts {
            workspace: &self.workspace,
            dir: &dir,
        };
        if let Some(text) = self.registry().context(&facts) {
            self.record(Body::Context { text });
        }
    }

    /// The model as the settings and the newest client's environment name
    /// it now, or why there is none (shown as the failed model call).
    fn model(&self) -> Result<Model, String> {
        let set = self.overrides();
        let env = lock(&self.env);
        self.shared.models.model(&self.workspace, env.as_ref(), set)
    }

    /// Asks `model` to reply to the transcript before `call`, offering the
    /// tools. Text streams to clients as it arrives; only the finished reply
    /// is committed, or on an interrupt the text so far.
    async fn run_model(&self, call: u64, model: Result<Model, String>) -> Outcome {
        let model = match model {
            Ok(model) => model,
            Err(error) => return Outcome::Failed { error },
        };
        let registry = self.registry();
        let mut request = self.request(call, &model, &registry);
        let mut decided = Decided::new();
        registry
            .hooks()
            .prepare_step(call, &mut request, &mut decided);
        if let Err(e) = self.decide(decided).await {
            return Outcome::Failed {
                error: e.to_string(),
            };
        }
        let mut said = String::new();
        let mut on_text = |text: &str| {
            said.push_str(text);
            self.broadcast(&DaemonMsg::Delta {
                call,
                text: text.to_owned(),
            });
        };
        let replied = self.reply(call, &model, &registry, request, &mut on_text);
        match contain(replied).await {
            Outcome::Interrupted { .. } => Outcome::Interrupted {
                output: (!said.is_empty()).then_some(said),
            },
            outcome => outcome,
        }
    }

    /// The request for model call `call`: the system prompt, the transcript
    /// before it, and the tools.
    fn request(&self, call: u64, model: &Model, registry: &Arc<Registry>) -> llm::Request {
        let mut history = self.history(call, registry);
        let system = self
            .shared
            .system
            .clone()
            .unwrap_or_else(|| registry.system_prompt().to_owned());
        if !system.is_empty() {
            history.insert(0, llm::Message::system(system));
        }
        model.request(history, registry.specs().to_vec())
    }

    /// The conversation before model call `call`, as the tools of
    /// `registry` present results: the transcript kept as entries commit,
    /// made again only for other tools, or for a call that recovery runs
    /// after entries it does not see.
    fn history(&self, call: u64, registry: &Arc<Registry>) -> Vec<llm::Message> {
        let mut st = self.lock();
        st.transcript = st.transcript.take().filter(|t| t.is_on(registry));
        let transcript = st
            .transcript
            .get_or_insert_with(|| Transcript::of(self.frames.entries(), Arc::clone(registry)));
        let history = if let Some(messages) = transcript.before(call) {
            messages.to_vec()
        } else {
            let before = self.frames.entries().take_while(|e| e.seq < call);
            Transcript::of(before, Arc::clone(registry)).into_messages()
        };
        #[cfg(test)]
        assert_eq!(
            history,
            oracle::transcript(
                &self.frames.entries().collect::<Vec<_>>(),
                call,
                registry.tools()
            ),
            "the transcript kept"
        );
        history
    }

    /// `model`'s reply to `request`, retried as the `request_error` hooks
    /// ask. The whole reply is the fact: replaying it later sends the
    /// provider's own items (signatures, encrypted reasoning) back.
    async fn reply(
        &self,
        call: u64,
        model: &Model,
        registry: &Registry,
        mut request: llm::Request,
        on_text: &mut (dyn FnMut(&str) + Send),
    ) -> Outcome {
        let cancel = self.turn();
        let mut retries = 0;
        loop {
            let Some(reply) = unless(&cancel, model.reply(&request, on_text)).await else {
                return Outcome::Interrupted { output: None };
            };
            let error = match reply {
                Ok(reply) => {
                    return match serde_json::to_value(&reply) {
                        Ok(output) => {
                            *lock(&self.replied) = Some((call, reply.message));
                            Outcome::Done {
                                output: output.into(),
                            }
                        }
                        Err(error) => Outcome::Failed {
                            error: error.to_string(),
                        },
                    };
                }
                Err(error) => error.to_string(),
            };
            let mut decided = Decided::new();
            // An interrupted turn asks for no retry.
            let retry = (retries < MAX_RETRIES && !cancel.is_cancelled())
                .then(|| {
                    registry.hooks().request_error(
                        (call, retries + 1),
                        &request,
                        &error,
                        &mut decided,
                    )
                })
                .flatten();
            match retry {
                Some(next) if self.decide(decided).await.is_ok() => {
                    request = next;
                    retries += 1;
                }
                _ => return Outcome::Failed { error },
            }
        }
    }

    /// Picks up where the log leaves off. Links are re-armed; the agent is
    /// rebuilt from the log; calls with an intent but no outcome are
    /// finished (leftover processes killed, safe calls run again, the rest
    /// marked interrupted with the output they committed, asks still
    /// waiting asked again); then whatever the agent asked for that the log
    /// does not show runs, and the input still queued is taken up. A child
    /// whose owner is gone is an orphan: nothing of its turn runs again.
    async fn recover(&self, agent: &mut dyn Agent, actions: VecDeque<Action>) -> io::Result<()> {
        self.relink().await?;
        let (dangling, commands, jobs) = {
            let st = self.lock();
            let boot = sys::boot_id();
            (
                dangling_calls(&st.ledger),
                st.ledger.open_commands().collect::<Vec<_>>(),
                st.ledger
                    .background()
                    .iter()
                    .filter(|(_, b)| Some(b.as_str()) == boot)
                    .map(|&(pgid, _)| pgid)
                    .collect::<Vec<_>>(),
            )
        };
        let orphan = self.orphaned();
        let registry = self.registry();
        self.requeue(&registry);
        // Owned by the session under a daemon that died without ending it:
        // ended now. Later loads in this daemon find them ended by `stop`.
        if lock(&self.shared.loaded).insert(self.id) {
            for pgid in jobs.into_iter().filter(|&pgid| sys::group_alive(pgid)) {
                self.kill(pgid);
            }
        }
        // A command cut off is not run again.
        for call in commands {
            self.record(Body::CommandResult {
                call,
                outcome: Outcome::Interrupted { output: None },
            });
        }
        let mut inputs = VecDeque::new();
        for dangling in dangling {
            inputs.push_back(self.finish_dangling(dangling, orphan, &registry).await?);
        }
        if orphan {
            self.warn("its owner is gone: what it was doing is not resumed");
            lock(&self.cancel).cancel();
        }
        self.drive(agent, actions, inputs).await
    }

    /// Queues again the input the log shows queued and not taken up, ahead
    /// of what was taken in since the session loaded. That input is in the
    /// log as well, and waiting already: taking input in holds `waiting`
    /// until its entry commits, so under that lock the two agree.
    fn requeue(&self, registry: &Registry) {
        let mut waiting = lock(&self.waiting);
        let queued: Vec<_> = self
            .lock()
            .ledger
            .untaken()
            .iter()
            .filter(|q| waiting.iter().all(|w| w.seq != q.seq))
            .cloned()
            .collect();
        let count = queued.len();
        for ledger::Queued { seq, text, steer } in queued.into_iter().rev() {
            waiting.push_front(Waiting {
                command: as_command(registry, &text).is_some(),
                seq,
                text,
                steer,
                started: false,
            });
        }
        drop(waiting);
        self.queued.fetch_add(count, Ordering::SeqCst);
        for _ in 0..count {
            let _ = self.mailbox.send(Msg::Queued);
        }
    }

    /// Commits an outcome for a call a crash left without one, after
    /// killing the processes it left running.
    async fn finish_dangling(
        &self,
        dangling: Dangling,
        orphan: bool,
        registry: &Registry,
    ) -> io::Result<Input> {
        let Dangling {
            seq,
            call,
            processes,
            phase,
        } = dangling;
        for (pgid, boot) in processes {
            if Some(boot.as_str()) == sys::boot_id() {
                self.kill(pgid);
            }
        }
        let interrupted = || Outcome::Interrupted {
            output: self
                .lock()
                .ledger
                .call(seq)
                .and_then(|open| open.progress.clone()),
        };
        let outcome = match &call {
            _ if orphan => interrupted(),
            Call::Tool { tool, input } if phase == Phase::Asked => {
                self.ask(seq, tool, input).await?
            }
            Call::Tool { .. } if phase == (Phase::Answered { approved: false }) => not_approved(),
            Call::Tool { tool, .. }
                if registry.tools().get(tool).map(DynTool::replay) != Some(Replay::Safe) =>
            {
                interrupted()
            }
            _ => {
                self.flush().await?;
                self.run_call(&call, seq).await
            }
        };
        self.finish(&call, seq, outcome).await
    }

    /// Whether this is a child whose owner is gone.
    fn orphaned(&self) -> bool {
        let daemon = self.shared.daemon.upgrade();
        self.parent
            .zip(daemon)
            .is_some_and(|(parent, daemon)| !daemon.knows(parent.session))
    }
}

/// What loading a session keeps of its log, folded an entry at a time as
/// it is read, so the entries themselves are not kept.
struct Load {
    ledger: Ledger,
    states: States,
    replay: Replayer,
    /// The agent, stepped through the log.
    agent: Box<dyn Agent>,
}

impl Load {
    fn fold(&mut self, e: &Entry) {
        self.ledger.apply(e);
        self.states.apply(e);
        self.replay.apply(self.agent.as_mut(), e);
    }
}

/// The native command that sets the session's model.
const MODEL: &str = "model";

/// `text` as a command's name and arguments, if it names one: an
/// extension's, or a native one.
fn as_command<'a>(registry: &Registry, text: &'a str) -> Option<(&'a str, &'a str)> {
    slash(text).filter(|&(name, _)| registry.command(name).is_some() || name == MODEL)
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

/// Model calls one request may be retried, as `request_error` hooks ask.
const MAX_RETRIES: u32 = 2;

/// The engine as tool or command call `call` reaches it, on the registry
/// generation it started with.
struct Effect {
    session: Arc<Session>,
    call: u64,
    registry: Arc<Registry>,
}

/// Entries a call records are not waited for: they still commit before
/// its result.
impl Host for Effect {
    fn spawned(&self, pgid: u32, background: bool) {
        if background {
            lock(&self.session.jobs).push(pgid);
        }
        let Some(boot) = sys::boot_id() else { return };
        self.session.record(Body::ToolProcess {
            call: self.call,
            pgid,
            boot: boot.to_owned(),
            background,
        });
        self.session.kick();
    }

    fn progress(&self, text: &str) {
        self.session.record(Body::Progress {
            call: self.call,
            text: text.to_owned(),
        });
        self.session.kick();
    }

    fn note(&self, ext: &'static str, state: &'static str, data: Value) {
        lock(&self.session.states).fold(ext, state, &data);
        self.session.record(Body::Ext {
            ext: ext.to_owned(),
            state: state.to_owned(),
            data: data.into(),
        });
        self.session.kick();
    }

    fn states(&self) -> MutexGuard<'_, States> {
        lock(&self.session.states)
    }

    fn hooks(&self) -> &Hooks {
        self.registry.hooks()
    }
}

/// How clients show a call of `tool` with `input`.
fn card(registry: &Registry, tool: &str, input: &Value) -> Option<Card> {
    registry.tools().get(tool)?.present(input)
}

/// `/name args` as a command's name and arguments.
fn slash(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    Some((name, args.trim()))
}

/// The entry logging the registry's extensions before the session's first
/// step, unless they are the ones its log records last.
fn note_extensions(header: &Header, ledger: &Ledger, registry: &Registry) -> Option<Body> {
    let recorded = ledger.extensions().unwrap_or(&header.extensions);
    let same = recorded
        .iter()
        .map(|i| (i.name.as_str(), i.version))
        .eq(registry
            .extensions()
            .map(|(name, version)| (name as &str, version)));
    (!same).then(|| Body::Extensions {
        installed: installed(registry),
    })
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

/// Runs an effect so that a panic inside it fails the call instead of taking
/// the session actor down.
async fn contain(effect: impl Future<Output = Outcome>) -> Outcome {
    let mut effect = pin!(effect);
    poll_fn(|cx| {
        catch_unwind(AssertUnwindSafe(|| effect.as_mut().poll(cx))).unwrap_or_else(|panic| {
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            Poll::Ready(Outcome::Failed {
                error: format!("panicked: {message}"),
            })
        })
    })
    .await
}

fn not_approved() -> Outcome {
    Outcome::Failed {
        error: "the user did not approve this call".to_owned(),
    }
}

/// The effect an agent asked for, between its committed intent and outcome.
enum Call {
    Model,
    Tool { tool: String, input: Value },
}

/// A call with an intent but no outcome, the process groups it started, and
/// where it stands.
struct Dangling {
    seq: u64,
    call: Call,
    processes: Vec<(u32, String)>,
    phase: Phase,
}

/// The calls a crash left with an intent and no outcome, in order.
fn dangling_calls(ledger: &Ledger) -> Vec<Dangling> {
    ledger
        .open_calls()
        .map(|(seq, open)| Dangling {
            seq,
            call: match &open.call {
                ledger::Call::Model => Call::Model,
                ledger::Call::Tool { tool, input } => Call::Tool {
                    tool: tool.clone(),
                    input: input.value(),
                },
            },
            processes: open.processes.clone(),
            phase: open.phase,
        })
        .collect()
}
