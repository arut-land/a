//! Extensions: every feature, compiled in or loaded, contributes its tools,
//! commands, handlers, text, state, services and agents through one
//! [`Registrar`] with the same verbs.
//!
//! Registration is ordered, by [`Tier`] and then install order, and that
//! order is the handlers' merge order. A later registration of a name
//! already taken replaces the earlier one in its install position: an
//! extension of the same name replaces all it contributed, and a tool,
//! command, text or agent of the same name, from another extension,
//! replaces that one. So a user's `bash` overrides the shell's without
//! reordering anything. Nothing replaces what a [`Tier::Guarded`] extension
//! contributes, except another guarded one. Within one extension a name
//! taken twice is an error, as is a second extension providing a service
//! another provides, or one needing a service none provides.

use std::any::{Any, TypeId, type_name};
use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;
use std::path::Path;
use std::sync::Arc;

use crate::assemble::Needs;
use crate::point::{ERASED, ErasedHandler, Handler, Hooked, Point};
use crate::query::{self, Answered, DynQuery, Query};
use crate::shape::Output;
use crate::state::Decl;
use crate::tool::Erased;
use crate::{Agent, DynTool, Fold, Name, Tool};

/// A unit of features, named so decisions can say who made them.
pub trait Extension: Send + Sync {
    /// Who the extension's handlers are, in logged decisions.
    const NAME: &'static str;
    const VERSION: u32;
    /// Whether the whole registration participates in owned child conversations.
    const INHERIT: bool = true;
    /// The services it needs of the extensions before it, as a list of
    /// [`Need`](crate::Need)s: `(Need<dyn CwdOf>, ())`. A static assembly
    /// ([`Built`](crate::host::Built)) that lacks one does not compile; a
    /// registry built at run time refuses the extension as it installs.
    type Needs: Needs = ();

    fn contribute(&self, r: &mut Registrar);
}

/// An extension named only at run time: what a Lua mod, a WASM component
/// or an extension process is. Every [`Extension`] is one.
pub trait DynExtension: Send + Sync {
    fn name(&self) -> Name;
    fn version(&self) -> u32;
    fn inherit(&self) -> bool {
        true
    }
    fn image(&self) -> Option<protocol::Json> {
        None
    }
    fn contribute(&self, r: &mut Registrar);
}

impl<E: Extension> DynExtension for E {
    fn name(&self) -> Name {
        Name::from(E::NAME)
    }

    fn version(&self) -> u32 {
        E::VERSION
    }

    fn inherit(&self) -> bool {
        E::INHERIT
    }

    fn contribute(&self, r: &mut Registrar) {
        E::Needs::declare(r);
        Extension::contribute(self, r);
    }
}

/// Where an extension loads: tiers load in this order, each in install
/// order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// What later tiers cannot replace or remove, such as the permission
    /// policy: repository config never loosens it.
    Guarded,
    /// What a binary links, which the user's extensions may replace.
    Pre,
    #[default]
    User,
    /// After the user's, to see what they did.
    Post,
}

/// Where a session runs, for the context the model is first given.
pub struct Facts<'a> {
    pub workspace: &'a Path,
    /// The directory its commands run in.
    pub dir: &'a Path,
}

