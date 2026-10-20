//! The turn and call state machine: one [`Core`] per session.

use std::collections::VecDeque;
use std::mem;
use std::sync::Arc;

use protocol::ledger::{Call, Ledger, OutcomeKind, Phase};
use protocol::{
    Audience, Author, Body, CallId, Entry, Json, Mode, Outcome, Seq, SessionId, ToolChoice,
    Verdict, WaitOn,
};
use smallvec::SmallVec;

use ext::Parsed;
use protocol::Card;

use crate::fold::{Fold, Next};
use crate::{
    Action, Answer, Chained, Concurrency, Effect, Effects, Event, Gate, Gated, Load, ModelPhase,
    Name, Note, Owner, Recover, Registry, Replay, Replied, Step, Ticket, Turn,
};

/// Who refused a call the agent would not run.
const AGENT: &str = "agent";

/// The most attempts a model call a restart cut off is sent again after,
/// whatever the `request_error` point says: a daemon that keeps dying
/// mid-call cannot retry it forever.
pub const RESTART_CAP: u16 = 10;

/// A session's kernel: its log folded, its turn, and the registry
/// generation its steps run on.
pub struct Core {
    registry: Arc<dyn Registry>,
    /// The generation to take up at the next step boundary.
    next: Option<Arc<dyn Registry>>,
    fold: Fold,
    turn: Turn,
    /// Whether the session is a child, which reports as it settles.
    child: bool,
    /// What the busy mark says.
    busy: bool,
    /// The last entry when the session last reported.
    reported: u64,
    /// Results of children no call waits on, held until the turn running
    /// ends: each starts a turn of its own.
    deferred: VecDeque<(SessionId, Outcome)>,
    /// Why the model call being recovered failed, while a handler decides.
    failed: Option<String>,
    /// Each open tool call's input as its tool read it at its intent, with
    /// the text it was read from: its run is given it rather than reading
    /// it again, unless the gate rewrote it.
    parsed: Vec<(CallId, Json, Parsed)>,
}

