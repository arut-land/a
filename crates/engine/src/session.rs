use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Display;
use std::future::{Future, poll_fn};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::pin::pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

use agent::{
    Action, Agent, Cancel, Cx, Decided, DynTool, Facts, Gate, Hooks, Host, Input, Replay, States,
};
use protocol::view::SessionView;
use protocol::{
    Activity, Body, Cap, DaemonMsg, Decision, Entry, Environment, Frame, Installed, Outcome,
    Residency, SessionId, SessionSummary,
};
use serde_json::Value;
use store::{Header, Loaded, Log, LogStore, Parent};

use crate::model::{Model, Models};
use crate::outbox::Outbox;
use crate::{Daemon, lock};

mod facts;
mod links;
#[cfg(test)]
mod tests;

use facts::{ask, inbox, recap, replay, transcript};

/// What every session runs with.
pub struct Shared {
    /// Where session actors run as tasks, and effects sleep and block.
    pub rt: rt::Rt,
    pub store: Arc<dyn LogStore>,
    /// The extensions' tools and hooks.
    pub registry: agent::Registry,
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
    state: Mutex<State>,
    /// The appender; only the job draining `queue` writes to it.
    log: Mutex<Box<dyn Log>>,
    /// Entries waiting to be committed, in order.
    queue: Mutex<Queue>,
    /// The actor's messages. The session's inbox, what waits for the user,
    /// is in its log: see [`facts::inbox`].
    mailbox: rt::channel::Sender<Msg>,
    answers: Mutex<Answers>,
    /// Inputs submitted but not yet fully handled. They pin the daemon and
    /// keep the session loaded, as do watches not yet fired.
    queued: AtomicUsize,
    /// Open client attachments; a session with any stays loaded.
    attached: AtomicUsize,
    last_active: Mutex<Instant>,
    /// What commands run in: the last client's capture, secrets included,
    /// or after a restart the last one logged, without them.
    env: Mutex<Option<Environment>>,
    /// The extensions' state, folded from the log.
    states: Mutex<States>,
    /// Stops the call running now.
    cancel: Mutex<Cancel>,
    /// Background process groups the session owns, until it ends.
    jobs: Mutex<Vec<u32>>,
    shared: Arc<Shared>,
}

enum Msg {
    /// The user's input, and what the `input` hooks decided about it.
    Input {
        text: String,
        decided: Decided,
    },
    Command {
        name: String,
        args: String,
        decided: Decided,
    },
    /// Child `child` settled with `outcome`.
    Settled {
        child: SessionId,
        outcome: Outcome,
    },
    Stop,
}

struct State {
    entries: Vec<Entry>,
    clients: Vec<Outbox>,
}