/// Makes context text from where a session runs.
pub type Context = dyn Fn(&Facts<'_>) -> Option<String> + Send + Sync;

/// Where text the model reads goes, with what makes it.
#[derive(Clone)]
pub enum Place {
    /// The system prompt, the same for every session, so a provider caches
    /// it.
    System(Cow<'static, str>),
    /// The facts a session's model is first given, made from where it
    /// runs; logged then, so later turns read the same.
    Context(Arc<Context>),
    /// Context logged before the first model call on each live registry generation.
    Generation(Cow<'static, str>),
}

/// Text the model reads. Texts join by `order`, then install order, in
/// their place; one tied to a `tool` is left out unless that tool is
/// offered.
#[derive(Clone)]
pub struct Text {
    pub name: Name,
    pub order: i32,
    pub tool: Option<Name>,
    pub place: Place,
}

impl Text {
    /// Part of the system prompt.
    pub fn system(name: &str, order: i32, text: impl Into<Cow<'static, str>>) -> Self {
        Self {
            name: Name::from(name),
            order,
            tool: None,
            place: Place::System(text.into()),
        }
    }

    /// Context for this immutable registry generation, logged when adopted.
    pub fn generation(name: &str, order: i32, text: impl Into<Cow<'static, str>>) -> Self {
        Self {
            name: Name::from(name),
            order,
            tool: None,
            place: Place::Generation(text.into()),
        }
    }

    /// Part of the context `make` gives for where a session runs.
    pub fn context(
        name: &str,
        order: i32,
        make: impl Fn(&Facts<'_>) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: Name::from(name),
            order,
            tool: None,
            place: Place::Context(Arc::new(make)),
        }
    }

    /// The same text, left out unless `tool` is offered.
    #[must_use]
    pub fn for_tool(self, tool: &str) -> Self {
        Self {
            tool: Some(Name::from(tool)),
            ..self
        }
    }
}

/// A tool just offered ([`Registrar::tool`]): the registrar goes on from
/// it, and a [`Resumable`](crate::Resumable) tool is offered as one.
pub struct Tooled<'r, T: Tool, M> {
    registrar: &'r mut Registrar,
    tool: Arc<Erased<T, M>>,
}

impl<'r, T: crate::Resumable, M> Tooled<'r, T, M> {
    /// Its calls gone on in the background, and the groups they leave, go
    /// on in the daemon's next build after an upgrade.
    pub fn resumable(self) -> &'r mut Registrar
    where
        T::resume(..): Output<T::Out, M>,
    {
        let _ = self.tool.2.set(crate::tool::Resumer::of::<M>());
        self.registrar
    }
}

impl<T: Tool, M> std::ops::Deref for Tooled<'_, T, M> {
    type Target = Registrar;
    fn deref(&self) -> &Registrar {
        self.registrar
    }
}

impl<T: Tool, M> std::ops::DerefMut for Tooled<'_, T, M> {
    fn deref_mut(&mut self) -> &mut Registrar {
        self.registrar
    }
}

/// Makes an agent for a session.
type MakeAgent = dyn Fn() -> Box<dyn Agent> + Send + Sync;

#[derive(Clone)]
pub(crate) struct RegisteredAgent {
    pub name: Name,
    pub make: Arc<MakeAgent>,
    pub inherit: crate::Inheritance,
    pub settings: bool,
    pub configure: fn(&mut crate::States),
}

/// A service an extension provides: an `Arc<S>` of its trait object type.
pub(crate) struct Provided {
    pub ty: TypeId,
    pub name: &'static str,
    pub service: Box<dyn Any + Send + Sync>,
}

/// What an extension contributes, as it registers.
#[derive(Default)]
pub struct Registrar {
    pub(crate) tools: Vec<Arc<dyn DynTool>>,
    pub(crate) queries: Vec<Arc<dyn DynQuery>>,
    pub(crate) hooks: Vec<Hooked>,
    pub(crate) texts: Vec<Text>,
    pub(crate) states: Vec<Decl>,
    pub(crate) services: Vec<Provided>,
    pub(crate) needs: Vec<(TypeId, &'static str)>,
    pub(crate) agents: Vec<RegisteredAgent>,
    pub(crate) errors: Vec<String>,
}

impl Registrar {
    /// Offers `tool`, whose calls are shaped `M` as what it returns.
    pub fn tool<T: Tool, M: 'static>(&mut self, tool: T) -> Tooled<'_, T, M>
    where
        T::call(..): Output<T::Out, M>,
    {
        let tool = Arc::new(Erased(tool, PhantomData, std::sync::OnceLock::new()));
        self.dyn_tool(Arc::clone(&tool) as Arc<dyn DynTool>);
        Tooled {
            registrar: self,
            tool,
        }
    }

    /// Offers a tool named at run time.
    pub fn dyn_tool(&mut self, tool: Arc<dyn DynTool>) -> &mut Self {
        let names = self.tools.iter().map(|t| t.name());
        self.errors.extend(twice("tool", tool.name(), names));
        self.tools.push(tool);
        self
    }

    /// Answers query `Q` for clients, its answers shaped `M` as what its
    /// `answer` returns.
    pub fn query<Q: Query, M: 'static>(&mut self, query: Q) -> &mut Self
    where
        Q::answer(..): Answered<Q::Reply, M>,
    {
        self.dyn_query(Arc::new(query::Erased(query, PhantomData)))
    }

