//! A stand-in registry, two agents and a recording session for the
//! kernel's table tests.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kernel::{
    Action, Actions, Agent, Answer, Callable, Chained, Core, Effect, Effects, Event, Gate, Gated,
    Input, Later, Load, Name, Recover, Registry, Replay, Replied,
};
use protocol::{
    Audience, Author, Body, CallId, ClientId, ClientSeq, Entry, Installed, Json, Mode, Outcome,
    Permission, Seq, SessionId, Verdict, WaitOn,
};
use serde_json::{Value, json};
use uuid::Uuid;

/// What the stand-in policy does with a tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    Allow,
    /// Asks the user for the tools named.
    Ask(&'static [&'static str]),
    /// Denies the tools named.
    Deny(&'static [&'static str]),
    /// Rewrites every input to `{"rewritten": true}`, then denies.
    RewriteThenDeny,
    /// Answers later, allowing.
    Later,
    /// Panics.
    Panic,
}

pub struct Reg {
    pub agent: fn() -> Box<dyn Agent>,
    pub policy: Policy,
    /// Retries a failed model call after this.
    pub retry: Option<Duration>,
    /// Has the agent go on once with this after it first stops.
    pub on_stop: Option<&'static str>,
    pub stopped: AtomicBool,
    pub extensions: &'static [&'static str],
}

impl Reg {
    pub fn new(agent: fn() -> Box<dyn Agent>) -> Self {
        Self {
            agent,
            policy: Policy::Allow,
            retry: None,
            on_stop: None,
            stopped: AtomicBool::new(false),
            extensions: &["fixtures"],
        }
    }

    pub fn echo() -> Self {
        Self::new(|| Box::new(Echo))
    }

    pub fn chat() -> Self {
        Self::new(|| Box::new(Chat::default()))
    }

    pub fn policy(mut self, policy: Policy) -> Self {
        self.policy = policy;
        self
    }

    pub fn retry(mut self, after: Duration) -> Self {
        self.retry = Some(after);
        self
    }

    pub fn on_stop(mut self, text: &'static str) -> Self {
        self.on_stop = Some(text);
        self
    }

    pub fn extensions(mut self, extensions: &'static [&'static str]) -> Self {
        self.extensions = extensions;
        self
    }

    pub fn arc(self) -> Arc<dyn Registry> {
        Arc::new(self)
    }
}

fn name(n: &str) -> Name {
    Name::from(n)
}

impl Registry for Reg {
    fn agent(&self) -> Box<dyn Agent> {
        (self.agent)()
    }

    fn installed(&self) -> Vec<Installed> {
        (self.extensions.iter())
            .map(|n| Installed {
                name: (*n).to_owned(),
                version: 1,
            })
            .collect()
    }

    fn callable(&self, tool: &str) -> Option<Callable> {
        let (audience, replay) = match tool {
            "sleep" | "task" => (Audience::Model, Replay::Safe),
            "bash" | "ask" => (Audience::Model, Replay::Unsafe),
            "jobs" => (Audience::User, Replay::Unsafe),
            _ => return None,
        };
        Some(Callable { audience, replay })
    }

    fn model(&self) -> String {
        "test:model".to_owned()
    }

    fn input(&self, text: String) -> Result<Chained<String>, String> {
        if text == "secret" {
            return Err("input: no secrets".to_owned());
        }
        let Some(loud) = text.strip_prefix("shout ") else {
            return Ok(Chained::new(text));
        };
        let decisions = vec![(name("input"), Verdict::Input { original: text.clone() })];
        Ok(Chained {
            value: loud.to_uppercase(),
            decisions,
        })
    }

