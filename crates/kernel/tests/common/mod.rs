//! The kernel's table tests run against the registry the engine binds,
//! [`ext::host::Bound`], over real extensions written inline: a policy, a
//! retry, an input filter, two agents and the tools' shapes. `Reg` is the
//! builder of one such registry, and a recording session steps the kernel.

#![allow(dead_code)]
// Helpers take `json!` values as they are written in a table.
#![allow(clippy::needless_pass_by_value)]

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ext::host::{Binding, Bound, Registry};
use ext::point::{
    Approve, Input as InputPoint, OnStop, Permission as PermissionPoint, PreTool, RequestError,
    View,
};
use ext::{
    Concurrency, Detach, DynExtension, DynTool, Extension, Later, Name, Parsed, Refused, Registrar,
    Replay, Rewrite, States, Tier,
};
pub use kernel::{StepPlan, Work};

use kernel::{
    Action, Actions, Agent, Answer, Barriers, Core, Durability, Event, Gated, Input, Load,
    LoopPoints, Recover, Replied,
};
use protocol::ledger::Ledger;
use protocol::{
    Audience, Author, Body, CallId, ClientId, ClientSeq, Entry, Epoch, Installed, Json, Mode,
    Outcome, Permission, Segments, Seq, SessionId, ToolChoice, Verdict, WaitOn,
};
use serde_json::{Value, json};
use uuid::Uuid;

/// What the policy extension does with a tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    Allow,
    /// Asks the user for the tools named.
    Ask(&'static [&'static str]),
    /// Denies the tools named.
    Deny(&'static [&'static str]),
    /// Rewrites every input to `{"rewritten": true}`, then denies.
    RewriteThenDeny,
    /// Asks about everything, and approves later.
    Later,
    /// The gate panics: a kernel bug's stand-in, not an extension's.
    Panic,
}

/// How often the retry extension retries a call.
pub const RETRIES: u16 = 3;

/// What makes the registry's extensions, beside the fixed ones.
type Extras = Box<dyn Fn(Registry) -> Result<Registry, Refused>>;

pub struct Reg {
    pub agent: Option<fn() -> Box<dyn Agent>>,
    pub policy: Policy,
    /// Retries a failed model call after this.
    pub retry: Option<Duration>,
    /// Has the agent go on once with this after it first stops.
    pub on_stop: Option<&'static str>,
    pub extensions: &'static [&'static str],
    /// Extensions of the test's own, the loop's handlers among them.
    pub extras: Option<Extras>,
    /// Recovers a failed model call later, by sending it again as the
    /// transcript is then (as compaction does).
    pub resend: bool,
    /// Retries every failure, however often: a retry policy with no limit.
    pub forever: bool,
}

impl Reg {
    pub fn new(agent: fn() -> Box<dyn Agent>) -> Self {
        Self {
            agent: Some(agent),
            policy: Policy::Allow,
            retry: None,
            on_stop: None,
            extensions: &["fixtures"],
            extras: None,
            resend: false,
            forever: false,
        }
    }

    pub fn forever(mut self) -> Self {
        self.forever = true;
        self
    }

    pub fn resend(mut self) -> Self {
        self.resend = true;
        self
    }

    /// The default loop, with the extensions `extras` adds.
    pub fn looping(extras: impl Fn(Registry) -> Result<Registry, Refused> + 'static) -> Self {
        Self {
            agent: None,
            extras: Some(Box::new(extras)),
            ..Self::new(|| Box::new(ext::Loop::default()))
        }
    }

    pub fn echo() -> Self {
        Self::new(|| Box::new(Echo))
    }

