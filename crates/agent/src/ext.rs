//! Extensions: every feature, built-in or not, contributes its tools and
//! hooks through one [`Registry`] with the same verbs.
//!
//! Registration is ordered, and that order is the hooks' merge order. A
//! later registration of a name already taken replaces the earlier one in
//! its install position: an extension of the same name replaces all it
//! contributed, and a tool of the same name, from any extension, replaces
//! that tool. So a user's `read_file` overrides the built-in one without
//! reordering anything.

use std::sync::Arc;

use crate::tools::{ReadFile, RunCommand, Sleep};
use crate::{AskFor, DynTool, Hook, Hooks, Tool, Toolset};

/// A unit of features, named so decisions can say who made them.
pub trait Extension {
    /// Who the extension's hooks are, in logged decisions.
    const NAME: &'static str;
    const VERSION: u32;

    fn contribute(&self, r: &mut Registrar);
}

/// What an extension contributes, as it registers.
#[derive(Default)]
pub struct Registrar {
    tools: Vec<Arc<dyn DynTool>>,
    hooks: Vec<Arc<dyn Hook>>,
}

impl Registrar {
    pub fn tool(&mut self, tool: impl Tool) -> &mut Self {
        self.tools.push(Arc::new(tool));
        self
    }

    /// Adds a hook at the decision points it implements.
    pub fn on(&mut self, hook: impl Hook + 'static) -> &mut Self {
        self.hooks.push(Arc::new(hook));
        self
    }
}

struct Installed {
    name: &'static str,
    version: u32,
    made: Registrar,
}

/// The extensions a daemon runs with, and the tools and hooks they make.
#[derive(Default)]
pub struct Registry {
    installed: Vec<Installed>,
    tools: Toolset,
    hooks: Hooks,
}

impl Registry {
    /// Registers `ext` after every extension so far, or in place of the one
    /// of the same name.
    #[must_use]
    pub fn with<E: Extension>(mut self, ext: &E) -> Self {
        let mut made = Registrar::default();
        ext.contribute(&mut made);
        self.install(Installed {
            name: E::NAME,
            version: E::VERSION,
            made,
        });
        self
    }

    /// Installs `new` and rebuilds the tools and hooks in install order.
    fn install(&mut self, new: Installed) {
        match self.installed.iter_mut().find(|i| i.name == new.name) {
            Some(slot) => *slot = new,
            None => self.installed.push(new),
        }
        let mut tools: Vec<Arc<dyn DynTool>> = Vec::new();
        let mut hooks = Vec::new();
        for Installed { name, made, .. } in &self.installed {
            for tool in &made.tools {
                match tools.iter_mut().find(|t| t.name() == tool.name()) {
                    Some(slot) => *slot = Arc::clone(tool),
                    None => tools.push(Arc::clone(tool)),
                }
            }
            hooks.extend(made.hooks.iter().map(|hook| (*name, Arc::clone(hook))));
        }
        self.tools = Toolset(tools);
        self.hooks = Hooks(hooks);
    }

    pub fn tools(&self) -> &Toolset {
        &self.tools
    }

    pub fn hooks(&self) -> &Hooks {
        &self.hooks
    }

    /// Every extension's name and version, in install order.
    pub fn extensions(&self) -> impl Iterator<Item = (&'static str, u32)> + '_ {
        self.installed.iter().map(|i| (i.name, i.version))
    }
}

/// The built-in tools.
pub struct Builtin;

impl Extension for Builtin {
    const NAME: &'static str = "builtin";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(RunCommand).tool(Sleep).tool(ReadFile);
    }
}

impl Extension for AskFor {
    const NAME: &'static str = "ask";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.on(AskFor(self.0.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cx, Decided};
    use protocol::{Decision, Permission};
    use serde_json::{Value, json};

    /// Another `read_file`.
    struct MyReadFile;

    impl Tool for MyReadFile {
        type In = Value;
        type Out = Value;
        const NAME: &'static str = "read_file";
        const DESCRIPTION: &'static str = "mine";

        fn schema() -> Value {
            json!({ "type": "object" })
        }

        fn call(&self, _: Value, _: &Cx<'_>) -> impl Future<Output = Result<Value, String>> + Send {
            std::future::ready(Ok(Value::Null))
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

    extension!(Mine, "mine", 1, |r| r.tool(MyReadFile));
    extension!(DenyA, "policy", 1, |r| r.on(Deny("a")));
    extension!(DenyB, "policy", 2, |r| r.on(Deny("b")));
    extension!(Other, "other", 1, |r| r.on(Deny("a")).on(Deny("b")));

    fn names(tools: &Toolset) -> Vec<String> {
        tools
            .specs()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }

    #[test]
    fn built_ins_register_as_extensions() {
        let registry = Registry::default()
            .with(&Builtin)
            .with(&AskFor(vec!["sleep".into()]));
        assert_eq!(
            names(registry.tools()),
            ["run_command", "sleep", "read_file"]
        );
        let extensions: Vec<_> = registry.extensions().collect();
        assert_eq!(extensions, [("builtin", 1), ("ask", 1)]);

        let mut decided = Decided::new();
        let gate = registry.hooks().gate("sleep", &mut json!({}), &mut decided);
        assert_eq!(gate, Ok(crate::Gate::Ask));
        assert!(matches!(
            &decided[..],
            [("ask", Decision::Permission { .. })]
        ));
    }

    #[test]
    fn a_later_tool_of_the_same_name_replaces_the_earlier_in_place() {
        let registry = Registry::default().with(&Builtin).with(&Mine);
        let tools = registry.tools();
        assert_eq!(names(tools), ["run_command", "sleep", "read_file"]);
        assert_eq!(tools.get("read_file").unwrap().spec().description, "mine");
    }

    #[test]
    fn a_later_extension_of_the_same_name_replaces_the_earlier_in_place() {
        let registry = Registry::default().with(&DenyA).with(&Other).with(&DenyB);
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
        let registry = Registry::default().with(&Other).with(&DenyA);
        let mut decided = Decided::new();
        let gate = registry.hooks().gate("a", &mut json!({}), &mut decided);
        assert_eq!(gate, Err("other denied this call".into()));
        let registry = Registry::default().with(&DenyA).with(&Other);
        let gate = registry.hooks().gate("a", &mut json!({}), &mut decided);
        assert_eq!(gate, Err("policy denied this call".into()));
    }
}
