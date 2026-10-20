//! A session's log folded: what the ledger says, the agent stepped through
//! every input the log records, and what the kernel reads that the ledger
//! does not keep. The kernel folds each entry it appends exactly as it
//! folds one it loads, so a live session and its reloaded log agree.

use std::collections::VecDeque;

use protocol::ledger::{Call, Ledger};
use protocol::{Audience, Body, CallId, Entry, Json, Outcome, Seq, Verdict, WaitOn};

use crate::{Action, Agent, Binding, Input};

/// What is asked for and not yet logged: the agent's actions, and a user's
/// command taken up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    Agent(Action),
    Command { name: String, args: String },
}

pub struct Fold {
    pub ledger: Ledger,
    agent: Box<dyn Agent>,
    /// What was asked for that the log does not show yet, in order: the
    /// front is carried out next, and the entry that carries it out takes
    /// it off. What is left after a load is what a crash cut off.
    pub asked: VecDeque<Next>,
    /// Whether the agent was given an input since the `on_stop` point was
    /// last asked.
    pub stepped: bool,
    /// The inputs the `pre_tool` chain left open calls with.
    inputs: Vec<(CallId, Json)>,
    /// The tool calls waiting on a child, by the child's link.
    owners: Vec<(Seq, CallId)>,
    /// Results of children that calls wait on, not yet handed to them.
    fired: Vec<(CallId, Outcome)>,
    /// The calls the agent refused, with why, until answered: a crash
    /// before the answer commits must not run them.
    refused: Vec<(CallId, String)>,
}

impl Fold {
    pub fn new(agent: Box<dyn Agent>) -> Self {
        Self {
            ledger: Ledger::default(),
            agent,
            asked: VecDeque::new(),
            stepped: false,
            inputs: Vec::new(),
            owners: Vec::new(),
            fired: Vec::new(),
            refused: Vec::new(),
        }
    }

    /// Folds entry `e`, the one after the last folded: the agent is given
    /// the input it records, if any.
    pub fn apply(&mut self, e: &Entry, registry: &dyn Binding) {
        let input = self.input(e, registry);
        self.ledger.apply(e);
        if let Some(input) = input {
            self.stepped = true;
            let actions = self.agent.step(input, registry.loop_points());
            self.asked.extend(actions.into_iter().map(Next::Agent));
        }
    }

    /// The agent's input `body` records, read against the log before it;
    /// and what else of it the kernel keeps.
    fn input(&mut self, e: &Entry, registry: &dyn Binding) -> Option<Input> {
        let body = &e.body;
        match body {
            Body::Input {
                text, taken: true, ..
            } => self.user(text, registry),
            Body::Take { input } => {
                let queued = self
                    .ledger
                    .untaken()
                    .iter()
                    .find(|q| q.seq == input.get())?;
                // A steer taken just before a model call is the model's to
                // read; one left when the turn ended starts the next.
                let steers = queued.steer
                    && matches!(
                        self.asked.front(),
                        Some(Next::Agent(Action::CallModel { .. }))
                    );
                let text = queued.text.clone();
                if steers {
                    None
                } else {
                    self.user(&text, registry)
                }
            }
            Body::Decision {
                d: Verdict::OnStop { text },
                ..
            } => Some(Input::User { text: text.clone() }),
            Body::Decision {
                call: Some(call),
                d: Verdict::PreTool { input },
                ..
            } => {
                self.inputs.retain(|(c, _)| c != call);
                self.inputs.push((*call, input.clone()));
                None
            }
            Body::Assistant { .. } | Body::ModelCall { .. } | Body::ToolCall { .. } => {
                self.intent(e);
                None
            }
            Body::ModelResult { call, outcome } => {
                let call = CallId::new(*call)?;
                // A side request's reply is its extension's.
                let open = self.ledger.call(call.get())?;
                if matches!(open.call, Call::Side { .. }) {
                    return None;
                }
                Some(Input::ModelDone {
                    call,
                    outcome: outcome.clone(),
                })
            }
            Body::ToolResult { call, outcome, .. } => {
                let call = CallId::new(*call)?;
                let open = self.ledger.call(call.get())?;
                self.inputs.retain(|(c, _)| *c != call);
                self.fired.retain(|(c, _)| *c != call);
                self.owners.retain(|(_, c)| *c != call);
                self.refused.retain(|(c, _)| *c != call);
                let by_user = matches!(
                    open.call,
                    Call::Tool {
                        by: Audience::User,
                        ..
                    }
                );
                (!by_user).then(|| Input::ToolDone {
                    call,
                    outcome: outcome.clone(),
                })
            }
            Body::Wait {
                call,
                on: WaitOn::Child { link },
            } => {
                self.owners.push((*link, *call));
                None
            }
            Body::Fired { watch, outcome } => {
                let watch = Seq::new(*watch)?;
                let child = self.watched(watch)?;
                if let Some(call) = self.owner(child) {
                    self.fired.push((call, outcome.clone()));
                    return None;
                }
                Some(Input::ChildDone {
                    watch,
                    outcome: outcome.clone(),
                })
            }
            // What the turn asked for and had not started is dropped.
            Body::Interrupt { .. } => {
                self.asked.clear();
                None
            }
            _ => None,
        }
    }

