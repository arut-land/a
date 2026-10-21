//! The engine's side of extensions: the registry they install into, what
//! it resolves for sessions, how a running call reaches the engine, and
//! the registry as the kernel binds it to a session. Extensions need none
//! of it.

use std::any::{Any, TypeId};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use protocol::{Audience, CallId, Card, Environment, Installed, Json, Outcome, Seq, SessionId};

use crate::ext::MakeAgent;
use crate::point::{Approving, Asked, Failed, Later, Stepped, View};
use crate::state::States;
use crate::{
    Agent, Answer, BoxFuture, Callable, Called, Cancel, Chained, CwdOf, Cx, Defaults, DynExtension,
    DynTool, Extension, Facts, Gated, Gating, Loop, LoopPoints, Name, Parsed, Place, Recover,
    Refused, Registrar, Text, Tier,
};

pub use crate::assemble::Built;
pub use crate::point::{Decided, Hooks};

/// The engine, as a running call reaches it.
pub trait Host: Send + Sync {
    /// See [`Cx::spawned`].
    fn spawned(&self, pgid: u32, background: bool);
    /// See [`Cx::own`]: the engine runs `ended`, and the session owns
    /// `pgid` until it resolves.
    fn own(&self, pgid: u32, ended: BoxFuture<'static, ()>);
    /// See [`Cx::progress`].
    fn progress(&self, text: &str);
    /// Commits `data` as a change to extension `ext`'s state `state`:
    /// resolves once it is committed and folded in.
    fn note(&self, ext: &str, state: &str, data: Json) -> BoxFuture<'static, ()>;
    /// Seals a publication identity and model context in one ordinary entry.
    fn publish(&self, namespace: &str, key: Json, text: String) -> BoxFuture<'static, ()>;
    /// Commits an `edit`: see [`Cx::edit`].
    fn edit(&self, target: Seq, upto: Option<Seq>, replace: Option<Json>);
    /// Lends the session's states to `read`, which cannot keep them.
    fn states(&self, read: &mut dyn FnMut(&States));
    /// The registry generation the call started on.
    fn registry(&self) -> &Registry;
}

/// The services the engine provides a session's calls, beside the
/// extensions': each by the type of its trait object, as
/// [`Cx::service`] asks. Built once per session.
#[derive(Default)]
pub struct Services(Vec<(TypeId, Box<dyn Any + Send + Sync>)>);

impl Services {
    /// With service `S`, as `services.with::<dyn Models>(Arc::new(..))`.
    #[must_use]
    pub fn with<S: ?Sized + Send + Sync + 'static>(mut self, service: Arc<S>) -> Self {
        self.0.push((TypeId::of::<S>(), Box::new(service)));
        self
    }

    pub(crate) fn get<S: ?Sized + 'static>(&self) -> Option<&S> {
        let ty = TypeId::of::<S>();
        (self.0.iter())
            .find(|(t, _)| *t == ty)
            .and_then(|(_, s)| s.downcast_ref::<Arc<S>>())
            .map(|s| &**s)
    }
}

/// What a [`Cx`] is made of, but its host.
pub struct Parts {
    pub workspace: Arc<Path>,
    pub env: Option<Arc<Environment>>,
    pub session: SessionId,
    pub call: CallId,
    pub model: Arc<str>,
    pub spill: Arc<Path>,
    pub cancel: Cancel,
    pub services: Arc<Services>,
}

impl Parts {
    /// The call's context, reaching the engine through `host`.
    pub fn cx(self, host: Arc<dyn Host>) -> Cx {
        Cx::new(self, host)
    }
}

/// The tools an agent may call, by name, as a [`Registry`] resolves them.
#[derive(Default)]
pub struct Toolset(Vec<Arc<dyn DynTool>>);

impl Toolset {
    pub fn get(&self, name: &str) -> Option<&dyn DynTool> {
        self.0.iter().map(|t| &**t).find(|t| t.name() == name)
    }

