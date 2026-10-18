//! Extensions: every feature, built-in or not, contributes its tools,
//! commands, hooks, prompt sections and state through one [`Registry`] with
//! the same verbs. The engine itself offers the model nothing.
//!
//! Registration is ordered, and that order is the hooks' merge order. A
//! later registration of a name already taken replaces the earlier one in
//! its install position: an extension of the same name replaces all it
//! contributed, and a tool, command or prompt section of the same name, from
//! another extension, replaces that one. So a user's `bash` overrides the
//! built-in one without reordering anything. Within one extension a name
//! taken twice is an error, as is a second extension claiming a [`Slot`]
//! another holds.

use std::fmt;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;

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
    fn run(&self, args: &str, cx: &Cx<'_>) -> impl Future<Output = Result<Value, String>> + Send;
}

/// A [`Command`] behind a pointer.
pub trait DynCommand: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn run<'a>(&'a self, args: &'a str, cx: &'a Cx<'a>) -> BoxFuture<'a, Outcome>;
}

impl<C: Command> DynCommand for C {
    fn name(&self) -> &'static str {
        C::NAME
    }

    fn description(&self) -> &'static str {
        C::DESCRIPTION
    }

    fn run<'a>(&'a self, args: &'a str, cx: &'a Cx<'a>) -> BoxFuture<'a, Outcome> {
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
}

impl Registry {
    /// Registers `ext` after every extension so far, or in place of the one
    /// of the same name.
    pub fn with<E: Extension>(mut self, ext: &E) -> Result<Self, Refused> {
        let mut made = Registrar::default();
        ext.contribute(&mut made);
        let refuse = |why: &str| Err(Refused(format!("extension {}: {why}", E::NAME)));
        if let Some(error) = made.errors.first() {
            return refuse(error);
        }
        for slot in &made.slots {
            let holder = self
                .installed
                .iter()
                .find(|i| i.name != E::NAME && i.made.slots.contains(slot));
            if let Some(holder) = holder {
                return refuse(&format!("{} already fills the {slot:?} slot", holder.name));
            }
        }
        self.install(Installed {
            name: E::NAME,
            version: E::VERSION,
            made,
        });
        Ok(self)
    }

    /// Installs `new` and rebuilds what the extensions make, in install
    /// order.
    fn install(&mut self, new: Installed) {
        match self.installed.iter_mut().find(|i| i.name == new.name) {
            Some(slot) => *slot = new,
            None => self.installed.push(new),
        }
        let mut tools: Vec<Arc<dyn DynTool>> = Vec::new();
        let mut commands: Vec<Arc<dyn DynCommand>> = Vec::new();
        let mut sections: Vec<Section> = Vec::new();
        let mut hooks = Vec::new();
        for Installed { name, made, .. } in &self.installed {
            for tool in &made.tools {
                replace(&mut tools, Arc::clone(tool), |t| t.name());
            }
            for command in &made.commands {
                replace(&mut commands, Arc::clone(command), |c| c.name());
            }
            for section in &made.sections {
                replace(&mut sections, section.clone(), |s| s.name);
            }
            hooks.extend(made.hooks.iter().map(|hook| (*name, Arc::clone(hook))));
        }
        sections.sort_by_key(|s| s.order);
        self.tools = Toolset(tools);
        self.commands = commands;
        self.sections = sections;
        self.hooks = Hooks(hooks);
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
    pub fn system_prompt(&self) -> String {
        let offered = |s: &&Section| s.tool.is_none_or(|tool| self.tools.get(tool).is_some());
        let texts: Vec<&str> = self
            .sections
            .iter()
            .filter(offered)
            .map(|s| s.text.as_str())
            .collect();
        texts.join("\n\n")
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

/// Puts `new` in place of the item of the same name, or last.
fn replace<T>(items: &mut Vec<T>, new: T, name: impl Fn(&T) -> &str) {
    match items.iter_mut().find(|t| name(t) == name(&new)) {
        Some(slot) => *slot = new,
        None => items.push(new),
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

        fn call(&self, _: Value, _: &Cx<'_>) -> impl Future<Output = Result<Value, String>> + Send {
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

        fn run(&self, _: &str, _: &Cx<'_>) -> impl Future<Output = Result<Value, String>> + Send {
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
            [("prompt", 1), ("shell", 1), ("edit", 1), ("policy", 1)]
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
        assert!(prompt.starts_with("You are `a`"), "{prompt}");
        assert!(
            prompt.contains("`bash`") && prompt.contains("`edit`"),
            "{prompt}"
        );
        assert!(prompt.len() < 2000, "{} bytes", prompt.len());
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