    /// An intent folded: it carries out what was asked for next.
    fn intent(&mut self, e: &Entry) {
        match &e.body {
            Body::Assistant { text } => {
                self.carried(|next| matches!(next, Next::Agent(Action::Say(said)) if said == text));
            }
            Body::ModelCall { attempt, r#for, .. } => {
                // A retry is not the agent's to ask for, nor a side request.
                if *attempt == 0 && r#for.is_none() {
                    self.carried(|next| matches!(next, Next::Agent(Action::CallModel { .. })));
                }
            }
            Body::ToolCall {
                tool,
                by: Audience::Model,
                ..
            } => {
                let refused = match self.asked.front() {
                    Some(Next::Agent(Action::Refuse { tool: t, why, .. })) if t == tool => {
                        Some(why.clone())
                    }
                    _ => None,
                };
                self.carried(|next| {
                    matches!(next, Next::Agent(Action::Call { tool: t, .. } | Action::Refuse { tool: t, .. }) if t == tool)
                });
                if let (Some(why), Some(call)) = (refused, CallId::new(e.seq)) {
                    self.refused.push((call, why));
                }
            }
            Body::ToolCall {
                tool,
                by: Audience::User,
                ..
            } => {
                // A command taken up.
                if matches!(self.asked.front(), Some(Next::Command { name, .. }) if name == tool) {
                    self.asked.pop_front();
                }
            }
            _ => {}
        }
    }

    /// Input `text` taken up: a user's command, or the agent's.
    fn user(&mut self, text: &str, registry: &dyn Binding) -> Option<Input> {
        match command(text, registry) {
            Some((name, args)) => {
                self.asked.push_back(Next::Command {
                    name: name.to_owned(),
                    args: args.to_owned(),
                });
                None
            }
            None => Some(Input::User {
                text: text.to_owned(),
            }),
        }
    }

    /// The entry just folded carries out what was asked for next, if it
    /// matches: else the log disagrees with the agent, and nothing is left
    /// to redo.
    fn carried(&mut self, matches: impl FnOnce(&Next) -> bool) {
        if self.asked.front().is_some_and(matches) {
            self.asked.pop_front();
        } else {
            self.asked.clear();
        }
    }

    /// The child an armed watch watches.
    fn watched(&self, watch: Seq) -> Option<protocol::SessionId> {
        self.ledger
            .armed()
            .find_map(|(seq, child)| (seq == watch.get()).then_some(child))
    }

    /// The open call waiting on `child`, if one does.
    pub fn owner(&self, child: protocol::SessionId) -> Option<CallId> {
        let link = self
            .ledger
            .links()
            .iter()
            .rev()
            .find(|l| l.child == child)?;
        self.owners
            .iter()
            .find_map(|&(l, call)| (l.get() == link.seq).then_some(call))
    }

    /// The input the gate left call `call` with, if it changed it.
    pub fn input_of(&self, call: CallId) -> Option<&Json> {
        self.inputs
            .iter()
            .find_map(|(c, input)| (*c == call).then_some(input))
    }

    /// Why the agent refused call `call`, if it did and it is unanswered.
    pub fn refused(&self, call: CallId) -> Option<&str> {
        self.refused
            .iter()
            .find_map(|(c, why)| (*c == call).then_some(why.as_str()))
    }

    /// The result of the child call `call` waits on, once it fired.
    pub fn fired(&self, call: CallId) -> Option<&Outcome> {
        self.fired
            .iter()
            .find_map(|(c, outcome)| (*c == call).then_some(outcome))
    }
}

/// `text` as a user's command and its arguments: `/name args`, for a
/// callable the user may call.
pub fn command<'a>(text: &'a str, registry: &dyn Binding) -> Option<(&'a str, &'a str)> {
    let rest = text.strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    registry
        .callable(name)
        .is_some_and(|c| c.audience == Audience::User)
        .then(|| (name, args.trim()))
}