    /// Every tool the model may call, as offered to it.
    pub fn specs(&self) -> Vec<llm::Tool> {
        (self.0.iter())
            .filter(|t| t.audience() == Audience::Model)
            .map(|t| t.spec())
            .collect()
    }

    /// Runs a call of tool `name` on `input`, read now unless `parsed`
    /// holds it read already.
    pub fn run<'a>(
        &'a self,
        name: &str,
        input: &Json,
        parsed: Option<Parsed>,
        cx: &'a Cx,
    ) -> BoxFuture<'a, Called> {
        let Some(tool) = self.get(name) else {
            let error = format!("unknown tool {name}");
            return Box::pin(std::future::ready(Called::failed(error)));
        };
        match parsed.map_or_else(|| tool.parse(input), Ok) {
            Ok(input) => tool.call(input, cx),
            Err(error) => Box::pin(std::future::ready(Called::failed(error))),
        }
    }

    /// What the model sees of a call's outcome.
    pub fn to_model(&self, name: &str, outcome: &Outcome) -> (llm::Output, bool) {
        match outcome {
            Outcome::Done { output } => (
                self.get(name)
                    .map_or_else(|| llm::Output::Json(output.clone()), |t| t.to_model(output)),
                false,
            ),
            Outcome::Failed { error } => (llm::Output::Text(error.clone()), true),
            Outcome::Interrupted { output } => {
                let mut text = "interrupted before it finished".to_owned();
                if let Some(output) = output.as_deref().filter(|o| !o.is_empty()) {
                    text = format!("{output}\n[{text}]");
                }
                (llm::Output::Text(text), true)
            }
        }
    }
}

#[derive(Clone)]
struct Loaded {
    name: Name,
    version: u32,
    tier: Tier,
    made: Arc<Registrar>,
}

/// The extensions a daemon runs with, and what they make.
#[derive(Default)]
pub struct Registry {
    installed: Vec<Loaded>,
    tools: Toolset,
    hooks: Hooks,
    texts: Vec<Text>,
    agents: Vec<(Name, Arc<MakeAgent>)>,
    /// What every model call on this generation sends, made on first use:
    /// a registry never changes once built, so neither do they.
    prompt: OnceLock<String>,
    specs: OnceLock<llm::Tools>,
    warnings: Vec<String>,
}

impl Clone for Registry {
    fn clone(&self) -> Self {
        let mut registry = Self {
            installed: self.installed.clone(),
            ..Self::default()
        };
        registry.rebuild();
        registry
    }
}

impl Registry {
    /// Registers `ext` in [`Tier::User`]: see [`Registry::with_in`].
    pub fn with<E: Extension>(self, ext: &E) -> Result<Self, Refused> {
        self.with_in(Tier::User, ext)
    }

    /// Registers `ext` after every extension so far in `tier`, or in place
    /// of the one of the same name. What `ext` needs is checked as it
    /// installs; [`Built`] checks it as the binary compiles.
    pub fn with_in<E: Extension>(self, tier: Tier, ext: &E) -> Result<Self, Refused> {
        self.with_dyn(tier, ext)
    }

    /// [`Registry::with_in`] for an extension named at run time.
    pub fn with_dyn(self, tier: Tier, ext: &dyn DynExtension) -> Result<Self, Refused> {
        let mut made = Registrar::default();
        ext.contribute(&mut made);
        self.install(Loaded {
            name: ext.name(),
            version: ext.version(),
            tier,
            made: Arc::new(made),
        })
    }