    fn gate(&self, _: CallId, tool: &str, input: &Json) -> Answer<Gated> {
        let permission = |permission| (name("policy"), Verdict::Permission { permission });
        let gated = match self.policy {
            Policy::Allow => Gated::run(input.clone()),
            Policy::Ask(tools) if tools.contains(&tool) => Gated {
                input: input.clone(),
                decisions: vec![permission(Permission::Ask)],
                gate: Gate::Ask,
            },
            Policy::Deny(tools) if tools.contains(&tool) => Gated {
                input: input.clone(),
                decisions: vec![permission(Permission::Deny)],
                gate: Gate::Refuse("policy denied this call".to_owned()),
            },
            Policy::Ask(_) | Policy::Deny(_) => Gated::run(input.clone()),
            Policy::RewriteThenDeny => {
                let rewritten = Json::new(&json!({ "rewritten": true }));
                Gated {
                    input: rewritten.clone(),
                    decisions: vec![
                        (name("policy"), Verdict::PreTool { input: rewritten }),
                        permission(Permission::Deny),
                    ],
                    gate: Gate::Refuse("policy denied this call".to_owned()),
                }
            }
            Policy::Later => {
                let input = input.clone();
                return Answer::Later(Later::new(async move { Gated::run(input) }));
            }
            Policy::Panic => panic!("the gate broke"),
        };
        Answer::Now(gated)
    }

    fn request_error(
        &self,
        _: CallId,
        _: u16,
        error: &llm::Error,
    ) -> Option<(Name, Recover)> {
        let after = self.retry.filter(|_| error.is_retryable())?;
        Some((name("retry"), Recover::Retry { after }))
    }

    fn on_stop(&self) -> Option<(Name, String)> {
        let text = self.on_stop?;
        (!self.stopped.swap(true, Ordering::SeqCst)).then(|| (name("policy"), text.to_owned()))
    }
}

/// Echoes messages back. `$ <command>` runs it with `bash`, `sleep <n>`
/// calls `sleep`, `task <input>` calls `task` and `refuse <tool>` refuses
/// a call; a child's result is said back.
pub struct Echo;

impl Agent for Echo {
    fn step(&mut self, input: Input) -> Actions {
        let text = match input {
            Input::User { text } => text,
            Input::ChildDone { outcome, .. } => {
                return Actions::from_iter([Action::Say(format!("child: {}", told(&outcome)))]);
            }
            Input::ToolDone { .. } | Input::ModelDone { .. } => return Actions::new(),
        };
        let call = |tool: &str, input: Value| Action::Call {
            tool: tool.to_owned(),
            input: Json::new(&input),
            id: None,
        };
        let action = if let Some(command) = text.strip_prefix("$ ") {
            call("bash", json!({ "command": command }))
        } else if let Some(seconds) = text.strip_prefix("sleep ") {
            call("sleep", json!({ "seconds": seconds.parse::<u64>().unwrap() }))
        } else if let Some(input) = text.strip_prefix("task ") {
            call("task", json!({ "input": input }))
        } else if let Some(tool) = text.strip_prefix("refuse ") {
            Action::Refuse {
                tool: tool.to_owned(),
                input: Json::null(),
                id: None,
                why: "not now".to_owned(),
            }
        } else {
            Action::Say(text)
        };
        Actions::from_iter([action])
    }
}

/// A model loop: each user message goes to the model, whose reply
/// (`{"calls": [{"tool", "input", "id"}]}`) names the calls to run, and
/// their results go back to it, until it replies with none.
#[derive(Default)]
pub struct Chat {
    pending: usize,
}

impl Agent for Chat {
    fn step(&mut self, input: Input) -> Actions {
        match input {
            Input::User { .. } | Input::ChildDone { .. } => {
                self.pending = 0;
                Actions::from_iter([Action::CallModel])
            }
            Input::ModelDone {
                outcome: Outcome::Done { output },
                ..
            } => {
                let calls: Vec<Value> = output.value()["calls"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                self.pending = calls.len();
                calls
                    .into_iter()
                    .map(|c| Action::Call {
                        tool: c["tool"].as_str().unwrap().to_owned(),
                        input: Json::new(&c["input"]),
                        id: c["id"].as_str().map(str::to_owned),
                    })
                    .collect()
            }
            Input::ToolDone { .. } if self.pending > 0 => {
                self.pending -= 1;
                if self.pending > 0 {
                    return Actions::new();
                }
                Actions::from_iter([Action::CallModel])
            }
            Input::ToolDone { .. } | Input::ModelDone { .. } => Actions::new(),
        }
    }
}

/// What an outcome says, for the echo agent.
fn told(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Done { output } => output.to_string(),
        Outcome::Failed { error } => format!("failed: {error}"),
        Outcome::Interrupted { .. } => "interrupted".to_owned(),
    }
}