impl Core {
    /// The session `log` records, picked up where it leaves off: the one
    /// way a session recovers, from a load, a failed commit (the driver
    /// cancels what it has in flight and passes what committed), a
    /// registry reload or a panic.
    ///
    /// Links are re-armed; process groups left over in this boot are
    /// killed; a command cut off is interrupted; each open call goes on as
    /// its phase says (a gate is asked again, a wait stays, a safe call
    /// runs again, an unsafe one is interrupted with the output it logged,
    /// a model call counts the attempt it cut off as failed); then what the agent
    /// asked for that the log does not show is carried out, and queued
    /// input taken up.
    pub fn resume<'e>(
        registry: Arc<dyn Registry>,
        log: impl IntoIterator<Item = &'e Entry>,
        load: &Load<'_>,
    ) -> (Self, Effects) {
        let mut core = Self::new(registry);
        for e in log {
            core.load(e);
        }
        let fx = core.resumed(load);
        (core, fx)
    }

    /// A session with nothing folded yet: its log is folded in an entry at
    /// a time with [`Core::load`], as it is read, then it is resumed with
    /// [`Core::resumed`]. [`Core::resume`] does all three.
    pub fn new(registry: Arc<dyn Registry>) -> Self {
        Self {
            fold: Fold::new(registry.agent()),
            registry,
            next: None,
            turn: Turn::Idle,
            child: false,
            busy: false,
            reported: 0,
            deferred: VecDeque::new(),
            failed: None,
            parsed: Vec::new(),
        }
    }

    /// Folds entry `e` of the log being loaded, the one after the last.
    pub fn load(&mut self, e: &Entry) {
        self.fold.apply(e, &*self.registry);
    }

    /// Picks the loaded log up where it leaves off: see [`Core::resume`].
    pub fn resumed(&mut self, load: &Load<'_>) -> Effects {
        // What the log shows the agent stepped through is done.
        self.fold.stepped = false;
        self.child = load.owner == Owner::Present;
        self.busy = load.busy;
        let mut fx = Effects::new();
        let installed = self.registry.installed();
        let logged = self.fold.ledger.extensions();
        if load.created || logged.is_some_and(|logged| logged != installed) {
            self.append(&mut fx, Body::Extensions { installed });
        }
        self.recover(load, &mut fx);
        self.finish(&mut fx);
        fx
    }

    /// What follows `event`.
    pub fn step(&mut self, event: Event) -> Effects {
        let mut fx = Effects::new();
        match event {
            Event::Input {
                text,
                mode,
                origin,
                ticket,
            } => self.input(text, mode, origin, ticket, &mut fx),
            Event::Interrupt { origin, ticket } => {
                if !self.resent(origin) {
                    self.interrupt(origin, &mut fx);
                }
                reply(&mut fx, ticket, Ok(()));
            }
            Event::Answer {
                call,
                value,
                origin,
                ticket,
            } => {
                let result = if self.resent(origin) {
                    Ok(())
                } else {
                    self.answer(call, value, origin, &mut fx)
                };
                reply(&mut fx, ticket, result);
            }
            Event::ModelDone { call, replied } => self.model_done(call, replied, &mut fx),
            Event::ToolDone {
                call,
                outcome,
                card,
            } => self.tool_done(call, outcome, card, &mut fx),
            Event::Note { call, note } => self.note(call, note, &mut fx),
            Event::Waits { call, on } => self.waits(call, on, &mut fx),
            Event::Spawn {
                call,
                child,
                input,
                wait,
            } => self.spawn(call, child, input, wait, &mut fx),
            Event::Gated { call, gated } => {
                if let Some(Step::Awaiting { skip, resumed }) = self.step_of(call) {
                    self.gated(call, gated, skip, resumed, &mut fx);
                }
            }
            Event::Woke { call } => {
                if let Turn::Model {
                    call: failed,
                    attempt,
                    phase: ModelPhase::Backoff,
                } = self.turn
                    && failed == call
                {
                    self.again(call, attempt, &mut fx);
                }
            }
            Event::Recovered { call, recovered } => self.recovered_later(call, recovered, &mut fx),
            Event::Side {
                of,
                purpose,
                model,
                ticket,
            } => {
                let side = self.side(of, purpose, model, &mut fx);
                fx.push(Effect::Side { ticket, side });
            }
            Event::Prepared { call, decisions } => {
                if matches!(self.turn, Turn::Model { call: c, phase: ModelPhase::Streaming, .. } if c == call)
                {
                    for (by, d) in decisions {
                        self.decide(Some(call), &by, d, &mut fx);
                    }
                }
            }
            Event::SideDone { side, outcome } => self.side_done(side, outcome, &mut fx),
            Event::ChildSettled { child, outcome } => self.child_settled(child, outcome, &mut fx),
            Event::Registry(registry) => self.next = Some(registry),
            Event::Environment(env) => {
                if self.fold.ledger.env() != Some(&env) {
                    self.append(&mut fx, Body::Environment(env));
                }
            }
        }
        self.finish(&mut fx);
        fx
    }

    pub const fn turn(&self) -> &Turn {
        &self.turn
    }

    /// What the log says now, through the last entry appended.
    pub const fn ledger(&self) -> &Ledger {
        &self.fold.ledger
    }

    fn finish(&mut self, fx: &mut Effects) {
        self.advance(fx);
        self.marks(fx);
    }

    // ── events ────────────────────────────────────────────────────────

    fn input(
        &mut self,
        text: String,
        mode: Mode,
        origin: Option<Author>,
        ticket: Option<Ticket>,
        fx: &mut Effects,
    ) {
        if self.resent(origin) {
            return reply(fx, ticket, Ok(()));
        }
        // Nothing runs or waits, so nothing comes between: it starts now, a
        // turn on the newest registry generation.
        let taken = self.turn == Turn::Idle
            && self.fold.asked.is_empty()
            && self.fold.ledger.untaken().is_empty();
        if taken {
            self.resolve(fx);
        }
        let Chained {
            value: text,
            decisions,
        } = match self.registry.input(text) {
            Ok(chained) => chained,
            Err(why) => return reply(fx, ticket, Err(why)),
        };
        for (by, d) in decisions {
            self.decide(None, &by, d, fx);
        }
        self.append(
            fx,
            Body::Input {
                text,
                mode,
                taken,
                origin,
            },
        );
        reply(fx, ticket, Ok(()));
    }

    /// Stops the turn running: its model call or the calls it waits on.
    /// An interrupt with no turn running stops nothing, and starts nothing
    /// in any state: what waits settles at once, what runs is cancelled and
    /// settles as it reports.
    fn interrupt(&mut self, origin: Option<Author>, fx: &mut Effects) {
        let open = match mem::take(&mut self.turn) {
            Turn::Idle => return,
            stopping @ Turn::Stopping { .. } => {
                self.turn = stopping;
                return;
            }
            Turn::Model { call, phase, .. } => {
                fx.push(Effect::Cancel { call });
                match phase {
                    ModelPhase::Streaming => SmallVec::from_elem(call, 1),
                    // Nothing is out to report.
                    ModelPhase::Backoff | ModelPhase::Recovering => {
                        self.failed = None;
                        self.settle(call, Outcome::Interrupted { output: None }, fx);
                        SmallVec::new()
                    }
                }
            }
            Turn::Acting { open } => {
                let mut left = SmallVec::new();
                for (call, step) in open {
                    match (step, self.child_of(call)) {
                        (Step::Running, _) => {
                            fx.push(Effect::Cancel { call });
                            left.push(call);
                        }
                        (Step::Awaiting { .. }, _) => {
                            fx.push(Effect::Cancel { call });
                            self.settle(call, self.interrupted(call), fx);
                        }
                        // Its child stops first, and reports.
                        (Step::Waiting, Some(child)) => {
                            fx.push(Effect::Interrupt { child });
                            left.push(call);
                        }
                        (Step::Waiting, None) => self.settle(call, self.interrupted(call), fx),
                    }
                }
                left
            }
        };
        self.turn = Turn::Stopping { open, origin };
    }

    /// The first answer to what call `call` waits on wins.
    fn answer(
        &mut self,
        call: CallId,
        value: Json,
        origin: Option<Author>,
        fx: &mut Effects,
    ) -> Result<(), String> {
        let on = match self.fold.ledger.call(call).map(|open| &open.phase) {
            Some(Phase::Waiting(on)) if !matches!(on, WaitOn::Child { .. }) => on.clone(),
            _ => return Err(format!("call {call} is not waiting for an answer")),
        };
        self.append(
            fx,
            Body::Answer {
                call,
                value: value.clone(),
                origin,
            },
        );
        match on {
            WaitOn::Approval if value.parse::<bool>().unwrap_or(false) => self.run(call, fx),
            WaitOn::Approval => {
                self.close(call);
                let error = "the user did not approve this call".to_owned();
                self.settle(call, Outcome::Failed { error }, fx);
            }
            _ => self.resume_call(call, value, fx),
        }
        Ok(())
    }

    fn model_done(&mut self, call: CallId, replied: Replied, fx: &mut Effects) {
        match &self.turn {
            &Turn::Model {
                call: sent,
                attempt,
                phase: ModelPhase::Streaming,
            } if sent == call => {
                let outcome = match replied {
                    Replied::Failed(error) => {
                        if self.retry(call, attempt, &error, fx) {
                            return;
                        }
                        Outcome::Failed {
                            error: error.to_string(),
                        }
                    }
                    replied => outcome(replied),
                };
                self.turn = Turn::Acting {
                    open: SmallVec::new(),
                };
                self.settle(call, outcome, fx);
            }
            Turn::Stopping { open, .. } if open.contains(&call) => {
                self.close(call);
                self.settle(call, outcome(replied), fx);
            }
            _ => {}
        }
    }

    /// Whether the `request_error` point recovers model call `call` from
    /// the failure of attempt `attempt`, now or once a handler answers
    /// later: if so, the next attempt is logged, and sent at once or once
    /// the backoff the point asks for is over.
    fn retry(&mut self, call: CallId, attempt: u16, error: &llm::Error, fx: &mut Effects) -> bool {
        match self.registry.request_error(call, attempt, error) {
            Answer::Now(None) => false,
            Answer::Now(Some((by, recover))) => {
                self.recovered(call, attempt, &by, recover, error.to_string(), fx);
                true
            }
            Answer::Later(answer) => {
                self.failed = Some(error.to_string());
                fx.push(Effect::Recover { call, answer });
                self.turn = Turn::Model {
                    call,
                    attempt,
                    phase: ModelPhase::Recovering,
                };
                true
            }
        }
    }

    /// Recovers model call `call` from the failure of attempt `attempt`
    /// as `by` says: the next attempt is logged, then sent.
    fn recovered(
        &mut self,
        call: CallId,
        attempt: u16,
        by: &str,
        recover: Recover,
        error: String,
        fx: &mut Effects,
    ) {
        let next = attempt.saturating_add(1);
        let d = Verdict::Retry {
            attempt: next,
            error,
        };
        self.decide(Some(call), by, d, fx);
        match recover {
            Recover::Retry { after } => {
                fx.push(Effect::Sleep { call, after });
                self.turn = Turn::Model {
                    call,
                    attempt: next,
                    phase: ModelPhase::Backoff,
                };
            }
            Recover::Resend => self.again(call, next, fx),
        }
    }

    /// How a handler that answered later says to recover model call
    /// `call`, if it still waits on it: with no answer, it fails.
    fn recovered_later(
        &mut self,
        call: CallId,
        recovered: Option<(Name, Recover)>,
        fx: &mut Effects,
    ) {
        let Turn::Model {
            call: failed,
            attempt,
            phase: ModelPhase::Recovering,
        } = self.turn
        else {
            return;
        };
        if failed != call {
            return;
        }
        let error = self.failed.take().unwrap_or_default();
        if let Some((by, recover)) = recovered {
            self.recovered(call, attempt, &by, recover, error, fx);
        } else {
            self.turn = Turn::Acting {
                open: SmallVec::new(),
            };
            self.settle(call, Outcome::Failed { error }, fx);
        }
    }

    /// Logs side request `side`'s outcome, unless it has one.
    fn side_done(&mut self, side: CallId, outcome: Outcome, fx: &mut Effects) {
        let open = self.fold.ledger.call(side).map(|open| &open.call);
        if matches!(open, Some(Call::Side { .. })) {
            self.append(
                fx,
                Body::ModelResult {
                    call: side,
                    outcome,
                },
            );
        }
    }

    /// Logs the intent of a side model request on behalf of call `of`, if
    /// it is open and its own: the side call's id.
    fn side(
        &mut self,
        of: CallId,
        purpose: String,
        model: String,
        fx: &mut Effects,
    ) -> Option<CallId> {
        let owner = self.fold.ledger.call(of)?;
        if matches!(owner.call, Call::Side { .. }) {
            return None;
        }
        let body = Body::ModelCall {
            model,
            attempt: 0,
            choice: ToolChoice::Auto,
            r#for: Some(of),
            purpose: Some(purpose),
        };
        Some(self.append(fx, body))
    }

    /// Sends model call `call` again, as attempt `attempt`.
    fn again(&mut self, call: CallId, attempt: u16, fx: &mut Effects) {
        let choice = match self.fold.ledger.call(call).map(|open| &open.call) {
            Some(&Call::Model { choice, .. }) => choice,
            _ => ToolChoice::Auto,
        };
        fx.push(Effect::CallModel {
            call,
            attempt,
            choice,
        });
        self.turn = Turn::Model {
            call,
            attempt,
            phase: ModelPhase::Streaming,
        };
    }

    /// Logs a call's outcome as the `post_tool` chain leaves it, with the
    /// card its tool showed it with, unless a handler replaced it.
    fn tool_done(&mut self, call: CallId, outcome: Outcome, card: Option<Card>, fx: &mut Effects) {
        if self.step_of(call) != Some(Step::Running) {
            return;
        }
        let Some((tool, input)) = self.tool_of(call) else {
            return;
        };
        let Chained {
            value: outcome,
            decisions,
        } = self.registry.post_tool(call, &tool, &input, outcome);
        let card = card.filter(|_| decisions.is_empty());
        for (by, d) in decisions {
            self.decide(Some(call), &by, d, fx);
        }
        self.close(call);
        self.append(
            fx,
            Body::ToolResult {
                call,
                outcome,
                card,
            },
        );
    }

    /// Logs what a running call reports; a call not running reports
    /// nothing more.
    /// Logs what a call reports: a running tool call anything, and a call
    /// whose handler answers later (a model call recovering, a gate
    /// asking) its extension's state and edits. A call not open reports
    /// nothing more.
    fn note(&mut self, call: CallId, note: Note, fx: &mut Effects) {
        let deciding = match &self.turn {
            Turn::Model { call: c, .. } => *c == call,
            _ => matches!(self.step_of(call), Some(Step::Awaiting { .. })),
        };
        let running = self.step_of(call) == Some(Step::Running);
        let ext = matches!(note, Note::State { .. } | Note::Edit { .. });
        if !(running || deciding && ext) {
            return;
        }
        let body = match note {
            Note::Process {
                pgid,
                boot,
                background,
            } => Body::ToolProcess {
                call,
                pgid,
                boot,
                background,
            },
            Note::Progress(text) => Body::Progress { call, text },
            Note::State { ext, state, data } => Body::Ext { ext, state, data },
            Note::Edit {
                target,
                upto,
                replace,
            } => Body::Edit {
                target,
                upto,
                replace,
            },
        };
        self.append(fx, body);
    }

    fn waits(&mut self, call: CallId, on: WaitOn, fx: &mut Effects) {
        let acting = matches!(self.turn, Turn::Acting { .. });
        if !acting
            || self.step_of(call) != Some(Step::Running)
            || !matches!(on, WaitOn::Dialog { .. } | WaitOn::Client { .. })
        {
            return;
        }
        self.append(fx, Body::Wait { call, on });
        self.set_step(call, Step::Waiting);
    }

    /// Links child `child` to call `call` and watches it: the link commits
    /// before the child starts, so a crash before it, or before the watch,
    /// leaves the rest to [`Core::resume`].
    fn spawn(
        &mut self,
        call: CallId,
        child: SessionId,
        input: String,
        wait: bool,
        fx: &mut Effects,
    ) {
        let acting = matches!(self.turn, Turn::Acting { .. });
        if !acting || self.step_of(call) != Some(Step::Running) {
            return;
        }
        let link = self.append(
            fx,
            Body::Link {
                child,
                input: input.clone(),
            },
        );
        if wait {
            let on = WaitOn::Child { link: link.0 };
            self.append(fx, Body::Wait { call, on });
            self.set_step(call, Step::Waiting);
        }
        self.append(fx, Body::Watch { child });
        fx.push(Effect::StartChild {
            link: link.0,
            child,
            input,
        });
    }

    /// Fires the watches on `child`. A call waiting on it goes on with its
    /// result; otherwise the result is the agent's, between turns.
    fn child_settled(&mut self, child: SessionId, outcome: Outcome, fx: &mut Effects) {
        let Some(call) = self.fold.owner(child) else {
            if self.turn == Turn::Idle {
                self.fire(child, &outcome, fx);
            } else if self.fold.ledger.armed().any(|(_, c)| c == child) {
                self.deferred.push_back((child, outcome));
            }
            return;
        };
        if !self.fire(child, &outcome, fx) {
            return;
        }
        match (&self.turn, self.step_of(call)) {
            (Turn::Stopping { .. }, Some(_)) => {
                self.close(call);
                self.settle(call, self.interrupted(call), fx);
            }
            (_, Some(Step::Waiting)) => self.resume_call(call, to_json(&outcome), fx),
            _ => {}
        }
    }

    /// Logs each armed watch on `child` fired; whether any was.
    fn fire(&mut self, child: SessionId, outcome: &Outcome, fx: &mut Effects) -> bool {
        let watches: SmallVec<[Seq; 1]> = (self.fold.ledger.armed())
            .filter_map(|(watch, watched)| (watched == child).then_some(watch))
            .collect();
        for &watch in &watches {
            let outcome = outcome.clone();
            self.append(fx, Body::Fired { watch, outcome });
        }
        !watches.is_empty()
    }

    // ── the turn ──────────────────────────────────────────────────────

    /// Carries out what is asked for while nothing is open: the agent's
    /// next action, or a shared call alongside the shared calls open;
    /// once it stopped, what the `on_stop` point gives it; then queued
    /// input, and children's results.
    fn advance(&mut self, fx: &mut Effects) {
        loop {
            match &self.turn {
                Turn::Model { .. } => return,
                Turn::Acting { open } if !open.is_empty() && !self.alongside(open) => return,
                Turn::Stopping { open, .. } if !open.is_empty() => return,
                Turn::Stopping { .. } => {
                    self.stopped(fx);
                    continue;
                }
                Turn::Acting { .. } | Turn::Idle => {}
            }
            if let Some(next) = self.fold.asked.front().cloned() {
                // A new generation is taken up only with nothing open.
                if !matches!(&self.turn, Turn::Acting { open } if !open.is_empty()) {
                    self.resolve(fx);
                    self.turn = Turn::Acting {
                        open: SmallVec::new(),
                    };
                }
                self.act(next, fx);
                continue;
            }
            let stopped = mem::take(&mut self.fold.stepped)
                .then(|| self.registry.on_stop())
                .flatten();
            if let Some((by, text)) = stopped {
                self.decide(None, &by, Verdict::OnStop { text }, fx);
                continue;
            }
            self.turn = Turn::Idle;
            if let Some(input) = self.fold.ledger.untaken().front().map(|q| q.seq) {
                self.resolve(fx);
                self.turn = Turn::Acting {
                    open: SmallVec::new(),
                };
                self.append(fx, Body::Take { input });
                continue;
            }
            if let Some((child, outcome)) = self.deferred.pop_front() {
                self.fire(child, &outcome, fx);
                continue;
            }
            return;
        }
    }

    /// Whether the next thing asked for starts alongside the calls `open`:
    /// a refusal, which runs nothing, or a shared call while every open
    /// one is shared. Anything else waits for them, so an exclusive call
    /// never overlaps another, and nothing starts ahead of what was asked
    /// before it.
    fn alongside(&self, open: &[(CallId, Step)]) -> bool {
        match self.fold.asked.front() {
            Some(Next::Agent(Action::Refuse { .. })) => true,
            Some(Next::Agent(Action::Call { tool, .. })) => {
                self.shared(tool)
                    && open
                        .iter()
                        .all(|&(call, _)| self.tool_of(call).is_some_and(|(t, _)| self.shared(&t)))
            }
            _ => false,
        }
    }

    /// Whether calls of `tool` run alongside others.
    fn shared(&self, tool: &str) -> bool {
        self.registry
            .callable(tool)
            .is_some_and(|c| c.concurrency == Concurrency::Shared)
    }

    /// Carries out `next`: its entry takes it off what is asked for.
    fn act(&mut self, next: Next, fx: &mut Effects) {
        match next {
            Next::Agent(Action::Say(text)) => {
                self.append(fx, Body::Assistant { text });
            }
            Next::Agent(Action::CallModel { choice }) => {
                // Steering input reaches the turn before the model is
                // next asked.
                let steers: SmallVec<[Seq; 2]> = (self.fold.ledger.untaken().iter())
                    .filter(|q| q.steer)
                    .map(|q| q.seq)
                    .collect();
                for input in steers {
                    self.append(fx, Body::Take { input });
                }
                if !self.fold.ledger.has_context()
                    && let Some(text) = self.registry.context()
                {
                    self.append(fx, Body::Context { text });
                }
                let model = self.registry.model();
                let body = Body::ModelCall {
                    model,
                    attempt: 0,
                    choice,
                    r#for: None,
                    purpose: None,
                };
                let call = self.append(fx, body);
                self.again(call, 0, fx);
            }
            Next::Agent(Action::Call { tool, input, id }) => {
                let call = self.call(&tool, &input, id, Audience::Model, fx);
                self.gate(call, &tool, &input, fx);
            }
            Next::Agent(Action::Refuse {
                tool,
                input,
                id,
                why,
            }) => {
                let call = self.call(&tool, &input, id, Audience::Model, fx);
                let reason = why.clone();
                self.decide(Some(call), AGENT, Verdict::Block { reason }, fx);
                self.settle(call, Outcome::Failed { error: why }, fx);
            }
            Next::Command { name, args } => {
                let input = args_of(args);
                let call = self.call(&name, &input, None, Audience::User, fx);
                self.gate(call, &name, &input, fx);
            }
        }
    }

    /// Logs the intent to call `tool`.
    fn call(
        &mut self,
        tool: &str,
        input: &Json,
        id: Option<String>,
        by: Audience,
        fx: &mut Effects,
    ) -> CallId {
        let (parsed, card) = self.registry.read(tool, input);
        let body = Body::ToolCall {
            tool: tool.to_owned(),
            input: input.clone(),
            id,
            card,
            by,
        };
        let call = self.append(fx, body);
        if let Some(parsed) = parsed {
            self.parsed.push((call, input.clone(), parsed));
        }
        call
    }

    /// Runs tool call `call`, on its input as read at its intent if the
    /// gate left it as it was.
    fn run_tool(&mut self, call: CallId, tool: String, input: Json, fx: &mut Effects) {
        let parsed = (self.parsed.iter().position(|(c, _, _)| *c == call))
            .map(|at| self.parsed.swap_remove(at))
            .and_then(|(_, read, parsed)| (read == input).then_some(parsed));
        self.set_step(call, Step::Running);
        fx.push(Effect::RunTool {
            call,
            tool,
            input,
            parsed,
        });
    }

    fn gate(&mut self, call: CallId, tool: &str, input: &Json, fx: &mut Effects) {
        match self.registry.gate(call, tool, input) {
            Answer::Now(gated) => self.gated(call, gated, 0, false, fx),
            Answer::Later(answer) => {
                self.set_step(
                    call,
                    Step::Awaiting {
                        skip: 0,
                        resumed: false,
                    },
                );
                fx.push(Effect::Await { call, answer });
            }
        }
    }

    /// Logs what call `call`'s gate decided (but the first `skip`, logged
    /// already), then runs it, has it wait for an approval or answers it
    /// with why not. One gated before a restart may have run: an unsafe
    /// one is interrupted rather than run twice.
    fn gated(&mut self, call: CallId, gated: Gated, skip: u16, resumed: bool, fx: &mut Effects) {
        let Gated {
            input,
            decisions,
            gate,
        } = gated;
        for (by, d) in decisions.into_iter().skip(usize::from(skip)) {
            self.decide(Some(call), &by, d, fx);
        }
        match gate {
            Gate::Run if resumed && !self.safe(call) => {
                self.close(call);
                self.settle(call, self.interrupted(call), fx);
            }
            Gate::Run => {
                let Some((tool, _)) = self.tool_of(call) else {
                    return;
                };
                self.run_tool(call, tool, input, fx);
            }
            Gate::Ask => {
                let asked = matches!(
                    self.fold.ledger.call(call).map(|open| &open.phase),
                    Some(Phase::Waiting(WaitOn::Approval))
                );
                if !asked {
                    let on = WaitOn::Approval;
                    self.append(fx, Body::Wait { call, on });
                }
                self.set_step(call, Step::Waiting);
            }
            Gate::Refuse(error) => {
                self.close(call);
                self.settle(call, Outcome::Failed { error }, fx);
            }
        }
    }

    /// Runs call `call`, approved.
    fn run(&mut self, call: CallId, fx: &mut Effects) {
        let Some((tool, input)) = self.tool_of(call) else {
            return;
        };
        self.run_tool(call, tool, input, fx);
    }

    /// Hands call `call` what it waited on.
    fn resume_call(&mut self, call: CallId, value: Json, fx: &mut Effects) {
        let Some((tool, input)) = self.tool_of(call) else {
            return;
        };
        self.set_step(call, Step::Running);
        fx.push(Effect::Resume {
            call,
            tool,
            input,
            value,
        });
    }

    /// Ends an interrupted turn whose calls have settled. Every tool call
    /// the agent asked for is answered, interrupted, so that each call a
    /// model made has its answer; nothing else it asked for is done.
    fn stopped(&mut self, fx: &mut Effects) {
        let Turn::Stopping { origin, .. } = mem::take(&mut self.turn) else {
            return;
        };
        // Only what was asked before the stop: an agent told of an
        // interrupted call may ask for more.
        for _ in 0..self.fold.asked.len() {
            let Some(next) = self.fold.asked.front().cloned() else {
                break;
            };
            let (tool, input, id, by) = match next {
                Next::Agent(
                    Action::Call { tool, input, id }
                    | Action::Refuse {
                        tool, input, id, ..
                    },
                ) => (tool, input, id, Audience::Model),
                Next::Command { name, args } => (name, args_of(args), None, Audience::User),
                Next::Agent(Action::Say(_) | Action::CallModel { .. }) => {
                    self.fold.asked.pop_front();
                    continue;
                }
            };
            let call = self.call(&tool, &input, id, by, fx);
            self.settle(call, Outcome::Interrupted { output: None }, fx);
        }
        self.append(fx, Body::Interrupt { origin });
        self.fold.stepped = false;
    }

    /// Takes up the newest registry generation, at a step boundary,
    /// logging the extensions it runs when they changed.
    fn resolve(&mut self, fx: &mut Effects) {
        let Some(next) = self.next.take() else {
            return;
        };
        let installed = next.installed();
        let changed = installed != self.registry.installed();
        self.registry = next;
        if changed {
            self.append(fx, Body::Extensions { installed });
        }
    }

    /// Marks the session busy while it has work in flight (waiting is not
    /// work), and has a child report each time it settles.
    fn marks(&mut self, fx: &mut Effects) {
        let ledger = &self.fold.ledger;
        let armed = ledger.armed().next().is_some();
        let queued = !ledger.untaken().is_empty();
        let working = match &self.turn {
            Turn::Idle => false,
            // A call waiting on its child waits on work of its own.
            Turn::Acting { open } => (open.iter())
                .any(|&(call, step)| step != Step::Waiting || self.child_of(call).is_some()),
            Turn::Model { .. } | Turn::Stopping { .. } => true,
        };
        let busy = working || queued || armed;
        if busy != self.busy {
            self.busy = busy;
            // Marked before the work starts, cleared after it ends.
            if busy {
                fx.insert(0, Effect::MarkBusy(true));
            } else {
                fx.push(Effect::MarkBusy(false));
            }
        }
        // Settled with something new since it last reported (one given no
        // input yet has not run).
        let settled = self.turn == Turn::Idle && !queued && !armed;
        let ran = ledger.turn_start() > 0;
        if settled && self.child && ran && ledger.seq() > self.reported {
            self.reported = ledger.seq();
            let outcome = ledger.result().cloned().unwrap_or_else(|| Outcome::Failed {
                error: "the child settled without a result".to_owned(),
            });
            fx.push(Effect::Report { outcome });
        }
    }

    // ── recovery ──────────────────────────────────────────────────────

    fn recover(&mut self, load: &Load<'_>, fx: &mut Effects) {
        let ledger = &self.fold.ledger;
        let here = |boot: &str| load.boot == Some(boot);
        let open = ledger.open_calls().map(|(_, open)| open);
        let commands = ledger.open_commands().filter_map(|seq| ledger.call(seq));
        let mut kills: SmallVec<[u32; 2]> = open
            .chain(commands)
            .flat_map(|open| &open.processes)
            .filter(|(_, boot)| here(boot))
            .map(|&(pgid, _)| pgid)
            .collect();
        if load.fresh {
            let jobs = ledger.background().iter().filter(|(_, boot)| here(boot));
            for &(pgid, _) in jobs {
                if !kills.contains(&pgid) {
                    kills.push(pgid);
                }
            }
        }
        fx.extend(kills.into_iter().map(|pgid| Effect::Kill { pgid }));
        self.relink(fx);
        // A command cut off is not run again.
        let commands: SmallVec<[CallId; 1]> = self.fold.ledger.open_commands().collect();
        for call in commands {
            self.settle(call, self.interrupted(call), fx);
        }
        let open: Vec<(CallId, Call, Phase)> = (self.fold.ledger.open_calls())
            .map(|(call, open)| (call, open.call.clone(), open.phase.clone()))
            .collect();
        if !open.is_empty() || !self.fold.asked.is_empty() {
            self.turn = Turn::Acting {
                open: SmallVec::new(),
            };
        }
        let orphan = load.owner == Owner::Gone;
        for (call, kind, phase) in open {
            if orphan {
                self.settle(call, self.interrupted(call), fx);
                continue;
            }
            match kind {
                Call::Model { attempt, .. } => self.restart(call, attempt, fx),
                Call::Tool { tool, input, .. } => self.reopen(call, &tool, &input, &phase, fx),
                // Its owner asks again, if it still wants an answer.
                Call::Side { .. } => self.settle(call, Outcome::Interrupted { output: None }, fx),
            }
        }
        if orphan && self.turn != Turn::Idle {
            self.turn = Turn::Stopping {
                open: SmallVec::new(),
                origin: None,
            };
        }
    }

    /// Re-arms links: a link not yet watched is watched, and every child
    /// still watched is started, so it runs to its end and reports back.
    fn relink(&mut self, fx: &mut Effects) {
        let ledger = &self.fold.ledger;
        let links: SmallVec<[_; 1]> = (ledger.links().iter())
            .filter_map(|link| {
                let watched = ledger.watched(link.child);
                let armed = ledger.armed().any(|(_, child)| child == link.child);
                (armed || !watched).then(|| (link.seq, link.child, link.input.clone(), watched))
            })
            .collect();
        for (link, child, input, watched) in links {
            if !watched {
                self.append(fx, Body::Watch { child });
            }
            fx.push(Effect::StartChild { link, child, input });
        }
    }

    /// A model call a restart cut off: the attempt it was on failed, and the
    /// `request_error` point says whether there is another, so restarts
    /// count as attempts and a crash loop cannot retry forever.
    fn restart(&mut self, call: CallId, attempt: u16, fx: &mut Effects) {
        let error = llm::Error::new(llm::Kind::Incomplete, "the daemon stopped during the call");
        if attempt >= RESTART_CAP {
            let error = format!("{error}; gave up after {attempt} attempts");
            return self.settle(call, Outcome::Failed { error }, fx);
        }
        if !self.retry(call, attempt, &error, fx) {
            let error = error.to_string();
            self.settle(call, Outcome::Failed { error }, fx);
        }
    }

    /// Picks tool call `call` up as its phase says: one the agent refused
    /// is answered so, whatever of it committed.
    fn reopen(&mut self, call: CallId, tool: &str, input: &Json, phase: &Phase, fx: &mut Effects) {
        if let Some(why) = self.fold.refused(call).map(str::to_owned) {
            if *phase == (Phase::Gating { at: 0 }) {
                let reason = why.clone();
                self.decide(Some(call), AGENT, Verdict::Block { reason }, fx);
            }
            return self.settle(call, Outcome::Failed { error: why }, fx);
        }
        match *phase {
            Phase::Gating { at } => match self.registry.gate(call, tool, input) {
                Answer::Now(gated) => self.gated(call, gated, at, true, fx),
                Answer::Later(answer) => {
                    let skip = at;
                    self.set_step(
                        call,
                        Step::Awaiting {
                            skip,
                            resumed: true,
                        },
                    );
                    fx.push(Effect::Await { call, answer });
                }
            },
            Phase::Waiting(WaitOn::Child { .. }) => match self.fold.fired(call).cloned() {
                Some(outcome) => self.resume_call(call, to_json(&outcome), fx),
                None => self.set_step(call, Step::Waiting),
            },
            Phase::Waiting(_) => self.set_step(call, Step::Waiting),
            Phase::Running if self.safe(call) => self.run(call, fx),
            Phase::Running => self.settle(call, self.interrupted(call), fx),
            Phase::Settled(kind) => {
                let outcome = match kind {
                    OutcomeKind::Done => Outcome::Done {
                        output: Json::null(),
                    },
                    OutcomeKind::Failed => Outcome::Failed {
                        error: "the call was refused".to_owned(),
                    },
                    OutcomeKind::Interrupted => self.interrupted(call),
                };
                self.settle(call, outcome, fx);
            }
        }
    }

    // ── helpers ───────────────────────────────────────────────────────

    /// Numbers `body` next, folds it and has it appended; its seq.
    fn append(&mut self, fx: &mut Effects, body: Body) -> CallId {
        let seq = Seq::new(self.fold.ledger.seq() + 1).expect("an entry after the last has a seq");
        let entry = Entry::new(seq, protocol::Epoch::default(), body);
        self.fold.apply(&entry, &*self.registry);
        fx.push(Effect::Append(entry));
        CallId(seq)
    }

    fn decide(&mut self, call: Option<CallId>, by: &str, d: Verdict, fx: &mut Effects) {
        let by = by.to_owned();
        self.append(fx, Body::Decision { by, call, d });
    }

    /// Logs call `call`'s outcome. One that did not finish (it failed, or
    /// was interrupted) takes the side requests made on its behalf that are
    /// still out with it, their ends logged first; one done leaves them to
    /// end as they do.
    fn settle(&mut self, call: CallId, outcome: Outcome, fx: &mut Effects) {
        let done = matches!(outcome, Outcome::Done { .. });
        let sides: SmallVec<[CallId; 1]> = (self.fold.ledger.open_calls())
            .filter(|(_, open)| !done && open.call == Call::Side { of: call })
            .map(|(seq, _)| seq)
            .collect();
        for side in sides {
            let outcome = Outcome::Interrupted { output: None };
            self.append(
                fx,
                Body::ModelResult {
                    call: side,
                    outcome,
                },
            );
        }
        let body = match self.fold.ledger.call(call).map(|open| &open.call) {
            Some(Call::Model { .. } | Call::Side { .. }) => Body::ModelResult { call, outcome },
            Some(Call::Tool { .. }) => Body::ToolResult {
                call,
                outcome,
                card: None,
            },
            None => return,
        };
        self.append(fx, body);
    }

    /// An interrupted outcome for call `call`, with the output it logged.
    fn interrupted(&self, call: CallId) -> Outcome {
        let output = (self.fold.ledger.call(call)).and_then(|open| open.progress.clone());
        Outcome::Interrupted { output }
    }

    /// Whether a mutation from `origin` was logged already: a resend,
    /// answered as the first was.
    fn resent(&self, origin: Option<Author>) -> bool {
        origin.is_some_and(|a| {
            self.fold
                .ledger
                .last_of(a.client)
                .is_some_and(|last| a.seq <= last)
        })
    }

    /// The tool and input call `call` runs with: as its gate left it.
    fn tool_of(&self, call: CallId) -> Option<(String, Json)> {
        match &self.fold.ledger.call(call)?.call {
            Call::Tool { tool, input, .. } => {
                let input = self.fold.input_of(call).unwrap_or(input);
                Some((tool.clone(), input.clone()))
            }
            Call::Model { .. } | Call::Side { .. } => None,
        }
    }

    fn safe(&self, call: CallId) -> bool {
        self.tool_of(call)
            .and_then(|(tool, _)| self.registry.callable(&tool))
            .is_some_and(|c| c.replay == Replay::Safe)
    }

    /// The child call `call` waits on.
    fn child_of(&self, call: CallId) -> Option<SessionId> {
        let ledger = &self.fold.ledger;
        let Phase::Waiting(WaitOn::Child { link }) = ledger.call(call)?.phase else {
            return None;
        };
        let link = ledger.links().iter().find(|l| l.seq == link)?;
        Some(link.child)
    }

    /// Where open call `call` stands in the turn; a call the interrupted
    /// turn waits for reports as running.
    fn step_of(&self, call: CallId) -> Option<Step> {
        match &self.turn {
            Turn::Acting { open } => open.iter().find(|(c, _)| *c == call).map(|&(_, s)| s),
            Turn::Stopping { open, .. } => open.contains(&call).then_some(Step::Running),
            Turn::Idle | Turn::Model { .. } => None,
        }
    }

    fn set_step(&mut self, call: CallId, step: Step) {
        if let Turn::Acting { open } = &mut self.turn {
            match open.iter_mut().find(|(c, _)| *c == call) {
                Some((_, s)) => *s = step,
                None => open.push((call, step)),
            }
        }
    }

    /// Takes call `call` off the turn's open calls.
    fn close(&mut self, call: CallId) {
        self.parsed.retain(|(c, _, _)| *c != call);
        match &mut self.turn {
            Turn::Acting { open } => open.retain(|(c, _)| *c != call),
            Turn::Stopping { open, .. } => open.retain(|c| *c != call),
            Turn::Idle | Turn::Model { .. } => {}
        }
    }
}

fn reply(fx: &mut Effects, ticket: Option<Ticket>, result: Result<(), String>) {
    if let Some(ticket) = ticket {
        fx.push(Effect::Reply { ticket, result });
    }
}

/// A model call's outcome, but a failure, which is first offered to retry.
fn outcome(replied: Replied) -> Outcome {
    match replied {
        Replied::Done(output) => Outcome::Done { output },
        Replied::Failed(error) => Outcome::Failed {
            error: error.to_string(),
        },
        Replied::Interrupted(output) => Outcome::Interrupted { output },
    }
}

fn to_json(outcome: &Outcome) -> Json {
    Json::encode(outcome).unwrap_or_default()
}

/// A command's input: the text after its name.
fn args_of(args: String) -> Json {
    Json::encode(&ext::Args { args }).unwrap_or_default()
}