    /// Installs `new` unless it breaks a rule, and rebuilds what the
    /// extensions make, in install order.
    fn install(mut self, mut new: Loaded) -> Result<Self, Refused> {
        let name = Name::clone(&new.name);
        let refuse = |why: &str| Err(Refused(format!("extension {name}: {why}")));
        if let Some(error) = new.made.errors.first() {
            return refuse(error);
        }
        let guarded = |i: &Loaded| i.name == new.name && i.tier == Tier::Guarded;
        if new.tier != Tier::Guarded && self.installed.iter().any(guarded) {
            return refuse("a guarded extension of that name is loaded");
        }
        for service in &new.made.services {
            let holder = self
                .installed
                .iter()
                .find(|i| i.name != new.name && i.made.services.iter().any(|s| s.ty == service.ty));
            if let Some(holder) = holder {
                return refuse(&format!(
                    "{} already provides {}",
                    holder.name, service.name
                ));
            }
        }
        if let Some(slot) = self.installed.iter_mut().find(|i| i.name == new.name) {
            new.tier = slot.tier;
            *slot = new;
        } else {
            let at = self.installed.partition_point(|i| i.tier <= new.tier);
            self.installed.insert(at, new);
        }
        for i in &self.installed {
            let provided = |ty| {
                self.installed
                    .iter()
                    .any(|p| p.made.services.iter().any(|s| s.ty == ty))
            };
            if let Some((_, need)) = i.made.needs.iter().find(|(ty, _)| !provided(*ty)) {
                let who = if i.name == name { "it" } else { &*i.name };
                return refuse(&format!("{who} needs {need}, which no extension provides"));
            }
        }
        self.rebuild();
        Ok(self)
    }

    fn rebuild(&mut self) {
        let mut tools = Vec::new();
        let mut texts = Vec::new();
        let mut agents = Vec::new();
        let mut hooks = Hooks::default();
        let mut warnings = Vec::new();
        for Loaded {
            name, made, tier, ..
        } in &self.installed
        {
            let by = || By {
                tier: *tier,
                ext: Name::clone(name),
            };
            let warn = &mut warnings;
            for tool in &made.tools {
                replace(
                    &mut tools,
                    (by(), Arc::clone(tool)),
                    |t| t.1.name(),
                    "tool",
                    warn,
                );
            }
            for text in &made.texts {
                replace(
                    &mut texts,
                    (by(), text.clone()),
                    |t| &t.1.name,
                    "text",
                    warn,
                );
            }
            for agent in &made.agents {
                replace(
                    &mut agents,
                    (by(), agent.clone()),
                    |a| &a.1.0,
                    "agent",
                    warn,
                );
            }
            for hooked in &made.hooks {
                hooks.add(name, hooked);
            }
        }
        self.warnings = warnings;
        texts.sort_by_key(|t| t.1.order);
        self.tools = Toolset(tools.into_iter().map(|t| t.1).collect());
        self.texts = texts.into_iter().map(|t| t.1).collect();
        self.agents = agents.into_iter().map(|a| a.1).collect();
        self.hooks = hooks;
        self.prompt = OnceLock::new();
        self.specs = OnceLock::new();
    }

    pub fn tools(&self) -> &Toolset {
        &self.tools
    }

    pub fn hooks(&self) -> &Hooks {
        &self.hooks
    }

    /// What the extensions did that is worth telling: one replacing an
    /// item of another of its tier, which a typo could do unseen.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The callables the user runs as `/name args`, in registration order.
    pub fn commands(&self) -> impl Iterator<Item = &dyn DynTool> {
        (self.tools.0.iter())
            .map(|t| &**t)
            .filter(|t| t.audience() == Audience::User)
    }

    /// A new loop of the agent named `name`: with none named, or the
    /// default's name, the default [`Loop`]. A name no extension registers
    /// is reported, and the default runs instead.
    pub fn agent(&self, name: Option<&str>) -> Box<dyn Agent> {
        let named = name.filter(|&n| n != crate::LOOP).and_then(|name| {
            let named = self.agents.iter().find(|(n, _)| &**n == name);
            if named.is_none() {
                eprintln!("warning: no agent named {name} is registered");
            }
            named
        });
        match named {
            Some((_, make)) => make(),
            None => Box::new(Loop::default()),
        }
    }

