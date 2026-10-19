//! Decision points: where extensions intercept the engine. Each point is a
//! type, and the kind of answer its handlers give fixes how their answers
//! merge, so any set of handlers gives one answer:
//!
//! | Point | Merge |
//! | --- | --- |
//! | [`Input`] | replacement chain over the user's text; the first rejection wins |
//! | [`PrepareStep`] | replacement chain: each handler sees the request the last left |
//! | [`RequestError`] | the first recovery wins |
//! | [`PreTool`] | replacement chain over the input; the first block wins |
//! | [`Permission`] | the most restrictive wins: deny > defer > ask > allow |
//! | [`Approve`] | the first answer wins |
//! | [`Spawn`] | replacement chain over every process a tool starts |
//! | [`PostTool`] | replacement chain over the outcome |
//! | [`OnStop`] | the first continuation wins |
//! | [`Observe`], [`SessionStart`], [`SessionEnd`] | every handler is told, and changes nothing |
//!
//! The four rules are [`Merge`]'s, each implemented once, by the
//! [`Rule`] that carries it, for every point. Handlers run in
//! registration order, each given a [`PointCx`]. What changed is reported
//! as [`Decision`]s, which the engine logs under the extension's name;
//! `spawn` rewrites are not logged, as they follow from configuration
//! rather than from what happened.

use std::any::{Any, TypeId};
use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use protocol::{Decision, Entry, Json, Outcome, SessionId};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::{Fold, Name, States};

/// How a point merges its handlers' answers, in registration order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Merge {
    /// Each handler sees the subject as the last left it, and may replace
    /// it; the first to stop ends the chain.
    Chain,
    /// The first answer wins; the handlers after it are not asked.
    FirstSome,
    /// Every handler answers, and the most restrictive answer wins: the
    /// earliest of equals.
    MostRestrictive,
    /// Every handler is told, and nothing changes.
    Observe,
}

/// A decision point.
pub trait Point: Sized + 'static {
    /// The point's name, as an [`ErasedHandler`] gives it.
    const NAME: &'static str;
    /// What the engine knows of the occasion.
    type Facts<'a>: Copy;
    /// What each handler reads: the facts, and under [`Merge::Chain`] the
    /// subject as the handlers before it left it.
    type In<'a>;
    /// What a handler answers; its kind fixes [`Point::MERGE`].
    type Out: Rule;
    const MERGE: Merge = <Self::Out as Rule>::MERGE;

    fn input<'a, 'b: 'a>(facts: Self::Facts<'b>, subject: &'a Subject<Self>) -> Self::In<'a>;
}

/// What a point's handlers rewrite: `()` but under [`Merge::Chain`].
pub type Subject<P> = <<P as Point>::Out as Rule>::Subject;
/// What a point's handlers' answers come to.
pub type Merged<P> = <<P as Point>::Out as Rule>::Merged;

/// Told of each replacement in a chain: by whom, of what, with what.
pub type Replaced<'r, T> = dyn FnMut(&Name, T, &T) + 'r;

/// A handler at point `P`.
pub type Handler<P> =
    Arc<dyn for<'a> Fn(&PointCx<'_>, <P as Point>::In<'a>) -> <P as Point>::Out + Send + Sync>;

/// The kinds of answer, one per [`Merge`] rule:
///
/// - [`Rewrite`] chains,
/// - `Option` is a first answer,
/// - a [`protocol::Permission`] is ranked,
/// - `()` observes.
pub trait Rule: Sized + 'static {
    const MERGE: Merge;
    type Subject;
    type Merged;

    /// Merges what `handlers` answer, telling `replaced` of each
    /// replacement of the subject with what it replaced.
    #[doc(hidden)]
    fn merge<P: Point<Out = Self>>(
        handlers: &[(Name, Handler<P>)],
        cx: &PointCx<'_>,
        facts: P::Facts<'_>,
        subject: &mut Self::Subject,
        replaced: &mut Replaced<'_, Self::Subject>,
    ) -> Self::Merged;
}

/// An answer in a replacement chain. A chain that cannot be stopped has
/// [`Infallible`] for `S`.
#[derive(Debug, PartialEq, Eq)]
pub enum Rewrite<T, S = Infallible> {
    Pass,
    Replace(T),
    /// Ends the chain, with why.
    Stop(S),
}

