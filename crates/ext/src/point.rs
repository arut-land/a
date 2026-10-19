//! Decision points: where extensions intercept the engine. Each point is a
//! type, and the kind of answer its handlers give fixes how their answers
//! merge, so any set of handlers gives one answer:
//!
//! | Point | Merge |
//! | --- | --- |
//! | [`Input`] | replacement chain over the user's text; the first rejection wins |
//! | [`Step`] | replacement chain over a model request; the first answer of its own wins |
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
//! registration order, each given a [`PointCx`]. A handler at a
//! first-answer point whose answer is an [`Answer`] may answer later: the
//! merge stops at it, and goes on from the handler after it if what comes
//! is no answer. What changed is reported
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
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{Actions, Cx, Fold, Name, States};

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

/// What a first-answer point whose handlers may answer later came to.
#[derive(Debug, PartialEq)]
pub enum Asked<T> {
    /// No handler answered.
    None,
    Now(Name, T),
    /// Handler `by`, the `at`th, answers later. With no answer, the merge
    /// goes on from the one after it.
    Later {
        by: Name,
        at: usize,
        answer: Later<Option<T>>,
    },
}

impl<T> Asked<T> {
    /// The answer of a merge begun at handler `from`.
    fn from(self, from: usize) -> Self {
        match self {
            Self::Later { by, at, answer } => Self::Later {
                by,
                at: at + from,
                answer,
            },
            asked => asked,
        }
    }
}

impl<T: 'static> Rule for Answer<Option<T>> {
    const MERGE: Merge = Merge::FirstSome;
    type Subject = ();
    type Merged = Asked<T>;

    fn merge<P: Point<Out = Self>>(
        handlers: &[(Name, Handler<P>)],
        cx: &P::Cx<'_>,
        facts: P::Facts<'_>,
        (): &mut (),
        _: &mut Replaced<'_, ()>,
    ) -> Asked<T> {
        for (at, (name, handler)) in handlers.iter().enumerate() {
            match handler(cx, P::input(facts, &())) {
                Answer::Now(None) => {}
                Answer::Now(Some(answer)) => return Asked::Now(Name::clone(name), answer),
                Answer::Later(answer) => {
                    let by = Name::clone(name);
                    return Asked::Later { by, at, answer };
                }
            }
        }
        Asked::None
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

/// What every handler is given: the session it decides for, and the call
/// it decides about, if one.
pub struct PointCx<'a> {
    pub session: SessionId,
    pub workspace: &'a Path,
    /// The call: the seq of its intent.
    pub call: Option<u64>,
    view: &'a dyn View,
}

/// The engine's side of a [`PointCx`], read only when a handler asks.
pub trait View: Sync {
    /// The model the session talks to, as `host:model`.
    fn model(&self) -> String;
    fn states(&self, read: &mut dyn FnMut(&States));
    /// What call `call` runs with, owned, for work its handler answers
    /// later with.
    fn cx(&self, _call: u64) -> Option<Cx> {
        None
    }
}

impl<'a> PointCx<'a> {
    pub fn new(session: SessionId, workspace: &'a Path, view: &'a dyn View) -> Self {
        Self {
            session,
            workspace,
            call: None,
            view,
        }
    }

    /// The same, about call `call`.
    #[must_use]
    pub fn about(self, call: u64) -> Self {
        Self {
            call: Some(call),
            ..self
        }
    }

    /// What the call this decides about runs with, owned and cheap to
    /// clone, so an [`Answer::Later`] can take it along: to a side model
    /// request, say ([`Cx::models`]). `None` for a point about no call.
    pub fn cx(&self) -> Option<Cx> {
        self.call.and_then(|call| self.view.cx(call))
    }

    pub fn model(&self) -> String {
        self.view.model()
    }

    /// Reads state `S`, or its default when no extension registered it.
    pub fn state<S: Fold, R>(&self, read: impl FnOnce(&S) -> R) -> R {
        crate::state::read(|f| self.view.states(f), read)
    }
}

/// A tool call, as handlers see it: its input the text the call holds,
/// which a handler that reads a field of it parses into its own type.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ToolUse<'a> {
    pub tool: &'a str,
    pub input: &'a Json,
}

/// A finished tool call, as `post_tool` handlers see it.
#[derive(Clone, Copy, Debug)]
pub struct Finished<'a> {
    pub tool: &'a str,
    pub input: &'a Json,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recover {
    /// Sends the same request again once `after` has passed; an interrupt
    /// meanwhile ends the call with nothing sent.
    Retry { after: Duration },
    /// Sends the request made from the transcript as it is now, at once:
    /// what a handler changed of the transcript (an `edit` that compacts
    /// it, say) is in it.
    Resend,
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
    /// a new one at once. With no answer the call fails. A handler may
    /// answer later, after a side model request, say: the call waits
    /// meanwhile, and an interrupt ends it.
    RequestError = "request_error",
    Failed<'a>,
    Answer<Option<Recover>>
);

/// The request of a model call, about to be sent: a handler may rewrite it
/// (a model fallback, a reminder appended at the tail) or answer it itself
/// (a fixture, a cache). Each rewrite is logged as `prepare_step`, and an
/// answer as the call's reply.
pub struct Step;