    pub fn modeled() -> Self {
        Self::new(|| Box::new(Modeled::default()))
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

    /// The registry, bound to a session as the engine binds it.
    pub fn arc(self) -> Arc<dyn Binding> {
        let registry = Registry::default()
            .with_dyn(Tier::Pre, &Fixtures(self.agent))
            .and_then(|r| r.with_in(Tier::Pre, &Filter))
            .and_then(|r| {
                let policed = Policed {
                    policy: self.policy,
                    on_stop: self.on_stop,
                };
                if policed.policy == Policy::Allow && policed.on_stop.is_none() {
                    Ok(r)
                } else {
                    r.with_in(Tier::Pre, &policed)
                }
            })
            .and_then(|r| {
                let retry = Retrying {
                    after: self.retry,
                    forever: self.forever,
                };
                if retry.after.is_some() || retry.forever {
                    r.with_in(Tier::Post, &retry)
                } else {
                    Ok(r)
                }
            })
            .and_then(|r| {
                if self.resend {
                    r.with_in(Tier::Pre, &Compacting)
                } else {
                    Ok(r)
                }
            })
            .and_then(|r| match &self.extras {
                Some(extras) => extras(r),
                None => Ok(r),
            })
            .expect("the test's extensions install");
        let stub = Arc::new(Stub {
            states: Mutex::new(registry.states()),
        });
        let agent = self.agent.map(|_| AGENT.to_owned());
        Arc::new(Harness {
            bound: Bound::new(Arc::new(registry), stub, agent),
            installed: (self.extensions.iter())
                .map(|n| Installed {
                    name: (*n).to_owned(),
                    version: 1,
                    image: None,
                })
                .collect(),
            panics: self.policy == Policy::Panic,
        })
    }
}

/// The session as the points read it: nothing, but its states.
struct Stub {
    states: Mutex<States>,
}

impl View for Stub {
    fn session(&self) -> SessionId {
        SessionId(Uuid::from_u128(0x5e5))
    }

    fn workspace(&self) -> &Path {
        Path::new("/w")
    }

    fn model(&self) -> String {
        "test:model".to_owned()
    }

    fn states(&self, read: &mut dyn FnMut(&States)) {
        read(&self.states.lock().unwrap());
    }
}

/// The bound registry, listing the extensions the test names rather than
/// the inline ones, and with a gate that panics on request.
struct Harness {
    bound: Bound<Stub>,
    installed: Vec<Installed>,
    panics: bool,
}

impl Binding for Harness {
    fn agent(&self) -> Box<dyn Agent> {
        self.bound.agent()
    }

    fn loop_points(&self) -> &dyn LoopPoints {
        self.bound.loop_points()
    }

    fn installed(&self) -> Vec<Installed> {
        self.installed.clone()
    }

    fn callable(&self, name: &str) -> Option<kernel::Callable> {
        self.bound.callable(name)
    }

    fn model(&self) -> String {
        self.bound.model()
    }

    fn context(&self) -> Option<String> {
        self.bound.context()
    }

    fn read(&self, call: CallId, tool: &str, input: &Json) -> Option<protocol::Card> {
        self.bound.read(call, tool, input)
    }

    fn input(&self, segments: Segments) -> Result<kernel::Chained<Segments>, String> {
        self.bound.input(segments)
    }

    fn gate(&self, call: CallId, tool: &str, input: &Json) -> Answer<Gated> {
        assert!(!self.panics, "the gate broke");
        self.bound.gate(call, tool, input)
    }

    fn request_error(
        &self,
        call: CallId,
        attempt: u16,
        error: &llm::Error,
    ) -> Answer<Option<(Name, Recover)>> {
        self.bound.request_error(call, attempt, error)
    }

    fn post_tool(
        &self,
        call: CallId,
        tool: &str,
        input: &Json,
        outcome: Outcome,
    ) -> kernel::Chained<Outcome> {
        self.bound.post_tool(call, tool, input, outcome)
    }

    fn on_stop(&self) -> Option<(Name, String)> {
        self.bound.on_stop()
    }
}

/// The agent a test runs, by name.
const AGENT: &str = "agent";

/// The tools' shapes, and the agent: what a daemon's fixtures contribute.
struct Fixtures(Option<fn() -> Box<dyn Agent>>);

impl DynExtension for Fixtures {
    fn name(&self) -> Name {
        Name::from("fixtures")
    }

    fn version(&self) -> u32 {
        1
    }

    fn contribute(&self, r: &mut Registrar) {
        let shape = |name, audience, replay, concurrency| {
            Arc::new(Shape {
                name,
                audience,
                replay,
                concurrency,
                // A sleep can go on in the background.
                detach: if name == "sleep" {
                    Detach::Background
                } else {
                    Detach::Never
                },
            }) as Arc<dyn DynTool>
        };
        for name in ["sleep", "task"] {
            r.dyn_tool(shape(
                name,
                Audience::Model,
                Replay::Safe,
                Concurrency::Shared,
            ));
        }
        for name in ["bash", "ask"] {
            r.dyn_tool(shape(
                name,
                Audience::Model,
                Replay::Unsafe,
                Concurrency::Exclusive,
            ));
        }
        r.dyn_tool(shape(
            "jobs",
            Audience::User,
            Replay::Unsafe,
            Concurrency::Exclusive,
        ));
        if let Some(make) = self.0 {
            r.agent(AGENT, make);
        }
    }
}

/// A tool as the kernel knows one: its callable shape. Never run here.
struct Shape {
    name: &'static str,
    audience: Audience,
    replay: Replay,
    concurrency: Concurrency,
    detach: Detach,
}

impl DynTool for Shape {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &'static str {
        "a shape"
    }

