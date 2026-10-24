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
//! | [`Spawn`] | replacement chain over every process a tool starts; the first refusal wins |
//! | [`PostTool`] | replacement chain over the outcome |
//! | [`OnStop`] | the first continuation wins |
//! | [`Observe`] | every handler is told, and may react with work of its own |
//! | [`SessionStart`], [`SessionEnd`] | every handler is told, and changes nothing |
//! | [`Turn`], [`Reply`] | replacement chain over what the loop does next |
//! | [`Resume`] | the first answer wins; with none, the loop goes on |
//! | [`Dequeue`] | the first answer wins; with none, the queue drains as it always has |
//!
//! The loop points ([`Turn`], [`Reply`], [`Resume`], [`Dequeue`]) run
//! inside the fold of the session's log, or decide what it logs next, and
//! rerun as it loads: their handlers are given their arguments and nothing
//! of the session (a `()` for its context), so what a replay asks of them
//! is what the live run did.
//!
//! The four rules are [`Merge`]'s, each implemented once, by the
//! [`Rule`] that carries it, for every point. Handlers run in
//! registration order, each given a [`PointCx`]. A handler at a
//! first-answer point whose answer is an [`Answer`] may answer later: the
//! merge stops at it, and goes on from the handler after it if what comes
//! is no answer. What changed is reported as [`Verdict`]s, which the
//! engine logs under the extension's name; `spawn` rewrites are not
//! logged, as they follow from configuration rather than from what
//! happened.
//!
//! A handler that panics is an extension's failure, never the session's:
//! the rule catches it and answers for it as the point's [`OnError`] says,
//! so the policy a point declares is the one applied, at every call,
//! wherever the point is asked. The points are a closed set, declared
//! once by [`points!`], which also makes the [`Hooks`] table that holds
//! their handlers: a point cannot be added without its slot.

use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fmt;
use std::future::{Future, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Poll;
use std::time::Duration;

use protocol::{CallId, Entry, Json, Outcome, SessionId, Verdict};
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

/// What a handler that fails (panics) at a point comes to: declared by
/// every point, applied by its rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnError {
    /// It is passed over: what it would have changed stays as it was.
    FailOpen,
    /// What it decides about is refused: a call is not run, a process not
    /// started, input not taken.
    FailClosed,
}

mod sealed {
    use super::{Handler, Hooked, Hooks, Point};
    use crate::Name;

    /// Implemented by [`points!`](super::points) alone: every point has a
    /// slot in [`Hooks`], and nothing outside this module is a point.
    pub trait Sealed: Sized {
        fn hook(handler: Handler<Self>) -> Hooked
        where
            Self: Point;
        fn slot(hooks: &Hooks) -> &[(Name, Handler<Self>)]
        where
            Self: Point;
    }
}

/// A decision point.
pub trait Point: sealed::Sealed + Sized + 'static {
    /// The point's name, as an [`ErasedHandler`] gives it.
    const NAME: &'static str;
    /// What a handler that fails here comes to.
    const ON_ERROR: OnError;
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

    /// The answer of a handler that panicked, saying `why`: the point's
    /// [`Point::ON_ERROR`] applied.
    fn failed(why: String) -> Self::Out;

    /// What `handler`, extension `name`'s, answers to `input`; its panic
    /// is the point's failure answer.
    fn call(
        name: &Name,
        handler: &Handler<Self>,
        cx: &Self::Cx<'_>,
        input: Self::In<'_>,
    ) -> Self::Out {
        catch_unwind(AssertUnwindSafe(|| handler(cx, input)))
            .unwrap_or_else(|panic| Self::failed(format!("{name} {}", panicked(&*panic))))
    }
}

