//! The turn and call state machine: one [`Core`] per session.

use protocol::publication::RunnerKind;
use std::mem;
use std::sync::Arc;

use protocol::ledger::{Call, Ledger, OutcomeKind, Phase, Queued};
use protocol::{
    Audience, Author, Body, CallId, Entry, Envelope, EnvelopeId, Held, Json, Mode, Outcome,
    OutputStream, Seq, SessionId, ToolChoice, Verdict, WaitOn,
};
use smallvec::SmallVec;

use protocol::Card;

use crate::fold::{Fold, Next};
use crate::{
    Action, Admission, Answer, Barriers, Binding, Boundary, Chained, Concurrency, Detach,
    Durability, Event, Gate, Gated, Load, ModelPhase, Name, Note, Owner, QueueView, Recover,
    Replay, Replied, Requirement, Step, StepPlan, Ticket, Turn, Work,
};

/// Who refused a call the agent would not run.
const AGENT: &str = "agent";

/// The most attempts a model call a restart cut off is sent again after,
/// whatever the `request_error` point says: a daemon that keeps dying
/// mid-call cannot retry it forever.
pub const RESTART_CAP: u16 = 10;

/// How a tool call ended, as its result is logged.
struct Ended {
    outcome: Outcome,
    card: Option<Card>,
    held: Vec<Held>,
}