/// How many entries behind a client must be to get a snapshot rather than
/// a replay, when it takes them.
const SNAPSHOT_GAP: usize = 256;

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
    pub fn start(header: Header, loaded: Loaded, shared: Arc<Shared>) -> Arc<Self> {
        Self::start_in(header, loaded, shared, None)
    }

    /// [`Session::start`], running commands in `env` rather than the last
    /// one logged: a child runs in its parent's, secrets included.
    fn start_in(
        header: Header,
        mut loaded: Loaded,
        shared: Arc<Shared>,
        env: Option<Environment>,
    ) -> Arc<Self> {
        if let Err(e) = note_extensions(&header, &mut loaded, &shared.registry) {
            eprintln!("session {}: recording extensions: {e}", header.id.short());
        }
        let (mailbox, rx) = rt::channel::unbounded();
        let armed = links::armed(&loaded.entries).count();
        let env = env.or_else(|| logged_env(&loaded.entries).cloned());
        let seq = loaded.entries.len() as u64;
        let mut states = shared.registry.states();
        states.replay(&loaded.entries);
        let session = Arc::new_cyclic(|this| Self {
            this: this.clone(),
            id: header.id,
            workspace: header.workspace,
            parent: header.parent,
            state: Mutex::new(State {
                entries: loaded.entries,
                clients: Vec::new(),
            }),
            log: Mutex::new(loaded.log),
            queue: Mutex::new(Queue {
                next: seq,
                ..Queue::default()
            }),
            mailbox,
            answers: Mutex::default(),
            // Recovery counts as queued work until it has run, and every
            // armed watch until it fires.
            queued: AtomicUsize::new(1 + armed),
            attached: AtomicUsize::new(0),
            last_active: Mutex::new(Instant::now()),
            env: Mutex::new(env),
            states: Mutex::new(states),
            cancel: Mutex::default(),
            jobs: Mutex::default(),
            shared,
        });
        session.shared.registry.hooks().session_start(session.id);
        let actor = Arc::clone(&session);
        session
            .shared
            .rt
            .spawn(Box::pin(async move { actor.run(&rx).await }));
        session
    }

    /// Opens a client attachment. Callers hold the session's slot lock, so a
    /// session cannot be unloaded between being found and being attached.
    pub fn open(self: &Arc<Self>) -> Attachment {
        self.attached.fetch_add(1, Ordering::SeqCst);
        self.touch();
        Attachment(Arc::clone(self))
    }

    /// Queues the user's input as the `input` hooks leave it: `/name args`
    /// runs command `name` if there is one, anything else goes to the
    /// agent. `Err` is why a hook refused it.
    pub fn submit(&self, mut text: String) -> Result<(), String> {
        let registry = &self.shared.registry;
        let mut decided = Decided::new();
        registry.hooks().input(&mut text, &mut decided)?;
        let msg = match slash(&text).filter(|(name, _)| registry.command(name).is_some()) {
            Some((name, args)) => Msg::Command {
                name: name.to_owned(),
                args: args.to_owned(),
                decided,
            },
            None => Msg::Input { text, decided },
        };
        self.queued.fetch_add(1, Ordering::SeqCst);
        let _ = self.mailbox.send(msg);
        Ok(())
    }

    /// Stops the call running now, if any.
    pub fn interrupt(&self) {
        lock(&self.cancel).cancel();
    }

    /// Answers the ask for tool call `call`; the actor logs the answer and
    /// runs or refuses the call. The first answer wins.
    pub fn answer(&self, call: u64, approved: bool) -> Result<(), String> {
        if !inbox(&self.lock().entries).contains(&call) {
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

    /// Whether any background process the session owns still runs.
    pub fn has_jobs(&self) -> bool {
        let mut jobs = lock(&self.jobs);
        jobs.retain(|&pgid| sys::group_alive(pgid));
        !jobs.is_empty()
    }

    /// How long the session has had no attachments, no work and no jobs.
    pub fn idle_for(&self) -> Duration {
        if self.attached.load(Ordering::SeqCst) > 0 || self.has_work() || self.has_jobs() {
            return Duration::ZERO;
        }
        lock(&self.last_active).elapsed()
    }

    /// Stops the actor, the call it runs and the processes it owns; the
    /// session lives on in its log. Callers check `idle_for` under the slot
    /// lock first, or are draining the daemon.
    pub fn stop(&self) {
        self.interrupt();
        let mut answers = lock(&self.answers);
        answers.stopped = true;
        answers.wake();
        drop(answers);
        let _ = self.mailbox.send(Msg::Stop);
        for pgid in lock(&self.jobs).drain(..) {
            if let Err(e) = sys::kill_process_group(pgid) {
                self.warn(format_args!("killing background group {pgid}: {e}"));
            }
        }
        self.shared.registry.hooks().session_end(self.id);
    }

    /// Attaches `client`, which has the entries up to `since`: it is sent
    /// what it missed, then everything live.
    pub fn attach(&self, client: &Outbox, since: u64) {
        let mut st = self.lock();
        let frames = self.catch_up_frames(&st.entries, client, since, true);
        let follower = client.follow(self.this.clone(), since, frames);
        st.clients.push(follower);
    }

    /// Catches up `client`, which fell behind after entry `since` while it
    /// followed this session as its `following`th.
    pub fn catch_up(&self, client: &Outbox, following: u64, since: u64) {
        let st = self.lock();
        let frames = self.catch_up_frames(&st.entries, client, since, false);
        client.caught_up(following, frames);
    }

    /// What a client that has the entries up to `since` is missing: the
    /// attach, a replay (or a snapshot, for one far behind that takes them)
    /// and its end, optionally a recap, and the asks still waiting.
    fn catch_up_frames(
        &self,
        entries: &[Entry],
        client: &Outbox,
        since: u64,
        with_recap: bool,
    ) -> Vec<Frame> {
        let mut frames = Vec::new();
        let mut push = |msg: &DaemonMsg| frames.extend(Frame::new(msg));
        push(&DaemonMsg::Attached {
            session: self.id,
            workspace: self.workspace.clone(),
        });
        let missed = entries.iter().filter(|e| e.seq > since);
        let snapshot = (client.caps().has(Cap::Snapshot) && missed.clone().count() > SNAPSHOT_GAP)
            .then(|| {
                let view = SessionView::from_log(self.id, self.workspace.clone(), entries);
                Frame::new(&DaemonMsg::Snapshot(Box::new(view)))
            });
        match snapshot {
            // One too large for a frame is replayed instead.
            Some(Ok(frame)) => frames.push(frame),
            _ => frames.extend(missed.filter_map(|e| Frame::entry(e).ok())),
        }
        let mut push = |msg: &DaemonMsg| frames.extend(Frame::new(msg));
        let seq = entries.last().map_or(0, |e| e.seq);
        push(&DaemonMsg::Synced { seq });
        if with_recap && let Some(recap) = recap(entries, since) {
            push(&DaemonMsg::Recap(recap));
        }
        for call in inbox(entries) {
            if let Some(msg) = ask(entries, call) {
                push(&msg);
            }
        }
        frames
    }

    /// Runs commands from now on in `env`, the newest client's. Its redacted
    /// form is logged when that differs from the last one logged, so
    /// re-attaching from the same shell adds nothing.
    pub fn set_env(&self, env: Environment) -> io::Result<()> {
        let logged = env.redacted();
        *lock(&self.env) = Some(env);
        if logged_env(&self.lock().entries) == Some(&logged) {
            return Ok(());
        }
        rt::block_on(self.append(Body::Environment(logged))).map(drop)
    }

    pub fn summary(&self) -> SessionSummary {
        let st = self.lock();
        let activity = if !inbox(&st.entries).is_empty() {
            Activity::Approval
        } else if self.has_work() {
            Activity::Running
        } else {
            Activity::Idle
        };
        SessionSummary {
            id: self.id,
            workspace: self.workspace.clone(),
            entries: st.entries.len() as u64,
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
        queue.pending.push(Entry {
            seq,
            epoch: 0,
            body,
        });
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
            let committed = self.lock().entries.len() as u64;
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
        let mut st = self.lock();
        let hooks = self.shared.registry.hooks();
        for entry in batch.drain(..) {
            debug_assert_eq!(entry.seq, st.entries.len() as u64 + 1);
            // An entry too large for a frame ends every client's view
            // rather than leaving a gap in it.
            match Frame::entry(&entry) {
                Ok(frame) => st.clients.retain(|c| c.send(frame.clone()).is_ok()),
                Err(_) => st.clients.clear(),
            }
            hooks.observe(&entry);
            let asked = match entry.body {
                Body::Ask { call } => Some(call),
                _ => None,
            };
            st.entries.push(entry);
            // Under the same lock as the entry, so a client attaching
            // meanwhile gets the ask exactly once.
            let msg = asked.and_then(|call| ask(&st.entries, call));
            if let Some(Ok(frame)) = msg.as_ref().map(Frame::new) {
                st.clients.retain(|c| c.send(frame.clone()).is_ok());
            }
        }
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
        let decided: Vec<Body> = decided
            .into_iter()
            .map(|(hook, decision)| Body::Hook {
                hook: hook.to_owned(),
                decision,
            })
            .collect();
        {
            let st = self.lock();
            // What a restart logs before recovery runs is not part of it.
            let logged: Vec<&Body> = st
                .entries
                .iter()
                .rev()
                .map(|e| &e.body)
                .filter(|b| !matches!(b, Body::Extensions { .. } | Body::Environment(_)))
                .take_while(|b| matches!(b, Body::Hook { .. }))
                .collect();
            if !logged.is_empty() && logged.into_iter().rev().eq(&decided) {
                return Ok(());
            }
        }
        for body in decided {
            self.record(body);
        }
        self.flush().await
    }

    /// Sends a message that is not part of the log to every attached client.
    fn broadcast(&self, msg: &DaemonMsg) {
        if let Ok(frame) = Frame::new(msg) {
            self.lock()
                .clients
                .retain(|c| c.send(frame.clone()).is_ok());
        }
    }

    /// An agent may step past entries that then fail to commit; after a
    /// failed unit of work it is rebuilt from the log. Each unit's future is
    /// boxed, so an idle session's actor holds only what waiting takes.
    async fn run(&self, mailbox: &rt::channel::Receiver<Msg>) {
        let kind = self.shared.models.settings.agent(&self.workspace);
        let mut agent = kind.agent();
        let mut ok = Box::pin(self.work(self.recover(agent.as_mut()))).await;
        loop {
            if !ok {
                agent = kind.agent();
                let st = self.lock();
                replay(agent.as_mut(), &st.entries);
                let mut states = self.shared.registry.states();
                states.replay(&st.entries);
                *lock(&self.states) = states;
            }
            ok = match mailbox.recv().await {
                Some(Msg::Input { text, decided }) => {
                    Box::pin(self.work(async {
                        self.decide(decided).await?;
                        self.record(Body::User { text: text.clone() });
                        let inputs = VecDeque::from([Input::User { text }]);
                        self.drive(agent.as_mut(), VecDeque::new(), inputs).await
                    }))
                    .await
                }
                Some(Msg::Command {
                    name,
                    args,
                    decided,
                }) => {
                    Box::pin(self.work(async {
                        self.decide(decided).await?;
                        self.command(&name, &args).await
                    }))
                    .await
                }
                Some(Msg::Settled { child, outcome }) => {
                    Box::pin(self.fire(agent.as_mut(), child, outcome)).await
                }
                Some(Msg::Stop) | None => return,
            };
        }
    }

    /// Runs one queued unit of work, with a busy marker on disk for as long
    /// as anything is in flight, and commits what it recorded. Returns
    /// whether it all went well.
    async fn work(&self, job: impl Future<Output = io::Result<()>>) -> bool {
        let began = crate::stamp();
        self.touch();
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
        let settled = self.queued.fetch_sub(1, Ordering::SeqCst) == 1;
        if settled && let Err(e) = self.mark_busy(false).await {
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
    /// the effects produce, in order. Once an agent that was stepped stops,
    /// the `on_stop` hooks may hand it more to do.
    async fn drive(
        &self,
        agent: &mut dyn Agent,
        mut actions: VecDeque<Action>,
        mut inputs: VecDeque<Input>,
    ) -> io::Result<()> {
        let mut stepped = false;
        loop {
            while let Some(action) = actions.pop_front() {
                inputs.extend(self.act(action).await?);
            }
            if let Some(input) = inputs.pop_front() {
                actions.extend(agent.step(input));
                stepped = true;
                continue;
            }
            if !std::mem::take(&mut stepped) {
                return Ok(());
            }
            self.flush().await?;
            let mut decided = Decided::new();
            let next = self
                .shared
                .registry
                .hooks()
                .on_stop(&self.lock().entries, &mut decided);
            self.decide(decided).await?;
            match next {
                Some(text) => inputs.push_back(Input::User { text }),
                None => return Ok(()),
            }
        }
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
                let gate = if let Some(error) = refuse {
                    Err(error)
                } else {
                    let mut decided = Decided::new();
                    let verdict =
                        self.shared
                            .registry
                            .hooks()
                            .gate(&tool, &mut input, &mut decided);
                    self.decide(decided).await?;
                    verdict
                };
                let card = self.shared.registry.tools().get(&tool);
                let seq = self
                    .append(Body::ToolCall {
                        card: card.and_then(|t| t.present(&input)),
                        tool: tool.clone(),
                        input: input.clone(),
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
    /// runs the call if approved.
    async fn ask(&self, seq: u64, tool: &str, input: &Value) -> io::Result<Outcome> {
        self.flush().await?;
        let approved = self.answer_to(seq).await?;
        self.append(Body::Hook {
            hook: "user".to_owned(),
            decision: Decision::Approval {
                tool: tool.to_owned(),
                approved,
                call: Some(seq),
            },
        })
        .await?;
        Ok(if approved {
            self.run_tool(seq, tool, input).await
        } else {
            not_approved()
        })
    }

    /// Waits until call `call` is answered. Waiting is not work: the session
    /// may hibernate, and the daemon exit, with the ask in its inbox;
    /// recovery asks again. A stop while waiting is an `Interrupted` error.
    async fn answer_to(&self, call: u64) -> io::Result<bool> {
        self.queued.fetch_sub(1, Ordering::SeqCst);
        self.touch();
        let answer = poll_fn(|cx| {
            let mut answers = lock(&self.answers);
            if let Some(approved) = answers.given.remove(&call) {
                return Poll::Ready(Some(approved));
            }
            if answers.stopped {
                return Poll::Ready(None);
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
                let hooks = self.shared.registry.hooks();
                hooks.post_tool(seq, (tool, input), &mut outcome, &mut decided);
                self.decide(decided).await?;
                let card = match &outcome {
                    Outcome::Done { output } => self
                        .shared
                        .registry
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

    async fn run_tool(&self, call: u64, tool: &str, input: &Value) -> Outcome {
        let began = crate::stamp();
        let effect = Effect::new(self, call);
        let outcome = contain(self.shared.registry.tools().run(tool, input, &effect.cx())).await;
        crate::session_event("effect", self.id, || {
            let elapsed = began.map_or(Duration::ZERO, |t| t.elapsed());
            let how = match &outcome {
                Outcome::Done { .. } => "done",
                Outcome::Failed { .. } => "failed",
                Outcome::Interrupted => "interrupted",
            };
            format!("{tool} call {call}: {how} in {elapsed:?}")
        });
        outcome
    }

    /// Runs slash command `/name args`: logged, but no turn for the agent.
    async fn command(&self, name: &str, args: &str) -> io::Result<()> {
        let call = self
            .append(Body::Command {
                name: name.to_owned(),
                args: args.to_owned(),
            })
            .await?;
        let outcome = match self.shared.registry.command(name) {
            Some(command) => {
                let effect = Effect::new(self, call);
                contain(command.run(args, &effect.cx())).await
            }
            None => Outcome::Failed {
                error: format!("no command /{name}"),
            },
        };
        self.record(Body::CommandResult { call, outcome });
        Ok(())
    }

    /// The directory the session's commands run in now.
    fn dir(&self) -> PathBuf {
        let env = lock(&self.env);
        agent::session_dir(&self.workspace, env.as_ref(), &lock(&self.states))
    }

    /// Logs the context the extensions give, before the session's first
    /// model call.
    fn note_context(&self) {
        let noted = self
            .lock()
            .entries
            .iter()
            .any(|e| matches!(e.body, Body::Context { .. }));
        if noted {
            return;
        }
        let dir = self.dir();
        let facts = Facts {
            workspace: &self.workspace,
            dir: &dir,
        };
        if let Some(text) = self.shared.registry.context(&facts) {
            self.record(Body::Context { text });
        }
    }

    /// The model as the settings and the newest client's environment name
    /// it now, or why there is none (shown as the failed model call).
    fn model(&self) -> Result<Model, String> {
        let env = lock(&self.env);
        self.shared.models.model(&self.workspace, env.as_ref())
    }

    /// Asks `model` to reply to the transcript before `call`, offering the
    /// tools. Text streams to clients as it arrives; only the finished reply
    /// is committed.
    async fn run_model(&self, call: u64, model: Result<Model, String>) -> Outcome {
        let model = match model {
            Ok(model) => model,
            Err(error) => return Outcome::Failed { error },
        };
        let registry = &self.shared.registry;
        let tools = registry.tools();
        let mut history = transcript(&self.lock().entries, call, tools);
        let system = self
            .shared
            .system
            .clone()
            .unwrap_or_else(|| registry.system_prompt());
        if !system.is_empty() {
            history.insert(0, llm::Message::system(system));
        }
        let mut request = model.request(history, tools.specs());
        let mut decided = Decided::new();
        registry
            .hooks()
            .prepare_step(call, &mut request, &mut decided);
        if let Err(e) = self.decide(decided).await {
            return Outcome::Failed {
                error: e.to_string(),
            };
        }
        let mut on_text = |text: &str| {
            self.broadcast(&DaemonMsg::Delta {
                call,
                text: text.to_owned(),
            });
        };
        // The whole reply is the fact: replaying it later sends the
        // provider's own items (signatures, encrypted reasoning) back.
        contain(async {
            let mut retries = 0;
            loop {
                let error = match model.reply(&request, &mut on_text).await {
                    Ok(reply) => {
                        return match serde_json::to_value(&reply) {
                            Ok(output) => Outcome::Done { output },
                            Err(error) => Outcome::Failed {
                                error: error.to_string(),
                            },
                        };
                    }
                    Err(error) => error.to_string(),
                };
                let mut decided = Decided::new();
                let retry = (retries < MAX_RETRIES)
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
        })
        .await
    }

    /// Picks up where the log leaves off. Links are re-armed; the agent is
    /// rebuilt from the log; calls with an intent but no outcome are
    /// finished (leftover processes killed, safe calls run again, the rest
    /// marked interrupted, asks still waiting asked again); then whatever
    /// the agent asked for that the log does not show runs.
    async fn recover(&self, agent: &mut dyn Agent) -> io::Result<()> {
        self.relink().await?;
        let (dangling, actions, commands, jobs) = {
            let st = self.lock();
            (
                dangling_calls(&st.entries),
                replay(agent, &st.entries),
                dangling_commands(&st.entries),
                background_groups(&st.entries),
            )
        };
        // Owned by the session under a daemon that died without ending it:
        // ended now. Later loads in this daemon find them ended by `stop`.
        if lock(&self.shared.loaded).insert(self.id) {
            for pgid in jobs.into_iter().filter(|&pgid| sys::group_alive(pgid)) {
                if let Err(e) = sys::kill_process_group(pgid) {
                    self.warn(format_args!("killing background group {pgid}: {e}"));
                }
            }
        }
        // A command cut off is not run again.
        for call in commands {
            self.record(Body::CommandResult {
                call,
                outcome: Outcome::Interrupted,
            });
        }
        let mut inputs = VecDeque::new();
        for Dangling {
            seq,
            call,
            processes,
            asked,
        } in dangling
        {
            let this_boot = processes
                .iter()
                .filter(|(_, boot)| Some(boot.as_str()) == sys::boot_id());
            for (pgid, _) in this_boot {
                if let Err(e) = sys::kill_process_group(*pgid) {
                    self.warn(format_args!("killing process group {pgid}: {e}"));
                }
            }
            let outcome = match &call {
                Call::Tool { tool, input } if asked == Asked::Waiting => {
                    self.ask(seq, tool, input).await?
                }
                Call::Tool { .. } if asked == Asked::Answered(false) => not_approved(),
                Call::Tool { tool, .. }
                    if self.shared.registry.tools().get(tool).map(DynTool::replay)
                        != Some(Replay::Safe) =>
                {
                    Outcome::Interrupted
                }
                _ => {
                    self.flush().await?;
                    self.run_call(&call, seq).await
                }
            };
            inputs.push_back(self.finish(&call, seq, outcome).await?);
        }
        self.drive(agent, actions, inputs).await
    }
}

/// Model calls one request may be retried, as `request_error` hooks ask.
const MAX_RETRIES: u32 = 2;

/// What a tool or command call runs with, held for the call's duration.
struct Effect<'a> {
    session: &'a Session,
    call: u64,
    env: Option<Environment>,
    spill: PathBuf,
    model: String,
    cancel: Cancel,
}

impl<'a> Effect<'a> {
    /// The effect of call `call`, which an interrupt now stops.
    fn new(session: &'a Session, call: u64) -> Self {
        let cancel = Cancel::default();
        *lock(&session.cancel) = cancel.clone();
        Self {
            session,
            call,
            env: lock(&session.env).clone(),
            spill: session.shared.spill.join(session.id.to_string()),
            model: session
                .shared
                .models
                .settings
                .model(&session.workspace)
                .unwrap_or_else(|_| "none".to_owned()),
            cancel,
        }
    }

    fn cx(&self) -> Cx<'_> {
        Cx {
            workspace: &self.session.workspace,
            env: self.env.as_ref(),
            session: self.session.id,
            call: self.call,
            model: &self.model,
            spill: &self.spill,
            cancel: &self.cancel,
            rt: &*self.session.shared.rt,
            host: self,
        }
    }
}

/// Entries a call records are not waited for: they still commit before
/// its result.
impl Host for Effect<'_> {
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

    fn note(&self, ext: &'static str, state: &'static str, data: Value) {
        lock(&self.session.states).fold(ext, state, &data);
        self.session.record(Body::Ext {
            ext: ext.to_owned(),
            state: state.to_owned(),
            data,
        });
        self.session.kick();
    }

    fn states(&self) -> MutexGuard<'_, States> {
        lock(&self.session.states)
    }

    fn hooks(&self) -> &Hooks {
        self.session.shared.registry.hooks()
    }
}

/// `/name args` as a command's name and arguments.
fn slash(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    Some((name, args.trim()))
}

/// Commands with an intent but no outcome.
fn dangling_commands(entries: &[Entry]) -> Vec<u64> {
    let done: HashSet<u64> = entries
        .iter()
        .filter_map(|e| match e.body {
            Body::CommandResult { call, .. } => Some(call),
            _ => None,
        })
        .collect();
    entries
        .iter()
        .filter(|e| matches!(e.body, Body::Command { .. }) && !done.contains(&e.seq))
        .map(|e| e.seq)
        .collect()
}

/// The background process groups the session started in this boot.
fn background_groups(entries: &[Entry]) -> Vec<u32> {
    entries
        .iter()
        .filter_map(|e| match &e.body {
            Body::ToolProcess {
                pgid,
                boot,
                background: true,
                ..
            } if Some(boot.as_str()) == sys::boot_id() => Some(*pgid),
            _ => None,
        })
        .collect()
}

/// Logs the registry's extensions before the session's first step, unless
/// they are the ones its log records last.
fn note_extensions(
    header: &Header,
    loaded: &mut Loaded,
    registry: &agent::Registry,
) -> io::Result<()> {
    let recorded = loaded
        .entries
        .iter()
        .rev()
        .find_map(|e| match &e.body {
            Body::Extensions { installed } => Some(installed),
            _ => None,
        })
        .unwrap_or(&header.extensions);
    let same = recorded
        .iter()
        .map(|i| (i.name.as_str(), i.version))
        .eq(registry
            .extensions()
            .map(|(name, version)| (name as &str, version)));
    if same {
        return Ok(());
    }
    let entry = Entry {
        seq: loaded.entries.len() as u64 + 1,
        epoch: 0,
        body: Body::Extensions {
            installed: installed(registry),
        },
    };
    loaded.log.append(&entry)?;
    loaded.entries.push(entry);
    Ok(())
}

/// The registry's extensions as a log records them.
pub fn installed(registry: &agent::Registry) -> Vec<Installed> {
    registry
        .extensions()
        .map(|(name, version)| Installed {
            name: name.to_owned(),
            version,
        })
        .collect()
}

/// The environment most recently logged.
fn logged_env(entries: &[Entry]) -> Option<&Environment> {
    entries.iter().rev().find_map(|e| match &e.body {
        Body::Environment(env) => Some(env),
        _ => None,
    })
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
/// whether the user was asked to approve it.
struct Dangling {
    seq: u64,
    call: Call,
    processes: Vec<(u32, String)>,
    asked: Asked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Asked {
    No,
    Waiting,
    Answered(bool),
}

fn find_call(dangling: &mut [Dangling], call: u64) -> Option<&mut Dangling> {
    dangling.iter_mut().rev().find(|d| d.seq == call)
}

fn dangling_calls(entries: &[Entry]) -> Vec<Dangling> {
    let finished: HashSet<u64> = entries
        .iter()
        .filter_map(|e| match &e.body {
            Body::ToolResult { call, .. } | Body::ModelResult { call, .. } => Some(*call),
            _ => None,
        })
        .collect();
    let mut dangling: Vec<Dangling> = Vec::new();
    for e in entries {
        let call = match &e.body {
            Body::ToolProcess {
                call, pgid, boot, ..
            } => {
                if let Some(d) = find_call(&mut dangling, *call) {
                    d.processes.push((*pgid, boot.clone()));
                }
                continue;
            }
            Body::Ask { call } => {
                if let Some(d) = find_call(&mut dangling, *call) {
                    d.asked = Asked::Waiting;
                }
                continue;
            }
            Body::Hook {
                decision:
                    Decision::Approval {
                        call: Some(call),
                        approved,
                        ..
                    },
                ..
            } => {
                if let Some(d) = find_call(&mut dangling, *call) {
                    d.asked = Asked::Answered(*approved);
                }
                continue;
            }
            _ if finished.contains(&e.seq) => continue,
            Body::ToolCall { tool, input, .. } => Call::Tool {
                tool: tool.clone(),
                input: input.clone(),
            },
            Body::ModelCall { .. } => Call::Model,
            _ => continue,
        };
        dangling.push(Dangling {
            seq: e.seq,
            call,
            processes: Vec::new(),
            asked: Asked::No,
        });
    }
    dangling
}