impl<T: 'static, S: 'static> Rule for Rewrite<T, S> {
    const MERGE: Merge = Merge::Chain;
    type Subject = T;
    /// `Err` is who stopped the chain, and why.
    type Merged = Result<(), (Name, S)>;

    fn merge<P: Point<Out = Self>>(
        handlers: &[(Name, Handler<P>)],
        cx: &PointCx<'_>,
        facts: P::Facts<'_>,
        subject: &mut T,
        replaced: &mut Replaced<'_, T>,
    ) -> Self::Merged {
        for (name, handler) in handlers {
            match handler(cx, P::input(facts, subject)) {
                Rewrite::Pass => {}
                Rewrite::Replace(next) => {
                    let before = std::mem::replace(subject, next);
                    replaced(name, before, subject);
                }
                Rewrite::Stop(why) => return Err((Name::clone(name), why)),
            }
        }
        Ok(())
    }
}

impl<T: 'static> Rule for Option<T> {
    const MERGE: Merge = Merge::FirstSome;
    type Subject = ();
    /// The first answer, and who gave it.
    type Merged = Option<(Name, T)>;

    fn merge<P: Point<Out = Self>>(
        handlers: &[(Name, Handler<P>)],
        cx: &PointCx<'_>,
        facts: P::Facts<'_>,
        (): &mut (),
        _: &mut Replaced<'_, ()>,
    ) -> Self::Merged {
        handlers.iter().find_map(|(name, handler)| {
            Some((Name::clone(name), handler(cx, P::input(facts, &()))?))
        })
    }
}

impl Rule for protocol::Permission {
    const MERGE: Merge = Merge::MostRestrictive;
    type Subject = ();
    /// The most restrictive answer and who gave it, unless every handler
    /// allowed.
    type Merged = Option<(Name, Self)>;

    fn merge<P: Point<Out = Self>>(
        handlers: &[(Name, Handler<P>)],
        cx: &PointCx<'_>,
        facts: P::Facts<'_>,
        (): &mut (),
        _: &mut Replaced<'_, ()>,
    ) -> Self::Merged {
        handlers
            .iter()
            .map(|(name, handler)| (name, handler(cx, P::input(facts, &()))))
            .filter(|(_, level)| *level > Self::Allow)
            .reduce(|most, next| if next.1 > most.1 { next } else { most })
            .map(|(name, level)| (Name::clone(name), level))
    }
}

impl Rule for () {
    const MERGE: Merge = Merge::Observe;
    type Subject = ();
    type Merged = ();

    fn merge<P: Point<Out = Self>>(
        handlers: &[(Name, Handler<P>)],
        cx: &PointCx<'_>,
        facts: P::Facts<'_>,
        (): &mut (),
        _: &mut Replaced<'_, ()>,
    ) {
        for (_, handler) in handlers {
            handler(cx, P::input(facts, &()));
        }
    }
}

/// What every handler is given: the session it decides for.
pub struct PointCx<'a> {
    pub session: SessionId,
    pub workspace: &'a Path,
    view: &'a dyn View,
}

/// The engine's side of a [`PointCx`], read only when a handler asks.
pub trait View: Sync {
    /// The model the session talks to, as `host:model`.
    fn model(&self) -> String;
    fn states(&self, read: &mut dyn FnMut(&States));
}

impl<'a> PointCx<'a> {
    pub fn new(session: SessionId, workspace: &'a Path, view: &'a dyn View) -> Self {
        Self {
            session,
            workspace,
            view,
        }
    }

    pub fn model(&self) -> String {
        self.view.model()
    }

    /// Reads state `S`, or its default when no extension registered it.
    pub fn state<S: Fold, R>(&self, read: impl FnOnce(&S) -> R) -> R {
        crate::state::read(|f| self.view.states(f), read)
    }
}

/// A tool call, as handlers see it.
#[derive(Clone, Copy, Debug)]
pub struct ToolUse<'a> {
    pub tool: &'a str,
    pub input: &'a Value,
}

/// A finished tool call, as `post_tool` handlers see it.
#[derive(Clone, Copy, Debug)]
pub struct Finished<'a> {
    pub tool: &'a str,
    pub input: &'a Value,
    pub outcome: &'a Outcome,
}

/// A failed model request, as `request_error` handlers see it.
#[derive(Clone, Copy, Debug)]
pub struct Failed<'a> {
    pub request: &'a llm::Request,
    pub error: &'a llm::Error,
    /// Which attempt failed: 0 for the first request, and counted in the
    /// log, so a restart does not start the count again.
    pub attempt: u32,
}