    fn audience(&self) -> Audience {
        self.audience
    }

    fn replay(&self) -> Replay {
        self.replay
    }

    fn concurrency(&self) -> Concurrency {
        self.concurrency
    }

    fn detach(&self) -> Detach {
        self.detach
    }

    fn spec(&self) -> llm::Tool {
        llm::Tool {
            name: self.name.into(),
            description: "a shape".into(),
            schema: json!({ "type": "object" }),
            strict: None,
        }
    }

    fn parse(&self, _: &Json) -> Result<Parsed, String> {
        Ok(Parsed::new(()))
    }

    fn call<'a>(&'a self, _: Parsed, _: &'a ext::Cx) -> ext::BoxFuture<'a, ext::Called> {
        Box::pin(std::future::ready(ext::Called::failed(
            "the kernel runs no tool".to_owned(),
        )))
    }

    fn to_model(&self, output: &Json, _places: &ext::blob::Places) -> llm::Output {
        llm::Output::Json(output.clone())
    }

    fn present(&self, _: &Parsed) -> Option<protocol::Card> {
        None
    }
}

/// Refuses the input `secret`, and shouts what starts with `shout `.
struct Filter;

impl Extension for Filter {
    const NAME: &'static str = "input";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.on::<InputPoint>(|_, text| {
            if text == "secret" {
                return Rewrite::Stop("no secrets".to_owned());
            }
            match text.as_text().and_then(|text| text.strip_prefix("shout ")) {
                Some(loud) => Rewrite::Replace(loud.to_uppercase().into()),
                None => Rewrite::Pass,
            }
        });
    }
}

/// The policy, as [`Policy`] says; and, once the agent first stops, the
/// text it goes on with.
struct Policed {
    policy: Policy,
    on_stop: Option<&'static str>,
}

impl Extension for Policed {
    const NAME: &'static str = "policy";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        let policy = self.policy;
        if let Some(text) = self.on_stop {
            let stopped = Arc::new(AtomicBool::new(false));
            r.on::<OnStop>(move |_, ()| {
                (!stopped.swap(true, Ordering::SeqCst)).then(|| text.to_owned())
            });
        }
        r.on::<PermissionPoint>(move |_, call| match policy {
            Policy::Ask(tools) if tools.contains(&call.tool) => {
                Permission::Ask(protocol::ApprovalReason::Mode { mode: "ask".into() })
            }
            Policy::Deny(tools) if tools.contains(&call.tool) => Permission::Deny,
            Policy::RewriteThenDeny => Permission::Deny,
            Policy::Later => Permission::Ask(protocol::ApprovalReason::Mode { mode: "ask".into() }),
            _ => Permission::Allow,
        });
        if policy == Policy::RewriteThenDeny {
            r.on::<PreTool>(|_, _| {
                Rewrite::Replace(Json::encode(&json!({ "rewritten": true })).unwrap())
            });
        }
        if policy == Policy::Later {
            r.on::<Approve>(|_, _| Answer::Later(Later::new(async { Some(true) })));
        }
    }
}

/// Retries what may pass, [`RETRIES`] times after `after`; or forever.
struct Retrying {
    after: Option<Duration>,
    forever: bool,
}

impl Extension for Retrying {
    const NAME: &'static str = "retry";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        let (after, forever) = (self.after, self.forever);
        r.on::<RequestError>(move |_, failed| {
            if forever {
                let after = Duration::ZERO;
                return Answer::Now(Some(Recover::Retry { after }));
            }
            let retry = failed.error.is_retryable() && failed.attempt < RETRIES;
            Answer::Now(
                after
                    .filter(|_| retry)
                    .map(|after| Recover::Retry { after }),
            )
        });
    }
}

/// Answers later to resend, as compaction does.
struct Compacting;

impl Extension for Compacting {
    const NAME: &'static str = "compact";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.on::<RequestError>(|_, _| Answer::Later(Later::new(async { Some(Recover::Resend) })));
    }
}

fn name(n: &str) -> Name {
    Name::from(n)
}

