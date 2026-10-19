//! Extensions: every feature, built-in or not, contributes its tools,
//! commands, hooks, prompt sections and state through one [`Registry`] with
//! the same verbs. The engine itself offers the model nothing.
//!
//! Registration is ordered, by [`Tier`] and then install order, and that
//! order is the hooks' merge order. A later registration of a name already
//! taken replaces the earlier one in its install position: an extension of
//! the same name replaces all it contributed, and a tool, command or prompt
//! section of the same name, from another extension, replaces that one. So a
//! user's `bash` overrides the built-in one without reordering anything.
//! Nothing replaces what a [`Tier::Guarded`] extension contributes, except
//! another guarded one. Within one extension a name taken twice is an error,
//! as is a second extension claiming a [`Slot`] another holds.

use std::fmt;
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use protocol::Outcome;
use rt::BoxFuture;
use serde_json::Value;

use crate::state::{Decl, State, States};
use crate::{Cx, DynTool, Hook, Hooks, Tool, Toolset};

/// A unit of features, named so decisions can say who made them.
pub trait Extension {
    /// Who the extension's hooks are, in logged decisions.
    const NAME: &'static str;
    const VERSION: u32;

    fn contribute(&self, r: &mut Registrar);
}

/// Where an extension loads: tiers load in this order, each in install
/// order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// What later tiers cannot replace or remove, such as the permission
    /// policy: repository config never loosens it.
    Guarded,
    /// The built-ins the user's extensions may replace.
    Pre,
    #[default]
    User,
    /// After the user's, to see what they did.
    Post,
}

/// A role only one extension may fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// Starts the processes tools run.
    Spawn,
    /// Shrinks a conversation that no longer fits the model.
    Compaction,
    /// Names sessions.
    Title,
}

/// A slash command: `/name args` runs it without a model turn.
pub trait Command: Send + Sync + 'static {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;

    /// The output, shown to the user and logged; never sent to the model.
    fn run(&self, args: &str, cx: &Cx) -> impl Future<Output = Result<Value, String>> + Send;
}

/// A [`Command`] behind a pointer.
pub trait DynCommand: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn run<'a>(&'a self, args: &'a str, cx: &'a Cx) -> BoxFuture<'a, Outcome>;
}

impl<C: Command> DynCommand for C {
    fn name(&self) -> &'static str {
        C::NAME
    }

    fn description(&self) -> &'static str {
        C::DESCRIPTION
    }

    fn run<'a>(&'a self, args: &'a str, cx: &'a Cx) -> BoxFuture<'a, Outcome> {
        Box::pin(async move {
            match Command::run(self, args, cx).await {
                Ok(output) => Outcome::Done { output },
                Err(error) => Outcome::Failed { error },
            }
        })
    }
}

/// Part of the system prompt. Sections are joined by `order`, then install
/// order; one tied to a `tool` is left out unless that tool is offered.
#[derive(Clone, Debug)]
pub struct Section {
    pub name: &'static str,
    pub order: i32,
    pub text: String,
    pub tool: Option<&'static str>,
}

/// Where a session runs, for the context the model is first given.
pub struct Facts<'a> {
    pub workspace: &'a Path,
    /// The directory its commands run in.
    pub dir: &'a Path,
}