/// What to do about a failed model request.
#[derive(Clone, Debug, PartialEq)]
pub enum Recover {
    /// Sends the same request again once `after` has passed; an interrupt
    /// meanwhile ends the call with nothing sent.
    Retry { after: Duration },
    /// Sends `request` instead, at once: a compacted one, say.
    Replace(Box<llm::Request>),
}

/// Declares a point whose handlers read its facts as they are.
macro_rules! point {
    ($(#[$doc:meta])* $point:ident = $name:literal, $facts:ty, $out:ty) => {
        $(#[$doc])*
        pub struct $point;

        impl Point for $point {
            const NAME: &'static str = $name;
            type Facts<'a> = $facts;
            type In<'a> = $facts;
            type Out = $out;

            fn input<'a, 'b: 'a>(facts: Self::Facts<'b>, (): &'a ()) -> Self::In<'a> {
                facts
            }
        }
    };
}

/// What becomes of the user's input before it is logged: a rejection
/// refuses it, and its client is told why.
pub struct Input;

impl Point for Input {
    const NAME: &'static str = "input";
    type Facts<'a> = ();
    type In<'a> = &'a str;
    type Out = Rewrite<String, String>;

    fn input<'a, 'b: 'a>((): (), text: &'a String) -> &'a str {
        text
    }
}

/// A replacement for the next model request.
pub struct PrepareStep;

impl Point for PrepareStep {
    const NAME: &'static str = "prepare_step";
    type Facts<'a> = ();
    type In<'a> = &'a llm::Request;
    type Out = Rewrite<llm::Request>;

    fn input<'a, 'b: 'a>((): (), request: &'a llm::Request) -> &'a llm::Request {
        request
    }
}

point!(
    /// How to recover from a failed model request: send it again later, or
    /// another in its place. With no answer the call fails.
    RequestError = "request_error",
    Failed<'a>,
    Option<Recover>
);

/// What a tool call runs with: a replacement input, or a block that
/// answers the call with why instead of running it.
pub struct PreTool;

impl Point for PreTool {
    const NAME: &'static str = "pre_tool";
    type Facts<'a> = &'a str;
    type In<'a> = ToolUse<'a>;
    type Out = Rewrite<Value, String>;

    fn input<'a, 'b: 'a>(tool: &'b str, input: &'a Value) -> ToolUse<'a> {
        ToolUse { tool, input }
    }
}

point!(
    /// Whether a call may run.
    Permission = "permission",
    ToolUse<'a>,
    protocol::Permission
);
point!(
    /// Answers a permission `ask`. An ask no handler answers goes to the
    /// user, durably: see [`Gate::Ask`].
    Approve = "approve",
    ToolUse<'a>,
    Option<bool>
);

/// A replacement for a process a tool is about to start.
pub struct Spawn;

impl Point for Spawn {
    const NAME: &'static str = "spawn";
    type Facts<'a> = ();
    type In<'a> = &'a crate::SpawnSpec;
    type Out = Rewrite<crate::SpawnSpec>;

    fn input<'a, 'b: 'a>((): (), spawn: &'a crate::SpawnSpec) -> &'a crate::SpawnSpec {
        spawn
    }
}

/// A replacement for a call's outcome, before it is committed.
pub struct PostTool;

impl Point for PostTool {
    const NAME: &'static str = "post_tool";
    type Facts<'a> = ToolUse<'a>;
    type In<'a> = Finished<'a>;
    type Out = Rewrite<Outcome>;

    fn input<'a, 'b: 'a>(call: ToolUse<'b>, outcome: &'a Outcome) -> Finished<'a> {
        Finished {
            tool: call.tool,
            input: call.input,
            outcome,
        }
    }
}

/// A session's log as a handler reads it: decoded on demand, each time
/// [`History::entries`] is called, as only a handler that reads it needs it.
pub trait History {
    fn entries(&self) -> Box<dyn Iterator<Item = Entry> + '_>;
}

impl History for Vec<Entry> {
    fn entries(&self) -> Box<dyn Iterator<Item = Entry> + '_> {
        Box::new(self.iter().cloned())
    }
}