    /// The names of the agents sessions can run: the default loop's, then
    /// the extensions'.
    pub fn agents(&self) -> impl Iterator<Item = &str> + '_ {
        std::iter::once(crate::LOOP).chain(self.agents.iter().map(|(name, _)| &**name))
    }

    /// Service `S`, if an extension provides it.
    pub fn service<S: ?Sized + 'static>(&self) -> Option<&S> {
        self.installed
            .iter()
            .flat_map(|i| &i.made.services)
            .find_map(|s| s.service.downcast_ref::<Arc<S>>())
            .map(|s| &**s)
    }

    /// The directory a session's commands run in, as the [`CwdOf`] service
    /// has it: with no extension to run commands, the workspace.
    pub fn dir(&self, workspace: &Path, env: Option<&Environment>, states: &States) -> PathBuf {
        self.service::<dyn CwdOf>().map_or_else(
            || workspace.to_path_buf(),
            |cwd| cwd.dir(workspace, env, states),
        )
    }

    /// Whether `text` is offered with the tools this registry has.
    fn offered(&self, text: &Text) -> bool {
        text.tool
            .as_ref()
            .is_none_or(|tool| self.tools.get(tool).is_some())
    }

    /// The system prompt the texts make with the tools offered.
    pub fn system_prompt(&self) -> &str {
        self.prompt.get_or_init(|| {
            let texts: Vec<&str> = self
                .texts
                .iter()
                .filter(|t| self.offered(t))
                .filter_map(|t| match &t.place {
                    Place::System(text) => Some(&**text),
                    Place::Context(_) | Place::Generation(_) => None,
                })
                .collect();
            texts.join("\n\n")
        })
    }

    /// Every tool, as offered to the model: each schema parsed once.
    pub fn specs(&self) -> &llm::Tools {
        self.specs.get_or_init(|| self.tools.specs().into())
    }

    /// The context the texts give for `facts`, if any.
    pub fn context(&self, facts: &Facts<'_>) -> Option<String> {
        let parts: Vec<String> = self
            .texts
            .iter()
            .filter(|t| self.offered(t))
            .filter_map(|t| match &t.place {
                Place::Context(make) => make(facts),
                Place::Generation(_) | Place::System(_) => None,
            })
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// Context supplied by this immutable registry revision.
    pub fn generation_context(&self) -> Option<String> {
        let parts: Vec<_> = self
            .texts
            .iter()
            .filter(|t| self.offered(t))
            .filter_map(|t| match &t.place {
                Place::Generation(text) => Some(text.as_ref()),
                _ => None,
            })
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// Fresh state for a session, as every extension registered it.
    pub fn states(&self) -> States {
        States::new(self.installed.iter().flat_map(|i| {
            i.made
                .states
                .iter()
                .map(|decl| (Name::clone(&i.name), decl.clone()))
        }))
    }

    /// Every extension's name and version, in install order.
    pub fn extensions(&self) -> impl Iterator<Item = (&str, u32)> + '_ {
        self.installed.iter().map(|i| (&*i.name, i.version))
    }

    /// The extensions, as a log records them.
    pub fn installed(&self) -> Vec<Installed> {
        self.extensions()
            .map(|(name, version)| Installed {
                name: name.to_owned(),
                version,
            })
            .collect()
    }
}

/// Puts `new` in place of the item of the same name, or last; a guarded
/// item is replaced only by another.
fn replace<T>(
    items: &mut Vec<(By, T)>,
    new: (By, T),
    name: impl Fn(&(By, T)) -> &str,
    what: &str,
    warnings: &mut Vec<String>,
) {
    match items.iter_mut().find(|t| name(t) == name(&new)) {
        Some(slot) if slot.0.tier == Tier::Guarded && new.0.tier != Tier::Guarded => {}
        Some(slot) => {
            if slot.0.tier == new.0.tier && slot.0.ext != new.0.ext {
                warnings.push(format!(
                    "{what} {} of extension {} replaces the one of {}, in the same tier",
                    name(&new),
                    new.0.ext,
                    slot.0.ext
                ));
            }
            *slot = new;
        }
        None => items.push(new),
    }
}

/// Who contributed an item, and where.
struct By {
    tier: Tier,
    ext: Name,
}

/// An owned workspace subscription. Reads never perform I/O or prepare work.
pub trait Generations: Send + Sync {
    fn generation(&self) -> u64;
    fn registry(&self) -> Arc<Registry>;
    /// Registers a weak callback. Mutable sources notify it after publication,
    /// off the async worker, before preparation returns.
    fn subscribe(&self, _changed: Weak<Changed>) {}
}