/// A session under test: the kernel, and every entry it appended.
pub struct Session {
    pub core: Core,
    pub log: Vec<Entry>,
}

impl Session {
    pub fn new(registry: Arc<dyn Registry>) -> Self {
        Self::resume(registry, Vec::new(), &Load::default()).0
    }

    /// Resumes `log`, returning what the resume did.
    pub fn resume(registry: Arc<dyn Registry>, mut log: Vec<Entry>, load: &Load<'_>) -> (Self, Effects) {
        let (core, fx) = Core::resume(registry, &log, load);
        log.extend(appended(&fx));
        (Self { core, log }, fx)
    }

    pub fn step(&mut self, event: Event) -> Effects {
        let fx = self.core.step(event);
        self.log.extend(appended(&fx));
        fx
    }

    /// Steps through `rows`, each event's effects as expected.
    pub fn table(&mut self, rows: Vec<(Event, Vec<Effect>)>) {
        for (i, (event, want)) in rows.into_iter().enumerate() {
            let shown = format!("{event:?}");
            let got = self.step(event);
            assert_eq!(got.as_slice(), want.as_slice(), "row {i}: {shown}\nlog: {:#?}", self.log);
        }
    }

    pub fn bodies(&self) -> Vec<&Body> {
        self.log.iter().map(|e| &e.body).collect()
    }
}

pub fn appended(fx: &Effects) -> impl Iterator<Item = Entry> + '_ {
    fx.iter().filter_map(|e| match e {
        Effect::Append(entry) => Some(entry.clone()),
        _ => None,
    })
}

// ── events ────────────────────────────────────────────────────────────

pub fn user(text: &str) -> Event {
    Event::Input {
        text: text.to_owned(),
        mode: Mode::Queue,
        origin: None,
        ticket: None,
    }
}

pub fn steer(text: &str) -> Event {
    Event::Input {
        text: text.to_owned(),
        mode: Mode::Steer,
        origin: None,
        ticket: None,
    }
}

pub const fn interrupt() -> Event {
    Event::Interrupt {
        origin: None,
        ticket: None,
    }
}

pub fn approve(call: u64, approved: bool) -> Event {
    Event::Answer {
        call: id(call),
        value: approved.into(),
        origin: None,
        ticket: None,
    }
}

pub fn tool_done(call: u64, outcome: Outcome) -> Event {
    Event::ToolDone {
        call: id(call),
        outcome,
    }
}

pub fn model_done(call: u64, calls: &[(&str, Value)]) -> Event {
    Event::ModelDone {
        call: id(call),
        replied: Replied::Done(reply(calls)),
    }
}

pub fn model_failed(call: u64) -> Event {
    Event::ModelDone {
        call: id(call),
        replied: Replied::Failed(llm::Error::new(llm::Kind::RateLimit, "slow down")),
    }
}

/// A model reply calling `calls`, as the chat agent reads it.
pub fn reply(calls: &[(&str, Value)]) -> Json {
    let calls: Vec<Value> = (calls.iter().enumerate())
        .map(|(i, (tool, input))| json!({ "tool": tool, "input": input, "id": format!("c{i}") }))
        .collect();
    Json::new(&json!({ "calls": calls }))
}

pub fn author(seq: u64) -> Author {
    Author {
        client: ClientId(Uuid::from_u128(0xc1)),
        seq: ClientSeq::new(seq).unwrap(),
    }
}