point!(
    /// Text to continue with once the agent stopped, given the session's
    /// log so far.
    OnStop = "on_stop",
    &'a dyn History,
    Option<String>
);
point!(
    /// Each entry, once committed.
    Observe = "observe",
    &'a Entry,
    ()
);
point!(
    /// The session's actor started: it was created, or loaded again.
    SessionStart = "session_start",
    (),
    ()
);
point!(
    /// The session's actor stopped: it hibernated, or the daemon drained.
    SessionEnd = "session_end",
    (),
    ()
);

/// A handler over JSON: what a Lua mod, a WASM component or an extension
/// process registers. It is given the point's input as JSON, and answers
/// as the point's [`Rule`] reads JSON: `null` passes, a chain takes
/// `{"replace": ..}` or `{"stop": ..}`, a permission its name.
pub trait ErasedHandler: Send + Sync {
    fn on(&self, cx: &PointCx<'_>, args: &Value) -> Value;
}

/// A point that takes [`ErasedHandler`]s.
trait Wire: Point {
    fn to_json(input: &Self::In<'_>) -> Value;
    fn from_json(answer: Value) -> Self::Out;
}

impl Wire for Input {
    fn to_json(text: &&str) -> Value {
        Value::from(*text)
    }

    fn from_json(answer: Value) -> Self::Out {
        rewrite(answer)
    }
}

impl Wire for PreTool {
    fn to_json(call: &ToolUse<'_>) -> Value {
        json!({ "tool": call.tool, "input": call.input })
    }

    fn from_json(answer: Value) -> Self::Out {
        rewrite(answer)
    }
}

impl Wire for Permission {
    fn to_json(call: &ToolUse<'_>) -> Value {
        PreTool::to_json(call)
    }

    /// One it cannot read denies: a policy fails closed.
    fn from_json(answer: Value) -> Self::Out {
        serde_json::from_value(answer).unwrap_or(protocol::Permission::Deny)
    }
}

impl Wire for Approve {
    fn to_json(call: &ToolUse<'_>) -> Value {
        PreTool::to_json(call)
    }

    fn from_json(answer: Value) -> Self::Out {
        answer.as_bool()
    }
}

impl Wire for OnStop {
    fn to_json(history: &&dyn History) -> Value {
        let entries: Vec<Entry> = history.entries().collect();
        serde_json::to_value(entries).unwrap_or_default()
    }

    fn from_json(answer: Value) -> Self::Out {
        answer.as_str().map(str::to_owned)
    }
}

impl Wire for Observe {
    fn to_json(entry: &&Entry) -> Value {
        serde_json::to_value(entry).unwrap_or_default()
    }

    fn from_json(_: Value) {}
}

/// A chain answer from JSON; one it cannot read passes.
fn rewrite<T: DeserializeOwned>(answer: Value) -> Rewrite<T, String> {
    let Value::Object(mut answer) = answer else {
        return Rewrite::Pass;
    };
    if let Some(Value::String(why)) = answer.remove("stop") {
        return Rewrite::Stop(why);
    }
    answer
        .remove("replace")
        .and_then(|next| serde_json::from_value(next).ok())
        .map_or(Rewrite::Pass, Rewrite::Replace)
}

/// The points that take erased handlers, and how to adapt one to each.
/// Adapts an erased handler to its point.
type Adapt = fn(Arc<dyn ErasedHandler>) -> Hooked;

pub(crate) const ERASED: &[(&str, Adapt)] = &[
    (Input::NAME, adapt::<Input>),
    (PreTool::NAME, adapt::<PreTool>),
    (Permission::NAME, adapt::<Permission>),
    (Approve::NAME, adapt::<Approve>),
    (OnStop::NAME, adapt::<OnStop>),
    (Observe::NAME, adapt::<Observe>),
];

fn adapt<P: Wire>(erased: Arc<dyn ErasedHandler>) -> Hooked {
    let handler: Handler<P> =
        Arc::new(move |cx, input| P::from_json(erased.on(cx, &P::to_json(&input))));
    Hooked::of::<P>(handler)
}

/// One handler as an extension registered it, waiting to be grouped with
/// the others at its point.
pub(crate) struct Hooked {
    point: TypeId,
    handler: Box<dyn Any + Send + Sync>,
    group: fn() -> Box<dyn Any + Send + Sync>,
    add: fn(&mut (dyn Any + Send + Sync), Name, &(dyn Any + Send + Sync)),
}

impl Hooked {
    pub fn of<P: Point>(handler: Handler<P>) -> Self {
        Self {
            point: TypeId::of::<P>(),
            handler: Box::new(handler),
            group: || Box::new(Vec::<(Name, Handler<P>)>::new()),
            add: |group, name, handler| {
                let group = group.downcast_mut::<Vec<(Name, Handler<P>)>>();
                let handler = handler.downcast_ref::<Handler<P>>();
                if let (Some(group), Some(handler)) = (group, handler) {
                    group.push((name, Arc::clone(handler)));
                }
            },
        }
    }
}

/// Decisions to log, with the extension whose handler made each.
pub type Decided = Vec<(Name, Decision)>;

/// A point's answer: now, or later as a future the call owns, so a slow
/// handler holds its call rather than the session.
pub enum Answer<T> {
    Now(T),
    Later(Later<T>),
}

/// An answer to come.
pub struct Later<T>(pub Pin<Box<dyn Future<Output = T> + Send>>);

impl<T> Later<T> {
    pub fn new(future: impl Future<Output = T> + Send + 'static) -> Self {
        Self(Box::pin(future))
    }
}

impl<T> fmt::Debug for Later<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Later")
    }
}