/// What a panic says.
pub(crate) fn panicked(panic: &(dyn Any + Send)) -> String {
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("unknown panic");
    format!("panicked: {message}")
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
/// - `()` and [`Reaction`] observe.
pub trait Rule: Sized + 'static {
    const MERGE: Merge;
    type Subject;
    type Merged;

    /// The answer of a handler that did not answer: what a failure at an
    /// [`OnError::FailOpen`] point comes to.
    fn open() -> Self;

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

/// An answer that can refuse: what a failure at an [`OnError::FailClosed`]
/// point comes to. A point declared to fail closed whose answer has no
/// refusal does not compile.
pub trait Closed: Rule {
    fn closed(why: String) -> Self;
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

    fn open() -> Self {
        Self::Pass
    }

    fn merge<P: Point<Out = Self>>(
        handlers: &[(Name, Handler<P>)],
        cx: &P::Cx<'_>,
        facts: P::Facts<'_>,
        subject: &mut T,
        replaced: &mut Replaced<'_, T>,
    ) -> Self::Merged {
        for (name, handler) in handlers {
            match P::call(name, handler, cx, P::input(facts, subject)) {
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

impl<T: 'static> Closed for Rewrite<T, String> {
    fn closed(why: String) -> Self {
        Self::Stop(why)
    }
}

impl<T: 'static> Rule for Option<T> {
    const MERGE: Merge = Merge::FirstSome;
    type Subject = ();
    /// The first answer, and who gave it.
    type Merged = Option<(Name, T)>;

    fn open() -> Self {
        None
    }

    fn merge<P: Point<Out = Self>>(
        handlers: &[(Name, Handler<P>)],
        cx: &P::Cx<'_>,
        facts: P::Facts<'_>,
        (): &mut (),
        _: &mut Replaced<'_, ()>,
    ) -> Self::Merged {
        handlers.iter().find_map(|(name, handler)| {
            let answer = P::call(name, handler, cx, P::input(facts, &()))?;
            Some((Name::clone(name), answer))
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

impl<T: Send + 'static> Rule for Answer<Option<T>> {
    const MERGE: Merge = Merge::FirstSome;
    type Subject = ();
    type Merged = Asked<T>;

    fn open() -> Self {
        Self::Now(None)
    }

    fn merge<P: Point<Out = Self>>(
        handlers: &[(Name, Handler<P>)],
        cx: &P::Cx<'_>,
        facts: P::Facts<'_>,
        (): &mut (),
        _: &mut Replaced<'_, ()>,
    ) -> Asked<T> {
        for (at, (name, handler)) in handlers.iter().enumerate() {
            match P::call(name, handler, cx, P::input(facts, &())) {
                Answer::Now(None) => {}
                Answer::Now(Some(answer)) => return Asked::Now(Name::clone(name), answer),
                Answer::Later(answer) => {
                    let by = Name::clone(name);
                    let answer = Later::new(guarded::<P, T>(Name::clone(&by), answer));
                    return Asked::Later { by, at, answer };
                }
            }
        }
        Asked::None
    }
}

/// `later`, handler `by`'s, its panic made the point's failure answer as
/// a handler's own is.
async fn guarded<P, T>(by: Name, mut later: Later<Option<T>>) -> Option<T>
where
    P: Point<Out = Answer<Option<T>>>,
    T: Send + 'static,
{
    let caught = poll_fn(
        |cx| match catch_unwind(AssertUnwindSafe(|| later.0.as_mut().poll(cx))) {
            Ok(poll) => poll.map(Ok),
            Err(panic) => Poll::Ready(Err(panicked(&*panic))),
        },
    )
    .await;
    caught.unwrap_or_else(|why| match P::failed(format!("{by} {why}")) {
        Answer::Now(answer) => answer,
        Answer::Later(_) => None,
    })
}

impl Closed for Answer<Option<bool>> {
    fn closed(_: String) -> Self {
        Self::Now(Some(false))
    }
}

impl Rule for protocol::Permission {
    const MERGE: Merge = Merge::MostRestrictive;
    type Subject = ();
    /// The most restrictive answer and who gave it, unless every handler
    /// allowed.
    type Merged = Option<(Name, Self)>;

    fn open() -> Self {
        Self::Allow
    }

    fn merge<P: Point<Out = Self>>(
        handlers: &[(Name, Handler<P>)],
        cx: &P::Cx<'_>,
        facts: P::Facts<'_>,
        (): &mut (),
        _: &mut Replaced<'_, ()>,
    ) -> Self::Merged {
        handlers
            .iter()
            .map(|(name, handler)| (name, P::call(name, handler, cx, P::input(facts, &()))))
            .filter(|(_, level)| level.rank() > Self::Allow.rank())
            .reduce(|most, next| {
                if next.1.rank() > most.1.rank() {
                    next
                } else {
                    most
                }
            })
            .map(|(name, level)| (Name::clone(name), level))
    }
}

impl Closed for protocol::Permission {
    fn closed(_: String) -> Self {
        Self::Deny
    }
}

impl Rule for () {
    const MERGE: Merge = Merge::Observe;
    type Subject = ();
    type Merged = ();

    fn open() -> Self {}

    fn merge<P: Point<Out = Self>>(
        handlers: &[(Name, Handler<P>)],
        cx: &P::Cx<'_>,
        facts: P::Facts<'_>,
        (): &mut (),
        _: &mut Replaced<'_, ()>,
    ) {
        for (name, handler) in handlers {
            P::call(name, handler, cx, P::input(facts, &()));
        }
    }
}

/// The log guarantee required before work can start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Requirement {
    Ordered,
    Durable,
}

/// An observer's answer: nothing, or work to do about what it saw, which
/// the engine runs owned by the session (a side model request, say) and
/// ends with it. An observer changes nothing itself.
pub struct Reaction {
    pub requirement: Requirement,
    pub work: Option<Later<()>>,
}

impl Reaction {
    pub const NONE: Self = Self {
        requirement: Requirement::Ordered,
        work: None,
    };

    pub fn later(
        requirement: Requirement,
        work: impl Future<Output = ()> + Send + 'static,
    ) -> Self {
        Self {
            requirement,
            work: Some(Later::new(work)),
        }
    }
}

impl Rule for Reaction {
    const MERGE: Merge = Merge::Observe;
    type Subject = ();
    /// What the handlers do about it, in their order.
    type Merged = Vec<Reaction>;

    fn open() -> Self {
        Self::NONE
    }

    fn merge<P: Point<Out = Self>>(
        handlers: &[(Name, Handler<P>)],
        cx: &P::Cx<'_>,
        facts: P::Facts<'_>,
        (): &mut (),
        _: &mut Replaced<'_, ()>,
    ) -> Self::Merged {
        handlers
            .iter()
            .map(|(name, handler)| P::call(name, handler, cx, P::input(facts, &())))
            .filter(|reaction| reaction.work.is_some())
            .collect()
    }
}

type DispatchLocals = BTreeMap<(TypeId, usize), Box<dyn Any + Send + Sync>>;

/// Ephemeral state owned by a call and shared with its named invocations.
/// It carries budgets across workers; it is neither a session service nor replay state.
#[derive(Clone, Default)]
pub struct InvocationContext(Arc<Mutex<DispatchLocals>>);
impl InvocationContext {
    pub fn local<T: Any + Send + Sync>(&self, key: usize, init: impl FnOnce() -> Arc<T>) -> Arc<T> {
        let slot = (TypeId::of::<T>(), key);
        let existing = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&slot)
            .and_then(|value| value.downcast_ref::<Arc<T>>())
            .cloned();
        if let Some(existing) = existing {
            return existing;
        }
        let value = init();
        let mut locals = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            locals
                .entry(slot)
                .or_insert_with(|| Box::new(value))
                .downcast_ref::<Arc<T>>()
                .expect("typed invocation local"),
        )
    }

    /// Installs this owned context for synchronous dispatch only, never across an await.
    pub fn scope<T>(&self, run: impl FnOnce() -> T) -> T {
        let _scope = InvocationScope(DISPATCH.replace(Some(self.clone())));
        run()
    }
}

thread_local! {
    static DISPATCH: RefCell<Option<InvocationContext>> = const { RefCell::new(None) };
}
struct InvocationScope(Option<InvocationContext>);
impl Drop for InvocationScope {
    fn drop(&mut self) {
        DISPATCH.replace(self.0.take());
    }
}
struct Dispatch {
    _scope: Option<InvocationScope>,
}
impl Dispatch {
    fn enter() -> Self {
        Self {
            _scope: DISPATCH
                .with_borrow(Option::is_none)
                .then(|| InvocationScope(DISPATCH.replace(Some(InvocationContext::default())))),
        }
    }
}

/// State shared by synchronous handlers and their owned invocation context.
/// `key` identifies a resource within the context. Without a context, state is fresh.
pub fn dispatch_local<T: Any + Send + Sync>(key: usize, init: impl FnOnce() -> Arc<T>) -> Arc<T> {
    let context = DISPATCH.with_borrow(Clone::clone);
    match context {
        Some(context) => context.local(key, init),
        None => init(),
    }
}

/// What every handler is given: the session it decides for, and the call
/// it decides about, if one.
pub struct PointCx<'a> {
    pub session: SessionId,
    pub workspace: &'a Path,
    /// The call: the seq of its intent.
    pub call: Option<CallId>,
    view: &'a dyn View,
    tool: Option<ToolUse<'a>>,
}

/// The engine's side of a [`PointCx`], read only when a handler asks.
pub trait View: Sync {
    fn session(&self) -> SessionId;
    fn workspace(&self) -> &Path;
    /// The model the session talks to, as `host:model`.
    fn model(&self) -> String;
    fn states(&self, read: &mut dyn FnMut(&States));
    /// The environment the session's commands run in, if a client gave
    /// one.
    fn env(&self) -> Option<Arc<protocol::Environment>> {
        None
    }
    /// What call `call` runs with, owned, for work its handler answers
    /// later with.
    fn cx(&self, _call: CallId) -> Option<Cx> {
        None
    }
}

impl<'a> PointCx<'a> {
    pub fn new(view: &'a dyn View) -> Self {
        Self {
            session: view.session(),
            workspace: view.workspace(),
            call: None,
            view,
            tool: None,
        }
    }

    /// The same, about call `call`.
    #[must_use]
    pub fn about(self, call: CallId) -> Self {
        Self {
            call: Some(call),
            ..self
        }
    }

    /// What the call this decides about runs with, owned and cheap to
    /// clone, so an [`Answer::Later`] can take it along: to a
    /// [`Conversation`](crate::Conversation), say. `None` for a point about no call.
    pub fn cx(&self) -> Option<Cx> {
        self.call.and_then(|call| self.view.cx(call))
    }

    /// The owning tool, when an observation is about one of its facts.
    pub const fn tool_use(&self) -> Option<ToolUse<'a>> {
        self.tool
    }

    #[must_use]
    pub fn with_tool(mut self, tool: ToolUse<'a>) -> Self {
        self.tool = Some(tool);
        self
    }

    pub fn model(&self) -> String {
        self.view.model()
    }

    /// Reads state `S`, or its default when no extension registered it.
    pub fn state<S: Fold, R>(&self, read: impl FnOnce(&S) -> R) -> R {
        crate::state::read(|f| self.view.states(f), read)
    }

    pub fn state_erased(&self, ext: &str, name: &str) -> Option<Json> {
        let mut value = None;
        self.view
            .states(&mut |states| value = states.erased(ext, name).cloned());
        value
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

/// A model's reply at a step of the loop, as [`Reply`] handlers see it.
#[derive(Clone, Copy, Debug)]
pub struct Replied<'a> {
    /// The model calls of the turn so far, this one included.
    pub step: u16,
    pub reply: &'a llm::Response,
    /// What the loop does with it, as the handlers before left it.
    pub actions: &'a Actions,
}

fn pre_tool<'a, 'b: 'a>(tool: &'b str, input: &'a Json) -> ToolUse<'a> {
    ToolUse { tool, input }
}

fn post_tool<'a, 'b: 'a>(call: ToolUse<'b>, outcome: &'a Outcome) -> Finished<'a> {
    Finished {
        tool: call.tool,
        input: call.input,
        outcome,
    }
}

fn turn<'a, 'b: 'a>(
    input: &'b crate::Input,
    actions: &'a Actions,
) -> (&'a crate::Input, &'a Actions) {
    (input, actions)
}

fn reply<'a, 'b: 'a>((step, reply): (u16, &'b llm::Response), actions: &'a Actions) -> Replied<'a> {
    Replied {
        step,
        reply,
        actions,
    }
}