pub fn child() -> SessionId {
    SessionId(Uuid::from_u128(0xc4))
}

// ── entries and effects ───────────────────────────────────────────────

pub fn id(seq: u64) -> CallId {
    CallId::new(seq).unwrap()
}

pub fn seq(seq: u64) -> Seq {
    Seq::new(seq).unwrap()
}

pub fn append(seq: u64, body: Body) -> Effect {
    Effect::Append(Entry::new(seq, 0, body))
}

pub fn entry(seq: u64, body: Body) -> Entry {
    Entry::new(seq, 0, body)
}

/// Entries numbered from 1.
pub fn log(bodies: impl IntoIterator<Item = Body>) -> Vec<Entry> {
    (1..).zip(bodies).map(|(seq, body)| entry(seq, body)).collect()
}

pub fn input(text: &str) -> Body {
    Body::Input {
        text: text.to_owned(),
        mode: Mode::Queue,
        taken: true,
        origin: None,
    }
}

pub fn queued(text: &str) -> Body {
    Body::Input {
        text: text.to_owned(),
        mode: Mode::Queue,
        taken: false,
        origin: None,
    }
}

pub fn take(input: u64) -> Body {
    Body::Take { input: seq(input) }
}

pub fn said(text: &str) -> Body {
    Body::Assistant {
        text: text.to_owned(),
    }
}

pub fn tool_call(tool: &str, input: Value) -> Body {
    Body::ToolCall {
        tool: tool.to_owned(),
        input: Json::new(&input),
        id: None,
        card: None,
        by: Audience::Model,
    }
}

pub fn bash(command: &str) -> Body {
    tool_call("bash", json!({ "command": command }))
}

pub fn sleep_call(seconds: u64) -> Body {
    tool_call("sleep", json!({ "seconds": seconds }))
}

pub fn tool_result(call: u64, outcome: Outcome) -> Body {
    Body::ToolResult {
        call,
        outcome,
        card: None,
    }
}

pub fn model_call(attempt: u16) -> Body {
    Body::ModelCall {
        model: "test:model".to_owned(),
        attempt,
    }
}

pub fn model_result(call: u64, outcome: Outcome) -> Body {
    Body::ModelResult { call, outcome }
}

pub fn decision(by: &str, call: Option<u64>, d: Verdict) -> Body {
    Body::Decision {
        by: by.to_owned(),
        call: call.map(id),
        d,
    }
}

pub fn permission(call: u64, permission: Permission) -> Body {
    decision("policy", Some(call), Verdict::Permission { permission })
}

pub fn done(output: Value) -> Outcome {
    Outcome::Done {
        output: Json::new(&output),
    }
}

pub fn failed(error: &str) -> Outcome {
    Outcome::Failed {
        error: error.to_owned(),
    }
}

pub const INTERRUPTED: Outcome = Outcome::Interrupted { output: None };

pub fn run_tool(call: u64, tool: &str, input: Value) -> Effect {
    Effect::RunTool {
        call: id(call),
        tool: tool.to_owned(),
        input: Json::new(&input),
    }
}

pub fn run_bash(call: u64, command: &str) -> Effect {
    run_tool(call, "bash", json!({ "command": command }))
}

pub const fn call_model(call: u64, attempt: u16) -> Effect {
    Effect::CallModel {
        call: match CallId::new(call) {
            Some(call) => call,
            None => panic!("no call 0"),
        },
        attempt,
    }
}

pub fn cancel(call: u64) -> Effect {
    Effect::Cancel { call: id(call) }
}

pub fn wait_child(call: u64, link: u64) -> Body {
    Body::Wait {
        call: id(call),
        on: WaitOn::Child { link: seq(link) },
    }
}

pub const BUSY: Effect = Effect::MarkBusy(true);
pub const IDLE: Effect = Effect::MarkBusy(false);