/// A future has no identity of its own: the call that awaits it names it.
impl<T> PartialEq for Later<T> {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

/// A chain's result, with what each extension decided on the way.
#[derive(Debug, PartialEq)]
pub struct Chained<T> {
    pub value: T,
    pub decisions: Vec<(Name, protocol::Verdict)>,
}

impl<T> Chained<T> {
    pub const fn new(value: T) -> Self {
        Self {
            value,
            decisions: Vec::new(),
        }
    }
}

/// What a call's gate decided: logged after the call's intent, then it
/// runs with `input`, waits for an approval, or is answered with why not.
#[derive(Debug, PartialEq)]
pub struct Gated {
    pub input: Json,
    pub decisions: Vec<(Name, protocol::Verdict)>,
    pub gate: Gate,
}

impl Gated {
    pub const fn run(input: Json) -> Self {
        Self {
            input,
            decisions: Vec::new(),
            gate: Gate::Run,
        }
    }
}

/// What the gate makes of a tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gate {
    Run,
    /// No handler answered a permission `ask`: the call runs once the user
    /// approves it.
    Ask,
    /// It is answered with why not, without running.
    Refuse(String),
}

/// The handlers at each point, in the order they run, each with its
/// extension's name, as a [`Registry`](crate::host::Registry) resolves
/// them. Grouped by point as they install, so a point with no handler
/// costs a lookup in a list of the points that have some. The default,
/// none, allows everything.
#[derive(Default)]
pub struct Hooks(Vec<(TypeId, Box<dyn Any + Send + Sync>)>);

impl Hooks {
    pub(crate) fn add(&mut self, name: &Name, hooked: &Hooked) {
        let found = self.0.iter().position(|(point, _)| *point == hooked.point);
        let at = found.unwrap_or_else(|| {
            self.0.push((hooked.point, (hooked.group)()));
            self.0.len() - 1
        });
        (hooked.add)(&mut *self.0[at].1, Name::clone(name), &*hooked.handler);
    }

    fn handlers<P: Point>(&self) -> &[(Name, Handler<P>)] {
        self.0
            .iter()
            .find(|(point, _)| *point == TypeId::of::<P>())
            .and_then(|(_, group)| group.downcast_ref::<Vec<(Name, Handler<P>)>>())
            .map_or(&[], Vec::as_slice)
    }

    /// Merges what the handlers at `P` answer, by `P`'s rule.
    pub fn run<P: Point>(
        &self,
        cx: &PointCx<'_>,
        facts: P::Facts<'_>,
        subject: &mut Subject<P>,
        replaced: &mut Replaced<'_, Subject<P>>,
    ) -> Merged<P> {
        P::Out::merge::<P>(self.handlers::<P>(), cx, facts, subject, replaced)
    }

    /// The answer at a point that rewrites nothing.
    fn ask<P: Point<Out: Rule<Subject = ()>>>(
        &self,
        cx: &PointCx<'_>,
        facts: P::Facts<'_>,
    ) -> Merged<P> {
        self.run::<P>(cx, facts, &mut (), &mut |_, (), ()| {})
    }