/// Declares the points, in one table: each one's name, whether it takes
/// erased handlers (`wire`), what a failing handler comes to, what its
/// handlers are given (their context, the facts, what they read) and what
/// they answer; and, for one whose handlers read the facts and the subject
/// together, how. From the same rows come [`Hooks`], with a slot per
/// point, and the table of erased points.
macro_rules! points {
    ($(
        $(#[$doc:meta])*
        $Point:ident($slot:ident) = $name:literal, $wire:tt, $on_error:ident,
        $cx:ty, $facts:ty, $in:ty, $out:ty $(, $read:path)?;
    )*) => {
        $(
            $(#[$doc])*
            pub struct $Point;

            impl sealed::Sealed for $Point {
                fn hook(handler: Handler<Self>) -> Hooked {
                    Hooked::$Point(handler)
                }

                fn slot(hooks: &Hooks) -> &[(Name, Handler<Self>)] {
                    &hooks.$slot
                }
            }

            impl Point for $Point {
                const NAME: &'static str = $name;
                const ON_ERROR: OnError = OnError::$on_error;
                type Cx<'c> = $cx;
                type Facts<'a> = $facts;
                type In<'a> = $in;
                type Out = $out;

                fn input<'a, 'b: 'a>(
                    facts: Self::Facts<'b>,
                    subject: &'a Subject<Self>,
                ) -> Self::In<'a> {
                    points!(@read facts, subject $(, $read)?)
                }

                fn failed(why: String) -> Self::Out {
                    points!(@failed $on_error, why)
                }
            }
        )*

        /// The handlers at each point, in the order they run, each with
        /// its extension's name, as a [`Registry`](crate::host::Registry)
        /// resolves them. The default, none, allows everything.
        #[derive(Default)]
        pub struct Hooks {
            $( $slot: Vec<(Name, Handler<$Point>)>, )*
        }

        /// One handler as an extension registered it, waiting for its
        /// slot.
        #[doc(hidden)]
        pub enum Hooked {
            $( $Point(Handler<$Point>), )*
        }

        impl Hooks {
            pub(crate) fn add(&mut self, name: &Name, hooked: &Hooked) {
                match hooked {
                    $( Hooked::$Point(handler) => {
                        self.$slot.push((Name::clone(name), Arc::clone(handler)));
                    } )*
                }
            }
        }

        /// Every point by name, and how an erased handler adapts to it,
        /// for the points that take one.
        pub(crate) const ERASED: &[(&str, Option<Adapt>)] = &[
            $( ($name, points!(@wire $wire, $Point)), )*
        ];
    };
    (@read $facts:ident, $subject:ident) => {{
        let _ = $subject;
        $facts
    }};
    (@read $facts:ident, $subject:ident, $read:path) => {
        $read($facts, $subject)
    };
    (@failed FailOpen, $why:ident) => {{
        let _ = $why;
        <Self::Out as Rule>::open()
    }};
    (@failed FailClosed, $why:ident) => {
        <Self::Out as Closed>::closed($why)
    };
    (@wire wire, $Point:ident) => {
        Some(adapt::<$Point>)
    };
    (@wire -, $Point:ident) => {
        None
    };
}

