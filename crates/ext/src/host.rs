//! The engine's side of extensions: the registry they install into, what
//! it resolves for sessions, and how a running call reaches the engine.
//! Extensions need none of it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use protocol::{Environment, Outcome, SessionId};
use rt::BoxFuture;
use serde_json::Value;

use crate::ext::{DynCommand, MakeAgent};
use crate::state::States;
use crate::{
    Agent, Cancel, CwdOf, Cx, DynExtension, DynTool, Extension, Facts, Name, Place, Refused,
    Registrar, Text, Tier,
};

pub use crate::point::{Decided, Gate, History, Hooks};

/// The engine, as a running call reaches it.
pub trait Host: Send + Sync {
    /// See [`Cx::spawned`].
    fn spawned(&self, pgid: u32, background: bool);
    /// See [`Cx::progress`].
    fn progress(&self, text: &str);
    /// Commits `data` as a change to extension `ext`'s state `state`, and
    /// folds it in.
    fn note(&self, ext: &str, state: &str, data: Value);
    /// Lends the session's states to `read`, which cannot keep them.
    fn states(&self, read: &mut dyn FnMut(&States));
    /// The registry generation the call started on.
    fn registry(&self) -> &Registry;
}

/// What a [`Cx`] is made of, but its host.
pub struct Parts {
    pub workspace: Arc<Path>,
    pub env: Option<Arc<Environment>>,
    pub session: SessionId,
    pub call: u64,
    pub model: Arc<str>,
    pub spill: Arc<Path>,
    pub cancel: Cancel,
    pub rt: rt::Rt,
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

    /// Every tool, as offered to the model.
    pub fn specs(&self) -> Vec<llm::Tool> {
        self.0.iter().map(|t| t.spec()).collect()
    }

    pub fn run<'a>(&'a self, name: &str, input: &Value, cx: &'a Cx) -> BoxFuture<'a, Outcome> {
        match self.get(name) {
            Some(tool) => tool.call(input, cx),
            None => Box::pin(std::future::ready(Outcome::Failed {
                error: format!("unknown tool {name}"),
            })),
        }
    }

    /// What the model sees of a call's outcome.
    pub fn to_model(&self, name: &str, outcome: &Outcome) -> (llm::Output, bool) {
        match outcome {
            Outcome::Done { output } => (
                self.get(name)
                    .map_or_else(|| llm::Output::Json(output.value()), |t| t.to_model(output)),
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

/// A slash command, as the engine runs it.
pub struct Command<'a>(&'a dyn DynCommand);

impl Command<'_> {
    pub fn description(&self) -> &str {
        self.0.description()
    }

    pub fn run<'a>(&'a self, args: &'a str, cx: &'a Cx) -> BoxFuture<'a, Outcome> {
        self.0.run(args, cx)
    }
}

struct Installed {
    name: Name,
    version: u32,
    tier: Tier,
    made: Registrar,
}

/// The extensions a daemon runs with, and what they make.
#[derive(Default)]
pub struct Registry {
    installed: Vec<Installed>,
    tools: Toolset,
    commands: Vec<Arc<dyn DynCommand>>,
    hooks: Hooks,
    texts: Vec<Text>,
    agents: Vec<(Name, Arc<MakeAgent>)>,
    /// What every model call on this generation sends, made on first use:
    /// a registry never changes once built, so neither do they.
    prompt: OnceLock<String>,
    specs: OnceLock<Vec<llm::Tool>>,
}

impl Registry {
    /// Registers `ext` in [`Tier::User`]: see [`Registry::with_in`].
    pub fn with<E: Extension>(self, ext: &E) -> Result<Self, Refused> {
        self.with_in(Tier::User, ext)
    }

    /// Registers `ext` after every extension so far in `tier`, or in place
    /// of the one of the same name.
    pub fn with_in<E: Extension>(self, tier: Tier, ext: &E) -> Result<Self, Refused> {
        self.with_dyn(tier, ext)
    }

    /// [`Registry::with_in`] for an extension named at run time.
    pub fn with_dyn(self, tier: Tier, ext: &dyn DynExtension) -> Result<Self, Refused> {
        let mut made = Registrar::default();
        ext.contribute(&mut made);
        self.install(Installed {
            name: ext.name(),
            version: ext.version(),
            tier,
            made,
        })
    }