    /// Answers a query named at run time.
    pub fn dyn_query(&mut self, query: Arc<dyn DynQuery>) -> &mut Self {
        let names = self.queries.iter().map(|q| q.name());
        self.errors.extend(twice("query", query.name(), names));
        self.queries.push(query);
        self
    }

    /// Adds a handler at point `P`.
    pub fn on<P: Point>(
        &mut self,
        handler: impl for<'a> Fn(&P::Cx<'_>, P::In<'a>) -> P::Out + Send + Sync + 'static,
    ) -> &mut Self {
        let handler: Handler<P> = Arc::new(handler);
        self.hooks.push(Hooked::of::<P>(handler));
        self
    }

    /// Adds a handler over JSON at the point named `point`.
    pub fn on_erased(&mut self, point: &str, handler: Arc<dyn ErasedHandler>) -> &mut Self {
        let adapt = ERASED.iter().find(|(name, _)| *name == point);
        match adapt {
            Some((_, Some(adapt))) => self.hooks.push(adapt(handler)),
            Some((_, None)) => {
                let error = format!("point {point} takes no erased handlers");
                self.errors.push(error);
            }
            None => self.errors.push(format!("no point is named {point}")),
        }
        self
    }

    pub fn text(&mut self, text: Text) -> &mut Self {
        let names = self.texts.iter().map(|t| &*t.name);
        self.errors.extend(twice("text", &text.name, names));
        self.texts.push(text);
        self
    }

    /// Keeps state `S` per session, durably: see [`Fold`].
    pub fn fold<S: Fold>(&mut self) -> &mut Self {
        let taken = twice("state", S::NAME, self.states.iter().map(|s| &*s.name));
        self.errors.extend(taken);
        self.states.push(Decl::of::<S>());
        self
    }

    /// Keeps a fold whose name and representation are known at run time.
    pub fn dyn_fold(&mut self, fold: Arc<dyn crate::DynFold>) -> &mut Self {
        self.errors.extend(twice(
            "state",
            &fold.name(),
            self.states.iter().map(|s| &*s.name),
        ));
        self.states.push(Decl::erased(fold));
        self
    }

    /// Provides service `S`, as `r.provide::<dyn CwdOf>(Arc::new(..))`:
    /// every extension's, and the engine's, from now on.
    pub fn provide<S: ?Sized + Send + Sync + 'static>(&mut self, service: Arc<S>) -> &mut Self {
        let ty = TypeId::of::<S>();
        if self.services.iter().any(|s| s.ty == ty) {
            self.errors
                .push(format!("{} provided twice", type_name::<S>()));
        }
        self.services.push(Provided {
            ty,
            name: type_name::<S>(),
            service: Box::new(service),
        });
        self
    }

    /// Needs service `S` ([`Extension::Needs`]): the extension is refused
    /// unless one provides it.
    pub(crate) fn need<S: ?Sized + 'static>(&mut self) {
        self.needs.push((TypeId::of::<S>(), type_name::<S>()));
    }

    /// Registers a kind with its compile-time admission declaration.
    pub fn agent_kind<K: crate::AgentKind>(&mut self) -> &mut Self {
        self.agent(K::NAME, K::make);
        let agent = self.agents.last_mut().expect("just registered");
        agent.inherit = K::INHERIT;
        agent.settings = K::INHERIT_SETTINGS;
        agent.configure = K::settings;
        self
    }

    /// Offers an agent loop, which `make` starts for each session that
    /// runs it.
    pub fn agent(
        &mut self,
        name: &str,
        make: impl Fn() -> Box<dyn Agent> + Send + Sync + 'static,
    ) -> &mut Self {
        let names = self.agents.iter().map(|a| &*a.name);
        self.errors.extend(twice("agent", name, names));
        self.agents.push(RegisteredAgent {
            name: Name::from(name),
            make: Arc::new(make),
            inherit: crate::Inheritance::All,
            settings: true,
            configure: |_| {},
        });
        self
    }
}

/// The error for registering `name` again, if `names` has it already.
fn twice<'a>(what: &str, name: &str, mut names: impl Iterator<Item = &'a str>) -> Option<String> {
    names
        .any(|n| n == name)
        .then(|| format!("{what} {name} registered twice"))
}

/// Why an extension could not be registered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused(pub String);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}