pub type Changed = dyn Fn() + Send + Sync;

/// Weak subscriptions do not keep derived sources or sessions alive.
#[derive(Default)]
pub struct Updates(Mutex<Vec<Weak<Changed>>>);

impl Updates {
    pub fn subscribe(&self, changed: Weak<Changed>) {
        let mut listeners = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        listeners.retain(|listener| listener.strong_count() > 0);
        listeners.push(changed);
    }

    pub fn notify(&self) {
        let listeners: Vec<_> = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for changed in listeners {
            changed();
        }
    }
}

/// Opens a workspace subscription off the async worker. Consumers own its
/// lifetime; a source must not retain an unbounded collection of workspaces.
pub type Prepare = Arc<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>;

pub trait Sources: Send + Sync {
    fn open(&self, workspace: &Path, prepare: Prepare) -> Result<Arc<dyn Generations>, Refused>;
}

/// The registry sessions run with. Replacing it (a reload) takes effect at
/// each session's next rest, between bursts of events.
pub struct Current(Mutex<Arc<Registry>>, AtomicU64);

impl Current {
    pub fn new(registry: Registry) -> Self {
        Self(Mutex::new(Arc::new(registry)), AtomicU64::new(0))
    }

    /// The newest generation.
    pub fn get(&self) -> Arc<Registry> {
        Arc::clone(&self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn generation(&self) -> u64 {
        self.1.load(Ordering::Acquire)
    }

    pub fn replace(&self, registry: Registry) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(registry);
        self.1.fetch_add(1, Ordering::Release);
    }
}

/// One registry generation, as the kernel asks it about a session: the
/// extensions' callables, decision points and agent. Points answer from
/// what they are given and the session's committed state; each method is
/// sync and must not block (a slow answer is [`Answer::Later`]). The
/// kernel never sees a model request: the `step` point is the engine's
/// to ask, on [`Bound`].
pub trait Binding: Send + Sync {
    /// A new agent, of the kind the session runs.
    fn agent(&self) -> Box<dyn Agent>;

    /// What the agent asks at its decisions: the handlers at the loop
    /// points. Pure, as the agent is: a load asks them again.
    fn loop_points(&self) -> &dyn LoopPoints {
        &Defaults
    }

    /// The extensions it runs, in install order, as a log records them.
    fn installed(&self) -> Vec<Installed>;

    fn callable(&self, name: &str) -> Option<Callable>;

    /// The model a call goes to now, as `host:model`.
    fn model(&self) -> String {
        "none".to_owned()
    }

    /// What the model is told of where the session runs, logged once
    /// before its first call.
    fn context(&self) -> Option<String> {
        None
    }

    /// Context logged when a live session adopts a new registry generation.
    fn generation_context(&self) -> Option<String> {
        None
    }

    /// Reads call `call`'s input as its tool reads it, once, keeping it
    /// for the call's run: the card clients show the call with.
    fn read(&self, _call: CallId, _tool: &str, _input: &Json) -> Option<Card> {
        None
    }

    /// The `input` point: the text as the chain leaves it, or why it is
    /// refused.
    fn input(&self, text: String) -> Result<Chained<String>, String> {
        Ok(Chained::new(text))
    }

    /// The `pre_tool`, `permission` and `approve` points, merged.
    fn gate(&self, _call: CallId, _tool: &str, input: &Json) -> Answer<Gated> {
        Answer::Now(Gated::run(input.clone()))
    }

    /// The `request_error` point: how to recover model call `call` from
    /// the failure of its attempt `attempt` (from 0, counted in the log, so
    /// a restart does not start again), and who says; now or later. With
    /// no answer the call fails.
    fn request_error(
        &self,
        _call: CallId,
        _attempt: u16,
        _error: &llm::Error,
    ) -> Answer<Option<(Name, Recover)>> {
        Answer::Now(None)
    }