/// Echoes messages back. `$ <command>` runs it with `bash`, `sleep <n>`
/// calls `sleep`, `task <input>` calls `task` and `refuse <tool>` refuses
/// a call; a child's result is said back.
pub struct Echo;

impl Agent for Echo {
    fn step(&mut self, input: Input, _: &dyn LoopPoints) -> Actions {
        let text = match input {
            Input::User { text } => text,
            Input::ChildDone { outcome, .. } => {
                return Actions::from_iter([Action::Say(format!("child: {}", told(&outcome)))]);
            }
            Input::Detached { call, outcome } => {
                let said = format!("detached {call}: {}", told(&outcome));
                return Actions::from_iter([Action::Say(said)]);
            }
            Input::ToolDone { .. } | Input::ModelDone { .. } => return Actions::new(),
        };
        let call = |tool: &str, input: Value| Action::Call {
            tool: tool.to_owned(),
            input: Json::encode(&input).unwrap(),
            id: None,
        };
        let action = if let Some(command) = text.strip_prefix("$ ") {
            call("bash", json!({ "command": command }))
        } else if let Some(seconds) = text.strip_prefix("sleep ") {
            call(
                "sleep",
                json!({ "seconds": seconds.parse::<u64>().unwrap() }),
            )
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
pub struct Modeled {
    pending: usize,
}

impl Agent for Modeled {
    fn step(&mut self, input: Input, _: &dyn LoopPoints) -> Actions {
        match input {
            Input::User { .. } | Input::ChildDone { .. } | Input::Detached { .. } => {
                self.pending = 0;
                Actions::from_iter([Action::CallModel {
                    choice: ToolChoice::Auto,
                }])
            }
            Input::ModelDone {
                outcome: Outcome::Done { output },
                ..
            } => {
                let calls: Vec<Value> = output.parse::<Value>().unwrap()["calls"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                self.pending = calls.len();
                calls
                    .into_iter()
                    .map(|c| Action::Call {
                        tool: c["tool"].as_str().unwrap().to_owned(),
                        input: Json::encode(&c["input"]).unwrap(),
                        id: c["id"].as_str().map(str::to_owned),
                    })
                    .collect()
            }
            Input::ToolDone { .. } if self.pending > 0 => {
                self.pending -= 1;
                if self.pending > 0 {
                    return Actions::new();
                }
                Actions::from_iter([Action::CallModel {
                    choice: ToolChoice::Auto,
                }])
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
    pub fn new(registry: Arc<dyn Binding>) -> Self {
        Self::resume(registry, Vec::new(), &Load::default()).0
    }

    /// A new session whose barriers fall as `barriers` says.
    pub fn with_barriers(registry: Arc<dyn Binding>, barriers: Barriers) -> Self {
        let mut core = Core::new(registry).with_barriers(barriers);
        let fx = core.resumed(&Load::default());
        barriered(&fx, core.ledger().seq());
        let log = fx.appends;
        Self { core, log }
    }

    /// Resumes `log`, returning what the resume did: the entries and the
    /// work, in order. Where their commits fall has invariants of its own
    /// ([`barriered`], held at every step), so they are left out.
    pub fn resume(
        registry: Arc<dyn Binding>,
        mut log: Vec<Entry>,
        load: &Load<'_>,
    ) -> (Self, StepPlan) {
        let (core, plan) = Core::resume(registry, &log, load);
        let fx = plan;
        barriered(&fx, core.ledger().seq());
        log.extend(fx.appends.iter().cloned());
        (Self { core, log }, fx)
    }

    /// Restores the session a snapshot of `ledger` was taken of, after
    /// the first `at` entries of `log`, where it rested; then resumes the
    /// rest of `log`, as [`Session::resume`] resumes the whole. The ledger
    /// goes through JSON, as a snapshot keeps it.
    pub fn restore(
        registry: Arc<dyn Binding>,
        ledger: &Ledger,
        mut log: Vec<Entry>,
        at: usize,
        load: &Load<'_>,
    ) -> (Self, StepPlan) {
        let image = serde_json::to_vec(ledger).unwrap();
        let mut core = Core::restore(registry, serde_json::from_slice(&image).unwrap());
        for e in &log[at..] {
            core.load(e);
        }
        let fx = core.resumed(load);
        barriered(&fx, core.ledger().seq());
        log.extend(fx.appends.iter().cloned());
        (Self { core, log }, fx)
    }

    pub fn step(&mut self, event: Event) -> StepPlan {
        let plan = self.core.step(event);
        barriered(&plan, self.core.ledger().seq());
        self.log.extend(plan.appends.iter().cloned());
        plan
    }

    pub fn table(&mut self, rows: Vec<(Event, Expected)>) {
        for (event, expected) in rows {
            let plan = self.step(event);
            assert_plan(&plan, expected);
        }
    }

    pub fn bodies(&self) -> Vec<&Body> {
        self.log.iter().map(|e| &e.body).collect()
    }
}

pub type Expected = (Vec<Entry>, Vec<Work>);

pub fn work(plan: &StepPlan) -> impl DoubleEndedIterator<Item = &Work> {
    plan.immediate
        .iter()
        .chain(plan.admitted.iter().map(|guarded| &guarded.work))
}

pub fn into_work(plan: StepPlan) -> impl Iterator<Item = Work> {
    plan.immediate
        .into_iter()
        .chain(plan.admitted.into_iter().map(|guarded| guarded.work))
}

pub fn assert_plan(plan: &StepPlan, (entries, work): Expected) {
    assert_eq!(plan.appends, entries, "{plan:?}");
    let (mut immediate, admitted): (Vec<_>, Vec<_>) =
        work.into_iter().partition(|work| !work.external());
    let mut actual: Vec<_> = plan.immediate.iter().collect();
    let phase = |work: &Work| match work {
        Work::MarkBusy(true) => 0,
        Work::Cancel { .. } => 1,
        _ => 2,
    };
    immediate.sort_by_key(&phase);
    actual.sort_by_key(|work| phase(work));
    assert_eq!(actual, immediate.iter().collect::<Vec<_>>(), "{plan:?}");
    assert_eq!(
        plan.admitted
            .iter()
            .map(|guarded| &guarded.work)
            .collect::<Vec<_>>(),
        admitted.iter().collect::<Vec<_>>(),
        "{plan:?}"
    );
}

pub fn barriered(plan: &StepPlan, through: u64) {
    assert_eq!(plan.commit.is_some(), !plan.appends.is_empty(), "{plan:?}");
    if let Some(commit) = plan.commit {
        assert_eq!(commit.through, plan.appends.last().unwrap().seq.get());
        if plan.admitted.iter().any(|guarded| guarded.work.external()) {
            assert_eq!(commit.durability, Durability::Barrier, "{plan:?}");
        }
    }
    assert!(
        plan.immediate.iter().all(|work| !work.external()),
        "{plan:?}"
    );
    assert!(
        plan.admitted
            .iter()
            .all(|guarded| guarded.admission.through == through),
        "{plan:?}"
    );
}

// ── events ────────────────────────────────────────────────────────────

pub fn user(text: &str) -> Event {
    Event::Input {
        segments: text.into(),
        mode: Mode::Queue,
        origin: None,
        ticket: None,
        force: false,
    }
}

pub fn steer(text: &str) -> Event {
    Event::Input {
        segments: text.into(),
        mode: Mode::Steer,
        origin: None,
        ticket: None,
        force: false,
    }
}

/// A steer that brings the turn's next boundary on now.
pub fn forced(text: &str) -> Event {
    Event::Input {
        segments: text.into(),
        mode: Mode::Steer,
        origin: None,
        ticket: None,
        force: true,
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
        value: Json::encode(&approved).unwrap(),
        origin: None,
        ticket: None,
    }
}

pub fn tool_done(call: u64, outcome: Outcome) -> Event {
    Event::ToolDone {
        call: id(call),
        outcome,
        card: None,
        held: Vec::new(),
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

/// The request of model call `call` prepared, with nothing decided.
pub fn prepared(call: u64) -> Event {
    Event::Prepared {
        call: id(call),
        decisions: Vec::new(),
        answered: None,
    }
}

/// A model's reply calling `calls`, as the model call stores it and the
/// default loop reads it.
pub fn response(calls: &[(&str, Value)]) -> Json {
    let content = (calls.iter().enumerate())
        .map(|(i, (tool, input))| {
            llm::AssistantPart::ToolCall(llm::ToolCall {
                id: format!("c{i}").into(),
                name: (*tool).into(),
                input: Json::encode(input).unwrap(),
                invalid: None,
                provider_executed: false,
                meta: llm::Meta::new(),
            })
        })
        .collect();
    let response = llm::Response {
        message: llm::Assistant {
            content,
            origin: None,
        },
        finish: llm::Finish::new(llm::Reason::Stop, None),
        usage: llm::Usage::default(),
        id: None,
        model: None,
        meta: llm::Meta::new(),
    };
    Json::encode(&response).unwrap()
}

/// A model reply calling `calls`, as the modeled agent reads it.
pub fn reply(calls: &[(&str, Value)]) -> Json {
    let calls: Vec<Value> = (calls.iter().enumerate())
        .map(|(i, (tool, input))| json!({ "tool": tool, "input": input, "id": format!("c{i}") }))
        .collect();
    Json::encode(&json!({ "calls": calls })).unwrap()
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

pub fn entry(seq: u64, body: Body) -> Entry {
    Entry::new(Seq::new(seq).unwrap(), Epoch(0), body)
}

/// Entries numbered from 1.
pub fn log(bodies: impl IntoIterator<Item = Body>) -> Vec<Entry> {
    (1..)
        .zip(bodies)
        .map(|(seq, body)| entry(seq, body))
        .collect()
}

pub fn input(text: &str) -> Body {
    Body::Input {
        segments: text.to_owned().into(),
        mode: Mode::Queue,
        taken: true,
        origin: None,
        force: false,
    }
}

pub fn queued(text: &str) -> Body {
    Body::Input {
        segments: text.to_owned().into(),
        mode: Mode::Queue,
        taken: false,
        origin: None,
        force: false,
    }
}

/// The session's own envelope with the result of the child it watched
/// at `watch`.
pub fn own_child(watch: u64, outcome: &Outcome) -> Body {
    Body::Enqueue {
        envelope: protocol::Envelope::own(
            SessionId::NIL,
            id(watch),
            protocol::EnvelopeId::CHILD,
            outcome.payload(),
        ),
    }
}

pub fn take(input: u64) -> Body {
    Body::Take {
        input: seq(input),
        into: None,
    }
}

pub fn said(text: &str) -> Body {
    Body::Assistant {
        text: text.to_owned(),
    }
}

pub fn tool_call(tool: &str, input: Value) -> Body {
    Body::ToolCall {
        invoke: None,
        tool: tool.to_owned(),
        input: Json::encode(&input).unwrap(),
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
        call: CallId::new(call).unwrap(),
        outcome,
        card: None,
        held: Vec::new(),
    }
}

/// The result call `call` gets as it detaches, in a session loaded with
/// no id: naming the stream it holds.
pub fn placeholder(call: u64) -> Body {
    let stream = protocol::OutputStream {
        session: SessionId::NIL,
        call: id(call),
    };
    Body::ToolResult {
        call: id(call),
        outcome: stream.placeholder(),
        card: None,
        held: vec![protocol::Held::Stream(stream)],
    }
}

pub fn model_call(attempt: u16) -> Body {
    Body::ModelCall {
        request: None,
        model: "test:model".to_owned(),
        attempt,
        choice: ToolChoice::Auto,
        r#for: None,
        purpose: None,
    }
}

pub fn model_result(call: u64, outcome: Outcome) -> Body {
    Body::ModelResult {
        r#for: None,
        failure: None,
        call: CallId::new(call).unwrap(),
        outcome,
    }
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
        output: Json::encode(&output).unwrap(),
    }
}

pub fn failed(error: &str) -> Outcome {
    Outcome::Failed {
        error: error.to_owned(),
    }
}

pub const INTERRUPTED: Outcome = Outcome::Interrupted { output: None };

pub fn run_tool(call: u64, tool: &str, input: Value) -> Work {
    Work::RunTool {
        call: id(call),
        tool: tool.to_owned(),
        input: Json::encode(&input).unwrap(),
    }
}

pub fn run_bash(call: u64, command: &str) -> Work {
    run_tool(call, "bash", json!({ "command": command }))
}

/// The request of model call `call` to be prepared, as attempt `attempt`.
pub fn prepare(call: u64, attempt: u16) -> Work {
    Work::Prepare {
        call: id(call),
        attempt,
        choice: ToolChoice::Auto,
    }
}

pub fn send(call: u64) -> Work {
    Work::Send { call: id(call) }
}

pub fn cancel(call: u64) -> Work {
    Work::Cancel { call: id(call) }
}

pub fn wait_child(call: u64, link: u64) -> Body {
    Body::Wait {
        call: id(call),
        on: WaitOn::Child { link: seq(link) },
    }
}

pub const BUSY: Work = Work::MarkBusy(true);
pub const IDLE: Work = Work::MarkBusy(false);
