//! Decision points: where extensions intercept the engine. Each point is a
//! type, and the kind of answer its handlers give fixes how their answers
//! merge, so any set of handlers gives one answer:
//!
//! | Point | Merge |
//! | --- | --- |
//! | [`Input`] | replacement chain over the user's text; the first rejection wins |
//! | [`RequestError`] | the first recovery wins |
//! | [`PreTool`] | replacement chain over the input; the first block wins |
//! | [`Permission`] | the most restrictive wins: deny > defer > ask > allow |
//! | [`Approve`] | the first answer wins |
//! | [`Spawn`] | replacement chain over every process a tool starts |
//! | [`PostTool`] | replacement chain over the outcome |
//! | [`OnStop`] | the first continuation wins |
//! | [`Observe`], [`SessionStart`], [`SessionEnd`] | every handler is told, and changes nothing |
//! | [`Turn`], [`Reply`] | replacement chain over what the loop does next |
//! | [`Resume`] | the first answer wins; with none, the loop goes on |
//!
//! The loop points ([`Turn`], [`Reply`], [`Resume`]) run inside the fold
//! of the session's log, rerun as it loads: their handlers are given their
//! arguments and nothing of the session (a `()` for its context), so what
//! a replay asks of them is what the live run did.
//!
//! The four rules are [`Merge`]'s, each implemented once, by the
//! [`Rule`] that carries it, for every point. Handlers run in
//! registration order, each given a [`PointCx`]. What changed is reported
//! as [`Verdict`]s, which the engine logs under the extension's name;
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

use protocol::{Entry, Json, Outcome, SessionId, Verdict};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::{Actions, Fold, Name, States};

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
    /// What a handler is given of the session: a [`PointCx`], or `()` at
    /// a loop point.
    type Cx<'c>;
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
pub type Handler<P> = Arc<
    dyn for<'a> Fn(&<P as Point>::Cx<'_>, <P as Point>::In<'a>) -> <P as Point>::Out + Send + Sync,
>;

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
        cx: &P::Cx<'_>,
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
        cx: &P::Cx<'_>,
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
        cx: &P::Cx<'_>,
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
        cx: &P::Cx<'_>,
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
        cx: &P::Cx<'_>,
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
    pub error: &'a llm::Error,
    /// Which attempt failed: 0 for the first request, and counted in the
    /// log, so a restart does not start the count again.
    pub attempt: u16,
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
            type Cx<'c> = PointCx<'c>;
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
    type Cx<'c> = PointCx<'c>;
    type Facts<'a> = ();
    type In<'a> = &'a str;
    type Out = Rewrite<String, String>;

    fn input<'a, 'b: 'a>((): (), text: &'a String) -> &'a str {
        text
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
    type Cx<'c> = PointCx<'c>;
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
    type Cx<'c> = PointCx<'c>;
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
    type Cx<'c> = PointCx<'c>;
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

point!(
    /// Text to continue with once the agent stopped. A handler that reads
    /// the session's log folds what it needs of it as typed state.
    OnStop = "on_stop",
    (),
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

/// What an input starts, as the default loop asks: one model call unless
/// a handler says otherwise (a planner, a plan mode, a command answered
/// without the model).
pub struct Turn;

impl Point for Turn {
    const NAME: &'static str = "turn";
    type Cx<'c> = ();
    type Facts<'a> = &'a crate::Input;
    type In<'a> = (&'a crate::Input, &'a Actions);
    type Out = Rewrite<Actions>;

    fn input<'a, 'b: 'a>(input: &'b crate::Input, actions: &'a Actions) -> Self::In<'a> {
        (input, actions)
    }
}

/// What the model's reply becomes, as the default loop asks: a call per
/// tool call unless a handler says otherwise (sequential calls only, a
/// dedupe, a refusal).
pub struct Reply;

/// A model's reply at a step of the loop, as [`Reply`] handlers see it.
#[derive(Clone, Copy, Debug)]
pub struct Replied<'a> {
    /// The model calls of the turn so far, this one included.
    pub step: u16,
    pub reply: &'a llm::Response,
    /// What the loop does with it, as the handlers before left it.
    pub actions: &'a Actions,
}

impl Point for Reply {
    const NAME: &'static str = "reply";
    type Cx<'c> = ();
    type Facts<'a> = (u16, &'a llm::Response);
    type In<'a> = Replied<'a>;
    type Out = Rewrite<Actions>;

    fn input<'a, 'b: 'a>(
        (step, reply): (u16, &'b llm::Response),
        actions: &'a Actions,
    ) -> Replied<'a> {
        Replied {
            step,
            reply,
            actions,
        }
    }
}

/// What follows once every call of a step's reply is answered, as the
/// default loop asks: with no answer, another model call.
pub struct Resume;

impl Point for Resume {
    const NAME: &'static str = "resume";
    type Cx<'c> = ();
    type Facts<'a> = u16;
    type In<'a> = u16;
    type Out = Option<crate::Resume>;

    fn input<'a, 'b: 'a>(step: u16, (): &'a ()) -> u16 {
        step
    }
}