points! {
    /// What becomes of the user's input before it is logged: a rejection
    /// refuses it, and its client is told why.
    Input(input) = "input", wire, FailClosed,
        PointCx<'c>, (), &'a str, Rewrite<String, String>, text;
    /// The request of a model call, about to be sent: a handler may
    /// rewrite it (a model fallback, a reminder appended at the tail) or
    /// answer it itself (a fixture, a cache), with a reply or an error (a
    /// request too large to send fails as an overflow before it costs
    /// anything). Each rewrite is logged as `prepare_step`, and an answer
    /// as the call's outcome.
    Step(step) = "step", -, FailOpen,
        PointCx<'c>, (), &'a llm::Request, Rewrite<llm::Request, Stepped>, same;
    /// How to recover from a failed model request: send it again later, or
    /// a new one at once. With no answer the call fails. A handler may
    /// answer later, after a side model request, say: the call waits
    /// meanwhile, and an interrupt ends it.
    RequestError(request_error) = "request_error", -, FailOpen,
        PointCx<'c>, Failed<'a>, Failed<'a>, Answer<Option<Recover>>;
    /// What a tool call runs with: a replacement input, or a block that
    /// answers the call with why instead of running it.
    PreTool(pre_tool) = "pre_tool", wire, FailClosed,
        PointCx<'c>, &'a str, ToolUse<'a>, Rewrite<Json, String>, pre_tool;
    /// Whether a call may run.
    Permission(permission) = "permission", wire, FailClosed,
        PointCx<'c>, ToolUse<'a>, ToolUse<'a>, protocol::Permission;
    /// Answers a permission `ask`. An ask no handler answers goes to the
    /// user, durably: see [`Gate::Ask`]. A handler may answer later, after
    /// a side model request, say: the call waits meanwhile.
    Approve(approve) = "approve", wire, FailClosed,
        PointCx<'c>, ToolUse<'a>, ToolUse<'a>, Answer<Option<bool>>;
    /// A replacement for a process a tool is about to start, or a refusal
    /// to start it: a sandbox that cannot wrap a process never lets it run
    /// bare, so a handler that fails refuses it too.
    Spawn(spawn) = "spawn", -, FailClosed,
        PointCx<'c>, (), &'a crate::SpawnSpec, Rewrite<crate::SpawnSpec, String>, same;
    /// A replacement for a call's outcome, before it is committed.
    PostTool(post_tool) = "post_tool", -, FailOpen,
        PointCx<'c>, ToolUse<'a>, Finished<'a>, Rewrite<Outcome>, post_tool;
    /// Text to continue with once the agent stopped. A handler that reads
    /// the session's log folds what it needs of it as typed state.
    OnStop(on_stop) = "on_stop", wire, FailOpen,
        PointCx<'c>, (), (), Option<String>;
    /// Each entry, once committed; about the call an intent names. A
    /// handler reacts with work the engine runs owned by the session.
    Observe(observe) = "observe", wire, FailOpen,
        PointCx<'c>, &'a Entry, &'a Entry, Reaction;
    /// The session's actor started: it was created, or loaded again.
    SessionStart(session_start) = "session_start", -, FailOpen,
        PointCx<'c>, (), (), ();
    /// The session's actor stopped: it hibernated, or the daemon drained.
    SessionEnd(session_end) = "session_end", -, FailOpen,
        PointCx<'c>, (), (), ();
    /// What an input starts, as the default loop asks: one model call
    /// unless a handler says otherwise (a planner, a plan mode, a command
    /// answered without the model).
    Turn(turn) = "turn", -, FailOpen,
        (), &'a crate::Input, (&'a crate::Input, &'a Actions), Rewrite<Actions>, turn;
    /// What the model's reply becomes, as the default loop asks: a call
    /// per tool call unless a handler says otherwise (sequential calls
    /// only, a dedupe, a refusal).
    Reply(reply) = "reply", -, FailOpen,
        (), (u16, &'a llm::Response), Replied<'a>, Rewrite<Actions>, reply;
    /// What follows once every call of a step's reply is answered, as the
    /// default loop asks: with no answer, another model call.
    Resume(resume) = "resume", -, FailOpen,
        (), u16, u16, Option<crate::Resume>;
    /// What the kernel takes of the session's queue at a boundary: with no
    /// answer, [`Take::default_at`](crate::Take::default_at). A policy
    /// (all at once, one at a time, into each tool's result) is a handler.
    Dequeue(dequeue) = "dequeue", -, FailOpen,
        (), (crate::Boundary, crate::QueueView<'a>), (crate::Boundary, crate::QueueView<'a>),
        Option<crate::Take>;
}

/// The subject of a chain whose handlers read it alone.
fn same<T>((): (), subject: &T) -> &T {
    subject
}

fn text((): (), text: &String) -> &str {
    text
}

/// A handler over JSON text: what a Lua mod, a WASM component or an
/// extension process registers. It is given the point's input as JSON,
/// and answers as the point's [`Rule`] reads JSON: `null` passes, a chain
/// takes `{"replace": ..}` or `{"stop": ..}`, a permission its name. The
/// text is handed over as it is, for the handler to read as it reads JSON.
pub trait ErasedHandler: Send + Sync {
    fn on(&self, cx: &PointCx<'_>, args: &Json) -> Result<Json, String>;
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

    fn from_json(_: &Json) -> Reaction {
        Reaction::NONE
    }
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

/// Adapts an erased handler to its point.
type Adapt = fn(Arc<dyn ErasedHandler>) -> Hooked;

fn adapt<P: Wire>(erased: Arc<dyn ErasedHandler>) -> Hooked {
    let handler: Handler<P> = Arc::new(move |cx, input| match erased.on(cx, &P::to_json(&input)) {
        Ok(answer) => P::from_json(&answer),
        Err(why) => P::failed(why),
    });
    P::hook(handler)
}

impl Hooked {
    pub(crate) fn of<P: Point>(handler: Handler<P>) -> Self {
        P::hook(handler)
    }
}

/// Decisions to log, with the extension whose handler made each.
pub type Decided = Vec<(Name, Verdict)>;

/// What a `step` handler answered a model call with, in its place.
pub type Stepped = Result<llm::Response, llm::Error>;

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

impl Hooks {
    fn handlers<P: Point>(&self) -> &[(Name, Handler<P>)] {
        P::slot(self)
    }

    /// Merges what the handlers at `P` answer, by `P`'s rule.
    pub fn run<P: Point>(
        &self,
        cx: &P::Cx<'_>,
        facts: P::Facts<'_>,
        subject: &mut Subject<P>,
        replaced: &mut Replaced<'_, Subject<P>>,
    ) -> Merged<P> {
        let _dispatch = Dispatch::enter();
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
    ) -> (Decided, Option<(Name, Stepped)>) {
        let mut decisions = Decided::new();
        let answered = self.run::<Step>(cx, (), request, &mut |name, before, after| {
            let model = (before.model != after.model).then(|| after.model.to_string());
            decisions.push((Name::clone(name), Verdict::PrepareStep { model }));
        });
        (decisions, answered.err())
    }

    /// `spawn` as the `spawn` chain leaves it; `Err` is who refused the
    /// process, and why.
    pub fn spawn(
        &self,
        cx: &PointCx<'_>,
        spawn: &mut crate::SpawnSpec,
    ) -> Result<(), (Name, String)> {
        self.run::<Spawn>(cx, (), spawn, &mut |_, _, _| {})
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
        decided.push((
            Name::clone(&by),
            Verdict::Permission {
                permission: permission.clone(),
            },
        ));
        Ok(match permission {
            protocol::Permission::Allow => Gate::Run,
            protocol::Permission::Ask(_) => match self.approve_from(cx, tool, input, 0) {
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
    fn ask_from<P: Point<Out = Answer<Option<T>>>, T: Send + 'static>(
        &self,
        cx: &P::Cx<'_>,
        facts: P::Facts<'_>,
        from: usize,
    ) -> Asked<T> {
        let handlers = self.handlers::<P>();
        let rest = handlers.get(from..).unwrap_or_default();
        let _dispatch = Dispatch::enter();
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

    /// Tells every observer of `entry`, about the call it is the intent
    /// of, if one: what they do about it, for the engine to run owned by
    /// the session.
    pub fn observe(&self, cx: PointCx<'_>, entry: &Entry) -> Vec<Reaction> {
        if self.observe.is_empty() {
            return Vec::new();
        }
        let intent = matches!(
            entry.body,
            protocol::Body::ToolCall { .. } | protocol::Body::ModelCall { .. }
        );
        let cx = if intent {
            cx.about(CallId(entry.seq))
        } else if let protocol::Body::Decision {
            call: Some(call), ..
        } = &entry.body
        {
            cx.about(*call)
        } else {
            cx
        };
        self.ask::<Observe>(&cx, entry)
    }
}

#[cfg(test)]
mod tests;
