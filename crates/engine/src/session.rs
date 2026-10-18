use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Display;
use std::future::{Future, poll_fn};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

use agent::{
    Action, Agent, Cancel, Cx, Decided, DynTool, Facts, Gate, Hooks, Host, Input, Registry, Replay,
    Route, States,
};
use protocol::{
    Body, DaemonMsg, Decision, Entry, Environment, Frame, Installed, Outcome, Overrides, SessionId,
    SessionSummary,
};
use serde_json::{Value, json};
use store::{Header, Loaded, Log, LogStore, Parent};

use crate::model::{Model, Models};
use crate::{Daemon, lock};

mod facts;
mod links;
#[cfg(test)]
mod tests;

use facts::{ask, inbox, partial, recap, replay, transcript, untaken};

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
    /// answer and so is not counted in `queued`.
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
    /// Background process groups the session owns, until it ends.
    jobs: Mutex<Vec<u32>>,
    shared: Arc<Shared>,
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
    entries: Vec<Entry>,
    clients: Vec<Sender<Frame>>,
}

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
        let registry = shared.registry.get();
        if let Err(e) = note_extensions(&header, &mut loaded, &registry) {
            eprintln!("session {}: recording extensions: {e}", header.id.short());
        }
        let (mailbox, rx) = rt::channel::unbounded();
        let armed = links::armed(&loaded.entries).count();
        // A new session has nothing to recover.
        let fresh = loaded.entries.is_empty();
        let env = env.or_else(|| logged_env(&loaded.entries).cloned());
        let seq = loaded.entries.len() as u64;
        let mut states = registry.states();
        states.replay(&loaded.entries);
        let session = Arc::new_cyclic(|this| Self {
            this: this.clone(),
            id: header.id,
            workspace: header.workspace,
            parent: header.parent,
            epoch: loaded.epoch,
            step: Mutex::new(Arc::clone(&registry)),
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
            waiting: Mutex::default(),
            // Recovery counts as queued work until it has run, and every
            // armed watch until it fires.
            queued: AtomicUsize::new(usize::from(!fresh) + armed),
            running: AtomicBool::new(false),
            attached: AtomicUsize::new(0),
            last_active: Mutex::new(Instant::now()),
            env: Mutex::new(env),
            states: Mutex::new(states),
            cancel: Mutex::default(),
            jobs: Mutex::default(),
            shared,
        });
        registry.hooks().session_start(session.id);
        let actor = Arc::clone(&session);
        session
            .shared
            .rt
            .spawn(Box::pin(async move { actor.run(&rx, fresh).await }));
        session
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
        let command = slash(&text).is_some_and(|(name, _)| is_command(&registry, name));
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
        let body = match slash(&text) {
            _ if !started => Body::Queued {
                text: text.clone(),
                steer,
            },
            Some((name, args)) if command => Body::Command {
                name: name.to_owned(),
                args: args.to_owned(),
            },
            _ => Body::User { text: text.clone() },
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
    /// first, bottom-up.
    pub fn interrupt(&self) {
        for child in self.children() {
            child.interrupt();
        }
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
            if let Err(e) = sys::kill_process_group(pgid) {
                self.warn(format_args!("killing background group {pgid}: {e}"));
            }
        }
        self.registry().hooks().session_end(self.id);
    }

    pub fn attach(&self, client: &Sender<Frame>, since: u64) {
        let send = |frame: io::Result<Frame>| {
            if let Ok(frame) = frame {
                let _ = client.send(frame);
            }
        };
        let mut st = self.lock();
        send(Frame::new(&DaemonMsg::Attached {
            session: self.id,
            workspace: self.workspace.clone(),
        }));
        for e in st.entries.iter().filter(|e| e.seq > since) {
            send(Frame::entry(e));
        }
        let seq = st.entries.last().map_or(0, |e| e.seq);
        send(Frame::new(&DaemonMsg::Synced { seq }));
        if let Some(recap) = recap(&st.entries, since) {
            send(Frame::new(&DaemonMsg::Recap(recap)));
        }
        for call in inbox(&st.entries) {
            if let Some(msg) = ask(&st.entries, call) {
                send(Frame::new(&msg));
            }
        }
        st.clients.push(client.clone());
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
        SessionSummary {
            id: self.id,
            workspace: self.workspace.clone(),
            entries: self.lock().entries.len() as u64,
            clients: u32::try_from(self.attached.load(Ordering::SeqCst)).unwrap_or(u32::MAX),
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
        let registry = self.registry();
        let hooks = registry.hooks();
        let mut st = self.lock();
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
    async fn run(&self, mailbox: &rt::channel::Receiver<Msg>, fresh: bool) {
        let kind = self.shared.models.settings.agent(&self.workspace);
        let mut agent = kind.agent();
        let mut ok = fresh || Box::pin(self.work(self.recover(agent.as_mut()))).await;
        loop {
            if !ok {
                agent = kind.agent();
                let st = self.lock();
                replay(agent.as_mut(), &st.entries);
                let mut states = self.registry().states();
                states.replay(&st.entries);
                *lock(&self.states) = states;
            }
            ok = match mailbox.recv().await {
                Some(Msg::Queued) => {
                    let next = lock(&self.waiting).pop_front();
                    let take = async {
                        match next {
                            Some(input) => self.take(agent.as_mut(), input).await,
                            // A turn took it up already, as a steer.
                            None => Ok(()),
                        }
                    };
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
    async fn work(&self, job: impl Future<Output = io::Result<()>>) -> bool {
        let began = crate::stamp();
        self.touch();
        *lock(&self.cancel) = Cancel::default();
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
        self.running.store(false, Ordering::SeqCst);
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
            let next = self
                .registry()
                .hooks()
                .on_stop(&self.lock().entries, &mut decided);
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
                let card = registry.tools().get(&tool).and_then(|t| t.present(&input));
                let call = self.record(Body::ToolCall {
                    tool,
                    input,
                    id,
                    card,
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
                let card = registry.tools().get(&tool);
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

    async fn run_tool(&self, call: u64, tool: &str, input: &Value) -> Outcome {
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
                output: json!({ "model": model, "from": from.to_string() }),
            },
            Err(error) => Outcome::Failed { error },
        }
    }

    /// The session's own settings, and the seq of the entry that set them.
    fn overrides(&self) -> (Overrides, Option<u64>) {
        self.lock()
            .entries
            .iter()
            .rev()
            .find_map(|e| match &e.body {
                Body::Settings(set) => Some((set.clone(), Some(e.seq))),
                _ => None,
            })
            .unwrap_or_default()
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
        let registry = &*registry;
        let cancel = self.turn();
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
        let mut said = String::new();
        let mut on_text = |text: &str| {
            said.push_str(text);
            self.broadcast(&DaemonMsg::Delta {
                call,
                text: text.to_owned(),
            });
        };
        // The whole reply is the fact: replaying it later sends the
        // provider's own items (signatures, encrypted reasoning) back.
        let outcome = contain(async {
            let mut retries = 0;
            loop {
                let Some(reply) = unless(&cancel, model.reply(&request, &mut on_text)).await else {
                    return Outcome::Interrupted { output: None };
                };
                let error = match reply {
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
        .await;
        match outcome {
            Outcome::Interrupted { .. } => Outcome::Interrupted {
                output: (!said.is_empty()).then_some(said),
            },
            outcome => outcome,
        }
    }

    /// Picks up where the log leaves off. Links are re-armed; the agent is
    /// rebuilt from the log; calls with an intent but no outcome are
    /// finished (leftover processes killed, safe calls run again, the rest
    /// marked interrupted with the output they committed, asks still
    /// waiting asked again); then whatever the agent asked for that the log
    /// does not show runs, and the input still queued is taken up. A child
    /// whose owner is gone is an orphan: nothing of its turn runs again.
    async fn recover(&self, agent: &mut dyn Agent) -> io::Result<()> {
        self.relink().await?;
        let (dangling, actions, commands, jobs, queued) = {
            let st = self.lock();
            (
                dangling_calls(&st.entries),
                replay(agent, &st.entries),
                dangling_commands(&st.entries),
                background_groups(&st.entries),
                untaken(&st.entries),
            )
        };
        let orphan = self.orphaned();
        let registry = self.registry();
        let queued = queued.into_iter().map(|(seq, text, steer)| Waiting {
            command: slash(&text).is_some_and(|(name, _)| is_command(&registry, name)),
            seq,
            text,
            steer,
            started: false,
        });
        let count = queued.len();
        lock(&self.waiting).extend(queued);
        self.queued.fetch_add(count, Ordering::SeqCst);
        for _ in 0..count {
            let _ = self.mailbox.send(Msg::Queued);
        }
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
                outcome: Outcome::Interrupted { output: None },
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
            let interrupted = || Outcome::Interrupted {
                output: partial(&self.lock().entries, seq),
            };
            let outcome = match &call {
                _ if orphan => interrupted(),
                Call::Tool { tool, input } if asked == Asked::Waiting => {
                    self.ask(seq, tool, input).await?
                }
                Call::Tool { .. } if asked == Asked::Answered(false) => not_approved(),
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
            inputs.push_back(self.finish(&call, seq, outcome).await?);
        }
        if orphan {
            self.warn("its owner is gone: what it was doing is not resumed");
            lock(&self.cancel).cancel();
        }
        self.drive(agent, actions, inputs).await
    }

    /// Whether this is a child whose owner is gone.
    fn orphaned(&self) -> bool {
        let daemon = self.shared.daemon.upgrade();
        self.parent
            .zip(daemon)
            .is_some_and(|(parent, daemon)| !daemon.knows(parent.session))
    }
}

/// The native command that sets the session's model.
const MODEL: &str = "model";

/// Whether `/name` is a command: an extension's, or a native one.
fn is_command(registry: &Registry, name: &str) -> bool {
    registry.command(name).is_some() || name == MODEL
}

/// `work`'s output, or `None` once `cancel` is cancelled first; `work` is
/// then dropped.
async fn unless<T>(cancel: &Cancel, work: impl Future<Output = T>) -> Option<T> {
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
            data,
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
fn note_extensions(header: &Header, loaded: &mut Loaded, registry: &Registry) -> io::Result<()> {
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
    let entry = Entry::new(
        loaded.entries.len() as u64 + 1,
        loaded.epoch,
        Body::Extensions {
            installed: installed(registry),
        },
    );
    loaded.log.append(&entry)?;
    loaded.entries.push(entry);
    Ok(())
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