    /// The `post_tool` point: the outcome as the chain leaves it.
    fn post_tool(
        &self,
        _call: CallId,
        _tool: &str,
        _input: &Json,
        outcome: Outcome,
    ) -> Chained<Outcome> {
        Chained::new(outcome)
    }

    /// The `on_stop` point: text for the agent to go on with once it
    /// stopped.
    fn on_stop(&self) -> Option<(Name, String)> {
        None
    }
}

impl std::fmt::Debug for dyn Binding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Binding")
    }
}

/// A registry generation bound to a session: the [`Binding`] the kernel
/// drives, over the session as its points read it (`V`). Pure glue: the
/// engine supplies the view and the agent's name, and tests bind real
/// extensions to a stub.
pub struct Bound<V> {
    registry: Arc<Registry>,
    view: Arc<V>,
    /// The agent the session runs, by name.
    agent: Option<String>,
    /// Each open tool call's input as its tool read it at its intent, with
    /// the text it was read from: its run is given it rather than reading
    /// it again, unless the gate rewrote it.
    parsed: Arc<Mutex<Vec<(CallId, Json, Parsed)>>>,
}

impl<V> Clone for Bound<V> {
    fn clone(&self) -> Self {
        Self {
            registry: Arc::clone(&self.registry),
            view: Arc::clone(&self.view),
            agent: self.agent.clone(),
            parsed: Arc::clone(&self.parsed),
        }
    }
}

impl<V: View + Send + Sync + 'static> Bound<V> {
    pub fn new(registry: Arc<Registry>, view: Arc<V>, agent: Option<String>) -> Self {
        Self {
            registry,
            view,
            agent,
            parsed: Arc::default(),
        }
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    pub fn view(&self) -> &Arc<V> {
        &self.view
    }

    /// The session, as handlers deciding about nothing in particular see
    /// it.
    pub fn point(&self) -> crate::PointCx<'_> {
        crate::PointCx::new(&*self.view)
    }

    /// The session, as handlers deciding about call `call` see it.
    pub fn about(&self, call: CallId) -> crate::PointCx<'_> {
        self.point().about(call)
    }

    /// The `step` point over model call `call`'s request: the rewrites to
    /// log and, if a handler answered it, who and with what.
    pub fn step(
        &self,
        call: CallId,
        request: &mut llm::Request,
    ) -> (Decided, Option<(Name, Stepped)>) {
        self.registry.hooks().step(&self.about(call), request)
    }

    /// Call `call`'s input as read at its intent, if it was and the call
    /// runs on `input` still.
    pub fn take_parsed(&self, call: CallId, input: &Json) -> Option<Parsed> {
        let mut parsed = self.parsed.lock().unwrap_or_else(PoisonError::into_inner);
        let at = parsed.iter().position(|(c, _, _)| *c == call)?;
        let (_, read, parsed) = parsed.swap_remove(at);
        (read == *input).then_some(parsed)
    }

    /// Forgets what was read for call `call`: it settled.
    pub fn settled(&self, call: CallId) {
        (self.parsed.lock().unwrap_or_else(PoisonError::into_inner)).retain(|(c, _, _)| *c != call);
    }

    /// The gate of call `call` once the `approve` handler it waits on
    /// answers: with none, the handlers after it are asked, and with no
    /// answer at all the user is.
    async fn approving(self, call: CallId, tool: String, mut asked: Approving) -> Gated {
        loop {
            let answer = if let Some(approved) = asked.answer.0.await {
                Some((asked.by, approved))
            } else {
                let rest = (self.registry.hooks()).approve_from(
                    &self.about(call),
                    &tool,
                    &asked.input,
                    asked.next,
                );
                match rest {
                    Asked::None => None,
                    Asked::Now(by, approved) => Some((by, approved)),
                    Asked::Later { by, at, answer } => {
                        (asked.by, asked.next, asked.answer) = (by, at + 1, answer);
                        continue;
                    }
                }
            };
            return Gated::approved(asked.input, asked.decisions, answer);
        }
    }

    /// How to recover model call `call` from `error` once the
    /// `request_error` handler `by`, the `at`th, answers: with none, the
    /// handlers after it are asked.
    async fn recovering(
        self,
        call: CallId,
        attempt: u16,
        error: llm::Error,
        (mut by, mut at, mut answer): (Name, usize, Later<Option<Recover>>),
    ) -> Option<(Name, Recover)> {
        loop {
            if let Some(recover) = answer.0.await {
                return Some((by, recover));
            }
            let failed = Failed {
                error: &error,
                attempt,
            };
            let rest =
                (self.registry.hooks()).request_error_from(&self.about(call), failed, at + 1);
            match rest {
                Asked::None => return None,
                Asked::Now(by, recover) => return Some((by, recover)),
                Asked::Later {
                    by: next,
                    at: next_at,
                    answer: next_answer,
                } => (by, at, answer) = (next, next_at, next_answer),
            }
        }
    }
}