type Context = dyn Fn(&Facts<'_>) -> Option<String> + Send + Sync;

/// What an extension contributes, as it registers.
#[derive(Default)]
pub struct Registrar {
    tools: Vec<Arc<dyn DynTool>>,
    commands: Vec<Arc<dyn DynCommand>>,
    hooks: Vec<Arc<dyn Hook>>,
    sections: Vec<Section>,
    contexts: Vec<Arc<Context>>,
    states: Vec<Decl>,
    slots: Vec<Slot>,
    errors: Vec<String>,
}

impl Registrar {
    pub fn tool(&mut self, tool: impl Tool) -> &mut Self {
        let taken = twice(
            "tool",
            DynTool::name(&tool),
            self.tools.iter().map(|t| t.name()),
        );
        self.errors.extend(taken);
        self.tools.push(Arc::new(tool));
        self
    }

    pub fn command(&mut self, command: impl Command) -> &mut Self {
        let name = DynCommand::name(&command);
        let taken = twice("command", name, self.commands.iter().map(|c| c.name()));
        self.errors.extend(taken);
        self.commands.push(Arc::new(command));
        self
    }

    /// Adds a hook at the decision points it implements.
    pub fn on(&mut self, hook: impl Hook + 'static) -> &mut Self {
        self.hooks.push(Arc::new(hook));
        self
    }

    pub fn prompt_section(&mut self, section: Section) -> &mut Self {
        let names = self.sections.iter().map(|s| s.name);
        let taken = twice("prompt section", section.name, names);
        self.errors.extend(taken);
        self.sections.push(section);
        self
    }

    /// Adds to the facts the model is given before a session's first model
    /// call; logged then, so later turns read the same.
    pub fn context(
        &mut self,
        context: impl Fn(&Facts<'_>) -> Option<String> + Send + Sync + 'static,
    ) -> &mut Self {
        self.contexts.push(Arc::new(context));
        self
    }

    /// Keeps state `S` per session, durably: see [`State`].
    pub fn state<S: State>(&mut self) -> &mut Self {
        let taken = twice("state", S::NAME, self.states.iter().map(|s| s.name));
        self.errors.extend(taken);
        self.states.push(Decl::of::<S>());
        self
    }

    pub fn claim(&mut self, slot: Slot) -> &mut Self {
        self.slots.push(slot);
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

struct Installed {
    name: &'static str,
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
    sections: Vec<Section>,
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
        let mut made = Registrar::default();
        ext.contribute(&mut made);
        self.install(Installed {
            name: E::NAME,
            version: E::VERSION,
            tier,
            made,
        })
    }

    /// Installs `new` unless it breaks a rule, and rebuilds what the
    /// extensions make, in install order.
    fn install(mut self, mut new: Installed) -> Result<Self, Refused> {
        let refuse = |why: &str| Err(Refused(format!("extension {}: {why}", new.name)));
        if let Some(error) = new.made.errors.first() {
            return refuse(error);
        }
        let guarded = |i: &Installed| i.name == new.name && i.tier == Tier::Guarded;
        if new.tier != Tier::Guarded && self.installed.iter().any(guarded) {
            return refuse("a guarded extension of that name is loaded");
        }
        for slot in &new.made.slots {
            let holder = self
                .installed
                .iter()
                .find(|i| i.name != new.name && i.made.slots.contains(slot));
            if let Some(holder) = holder {
                return refuse(&format!("{} already fills the {slot:?} slot", holder.name));
            }
        }
        if let Some(slot) = self.installed.iter_mut().find(|i| i.name == new.name) {
            new.tier = slot.tier;
            *slot = new;
        } else {
            let at = self.installed.partition_point(|i| i.tier <= new.tier);
            self.installed.insert(at, new);
        }
        let mut tools = Vec::new();
        let mut commands = Vec::new();
        let mut sections = Vec::new();
        let mut hooks = Vec::new();
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
            for section in &made.sections {
                replace(&mut sections, (guarded, section.clone()), |s| s.1.name);
            }
            hooks.extend(made.hooks.iter().map(|hook| (*name, Arc::clone(hook))));
        }
        sections.sort_by_key(|s| s.1.order);
        self.tools = Toolset(tools.into_iter().map(|t| t.1).collect());
        self.commands = commands.into_iter().map(|c| c.1).collect();
        self.sections = sections.into_iter().map(|s| s.1).collect();
        self.hooks = Hooks(hooks);
        self.prompt = OnceLock::new();
        self.specs = OnceLock::new();
        Ok(self)
    }

    pub fn tools(&self) -> &Toolset {
        &self.tools
    }

    pub fn hooks(&self) -> &Hooks {
        &self.hooks
    }

    pub fn command(&self, name: &str) -> Option<&dyn DynCommand> {
        self.commands
            .iter()
            .map(|c| &**c)
            .find(|c| c.name() == name)
    }

    /// The system prompt the sections make with the tools offered.
    pub fn system_prompt(&self) -> &str {
        self.prompt.get_or_init(|| {
            let offered = |s: &&Section| s.tool.is_none_or(|tool| self.tools.get(tool).is_some());
            let texts: Vec<&str> = self
                .sections
                .iter()
                .filter(offered)
                .map(|s| s.text.as_str())
                .collect();
            texts.join("\n\n")
        })
    }

    /// Every tool, as offered to the model: each schema parsed once.
    pub fn specs(&self) -> &[llm::Tool] {
        self.specs.get_or_init(|| self.tools.specs())
    }

    /// The context every extension gives for `facts`, if any.
    pub fn context(&self, facts: &Facts<'_>) -> Option<String> {
        let parts: Vec<String> = self
            .installed
            .iter()
            .flat_map(|i| &i.made.contexts)
            .filter_map(|context| context(facts))
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// Fresh state for a session, as every extension registered it.
    pub fn states(&self) -> States {
        States::new(
            self.installed
                .iter()
                .flat_map(|i| i.made.states.iter().map(|decl| (i.name, *decl))),
        )
    }

    /// Every extension's name and version, in install order.
    pub fn extensions(&self) -> impl Iterator<Item = (&'static str, u32)> + '_ {
        self.installed.iter().map(|i| (i.name, i.version))
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
mod tests {
    use super::*;
    use crate::{Decided, Policy};
    use protocol::{Decision, Permission};
    use serde_json::json;

    /// A tool named `name`, described as `description`.
    struct Named(&'static str);

    impl Tool for Named {
        type In = Value;
        type Out = Value;
        const NAME: &'static str = "bash";
        const DESCRIPTION: &'static str = "mine";

        fn schema() -> Value {
            json!({ "type": "object" })
        }

        fn call(&self, _: Value, _: &Cx) -> impl Future<Output = Result<Value, String>> + Send {
            std::future::ready(Ok(self.0.into()))
        }
    }

    /// Denies `tool`.
    struct Deny(&'static str);

    impl Hook for Deny {
        fn permission(&self, tool: &str, _: &Value) -> Permission {
            if tool == self.0 {
                Permission::Deny
            } else {
                Permission::Allow
            }
        }
    }

    struct Jobs;

    impl Command for Jobs {
        const NAME: &'static str = "jobs";
        const DESCRIPTION: &'static str = "Lists jobs.";

        fn run(&self, _: &str, _: &Cx) -> impl Future<Output = Result<Value, String>> + Send {
            std::future::ready(Ok("none".into()))
        }
    }

    macro_rules! extension {
        ($ty:ident, $name:literal, $version:literal, |$r:ident| $body:expr) => {
            struct $ty;
            impl Extension for $ty {
                const NAME: &'static str = $name;
                const VERSION: u32 = $version;
                fn contribute(&self, $r: &mut Registrar) {
                    $body;
                }
            }
        };
    }

    extension!(Mine, "mine", 1, |r| r.tool(Named("mine")));
    extension!(DenyA, "policy", 1, |r| r.on(Deny("a")));
    extension!(DenyB, "policy", 2, |r| r.on(Deny("b")));
    extension!(Other, "other", 1, |r| r.on(Deny("a")).on(Deny("b")));
    extension!(Twice, "twice", 1, |r| r.tool(Named("1")).tool(Named("2")));
    extension!(Spawner, "spawner", 1, |r| r.claim(Slot::Spawn));
    extension!(Commands, "commands", 1, |r| r.command(Jobs));
    extension!(Guide, "guide", 1, |r| r
        .prompt_section(Section {
            name: "bash",
            order: 5,
            text: "Use bash well.".into(),
            tool: Some("bash"),
        })
        .prompt_section(Section {
            name: "edit",
            order: 6,
            text: "Edit carefully.".into(),
            tool: Some("edit"),
        }));

    fn names(tools: &Toolset) -> Vec<String> {
        tools
            .specs()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }

    #[test]
    fn built_ins_register_as_extensions() {
        let registry = crate::builtins(&Policy::ask(&["bash"])).unwrap();
        assert_eq!(names(registry.tools()), ["bash", "edit"]);
        let extensions: Vec<_> = registry.extensions().collect();
        assert_eq!(
            extensions,
            [("policy", 1), ("prompt", 1), ("shell", 1), ("edit", 1)]
        );

        let mut decided = Decided::new();
        let gate = registry.hooks().gate("bash", &mut json!({}), &mut decided);
        assert_eq!(gate, Ok(crate::Gate::Ask));
        assert!(matches!(
            &decided[..],
            [("policy", Decision::Permission { .. })]
        ));
    }

    #[test]
    fn a_later_tool_of_the_same_name_replaces_the_earlier_in_place() {
        let registry = crate::builtins(&Policy::default())
            .and_then(|r| r.with(&Mine))
            .unwrap();
        let tools = registry.tools();
        assert_eq!(names(tools), ["bash", "edit"]);
        assert_eq!(tools.get("bash").unwrap().spec().description, "mine");
    }

    #[test]
    fn a_name_taken_twice_in_one_extension_or_a_slot_filled_twice_is_refused() {
        let twice = Registry::default().with(&Twice).err().unwrap();
        assert_eq!(twice.0, "extension twice: tool bash registered twice");
        let builtins = crate::builtins(&Policy::default()).unwrap();
        let slot = builtins.with(&Spawner).err().unwrap();
        assert_eq!(
            slot.0,
            "extension spawner: shell already fills the Spawn slot"
        );
    }

    #[test]
    fn a_later_extension_of_the_same_name_replaces_the_earlier_in_place() {
        let registry = Registry::default()
            .with(&DenyA)
            .and_then(|r| r.with(&Other))
            .and_then(|r| r.with(&DenyB))
            .unwrap();
        let extensions: Vec<_> = registry.extensions().collect();
        assert_eq!(extensions, [("policy", 2), ("other", 1)]);

        // `policy` now denies only `b`, and still decides before `other`.
        let mut decided = Decided::new();
        let gate = registry.hooks().gate("b", &mut json!({}), &mut decided);
        assert_eq!(gate, Err("policy denied this call".into()));
        assert!(matches!(&decided[..], [("policy", _)]));
        let gate = registry.hooks().gate("a", &mut json!({}), &mut decided);
        assert_eq!(gate, Err("other denied this call".into()));
    }

    #[test]
    fn hooks_merge_in_registration_order() {
        let registry = Registry::default()
            .with(&Other)
            .and_then(|r| r.with(&DenyA))
            .unwrap();
        let mut decided = Decided::new();
        let gate = registry.hooks().gate("a", &mut json!({}), &mut decided);
        assert_eq!(gate, Err("other denied this call".into()));
        let registry = Registry::default()
            .with(&DenyA)
            .and_then(|r| r.with(&Other))
            .unwrap();
        let gate = registry.hooks().gate("a", &mut json!({}), &mut decided);
        assert_eq!(gate, Err("policy denied this call".into()));
    }

    #[test]
    fn prompt_sections_follow_the_tools_offered() {
        let without = Registry::default().with(&Guide).unwrap();
        assert_eq!(without.system_prompt(), "");
        let with = without.with(&Mine).unwrap();
        assert_eq!(with.system_prompt(), "Use bash well.");
        let builtins = crate::builtins(&Policy::default()).unwrap();
        let prompt = builtins.system_prompt();
        // Made once per registry, as are the tool specs.
        assert!(std::ptr::eq(prompt, builtins.system_prompt()));
        assert!(std::ptr::eq(builtins.specs(), builtins.specs()));
        assert_eq!(builtins.specs().len(), builtins.tools().specs().len());
        assert!(prompt.starts_with("You are `a`"), "{prompt}");
        assert!(
            prompt.contains("`bash`") && prompt.contains("`edit`"),
            "{prompt}"
        );
        assert!(prompt.len() < 2000, "{} bytes", prompt.len());
    }

    extension!(Loose, "policy", 9, |r| r.on(Deny("nothing")));
    extension!(Watcher, "watcher", 1, |r| r.on(Deny("c")));
    extension!(Shadow, "shadow", 1, |r| r.command(Jobs));

    #[test]
    fn tiers_order_extensions_and_guarded_ones_stay() {
        let registry = Registry::default()
            .with_in(Tier::Post, &Watcher)
            .and_then(|r| r.with(&Mine))
            .and_then(|r| r.with_in(Tier::Guarded, &Commands))
            .and_then(|r| r.with_in(Tier::Pre, &DenyA))
            .unwrap();
        let extensions: Vec<_> = registry.extensions().map(|(name, _)| name).collect();
        assert_eq!(extensions, ["commands", "policy", "mine", "watcher"]);

        let refused = registry.with(&Commands).err().unwrap();
        assert!(refused.0.contains("guarded"), "{refused}");
        let registry = crate::builtins(&Policy::default()).unwrap();
        assert!(registry.with(&Loose).is_err());

        // A user's command of a guarded one's name leaves it in place.
        let registry = Registry::default()
            .with_in(Tier::Guarded, &Commands)
            .and_then(|r| r.with(&Shadow))
            .unwrap();
        assert_eq!(registry.commands.len(), 1);
        assert!(Arc::ptr_eq(
            &registry.commands[0],
            &registry.installed[0].made.commands[0]
        ));
    }

    #[test]
    fn commands_are_found_by_name() {
        let registry = Registry::default().with(&Commands).unwrap();
        assert_eq!(
            registry.command("jobs").unwrap().description(),
            "Lists jobs."
        );
        assert!(registry.command("bash").is_none());
    }
}