impl Point for Step {
    const NAME: &'static str = "step";
    type Cx<'c> = PointCx<'c>;
    type Facts<'a> = ();
    type In<'a> = &'a llm::Request;
    type Out = Rewrite<llm::Request, llm::Response>;

    fn input<'a, 'b: 'a>((): (), request: &'a llm::Request) -> &'a llm::Request {
        request
    }
}

/// What a tool call runs with: a replacement input, or a block that
/// answers the call with why instead of running it.
pub struct PreTool;

impl Point for PreTool {
    const NAME: &'static str = "pre_tool";
    type Cx<'c> = PointCx<'c>;
    type Facts<'a> = &'a str;
    type In<'a> = ToolUse<'a>;
    type Out = Rewrite<Json, String>;

    fn input<'a, 'b: 'a>(tool: &'b str, input: &'a Json) -> ToolUse<'a> {
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
    /// user, durably: see [`Gate::Ask`]. A handler may answer later, after
    /// a side model request, say: the call waits meanwhile.
    Approve = "approve",
    ToolUse<'a>,
    Answer<Option<bool>>
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

/// A handler over JSON text: what a Lua mod, a WASM component or an
/// extension process registers. It is given the point's input as JSON,
/// and answers as the point's [`Rule`] reads JSON: `null` passes, a chain
/// takes `{"replace": ..}` or `{"stop": ..}`, a permission its name. The
/// text is handed over as it is, for the handler to read as it reads JSON.
pub trait ErasedHandler: Send + Sync {
    fn on(&self, cx: &PointCx<'_>, args: &Json) -> Json;
}

/// A point that takes [`ErasedHandler`]s.
trait Wire: for<'c> Point<Cx<'c> = PointCx<'c>> {
    fn to_json(input: &Self::In<'_>) -> Json;
    fn from_json(answer: &Json) -> Self::Out;
}

/// `value` as JSON text; `null` for one that cannot be.
fn encode(value: &impl Serialize) -> Json {
    Json::encode(value).unwrap_or_default()
}

impl Wire for Input {
    fn to_json(text: &&str) -> Json {
        encode(text)
    }

    fn from_json(answer: &Json) -> Self::Out {
        rewrite(answer)
    }
}

impl Wire for PreTool {
    fn to_json(call: &ToolUse<'_>) -> Json {
        encode(call)
    }

    fn from_json(answer: &Json) -> Self::Out {
        rewrite(answer)
    }
}

impl Wire for Permission {
    fn to_json(call: &ToolUse<'_>) -> Json {
        encode(call)
    }

    /// One it cannot read denies: a policy fails closed.
    fn from_json(answer: &Json) -> Self::Out {
        answer.parse().unwrap_or(protocol::Permission::Deny)
    }
}

impl Wire for Approve {
    fn to_json(call: &ToolUse<'_>) -> Json {
        encode(call)
    }

    fn from_json(answer: &Json) -> Self::Out {
        Answer::Now(answer.parse().ok())
    }
}

impl Wire for OnStop {
    fn to_json((): &()) -> Json {
        Json::null()
    }

    fn from_json(answer: &Json) -> Self::Out {
        answer.parse().ok()
    }
}

impl Wire for Observe {
    fn to_json(entry: &&Entry) -> Json {
        encode(entry)
    }

    fn from_json(_: &Json) {}
}

/// A chain answer from JSON; one it cannot read passes.
fn rewrite<T: DeserializeOwned>(answer: &Json) -> Rewrite<T, String> {
    #[derive(serde::Deserialize)]
    struct Chain<T> {
        stop: Option<String>,
        replace: Option<T>,
    }
    match answer.parse::<Chain<T>>() {
        Ok(Chain {
            stop: Some(why), ..
        }) => Rewrite::Stop(why),
        Ok(Chain {
            replace: Some(next),
            ..
        }) => Rewrite::Replace(next),
        _ => Rewrite::Pass,
    }
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
        Arc::new(move |cx, input| P::from_json(&erased.on(cx, &P::to_json(&input))));
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

    /// The gate of a call asked about once the `approve` point answered:
    /// with no answer, the user is asked.
    pub fn approved(input: Json, mut decisions: Decided, answer: Option<(Name, bool)>) -> Self {
        let gate = match answer {
            None => Gate::Ask,
            Some((who, approved)) => approval(who, approved, &mut decisions),
        };
        Self {
            input,
            decisions,
            gate,
        }
    }
}

/// The gate once `who` answered an ask, its decision logged.
fn approval(who: Name, approved: bool, decided: &mut Decided) -> Gate {
    let gate = if approved {
        Gate::Run
    } else {
        Gate::Refuse(format!("{who} did not approve this call"))
    };
    decided.push((who, Verdict::Approval { approved }));
    gate
}

/// What the gate makes of a call: decided now, or once an `approve`
/// handler's later answer comes.
#[derive(Debug, PartialEq)]
pub enum Gating {
    Now(Gated),
    Approving(Approving),
}

impl Gating {
    /// What was decided, if it all was now.
    pub fn now(self) -> Option<Gated> {
        match self {
            Self::Now(gated) => Some(gated),
            Self::Approving(_) => None,
        }
    }
}

/// A call's gate waiting on an `approve` handler's later answer.
#[derive(Debug, PartialEq)]
pub struct Approving {
    /// The input as the `pre_tool` chain left it.
    pub input: Json,
    /// What was decided before the ask.
    pub decisions: Decided,
    pub by: Name,
    /// The `approve` handler the merge goes on from if the answer is none.
    pub next: usize,
    pub answer: Later<Option<bool>>,
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
    /// who said: the kernel logs it as the attempt it starts. A handler
    /// that answers later is asked from: if its answer is none, the
    /// handlers after it are asked with [`Hooks::request_error_from`].
    pub fn request_error(&self, cx: &PointCx<'_>, failed: Failed<'_>) -> Asked<Recover> {
        self.request_error_from(cx, failed, 0)
    }

    /// The `request_error` merge from its `from`th handler on.
    pub fn request_error_from(
        &self,
        cx: &PointCx<'_>,
        failed: Failed<'_>,
        from: usize,
    ) -> Asked<Recover> {
        self.ask_from::<RequestError, _>(cx, failed, from)
    }

    /// The request as the `step` chain leaves it, with the rewrites to log
    /// and, if a handler answered it, who and with what.
    pub fn step(
        &self,
        cx: &PointCx<'_>,
        request: &mut llm::Request,
    ) -> (Decided, Option<(Name, llm::Response)>) {
        let mut decisions = Decided::new();
        let answered = self.run::<Step>(cx, (), request, &mut |name, before, after| {
            let model = (before.model != after.model).then(|| after.model.to_string());
            decisions.push((Name::clone(name), Verdict::PrepareStep { model }));
        });
        (decisions, answered.err())
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
    /// `pre_tool` chain leaves it, and what each handler decided; or, when
    /// an `approve` handler answers later, what was decided before it.
    pub fn gate(&self, cx: &PointCx<'_>, tool: &str, input: &Json) -> Gating {
        let mut input = input.clone();
        let mut decisions = Decided::new();
        let chained = self.run::<PreTool>(cx, tool, &mut input, &mut |name, _, input| {
            let input = input.clone();
            decisions.push((Name::clone(name), Verdict::PreTool { input }));
        });
        let gate = match chained {
            Err((name, reason)) => {
                let block = Verdict::Block {
                    reason: reason.clone(),
                };
                decisions.push((name, block));
                Gate::Refuse(reason)
            }
            Ok(()) => match self.permit(cx, tool, &input, &mut decisions) {
                Ok(gate) => gate,
                Err(Asked::Later { by, at, answer }) => {
                    return Gating::Approving(Approving {
                        input,
                        decisions,
                        by,
                        next: at + 1,
                        answer,
                    });
                }
                Err(_) => unreachable!("only a later answer is left to come"),
            },
        };
        Gating::Now(Gated {
            input,
            decisions,
            gate,
        })
    }

    /// The `permission` and `approve` points over a call let through: the
    /// gate, or the `approve` answer that comes later.
    fn permit(
        &self,
        cx: &PointCx<'_>,
        tool: &str,
        input: &Json,
        decided: &mut Decided,
    ) -> Result<Gate, Asked<bool>> {
        let call = ToolUse { tool, input };
        let Some((by, permission)) = self.ask::<Permission>(cx, call) else {
            return Ok(Gate::Run);
        };
        decided.push((Name::clone(&by), Verdict::Permission { permission }));
        Ok(match permission {
            protocol::Permission::Allow => Gate::Run,
            protocol::Permission::Ask => match self.approve_from(cx, tool, input, 0) {
                Asked::None => Gate::Ask,
                Asked::Now(who, approved) => approval(who, approved, decided),
                later @ Asked::Later { .. } => return Err(later),
            },
            protocol::Permission::Defer => Gate::Refuse(format!("{by} deferred this call")),
            protocol::Permission::Deny => Gate::Refuse(format!("{by} denied this call")),
        })
    }

    /// The `approve` merge over a call asked about, from its `from`th
    /// handler on: what an answer that came later as none goes on to.
    pub fn approve_from(
        &self,
        cx: &PointCx<'_>,
        tool: &str,
        input: &Json,
        from: usize,
    ) -> Asked<bool> {
        self.ask_from::<Approve, _>(cx, ToolUse { tool, input }, from)
    }

    /// The answer at a first-answer point whose handlers may answer later,
    /// from its `from`th handler on.
    fn ask_from<P: Point<Out = Answer<Option<T>>>, T: 'static>(
        &self,
        cx: &P::Cx<'_>,
        facts: P::Facts<'_>,
        from: usize,
    ) -> Asked<T> {
        let handlers = self.handlers::<P>();
        let rest = handlers.get(from..).unwrap_or_default();
        P::Out::merge::<P>(rest, cx, facts, &mut (), &mut |_, (), ()| {}).from(from)
    }

    /// A call's outcome as the `post_tool` chain leaves it.
    pub fn post_tool(
        &self,
        cx: &PointCx<'_>,
        (tool, input): (&str, &Json),
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