impl<V: View + Send + Sync + 'static> Binding for Bound<V> {
    fn agent(&self) -> Box<dyn Agent> {
        self.registry.agent(self.agent.as_deref())
    }

    fn loop_points(&self) -> &dyn LoopPoints {
        self.registry.hooks()
    }

    fn installed(&self) -> Vec<Installed> {
        self.registry.installed()
    }

    fn callable(&self, name: &str) -> Option<Callable> {
        let tool = self.registry.tools().get(name)?;
        Some(Callable {
            audience: tool.audience(),
            replay: tool.replay(),
            concurrency: tool.concurrency(),
        })
    }

    fn model(&self) -> String {
        self.view.model()
    }

    fn generation_context(&self) -> Option<String> {
        self.registry.generation_context()
    }

    fn context(&self) -> Option<String> {
        let workspace = self.view.workspace();
        let env = self.view.env();
        let mut dir = None;
        self.view.states(&mut |states| {
            dir = Some(self.registry.dir(workspace, env.as_deref(), states));
        });
        let dir = dir.unwrap_or_else(|| workspace.to_path_buf());
        let facts = Facts {
            workspace,
            dir: &dir,
        };
        self.registry.context(&facts)
    }

    fn read(&self, call: CallId, tool: &str, input: &Json) -> Option<Card> {
        let tool = self.registry.tools().get(tool)?;
        let parsed = tool.parse(input).ok()?;
        let card = tool.present(&parsed);
        (self.parsed.lock().unwrap_or_else(PoisonError::into_inner)).push((
            call,
            input.clone(),
            parsed,
        ));
        card
    }

    fn input(&self, text: String) -> Result<Chained<String>, String> {
        self.registry.hooks().input(&self.point(), text)
    }

    fn gate(&self, call: CallId, tool: &str, input: &Json) -> Answer<Gated> {
        match self.registry.hooks().gate(&self.about(call), tool, input) {
            Gating::Now(gated) => Answer::Now(gated),
            Gating::Approving(asked) => {
                let later = self.clone().approving(call, tool.to_owned(), asked);
                Answer::Later(Later::new(later))
            }
        }
    }

    fn request_error(
        &self,
        call: CallId,
        attempt: u16,
        error: &llm::Error,
    ) -> Answer<Option<(Name, Recover)>> {
        let failed = Failed { error, attempt };
        match self
            .registry
            .hooks()
            .request_error(&self.about(call), failed)
        {
            Asked::None => Answer::Now(None),
            Asked::Now(by, recover) => Answer::Now(Some((by, recover))),
            Asked::Later { by, at, answer } => {
                let later =
                    (self.clone()).recovering(call, attempt, error.clone(), (by, at, answer));
                Answer::Later(Later::new(later))
            }
        }
    }

    fn post_tool(
        &self,
        call: CallId,
        tool: &str,
        input: &Json,
        outcome: Outcome,
    ) -> Chained<Outcome> {
        (self.registry.hooks()).post_tool(&self.about(call), (tool, input), outcome)
    }

    fn on_stop(&self) -> Option<(Name, String)> {
        self.registry.hooks().on_stop(&self.point())
    }
}

#[cfg(test)]
mod tests;