/// A session's kernel: its log folded, its turn, and the registry
/// generation its steps run on.
pub struct Core {
    registry: Arc<dyn Binding>,
    /// The session's id, which its own envelopes are addressed by.
    session: SessionId,
    generation_context: Option<String>,
    fold: Fold,
    turn: Turn,
    /// Whether the session is a child, which reports as it settles.
    child: bool,
    owned_cancelled: bool,
    /// What the busy mark says.
    busy: bool,
    /// The last entry when the session last reported.
    reported: u64,
    /// Why the model call being recovered failed, while a handler decides.
    failed: Option<String>,
    /// Detached calls an interrupt cancelled before its entry, which ends
    /// them, is logged: what they report meanwhile is dropped.
    dropped: SmallVec<[CallId; 1]>,
    barriers: Barriers,
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
        registry: Arc<dyn Binding>,
        log: impl IntoIterator<Item = &'e Entry>,
        load: &Load<'_>,
    ) -> (Self, StepPlan) {
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
    pub fn new(registry: Arc<dyn Binding>) -> Self {
        Self::restore(registry, Ledger::default())
    }

    /// A session folded up to a point at which it was at rest
    /// ([`Core::at_rest`]), from the ledger it had there: the rest of the
    /// fold is empty between turns, and the agent starts each turn afresh,
    /// so a snapshot keeps only the ledger, and the log after it is folded
    /// with [`Core::load`] as a load folds it.
    pub fn restore(registry: Arc<dyn Binding>, ledger: Ledger) -> Self {
        Self {
            fold: Fold::restore(registry.agent(), ledger),
            generation_context: registry.generation_context(),
            registry,
            session: SessionId::NIL,
            turn: Turn::Idle,
            child: false,
            owned_cancelled: false,
            busy: false,
            reported: 0,
            failed: None,
            dropped: SmallVec::new(),
            barriers: Barriers::default(),
        }
    }

    /// The same, with its barriers falling as `barriers` says.
    #[must_use]
    pub const fn with_barriers(mut self, barriers: Barriers) -> Self {
        self.barriers = barriers;
        self
    }

    /// Folds entry `e` of the log being loaded, the one after the last.
    pub fn load(&mut self, e: &Entry) {
        self.fold.apply(e, &*self.registry);
    }

    /// Picks the loaded log up where it leaves off: see [`Core::resume`].
    pub fn resumed(&mut self, load: &Load<'_>) -> StepPlan {
        // What the log shows the agent stepped through is done.
        self.fold.stepped = false;
        if let Some(session) = load.session {
            self.session = session;
        }
        self.child = matches!(load.owner, Owner::Present | Owner::Cancelled);
        self.owned_cancelled = matches!(load.owner, Owner::Gone | Owner::Cancelled);
        self.busy = load.busy;
        let mut fx = StepPlan::default();
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
    #[allow(clippy::too_many_lines, reason = "one arm per event")]
    pub fn step(&mut self, event: Event) -> StepPlan {
        let mut fx = StepPlan::default();
        match event {
            Event::Invoke {
                existing,
                parent,
                name,
                tool,
                input,
                ticket,
            } => {
                let result = self.invoke(existing, parent, name, &tool, &input, &mut fx);
                fx.immediate.push(Work::Invocation { ticket, result });
            }
            Event::Input {
                segments,
                mode,
                force,
                origin,
                ticket,
            } => self.input(segments, mode, force, origin, ticket, &mut fx),
            Event::Withdraw {
                input,
                origin,
                ticket,
            } => {
                let result = self.withdraw(input, origin, &mut fx);
                reply(&mut fx, ticket, result);
            }
            Event::OwnerCancelled => {
                self.owned_cancelled = true;
                self.interrupt(None, &mut fx);
            }
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
                held,
            } => self.tool_done(
                call,
                Ended {
                    outcome,
                    card,
                    held,
                },
                &mut fx,
            ),
            Event::Detached { call } => self.detached(call, &mut fx),
            Event::JobEnded { call, text, held } => {
                self.own(call, EnvelopeId::JOB, text, held, &mut fx);
            }
            Event::Note { call, note } => self.note(call, note, &mut fx),
            Event::Waits { call, on } => self.waits(call, on, &mut fx),
            Event::Spawn {
                call,
                child,
                input,
                intent,
                wait,
            } => self.spawn(call, child, input, intent, wait, &mut fx),
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
            Event::Side(event) => self.side_event(event, &mut fx),
            Event::Prepared {
                call,
                decisions,
                answered,
            } => self.prepared(call, decisions, answered, &mut fx),
            Event::SendEnvelope { outgoing, ticket } => {
                self.send_envelope(&outgoing, ticket, &mut fx);
            }
            Event::Enqueue { envelope, ticket } => self.enqueue(envelope, ticket, &mut fx),
            Event::Delivered {
                id,
                recipient,
                result,
                ticket,
            } => self.delivered(&id, recipient, result, ticket, &mut fx),
            Event::ChildSettled { child, outcome } => self.child_settled(child, &outcome, &mut fx),
            Event::Registry(registry) => {
                if self.at_rest() {
                    self.resolve(registry, &mut fx);
                }
            }
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

    /// Whether nothing is in flight, asked for, queued (but news of a
    /// job's end, which waits for the next model call) or in the
    /// background: the ledger alone says where the session stands, so a
    /// snapshot of it restores the session ([`Core::restore`]).
    pub fn at_rest(&self) -> bool {
        self.turn == Turn::Idle
            && self.failed.is_none()
            && self.fold.at_rest()
            && self.fold.ledger.untaken().iter().all(Queued::notice)
            && self.fold.ledger.detached().is_empty()
            && self.fold.ledger.open_calls().next().is_none()
            && self.fold.ledger.pending_outbox().next().is_none()
    }

    /// What the log says now, through the last entry appended.
    pub const fn ledger(&self) -> &Ledger {
        &self.fold.ledger
    }

    fn finish(&mut self, fx: &mut StepPlan) {
        self.advance(fx);
        self.marks(fx);
        let through = self.fold.ledger.seq();
        if !fx.appends.is_empty() {
            let fact = fx
                .appends
                .iter()
                .any(|entry| !matches!(entry.body, Body::Progress { .. } | Body::Ext { .. }));
            fx.commit = Some(crate::Commit {
                through,
                durability: self.commit(fact, fx.immediate.iter().any(Work::external)),
                publish: fact && self.turn == Turn::Idle,
            });
        }
        for work in mem::take(&mut fx.immediate) {
            let requirement = match &work {
                Work::RunTool { call, .. } | Work::Resume { call, .. } => {
                    Some(if self.safe(*call) {
                        Requirement::Ordered
                    } else {
                        Requirement::Durable
                    })
                }
                Work::Prepare { .. } | Work::Send { .. }
                    if self.registry.runner() == RunnerKind::ModelOnly =>
                {
                    Some(Requirement::Durable)
                }
                Work::Prepare { .. }
                | Work::Send { .. }
                | Work::Sleep { .. }
                | Work::Await { .. }
                | Work::Recover { .. }
                | Work::Kill { .. } => Some(Requirement::Ordered),
                Work::Invocation { .. }
                | Work::Reply { .. }
                | Work::Deliver { .. }
                | Work::Delivery { .. }
                | Work::Side { .. }
                | Work::StartChild { .. }
                | Work::Interrupt { .. }
                | Work::Report { .. } => Some(Requirement::Durable),
                Work::Cancel { .. } | Work::Detach { .. } | Work::MarkBusy(_) => None,
            };
            match requirement {
                Some(requirement) => fx.admitted.push(crate::Guarded {
                    admission: Admission {
                        through,
                        requirement,
                    },
                    work,
                }),
                None => fx.immediate.push(work),
            }
        }
    }

    /// How durable a step's entries are to be: holding `fact`s beyond
    /// notes or not, `before` an external effect or not, with the turn as
    /// it stands.
    fn commit(&self, fact: bool, before: bool) -> Durability {
        let barrier = before
            || match self.barriers {
                Barriers::Step => fact,
                Barriers::Effects => self.turn == Turn::Idle,
            };
        if barrier {
            Durability::Barrier
        } else {
            Durability::Lazy
        }
    }

    /// Sends `outgoing` to each recipient its route resolved, once: the
    /// intent commits before any delivery, and the send is answered once
    /// every recipient has answered.
    fn send_envelope(&mut self, outgoing: &protocol::Outgoing, ticket: Ticket, fx: &mut StepPlan) {
        let id = &outgoing.id;
        if let Some(refusal) = self.fold.ledger.acknowledged(id) {
            let result = refusal.clone().map_or(Ok(()), Err);
            fx.immediate.push(Work::Delivery { ticket, result });
            return;
        }
        if outgoing.recipients.is_empty() {
            fx.immediate.push(Work::Delivery {
                ticket,
                result: Ok(()),
            });
            return;
        }
        if !self.fold.ledger.outbox().iter().any(|e| e.id == *id) {
            self.append(
                fx,
                Body::Outbox {
                    envelope_id: outgoing.id.clone(),
                    recipients: outgoing.recipients.clone(),
                    payload: outgoing.payload.clone(),
                },
            );
        }
        let envelopes: Vec<Envelope> = (self.fold.ledger.outbox().iter())
            .filter(|e| e.id == *id)
            .flat_map(protocol::ledger::Sending::envelopes)
            .collect();
        for envelope in envelopes {
            fx.immediate.push(Work::Deliver {
                envelope,
                ticket: Some(ticket),
            });
        }
    }

    fn enqueue(&mut self, envelope: Envelope, ticket: Ticket, fx: &mut StepPlan) {
        if self.registry.runner() == RunnerKind::ModelOnly
            && !self.fold.ledger.received(&envelope.id)
        {
            self.owned_cancelled = false;
        }
        let result = if self.owned_cancelled {
            Err(protocol::InboxRefusal::Unavailable)
        } else if self.fold.ledger.received(&envelope.id) {
            Ok(())
        } else if self.fold.ledger.inbox_len() >= protocol::INBOX_CAPACITY {
            Err(protocol::InboxRefusal::Full)
        } else {
            self.append(fx, Body::Enqueue { envelope });
            Ok(())
        };
        fx.immediate.push(Work::Delivery { ticket, result });
    }

    fn delivered(
        &mut self,
        id: &EnvelopeId,
        recipient: SessionId,
        result: Result<(), protocol::InboxRefusal>,
        ticket: Option<Ticket>,
        fx: &mut StepPlan,
    ) {
        let pending = (self.fold.ledger.outbox().iter())
            .any(|e| e.id == *id && e.pending.contains(&recipient));
        if pending {
            self.append(
                fx,
                Body::Ack {
                    envelope_id: id.clone(),
                    recipient,
                    refusal: result.err(),
                },
            );
        }
        if let (Some(ticket), Some(settled)) = (ticket, self.fold.ledger.acknowledged(id)) {
            let result = settled.clone().map_or(Ok(()), Err);
            fx.immediate.push(Work::Delivery { ticket, result });
        }
    }

    // ── events ────────────────────────────────────────────────────────

    fn input(
        &mut self,
        segments: protocol::Segments,
        mode: Mode,
        force: bool,
        origin: Option<Author>,
        ticket: Option<Ticket>,
        fx: &mut StepPlan,
    ) {
        if self.owned_cancelled {
            return reply(
                fx,
                ticket,
                Err("conversation ownership was cancelled".into()),
            );
        }
        if self.resent(origin) {
            return reply(fx, ticket, Ok(()));
        }
        // Nothing runs or waits, so nothing comes between: it starts now, a
        // turn on the newest registry generation.
        let taken = self.turn == Turn::Idle
            && self.fold.asked.is_empty()
            && self.fold.ledger.untaken().is_empty();
        let Chained {
            value: segments,
            decisions,
        } = match self.registry.input(segments) {
            Ok(chained) => chained,
            Err(why) => return reply(fx, ticket, Err(why)),
        };
        for (by, d) in decisions {
            self.decide(None, &by, d, fx);
        }
        let force = force && mode == Mode::Steer && !taken;
        self.append(
            fx,
            Body::Input {
                segments,
                mode,
                taken,
                origin,
                force,
            },
        );
        if force {
            self.force(fx);
        }
        reply(fx, ticket, Ok(()));
    }

    /// Brings the running turn's next boundary on now, for a forced steer:
    /// the model call is cut short, its partial reply dropped, and the
    /// running calls that can go on in the background are detached. What
    /// cannot be detached runs on; the steer is read once it is answered.
    fn force(&mut self, fx: &mut StepPlan) {
        match self.turn {
            Turn::Model {
                call,
                phase: ModelPhase::Streaming,
                ..
            } => fx.immediate.push(Work::Cancel { call }),
            Turn::Model { call, .. } => {
                fx.immediate.push(Work::Cancel { call });
                self.failed = None;
                self.turn = Turn::Acting {
                    open: SmallVec::new(),
                };
                self.settle(call, Outcome::Interrupted { output: None }, fx);
            }
            Turn::Acting { ref open } => {
                let running: SmallVec<[CallId; 2]> = (open.iter())
                    .filter(|&&(call, step)| step == Step::Running && self.detachable(call))
                    .map(|&(call, _)| call)
                    .collect();
                fx.immediate
                    .extend(running.into_iter().map(|call| Work::Detach { call }));
            }
            Turn::Idle | Turn::Stopping { .. } => {}
        }
    }

    /// Whether running call `call` may go on in the background: its tool
    /// says so, and it waits on nothing it started (a call of its own).
    fn detachable(&self, call: CallId) -> bool {
        let invoking = (self.fold.ledger.open_calls()).any(
            |(_, open)| matches!(&open.call, Call::Tool { invoke: Some(key), .. } if key.parent == call),
        );
        !invoking
            && self
                .tool_of(call)
                .and_then(|(tool, _)| self.registry.callable(&tool))
                .is_some_and(|c| c.detach == Detach::Background)
    }

    /// Takes back the user's queued input `input`, unless it was taken
    /// first: the first of the two logged wins.
    fn withdraw(
        &mut self,
        input: Seq,
        origin: Option<Author>,
        fx: &mut StepPlan,
    ) -> Result<(), String> {
        if self.resent(origin) {
            return Ok(());
        }
        let queued =
            (self.fold.ledger.untaken().iter()).any(|q| q.seq == input && q.source.visible());
        if !queued {
            return Err(format!("input {input} is no longer queued"));
        }
        self.append(fx, Body::Withdraw { input, origin });
        Ok(())
    }

    /// Running call `call` goes on in the background: its result now is a
    /// placeholder naming its output stream, and its outcome comes back as
    /// the session's own envelope ([`Core::tool_done`]).
    fn detached(&mut self, call: CallId, fx: &mut StepPlan) {
        if !matches!(self.turn, Turn::Acting { .. }) || self.step_of(call) != Some(Step::Running) {
            return;
        }
        self.append(fx, Body::Detached { call });
        self.close(call);
        self.result(call, self.placeholder(call), fx);
    }

    /// Queues an envelope from the session to itself, about its own call
    /// `call`, unless one with its id was queued already.
    fn own(
        &mut self,
        call: CallId,
        purpose: &str,
        payload: String,
        held: Vec<Held>,
        fx: &mut StepPlan,
    ) -> Option<Seq> {
        let envelope = Envelope::own(self.session, call, purpose, payload).holding(held);
        if self.fold.ledger.received(&envelope.id) {
            return None;
        }
        Some(self.append(fx, Body::Enqueue { envelope }).0)
    }

    /// Cancels the calls running in the background, as an interrupt does:
    /// the interrupt's entry ends them, queuing nothing, so they start
    /// nothing; whether there were any.
    fn drop_detached(&mut self, fx: &mut StepPlan) -> bool {
        for d in self.fold.ledger.detached() {
            if !self.dropped.contains(&d.call) {
                self.dropped.push(d.call);
            }
        }
        for &call in &self.dropped {
            fx.immediate.push(Work::Cancel { call });
        }
        !self.dropped.is_empty()
    }

    /// Logs an interrupt, which ends the calls in the background.
    fn interrupted_entry(&mut self, origin: Option<Author>, fx: &mut StepPlan) {
        self.append(fx, Body::Interrupt { origin });
        self.dropped.clear();
    }

    /// Stops the turn running: its model call or the calls it waits on.
    /// An interrupt with no turn running stops nothing, and starts nothing
    /// in any state: what waits settles at once, what runs is cancelled and
    /// settles as it reports.
    fn interrupt(&mut self, origin: Option<Author>, fx: &mut StepPlan) {
        let background = self.drop_detached(fx);
        let detached: Vec<_> = self
            .fold
            .ledger
            .armed()
            .filter_map(|(_, child)| self.fold.owner(child).is_none().then_some(child))
            .collect();
        for child in detached {
            fx.immediate.push(Work::Interrupt { child });
            self.fire(child, &Outcome::Interrupted { output: None }, fx);
        }
        let sides: Vec<_> = self
            .fold
            .ledger
            .open_calls()
            .filter_map(|(call, open)| matches!(open.call, Call::Side { .. }).then_some(call))
            .collect();
        for side in sides {
            fx.immediate.push(Work::Cancel { call: side });
            self.settle(side, Outcome::Interrupted { output: None }, fx);
        }
        let open = match mem::take(&mut self.turn) {
            // With no turn to stop, the calls in the background end now.
            Turn::Idle => {
                if background {
                    self.interrupted_entry(origin, fx);
                }
                return;
            }
            stopping @ Turn::Stopping { .. } => {
                self.turn = stopping;
                return;
            }
            Turn::Model { call, phase, .. } => {
                fx.immediate.push(Work::Cancel { call });
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
                            fx.immediate.push(Work::Cancel { call });
                            left.push(call);
                        }
                        (Step::Queued { .. } | Step::Awaiting { .. }, _) => {
                            fx.immediate.push(Work::Cancel { call });
                            self.settle(call, self.interrupted(call), fx);
                        }
                        // Its child stops first, and reports.
                        (Step::Waiting, Some(child)) => {
                            fx.immediate.push(Work::Interrupt { child });
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
        fx: &mut StepPlan,
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

    fn model_done(&mut self, call: CallId, replied: Replied, fx: &mut StepPlan) {
        match &self.turn {
            &Turn::Model {
                call: sent,
                attempt,
                phase: ModelPhase::Streaming,
            } if sent == call => {
                let failure = match &replied {
                    Replied::Failed(error) => Json::encode(error).ok(),
                    _ => None,
                };
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
                if self.registry.runner() == RunnerKind::ModelOnly && failure.is_some() {
                    self.append(
                        fx,
                        Body::ModelResult {
                            call,
                            r#for: None,
                            outcome,
                            failure,
                        },
                    );
                } else {
                    self.settle(call, outcome, fx);
                }
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
    fn retry(&mut self, call: CallId, attempt: u16, error: &llm::Error, fx: &mut StepPlan) -> bool {
        match self.registry.request_error(call, attempt, error) {
            Answer::Now(None) => false,
            Answer::Now(Some((by, recover))) => {
                self.recovered(call, attempt, &by, recover, error.to_string(), fx);
                true
            }
            Answer::Later(answer) => {
                self.failed = Some(error.to_string());
                fx.immediate.push(Work::Recover { call, answer });
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
        fx: &mut StepPlan,
    ) {
        let next = attempt.saturating_add(1);
        let d = Verdict::Retry {
            attempt: next,
            error,
        };
        self.decide(Some(call), by, d, fx);
        match recover {
            Recover::Retry { after } => {
                fx.immediate.push(Work::Sleep { call, after });
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
        fx: &mut StepPlan,
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

    fn side_event(&mut self, event: crate::SideEvent, fx: &mut StepPlan) {
        match event {
            crate::SideEvent::Open {
                of,
                purpose,
                model,
                request,
                background,
                ticket,
            } => {
                let side = self.side(of, purpose, model, request, background, fx);
                fx.immediate.push(Work::Side { ticket, side });
            }
            crate::SideEvent::Fact { side, fact, ticket } => match fact {
                crate::SideFact::Retry {
                    by,
                    attempt,
                    failure,
                    after_ms,
                } => self.side_retry(
                    side,
                    &by,
                    Verdict::SideRetry {
                        attempt,
                        failure,
                        after_ms,
                    },
                    ticket,
                    fx,
                ),
                crate::SideFact::Done { outcome, failure } => {
                    self.side_done(side, outcome, failure, ticket, fx);
                }
            },
        }
    }

    fn side_retry(
        &mut self,
        side: CallId,
        by: &str,
        decision: Verdict,
        ticket: Ticket,
        fx: &mut StepPlan,
    ) {
        let open = self.fold.ledger.call(side).is_some_and(|o| {
            matches!(o.call, Call::Side { .. }) || self.registry.runner() == RunnerKind::ModelOnly
        });
        if open {
            self.decide(Some(side), by, decision, fx);
        }
        fx.immediate.push(Work::Reply {
            ticket,
            result: if open {
                Ok(())
            } else {
                Err("side call closed".into())
            },
        });
    }

    /// Logs side request `side`'s outcome, unless it has one.
    fn side_done(
        &mut self,
        side: CallId,
        outcome: Outcome,
        failure: Option<Json>,
        ticket: Ticket,
        fx: &mut StepPlan,
    ) {
        let owner = self
            .fold
            .ledger
            .call(side)
            .and_then(|open| match open.call {
                Call::Side { of } => Some(of),
                _ => None,
            });
        if let Some(of) = owner {
            self.append(
                fx,
                Body::ModelResult {
                    failure,
                    call: side,
                    r#for: Some(of),
                    outcome,
                },
            );
        }
        fx.immediate.push(Work::Reply {
            ticket,
            result: if owner.is_some() {
                Ok(())
            } else {
                Err("side call closed".into())
            },
        });
    }

    /// Logs the intent of a side model request on behalf of call `of`, if
    /// it is open and its own: the side call's id.
    fn side(
        &mut self,
        of: CallId,
        purpose: String,
        model: String,
        request: Json,
        background: bool,
        fx: &mut StepPlan,
    ) -> Option<CallId> {
        let owner = self.fold.ledger.call(of);
        if owner.is_none() && !background
            || owner.is_some_and(|o| matches!(o.call, Call::Side { .. }))
        {
            return None;
        }
        let body = Body::ModelCall {
            request: Some(request),
            model,
            attempt: 0,
            choice: ToolChoice::Auto,
            r#for: Some(of),
            purpose: Some(purpose),
        };
        Some(self.append(fx, body))
    }

    fn publish_generation(&mut self, fx: &mut StepPlan) {
        let Some(text) = self.generation_context.as_ref() else {
            return;
        };
        let key = Json::encode(text).expect("context text encodes");
        if !self.fold.ledger.publications().contains("generation", &key) {
            let publication = protocol::publication::Publication {
                namespace: "generation".into(),
                key,
            };
            self.append(
                fx,
                Body::Context {
                    text: text.clone(),
                    publication: Some(publication),
                },
            );
        }
    }

    /// Has model call `call` prepared again, as attempt `attempt`.
    fn again(&mut self, call: CallId, attempt: u16, fx: &mut StepPlan) {
        self.publish_generation(fx);
        let choice = match self.fold.ledger.call(call).map(|open| &open.call) {
            Some(&Call::Model { choice, .. }) => choice,
            _ => ToolChoice::Auto,
        };
        fx.immediate.push(Work::Prepare {
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

    /// Model call `call`'s request is prepared: what the `step` point
    /// decided is logged, then the request is sent, or a handler's answer
    /// stands as the model's. One prepared for a turn interrupted
    /// meanwhile was never sent: it ends interrupted.
    fn prepared(
        &mut self,
        call: CallId,
        decisions: ext::host::Decided,
        answered: Option<(Name, Replied)>,
        fx: &mut StepPlan,
    ) {
        match &self.turn {
            Turn::Model {
                call: c,
                phase: ModelPhase::Streaming,
                ..
            } if *c == call => {}
            Turn::Stopping { open, .. } if open.contains(&call) => {
                return self.model_done(call, Replied::Interrupted(None), fx);
            }
            _ => return,
        }
        for (by, d) in decisions {
            self.decide(Some(call), &by, d, fx);
        }
        match answered {
            Some((by, replied)) => {
                self.decide(Some(call), &by, Verdict::Answered, fx);
                self.model_done(call, replied, fx);
            }
            None => fx.immediate.push(Work::Send { call }),
        }
    }

    /// Logs a call's outcome as the `post_tool` chain leaves it, with the
    /// card its tool showed it with, unless a handler replaced it. What it
    /// holds stays held either way: a replaced outcome's original is
    /// logged with the decision.
    fn tool_done(&mut self, call: CallId, ended: Ended, fx: &mut StepPlan) {
        let Ended {
            outcome,
            card,
            held,
        } = ended;
        if self.fold.ledger.detached().iter().any(|d| d.call == call) {
            if !self.dropped.contains(&call) {
                self.own(call, EnvelopeId::DETACHED, outcome.payload(), held, fx);
            }
            return;
        }
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
        self.cancel_invocations(call, fx);
        self.close(call);
        self.result(
            call,
            Ended {
                outcome,
                card,
                held,
            },
            fx,
        );
    }

    /// Logs tool call `call`'s result: a boundary at which queued items
    /// may go into it, for a call the model made.
    fn result(&mut self, call: CallId, ended: Ended, fx: &mut StepPlan) {
        let Ended {
            outcome,
            card,
            held,
        } = ended;
        let model = matches!(
            self.fold.ledger.call(call).map(|open| &open.call),
            Some(Call::Tool {
                by: Audience::Model,
                invoke: None,
                ..
            })
        );
        self.append(
            fx,
            Body::ToolResult {
                call,
                outcome,
                card,
                held,
            },
        );
        if model {
            for input in self.dequeue(Boundary::ToolDone(call)) {
                let into = Some(call);
                self.append(fx, Body::Take { input, into });
            }
        }
    }

    /// What of the queue to take up at `boundary`, as the `dequeue` point
    /// answers. A model-only runner answers one input per turn, so what
    /// the user did not send waits for the turn's end.
    fn dequeue(&self, boundary: Boundary) -> SmallVec<[Seq; 4]> {
        let items = self.fold.ledger.untaken();
        if items.is_empty() {
            return SmallVec::new();
        }
        let visible: Vec<Queued>;
        let items =
            if self.registry.runner() == RunnerKind::ModelOnly && boundary != Boundary::TurnEnd {
                visible = items
                    .iter()
                    .filter(|q| q.source.visible())
                    .cloned()
                    .collect();
                &visible[..]
            } else {
                items
            };
        let queue = QueueView::new(items);
        (self.registry.loop_points().dequeue(boundary, queue)).of(boundary, queue)
    }

    /// Extension-owned facts may arrive while an open call waits. Closing
    /// the call revokes every mutation, including optional observations.
    fn note(&mut self, call: CallId, note: Note, fx: &mut StepPlan) {
        let deciding = match &self.turn {
            Turn::Model { call: c, .. } => *c == call,
            _ => matches!(self.step_of(call), Some(Step::Awaiting { .. })),
        };
        let running = self.step_of(call) == Some(Step::Running);
        let ext = matches!(
            note,
            Note::State { .. } | Note::Publish { .. } | Note::Edit { .. } | Note::Fact(_)
        );
        let fact = matches!(note, Note::Fact(_)) && self.step_of(call).is_some();
        if !(running || deciding && ext || fact) {
            return;
        }
        let body = match note {
            Note::Fact(fact) => Body::Fact { fact },
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
            Note::Publish {
                namespace,
                key,
                text,
            } => {
                if !self.fold.ledger.has_context()
                    && let Some(text) = self.registry.context()
                {
                    self.append(
                        fx,
                        Body::Context {
                            text,
                            publication: None,
                        },
                    );
                }
                Body::Context {
                    text,
                    publication: Some(protocol::publication::Publication { namespace, key }),
                }
            }
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

    fn waits(&mut self, call: CallId, on: WaitOn, fx: &mut StepPlan) {
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
        intent: Json,
        wait: bool,
        fx: &mut StepPlan,
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
                intent,
            },
        );
        if wait {
            let on = WaitOn::Child { link: link.0 };
            self.append(fx, Body::Wait { call, on });
            self.set_step(call, Step::Waiting);
        }
        self.append(fx, Body::Watch { child });
        fx.immediate.push(Work::StartChild {
            link: link.0,
            child,
            input,
        });
    }

    /// Fires the watches on `child`. A call waiting on it goes on with its
    /// result; otherwise the result is queued, as the session's own
    /// envelope under the watch, for the agent to take up.
    fn child_settled(&mut self, child: SessionId, outcome: &Outcome, fx: &mut StepPlan) {
        let Some(call) = self.fold.owner(child) else {
            self.fire(child, outcome, fx);
            self.queue_children(fx);
            return;
        };
        if self.fire(child, outcome, fx).is_empty() {
            return;
        }
        match (&self.turn, self.step_of(call)) {
            (Turn::Stopping { .. }, Some(_)) => {
                self.close(call);
                self.settle(call, self.interrupted(call), fx);
            }
            (_, Some(Step::Waiting)) => self.resume_call(call, to_json(outcome), fx),
            _ => {}
        }
    }

    /// Logs each armed watch on `child` fired: the watches.
    fn fire(
        &mut self,
        child: SessionId,
        outcome: &Outcome,
        fx: &mut StepPlan,
    ) -> SmallVec<[Seq; 1]> {
        let watches: SmallVec<[Seq; 1]> = (self.fold.ledger.armed())
            .filter_map(|(watch, watched)| (watched == child).then_some(watch))
            .collect();
        for &watch in &watches {
            let outcome = outcome.clone();
            self.append(fx, Body::Fired { watch, outcome });
        }
        watches
    }

    // ── the turn ──────────────────────────────────────────────────────

    /// Carries out what is asked for while nothing is open: the agent's
    /// next action, or a shared call alongside the shared calls open, the
    /// queue's boundary before a model call taken first; once it stopped,
    /// what the `on_stop` point gives it; then the queue's boundary at the
    /// turn's end.
    #[allow(clippy::too_many_lines)]
    fn advance(&mut self, fx: &mut StepPlan) {
        loop {
            self.admit_queued(fx);
            match &self.turn {
                Turn::Model { .. } => return,
                Turn::Acting { open } if !open.is_empty() => {
                    let alongside = match self.fold.asked.front() {
                        Some(Next::Agent(Action::Refuse { .. })) => true,
                        Some(Next::Agent(Action::Call { tool, .. })) => {
                            self.alongside(tool, None, open)
                        }
                        _ => false,
                    };
                    if !alongside {
                        return;
                    }
                }
                Turn::Stopping { open, .. } if !open.is_empty() => return,
                Turn::Stopping { .. } => {
                    self.stopped(fx);
                    continue;
                }
                Turn::Acting { .. } | Turn::Idle => {}
            }
            if self.owned_cancelled {
                let pending: Vec<_> = (self.fold.ledger.pending_outbox())
                    .flat_map(|e| e.pending.iter().map(|&to| (e.id.clone(), to)))
                    .collect();
                for (envelope_id, recipient) in pending {
                    self.append(
                        fx,
                        Body::Ack {
                            envelope_id,
                            recipient,
                            refusal: Some(protocol::InboxRefusal::Interrupted),
                        },
                    );
                }
                let queued = !self.fold.ledger.untaken().is_empty();
                while let Some(input) = self.fold.ledger.untaken().first().map(|q| q.seq) {
                    self.append(fx, Body::Take { input, into: None });
                }
                if queued {
                    self.interrupted_entry(None, fx);
                }
                self.fold.asked.clear();
                self.fold.stepped = false;
                self.turn = Turn::Idle;
                return;
            }
            let next_request = matches!(&self.turn, Turn::Acting { open } if open.is_empty())
                && matches!(
                    self.fold.asked.front(),
                    Some(Next::Agent(Action::CallModel { .. }))
                );
            if next_request {
                // Read at the request's tail, which keeps its prefix cached.
                for input in self.dequeue(Boundary::BeforeModel) {
                    self.append(fx, Body::Take { input, into: None });
                }
            }
            if let Some(next) = self.fold.asked.front().cloned() {
                if !matches!(&self.turn, Turn::Acting { open } if !open.is_empty()) {
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
            let taken = self.dequeue(Boundary::TurnEnd);
            if taken.is_empty() {
                return;
            }
            self.turn = Turn::Acting {
                open: SmallVec::new(),
            };
            for input in taken {
                self.append(fx, Body::Take { input, into: None });
            }
        }
    }

    fn ancestors(&self, call: CallId) -> impl Iterator<Item = CallId> + '_ {
        std::iter::successors(Some(call), |call| {
            match self.fold.ledger.call(*call).map(|open| &open.call) {
                Some(Call::Tool {
                    invoke: Some(key), ..
                }) => Some(key.parent),
                _ => None,
            }
        })
    }

    fn alongside(&self, tool: &str, call: Option<CallId>, open: &[(CallId, Step)]) -> bool {
        let shared = self.shared(tool);
        open.iter().all(|(other, step)| {
            matches!(step, Step::Queued { .. })
                || call.is_some_and(|call| self.ancestors(call).any(|ancestor| ancestor == *other))
                || shared
                    && self
                        .tool_of(*other)
                        .is_some_and(|(tool, _)| self.shared(&tool))
        })
    }

    /// Whether calls of `tool` run alongside others.
    fn shared(&self, tool: &str) -> bool {
        self.registry
            .callable(tool)
            .is_some_and(|c| c.concurrency == Concurrency::Shared)
    }

    /// Carries out `next`: its entry takes it off what is asked for.
    fn act(&mut self, next: Next, fx: &mut StepPlan) {
        match next {
            Next::Agent(Action::Say(text)) => {
                self.append(fx, Body::Assistant { text });
            }
            Next::Agent(Action::CallModel { choice }) => {
                if !self.fold.ledger.has_context()
                    && let Some(text) = self.registry.context()
                {
                    self.append(
                        fx,
                        Body::Context {
                            text,
                            publication: None,
                        },
                    );
                }
                self.publish_generation(fx);
                let model = self.registry.model();
                let body = Body::ModelCall {
                    request: None,
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
                let call = self.call(&tool, &input, id, Audience::Model, None, fx);
                self.set_step(call, Step::Queued { recovered: false });
            }
            Next::Agent(Action::Refuse {
                tool,
                input,
                id,
                why,
            }) => {
                let call = self.call(&tool, &input, id, Audience::Model, None, fx);
                let reason = why.clone();
                self.decide(Some(call), AGENT, Verdict::Block { reason }, fx);
                self.settle(call, Outcome::Failed { error: why }, fx);
            }
            Next::Command { name, args } => {
                let input = args_of(args);
                let call = self.call(&name, &input, None, Audience::User, None, fx);
                self.set_step(call, Step::Queued { recovered: false });
            }
        }
    }

    fn invoke(
        &mut self,
        existing: Option<CallId>,
        parent: CallId,
        name: String,
        tool: &str,
        input: &Json,
        fx: &mut StepPlan,
    ) -> Result<CallId, String> {
        if self.step_of(parent) != Some(Step::Running) {
            return Err("invocation owner is not running".into());
        }
        let key = protocol::Invocation { parent, name };
        if let Some(call) = existing {
            if !matches!(self.fold.ledger.call(call).map(|open| &open.call),
                Some(Call::Tool { invoke: Some(owner), .. }) if owner == &key)
            {
                return Err("invocation is not open".into());
            }
            if self.step_of(call).is_none() {
                self.set_step(call, Step::Queued { recovered: true });
            }
            return Ok(call);
        }
        if !self.shared(tool)
            && self.ancestors(parent).any(|ancestor| {
                self.tool_of(ancestor)
                    .is_some_and(|(tool, _)| self.shared(&tool))
            })
        {
            return Err("exclusive invocation requires exclusive ancestors".into());
        }
        let call = self.call(tool, input, None, Audience::Model, Some(key), fx);
        self.set_step(call, Step::Queued { recovered: false });
        Ok(call)
    }

    fn admit_queued(&mut self, fx: &mut StepPlan) {
        let queued: Vec<_> = match &self.turn {
            Turn::Acting { open } => open
                .iter()
                .filter_map(|(call, step)| {
                    matches!(step, Step::Queued { .. }).then_some((*call, *step))
                })
                .collect(),
            _ => return,
        };
        for (call, step) in queued {
            let Some((tool, input)) = self.tool_of(call) else {
                continue;
            };
            let Turn::Acting { open } = &self.turn else {
                return;
            };
            if self.alongside(&tool, Some(call), open) {
                if step == (Step::Queued { recovered: true }) {
                    let phase = self
                        .fold
                        .ledger
                        .call(call)
                        .expect("open invocation")
                        .phase
                        .clone();
                    self.reopen(call, &tool, &input, &phase, fx);
                } else {
                    self.gate(call, &tool, &input, fx);
                }
            }
        }
    }

    fn cancel_invocations(&mut self, parent: CallId, fx: &mut StepPlan) {
        let children: Vec<_> = self
            .fold
            .ledger
            .open_calls()
            .filter_map(|(call, open)| {
                matches!(&open.call, Call::Tool { invoke: Some(key), .. } if key.parent == parent)
                    .then_some(call)
            })
            .collect();
        for call in children {
            fx.immediate.push(Work::Cancel { call });
            self.close(call);
            self.settle(call, self.interrupted(call), fx);
        }
    }

    /// Logs the intent to call `tool`: its input read by its tool once,
    /// here, for the card and for the run.
    fn call(
        &mut self,
        tool: &str,
        input: &Json,
        id: Option<String>,
        by: Audience,
        invoke: Option<protocol::Invocation>,
        fx: &mut StepPlan,
    ) -> CallId {
        let next = self.next_seq();
        let card = self.registry.read(next, tool, input);
        let body = Body::ToolCall {
            invoke,
            tool: tool.to_owned(),
            input: input.clone(),
            id,
            card,
            by,
        };
        self.append(fx, body)
    }

    fn run_tool(&mut self, call: CallId, tool: String, input: Json, fx: &mut StepPlan) {
        self.set_step(call, Step::Running);
        fx.immediate.push(Work::RunTool { call, tool, input });
    }

    fn gate(&mut self, call: CallId, tool: &str, input: &Json, fx: &mut StepPlan) {
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
                fx.immediate.push(Work::Await { call, answer });
            }
        }
    }

    /// Logs what call `call`'s gate decided (but the first `skip`, logged
    /// already), then runs it, has it wait for an approval or answers it
    /// with why not. One gated before a restart may have run: an unsafe
    /// one is interrupted rather than run twice.
    fn gated(&mut self, call: CallId, gated: Gated, skip: u16, resumed: bool, fx: &mut StepPlan) {
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
    fn run(&mut self, call: CallId, fx: &mut StepPlan) {
        let Some((tool, input)) = self.tool_of(call) else {
            return;
        };
        self.run_tool(call, tool, input, fx);
    }

    /// Hands call `call` what it waited on.
    fn resume_call(&mut self, call: CallId, value: Json, fx: &mut StepPlan) {
        let Some((tool, input)) = self.tool_of(call) else {
            return;
        };
        self.set_step(call, Step::Running);
        fx.immediate.push(Work::Resume {
            call,
            tool,
            input,
            value,
        });
    }

    /// Ends an interrupted turn whose calls have settled. Every tool call
    /// the agent asked for is answered, interrupted, so that each call a
    /// model made has its answer; nothing else it asked for is done.
    fn stopped(&mut self, fx: &mut StepPlan) {
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
            let call = self.call(&tool, &input, id, by, None, fx);
            self.settle(call, Outcome::Interrupted { output: None }, fx);
        }
        self.interrupted_entry(origin, fx);
        self.fold.stepped = false;
    }

    /// Takes up a registry generation at rest,
    /// logging the extensions it runs when they changed.
    fn resolve(&mut self, next: Arc<dyn Binding>, fx: &mut StepPlan) {
        let installed = next.installed();
        let changed = installed != self.registry.installed();
        self.generation_context = next.generation_context();
        self.registry = next;
        if changed {
            self.append(fx, Body::Extensions { installed });
        }
    }

    /// Marks the session busy while it has work in flight (waiting is not
    /// work), and has a child report each time it settles.
    fn marks(&mut self, fx: &mut StepPlan) {
        let ledger = &self.fold.ledger;
        let armed = ledger.armed().next().is_some();
        let queued = !ledger.untaken().iter().all(Queued::notice);
        let working = match &self.turn {
            Turn::Idle => false,
            // A call waiting on its child waits on work of its own.
            Turn::Acting { open } => (open.iter())
                .any(|&(call, step)| step != Step::Waiting || self.child_of(call).is_some()),
            Turn::Model { .. } | Turn::Stopping { .. } => true,
        };
        let outgoing = ledger.pending_outbox().next().is_some();
        let background = !ledger.detached().is_empty();
        let busy = working || queued || armed || outgoing || background;
        if busy != self.busy {
            self.busy = busy;
            // Marked before the work starts, cleared after it ends.
            if busy {
                fx.immediate.insert(0, Work::MarkBusy(true));
            } else {
                fx.immediate.push(Work::MarkBusy(false));
            }
        }
        // Settled with something new since it last reported (one given no
        // input yet has not run).
        let settled = self.turn == Turn::Idle && !queued && !armed && !outgoing && !background;
        let ran = ledger.turn_start() > 0;
        if settled && self.child && ran && ledger.seq() > self.reported {
            self.reported = ledger.seq();
            let outcome = ledger.result().cloned().unwrap_or_else(|| Outcome::Failed {
                error: "the child settled without a result".to_owned(),
            });
            fx.immediate.push(Work::Report { outcome });
        }
    }

    // ── recovery ──────────────────────────────────────────────────────

    fn recover(&mut self, load: &Load<'_>, fx: &mut StepPlan) {
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
        fx.immediate
            .extend(kills.into_iter().map(|pgid| Work::Kill { pgid }));
        if self.owned_cancelled {
            self.cancel_recovered(fx);
            return;
        }
        if !load.abandoned.is_empty() {
            self.cancel_recovered(fx);
            for cancelled in load.abandoned {
                let input = cancelled.input;
                if self.fold.ledger.untaken().iter().any(|q| q.seq == input) {
                    self.append(fx, Body::Take { input, into: None });
                }
            }
            self.interrupted_entry(None, fx);
        }
        self.relink(fx);
        self.queue_children(fx);
        self.placeholders(fx);
        self.recover_detached(fx);
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
        for (call, kind, phase) in open {
            match kind {
                Call::Model { attempt, .. } => {
                    if self.registry.runner() == RunnerKind::Agent {
                        self.restart(call, attempt, fx);
                    } else {
                        self.settle(call, Outcome::Interrupted { output: None }, fx);
                    }
                }
                Call::Tool {
                    invoke: Some(key), ..
                } => {
                    if self.fold.ledger.call(key.parent).is_none() {
                        self.settle(call, self.interrupted(call), fx);
                    }
                }
                Call::Tool { tool, input, .. } => self.reopen(call, &tool, &input, &phase, fx),
                // Its owner asks again, if it still wants an answer.
                Call::Side { .. } => self.settle(call, Outcome::Interrupted { output: None }, fx),
            }
        }
    }

    fn cancel_recovered(&mut self, fx: &mut StepPlan) {
        let background = self.drop_detached(fx);
        let children: Vec<_> = self.fold.ledger.armed().map(|(_, child)| child).collect();
        for child in children {
            fx.immediate.push(Work::Interrupt { child });
            self.fire(child, &Outcome::Interrupted { output: None }, fx);
        }
        let calls: Vec<_> = self
            .fold
            .ledger
            .open_calls()
            .map(|(call, _)| call)
            .chain(self.fold.ledger.open_commands())
            .collect();
        let interrupted = !calls.is_empty() || background;
        for call in calls {
            self.settle(call, self.interrupted(call), fx);
        }
        if interrupted || !matches!(self.fold.ledger.result(), Some(Outcome::Interrupted { .. })) {
            self.interrupted_entry(None, fx);
        }
        self.turn = Turn::Idle;
    }

    /// Queues, as the session's own envelopes, the results of children no
    /// call waits on that have fired and are not queued yet.
    fn queue_children(&mut self, fx: &mut StepPlan) {
        for (watch, outcome) in mem::take(&mut self.fold.unqueued) {
            self.own(
                CallId(watch),
                EnvelopeId::CHILD,
                outcome.payload(),
                Vec::new(),
                fx,
            );
        }
    }

    /// Answers each detached call whose placeholder a crash cut off with
    /// it, as the step that detached it would have, so it is recovered
    /// once, as a call in the background, and never as an open call too.
    fn placeholders(&mut self, fx: &mut StepPlan) {
        let open: SmallVec<[CallId; 1]> = (self.fold.ledger.detached().iter())
            .map(|d| d.call)
            .filter(|&call| self.fold.ledger.call(call).is_some())
            .collect();
        for call in open {
            self.cancel_invocations(call, fx);
            self.result(call, self.placeholder(call), fx);
        }
    }

    /// Calls a restart cut off in the background go on as each tool's
    /// replay says: one safe to run again runs again, still detached; any
    /// other's outcome is that it was cut off and may not have completed,
    /// holding the stream its output so far went to.
    fn recover_detached(&mut self, fx: &mut StepPlan) {
        for d in self.fold.ledger.detached().to_vec() {
            let safe = (self.registry.callable(&d.tool)).is_some_and(|c| c.replay == Replay::Safe);
            if safe {
                fx.immediate.push(Work::RunTool {
                    call: d.call,
                    tool: d.tool,
                    input: d.input,
                });
                continue;
            }
            let stream = OutputStream {
                session: self.session,
                call: d.call,
            };
            let output = "[the daemon stopped while this call ran in the background: it may \
                          not have completed; its output so far is kept]";
            let outcome = Outcome::Interrupted {
                output: Some(output.to_owned()),
            };
            let held = vec![Held::Stream(stream)];
            self.own(d.call, EnvelopeId::DETACHED, outcome.payload(), held, fx);
        }
    }

    /// Re-arms links: a link not yet watched is watched, and every child
    /// still watched is started, so it runs to its end and reports back.
    fn relink(&mut self, fx: &mut StepPlan) {
        let unacknowledged =
            (self.fold.ledger.outbox().iter()).flat_map(protocol::ledger::Sending::envelopes);
        fx.immediate
            .extend(unacknowledged.map(|envelope| Work::Deliver {
                envelope,
                ticket: None,
            }));
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
            fx.immediate.push(Work::StartChild { link, child, input });
        }
    }

    /// A model call a restart cut off: the attempt it was on failed, and the
    /// `request_error` point says whether there is another, so restarts
    /// count as attempts and a crash loop cannot retry forever.
    fn restart(&mut self, call: CallId, attempt: u16, fx: &mut StepPlan) {
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
    fn reopen(&mut self, call: CallId, tool: &str, input: &Json, phase: &Phase, fx: &mut StepPlan) {
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
                    fx.immediate.push(Work::Await { call, answer });
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
    fn append(&mut self, fx: &mut StepPlan, body: Body) -> CallId {
        let next = self.next_seq();
        let entry = Entry::new(next.0, protocol::Epoch::default(), body);
        self.fold.apply(&entry, &*self.registry);
        fx.appends.push(entry);
        next
    }

    /// The seq the next entry appended gets.
    fn next_seq(&self) -> CallId {
        CallId::new(self.fold.ledger.seq() + 1).expect("an entry after the last has a seq")
    }

    fn decide(&mut self, call: Option<CallId>, by: &str, d: Verdict, fx: &mut StepPlan) {
        let by = by.to_owned();
        self.append(fx, Body::Decision { by, call, d });
    }

    /// Logs call `call`'s outcome. One that did not finish (it failed, or
    /// was interrupted) takes the side requests made on its behalf that are
    /// still out with it, their ends logged first; one done leaves them to
    /// end as they do.
    fn settle(&mut self, call: CallId, outcome: Outcome, fx: &mut StepPlan) {
        self.cancel_invocations(call, fx);
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
                    r#for: Some(call),
                    failure: None,
                    call: side,
                    outcome,
                },
            );
        }
        let body = match self.fold.ledger.call(call).map(|open| &open.call) {
            Some(kind @ (Call::Model { .. } | Call::Side { .. })) => Body::ModelResult {
                r#for: match kind {
                    Call::Side { of } => Some(*of),
                    _ => None,
                },
                failure: None,
                call,
                outcome,
            },
            Some(Call::Tool { .. }) => Body::ToolResult {
                call,
                outcome,
                card: None,
                held: Vec::new(),
            },
            None => return,
        };
        self.append(fx, body);
    }

    /// The result of call `call`, detached: done, naming the stream its
    /// output goes on in, which it holds.
    fn placeholder(&self, call: CallId) -> Ended {
        let stream = OutputStream {
            session: self.session,
            call,
        };
        Ended {
            outcome: Outcome::detached(),
            card: None,
            held: vec![Held::Stream(stream)],
        }
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
    pub fn tool_of(&self, call: CallId) -> Option<(String, Json)> {
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
        match &mut self.turn {
            Turn::Acting { open } => open.retain(|(c, _)| *c != call),
            Turn::Stopping { open, .. } => open.retain(|c| *c != call),
            Turn::Idle | Turn::Model { .. } => {}
        }
    }
}

fn reply(fx: &mut StepPlan, ticket: Option<Ticket>, result: Result<(), String>) {
    if let Some(ticket) = ticket {
        fx.immediate.push(Work::Reply { ticket, result });
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

#[cfg(test)]
mod fact_tests {
    use super::*;

    struct Registry;

    impl Binding for Registry {
        fn agent(&self) -> Box<dyn crate::Agent> {
            Box::new(ext::Loop::default())
        }
        fn installed(&self) -> Vec<protocol::Installed> {
            Vec::new()
        }
        fn callable(&self, _: &str) -> Option<ext::Callable> {
            None
        }
    }

    #[test]
    fn extension_facts_commit_for_open_waits_without_changing_execution() {
        let mut core = Core::new(Arc::new(Registry));
        let call = CallId::new(1).unwrap();
        core.turn = Turn::Acting {
            open: [(call, Step::Waiting)].into_iter().collect(),
        };
        let turn = core.turn.clone();
        let fact = protocol::ExtensionFact::Annotation(protocol::Annotation {
            target: call,
            lifetime: protocol::AnnotationLifetime::Question,
            card: Card::default(),
        });
        let mut plan = StepPlan::default();
        core.note(call, Note::Fact(fact.clone()), &mut plan);
        assert_eq!(plan.appends.len(), 1);
        assert_eq!(plan.appends[0].body, Body::Fact { fact: fact.clone() });
        assert_eq!(core.turn, turn);
        assert_eq!(plan.immediate, []);
        core.close(call);
        core.note(call, Note::Fact(fact), &mut plan);
        assert_eq!(plan.appends.len(), 1);
    }
}