    /// Installs `new` unless it breaks a rule, and rebuilds what the
    /// extensions make, in install order.
    fn install(mut self, mut new: Installed) -> Result<Self, Refused> {
        let name = Name::clone(&new.name);
        let refuse = |why: &str| Err(Refused(format!("extension {name}: {why}")));
        if let Some(error) = new.made.errors.first() {
            return refuse(error);
        }
        let guarded = |i: &Installed| i.name == new.name && i.tier == Tier::Guarded;
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
        let mut commands = Vec::new();
        let mut texts = Vec::new();
        let mut agents = Vec::new();
        let mut hooks = Hooks::default();
        for Installed {
            name, made, tier, ..
        } in &self.installed
        {
            let guarded = *tier == Tier::Guarded;
            for tool in &made.tools {
                replace(&mut tools, (guarded, Arc::clone(tool)), |t| t.1.name());
            }
            for command in &made.commands {
                replace(&mut commands, (guarded, Arc::clone(command)), |c| {
                    c.1.name()
                });
            }
            for text in &made.texts {
                replace(&mut texts, (guarded, text.clone()), |t| &t.1.name);
            }
            for agent in &made.agents {
                replace(&mut agents, (guarded, agent.clone()), |a| &a.1.0);
            }
            for hooked in &made.hooks {
                hooks.add(name, hooked);
            }
        }
        texts.sort_by_key(|t| t.1.order);
        self.tools = Toolset(tools.into_iter().map(|t| t.1).collect());
        self.commands = commands.into_iter().map(|c| c.1).collect();
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

    pub fn command(&self, name: &str) -> Option<Command<'_>> {
        self.commands
            .iter()
            .find(|c| c.name() == name)
            .map(|c| Command(&**c))
    }

    /// A new loop of the agent named `name`. A name no extension registers
    /// is reported, and the first agent registered runs instead; with none,
    /// a loop that does nothing.
    pub fn agent(&self, name: &str) -> Box<dyn Agent> {
        let named = self.agents.iter().find(|(n, _)| &**n == name);
        if named.is_none() {
            eprintln!("warning: no agent named {name} is registered");
        }
        match named.or(self.agents.first()) {
            Some((_, make)) => make(),
            None => Box::new(Idle),
        }
    }

    /// The names of the agents sessions can run.
    pub fn agents(&self) -> impl Iterator<Item = &str> + '_ {
        self.agents.iter().map(|(name, _)| &**name)
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
                    Place::Context(_) => None,
                })
                .collect();
            texts.join("\n\n")
        })
    }

    /// Every tool, as offered to the model: each schema parsed once.
    pub fn specs(&self) -> &[llm::Tool] {
        self.specs.get_or_init(|| self.tools.specs())
    }

    /// The context the texts give for `facts`, if any.
    pub fn context(&self, facts: &Facts<'_>) -> Option<String> {
        let parts: Vec<String> = self
            .texts
            .iter()
            .filter(|t| self.offered(t))
            .filter_map(|t| match &t.place {
                Place::Context(make) => make(facts),
                Place::System(_) => None,
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
                .map(|decl| (Name::clone(&i.name), *decl))
        }))
    }

    /// Every extension's name and version, in install order.
    pub fn extensions(&self) -> impl Iterator<Item = (&str, u32)> + '_ {
        self.installed.iter().map(|i| (&*i.name, i.version))
    }
}

/// Puts `new` in place of the item of the same name, or last; a guarded
/// item is replaced only by another.
fn replace<T>(items: &mut Vec<(bool, T)>, new: (bool, T), name: impl Fn(&(bool, T)) -> &str) {
    match items.iter_mut().find(|t| name(t) == name(&new)) {
        Some(slot) if slot.0 && !new.0 => {}
        Some(slot) => *slot = new,
        None => items.push(new),
    }
}

/// The agent of a registry with none.
struct Idle;

impl Agent for Idle {
    fn step(&mut self, _: crate::Input) -> Vec<crate::Action> {
        Vec::new()
    }
}

/// The registry sessions run with. Replacing it (a reload) takes effect at
/// each session's next step boundary: work already running finishes on the
/// generation it started with.
pub struct Current(Mutex<Arc<Registry>>);

impl Current {
    pub fn new(registry: Registry) -> Self {
        Self(Mutex::new(Arc::new(registry)))
    }

    /// The newest generation.
    pub fn get(&self) -> Arc<Registry> {
        Arc::clone(&self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn replace(&self, registry: Registry) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(registry);
    }
}

#[cfg(test)]
mod tests;