/// A handler over JSON: what a Lua mod, a WASM component or an extension
/// process registers. It is given the point's input as JSON, and answers
/// as the point's [`Rule`] reads JSON: `null` passes, a chain takes
/// `{"replace": ..}` or `{"stop": ..}`, a permission its name.
pub trait ErasedHandler: Send + Sync {
    fn on(&self, cx: &PointCx<'_>, args: &Value) -> Value;
}

/// A point that takes [`ErasedHandler`]s.
trait Wire: for<'c> Point<Cx<'c> = PointCx<'c>> {
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
    fn to_json((): &()) -> Value {
        Value::Null
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
pub type Decided = Vec<(Name, Verdict)>;

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
    pub decisions: Decided,
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
    pub decisions: Decided,
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
        cx: &P::Cx<'_>,
        facts: P::Facts<'_>,
        subject: &mut Subject<P>,
        replaced: &mut Replaced<'_, Subject<P>>,
    ) -> Merged<P> {
        P::Out::merge::<P>(self.handlers::<P>(), cx, facts, subject, replaced)
    }

    /// The answer at a point that rewrites nothing.
    fn ask<P: Point<Out: Rule<Subject = ()>>>(
        &self,
        cx: &P::Cx<'_>,
        facts: P::Facts<'_>,
    ) -> Merged<P> {
        self.run::<P>(cx, facts, &mut (), &mut |_, (), ()| {})
    }

    /// The user's text as the `input` chain leaves it; `Err` is why it was
    /// refused.
    pub fn input(&self, cx: &PointCx<'_>, mut text: String) -> Result<Chained<String>, String> {
        let mut decisions = Decided::new();
        self.run::<Input>(cx, (), &mut text, &mut |name, original, _| {
            decisions.push((Name::clone(name), Verdict::Input { original }));
        })
        .map_err(|(name, reason)| format!("{name}: {reason}"))?;
        Ok(Chained {
            value: text,
            decisions,
        })
    }

    /// How to recover a model call from `failed`, if any handler says, and
    /// who said: the kernel logs it as the attempt it starts.
    pub fn request_error(&self, cx: &PointCx<'_>, failed: Failed<'_>) -> Option<(Name, Recover)> {
        self.ask::<RequestError>(cx, failed)
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

    /// Whether a call of `tool` on `input` may run, with the input as the
    /// `pre_tool` chain leaves it, and what each handler decided.
    pub fn gate(&self, cx: &PointCx<'_>, tool: &str, input: &Json) -> Gated {
        let mut value = input.value();
        let mut decisions = Decided::new();
        let chained = self.run::<PreTool>(cx, tool, &mut value, &mut |name, _, input| {
            let input = Json::new(input);
            decisions.push((Name::clone(name), Verdict::PreTool { input }));
        });
        let input = if decisions.is_empty() {
            input.clone()
        } else {
            Json::new(&value)
        };
        let gate = match chained {
            Err((name, reason)) => {
                let block = Verdict::Block {
                    reason: reason.clone(),
                };
                decisions.push((name, block));
                Gate::Refuse(reason)
            }
            Ok(()) => self.permit(cx, tool, &value, &mut decisions),
        };
        Gated {
            input,
            decisions,
            gate,
        }
    }

    /// The `permission` and `approve` points over a call let through.
    fn permit(&self, cx: &PointCx<'_>, tool: &str, input: &Value, decided: &mut Decided) -> Gate {
        let call = ToolUse { tool, input };
        let Some((by, permission)) = self.ask::<Permission>(cx, call) else {
            return Gate::Run;
        };
        decided.push((Name::clone(&by), Verdict::Permission { permission }));
        match permission {
            protocol::Permission::Allow => Gate::Run,
            protocol::Permission::Ask => {
                let Some((who, approved)) = self.ask::<Approve>(cx, call) else {
                    return Gate::Ask;
                };
                let why = (!approved).then(|| format!("{who} did not approve this call"));
                decided.push((who, Verdict::Approval { approved }));
                why.map_or(Gate::Run, Gate::Refuse)
            }
            protocol::Permission::Defer => Gate::Refuse(format!("{by} deferred this call")),
            protocol::Permission::Deny => Gate::Refuse(format!("{by} denied this call")),
        }
    }

    /// A call's outcome as the `post_tool` chain leaves it.
    pub fn post_tool(
        &self,
        cx: &PointCx<'_>,
        (tool, input): (&str, &Value),
        mut outcome: Outcome,
    ) -> Chained<Outcome> {
        let mut decisions = Decided::new();
        let facts = ToolUse { tool, input };
        let Ok(()) = self.run::<PostTool>(cx, facts, &mut outcome, &mut |name, original, _| {
            decisions.push((Name::clone(name), Verdict::PostTool { original }));
        });
        Chained {
            value: outcome,
            decisions,
        }
    }

    /// Text for the agent to go on with once it stopped, and who gave it.
    pub fn on_stop(&self, cx: &PointCx<'_>) -> Option<(Name, String)> {
        self.ask::<OnStop>(cx, ())
    }

    pub fn observe(&self, cx: &PointCx<'_>, entry: &Entry) {
        self.ask::<Observe>(cx, entry);
    }
}

#[cfg(test)]
mod tests;