    /// The user's text as the `input` chain leaves it; `Err` is why it was
    /// refused.
    pub fn input(
        &self,
        cx: &PointCx<'_>,
        text: &mut String,
        decided: &mut Decided,
    ) -> Result<(), String> {
        self.run::<Input>(cx, (), text, &mut |name, original, _| {
            decided.push((Name::clone(name), Decision::Input { original }));
        })
        .map_err(|(name, reason)| format!("{name}: {reason}"))
    }

    /// How to recover model call `call` from `failed`, if any handler
    /// says; logged as the attempt it starts.
    pub fn request_error(
        &self,
        cx: &PointCx<'_>,
        call: u64,
        failed: Failed<'_>,
        decided: &mut Decided,
    ) -> Option<Recover> {
        let (name, recover) = self.ask::<RequestError>(cx, failed)?;
        let decision = Decision::Retry {
            call,
            attempt: failed.attempt + 1,
            error: failed.error.to_string(),
        };
        decided.push((name, decision));
        Some(recover)
    }

    pub fn spawn(&self, cx: &PointCx<'_>, spawn: &mut crate::SpawnSpec) {
        let Ok(()) = self.run::<Spawn>(cx, (), spawn, &mut |_, _, _| {});
    }

    pub fn session_start(&self, cx: &PointCx<'_>) {
        self.ask::<SessionStart>(cx, ());
    }

    pub fn session_end(&self, cx: &PointCx<'_>) {
        self.ask::<SessionEnd>(cx, ());
    }

    pub fn prepare_step(
        &self,
        cx: &PointCx<'_>,
        call: u64,
        request: &mut llm::Request,
        decided: &mut Decided,
    ) {
        let Ok(()) = self.run::<PrepareStep>(cx, (), request, &mut |name, _, _| {
            decided.push((Name::clone(name), Decision::PrepareStep { call }));
        });
    }

    /// Whether a call may run, with `input` as the `pre_tool` chain leaves
    /// it.
    pub fn gate(
        &self,
        cx: &PointCx<'_>,
        tool: &str,
        input: &mut Value,
        decided: &mut Decided,
    ) -> Gate {
        let chained = self.run::<PreTool>(cx, tool, input, &mut |name, _, input| {
            let input = Json::new(input);
            let tool = tool.to_owned();
            decided.push((Name::clone(name), Decision::PreTool { tool, input }));
        });
        if let Err((name, reason)) = chained {
            let tool = tool.to_owned();
            let decision = Decision::Block {
                tool,
                reason: reason.clone(),
            };
            decided.push((name, decision));
            return Gate::Refuse(reason);
        }
        let call = ToolUse {
            tool,
            input: &*input,
        };
        let Some((by, permission)) = self.ask::<Permission>(cx, call) else {
            return Gate::Run;
        };
        let tool = tool.to_owned();
        let decision = Decision::Permission {
            tool: tool.clone(),
            permission,
        };
        decided.push((Name::clone(&by), decision));
        match permission {
            protocol::Permission::Allow => Gate::Run,
            protocol::Permission::Ask => {
                let Some((who, approved)) = self.ask::<Approve>(cx, call) else {
                    return Gate::Ask;
                };
                let why = (!approved).then(|| format!("{who} did not approve this call"));
                let decision = Decision::Approval {
                    tool,
                    approved,
                    call: None,
                };
                decided.push((who, decision));
                why.map_or(Gate::Run, Gate::Refuse)
            }
            protocol::Permission::Defer => Gate::Refuse(format!("{by} deferred this call")),
            protocol::Permission::Deny => Gate::Refuse(format!("{by} denied this call")),
        }
    }

    pub fn post_tool(
        &self,
        cx: &PointCx<'_>,
        call: u64,
        (tool, input): (&str, &Value),
        outcome: &mut Outcome,
        decided: &mut Decided,
    ) {
        let facts = ToolUse { tool, input };
        let Ok(()) = self.run::<PostTool>(cx, facts, outcome, &mut |name, original, _| {
            decided.push((Name::clone(name), Decision::PostTool { call, original }));
        });
    }

    pub fn on_stop(
        &self,
        cx: &PointCx<'_>,
        history: &dyn History,
        decided: &mut Decided,
    ) -> Option<String> {
        let (name, text) = self.ask::<OnStop>(cx, history)?;
        decided.push((name, Decision::OnStop { text: text.clone() }));
        Some(text)
    }

    pub fn observe(&self, cx: &PointCx<'_>, entry: &Entry) {
        self.ask::<Observe>(cx, entry);
    }
}

#[cfg(test)]
mod tests;
