use std::future::Future;

use protocol::{Card, Json, Permission, Verdict};
use serde_json::json;

use super::*;
use crate::point::{self, PointCx, View};
use crate::{Args, Replay, Route, Tool};

/// A tool named `bash`, described as `mine`.
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
fn deny(r: &mut Registrar, tool: &'static str) {
    r.on::<point::Permission>(move |_, call| {
        if call.tool == tool {
            Permission::Deny
        } else {
            Permission::Allow
        }
    });
}

struct Jobs;

impl Tool for Jobs {
    type In = Args;
    type Out = Value;

    const NAME: &'static str = "jobs";
    const DESCRIPTION: &'static str = "Lists jobs.";
    const AUDIENCE: Audience = Audience::User;

    fn schema() -> Value {
        json!({ "type": "object" })
    }

    fn call(&self, _: Args, _: &Cx) -> impl Future<Output = Result<Value, String>> + Send {
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

struct Here;

impl CwdOf for Here {
    fn dir(&self, _: &Path, _: Option<&Environment>, _: &States) -> PathBuf {
        "/here".into()
    }
}

extension!(Mine, "mine", 1, |r| r.tool(Named("mine")));
extension!(DenyA, "policy", 1, |r| deny(r, "a"));
extension!(DenyB, "policy", 2, |r| deny(r, "b"));
extension!(Other, "other", 1, |r| {
    deny(r, "a");
    deny(r, "b");
});
extension!(Twice, "twice", 1, |r| r.tool(Named("1")).tool(Named("2")));
extension!(Commands, "commands", 1, |r| r.tool(Jobs));
extension!(Shell, "shell", 1, |r| r
    .provide::<dyn CwdOf>(Arc::new(Here)));
extension!(Rival, "rival", 1, |r| r
    .provide::<dyn CwdOf>(Arc::new(Here)));
extension!(Editor, "editor", 1, |r| r.need::<dyn CwdOf>());
extension!(Guide, "guide", 1, |r| r
    .text(Text::system("bash", 5, "Use bash well.").for_tool("bash"))
    .text(Text::system("edit", 6, "Edit carefully.").for_tool("edit"))
    .text(Text::system("identity", 0, "I am."))
    .text(Text::context("where", 0, |facts| Some(format!(
        "in {}",
        facts.dir.display()
    )))));
extension!(Agents, "agents", 1, |r| r.agent("idle", || Box::new(Idle)));

/// An agent that does nothing.
struct Idle;

impl Agent for Idle {
    fn step(&mut self, _: crate::Input, _: &dyn crate::LoopPoints) -> crate::Actions {
        crate::Actions::new()
    }
}

/// A session no handler reads.
struct Bare;

impl View for Bare {
    fn model(&self) -> String {
        String::new()
    }
    fn states(&self, read: &mut dyn FnMut(&States)) {
        read(&States::default());
    }
}

fn gate(registry: &Registry, tool: &str, decided: &mut Decided) -> point::Gate {
    let view = Bare;
    let cx = PointCx::new(SessionId::new(), Path::new("/w"), &view);
    let gated = registry.hooks().gate(&cx, tool, &Json::new(&json!({})));
    *decided = gated.decisions;
    gated.gate
}

fn names(tools: &Toolset) -> Vec<String> {
    tools
        .specs()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect()
}

#[test]
fn a_name_taken_twice_in_one_extension_is_refused() {
    let twice = Registry::default().with(&Twice).err().unwrap();
    assert_eq!(twice.0, "extension twice: tool bash registered twice");
}

#[test]
fn a_service_has_one_provider_and_a_need_is_met_at_install() {
    let refused = Registry::default().with(&Editor).err().unwrap();
    assert_eq!(
        refused.0,
        "extension editor: it needs dyn ext::service::CwdOf, which no extension provides"
    );
    let registry = Registry::default()
        .with(&Shell)
        .and_then(|r| r.with(&Editor))
        .unwrap();
    let second = registry.with(&Rival).err().unwrap();
    assert_eq!(
        second.0,
        "extension rival: shell already provides dyn ext::service::CwdOf"
    );

    let registry = Registry::default().with(&Shell).unwrap();
    let dir = registry.dir(Path::new("/w"), None, &States::default());
    assert_eq!(dir, Path::new("/here"));
    assert_eq!(
        Registry::default().dir(Path::new("/w"), None, &States::default()),
        Path::new("/w")
    );
}

#[test]
fn a_later_tool_of_the_same_name_replaces_the_earlier_in_place() {
    let registry = Registry::default()
        .with(&Mine)
        .and_then(|r| r.with(&Guide))
        .unwrap();
    assert_eq!(names(registry.tools()), ["bash"]);
    assert_eq!(
        registry.tools().get("bash").unwrap().spec().description,
        "mine"
    );
}

/// A tool known only by its JSON: what a client or a mod declares.
struct Declared(String);

impl DynTool for Declared {
    fn name(&self) -> &str {
        &self.0
    }
    fn description(&self) -> &'static str {
        "declared"
    }
    fn audience(&self) -> Audience {
        Audience::Model
    }
    fn replay(&self) -> Replay {
        Replay::Safe
    }
    fn route(&self) -> Route {
        Route::Owner
    }
    fn spec(&self) -> llm::Tool {
        llm::Tool {
            name: self.0.as_str().into(),
            description: "declared".into(),
            schema: json!({ "type": "object" }),
            strict: None,
        }
    }
    fn call<'a>(&'a self, _: &Value, _: &'a Cx) -> BoxFuture<'a, Outcome> {
        Box::pin(std::future::ready(Outcome::Done {
            output: json!("declared").into(),
        }))
    }
    fn to_model(&self, output: &Json) -> llm::Output {
        llm::Output::Json(output.value())
    }
    fn present(&self, _: &Value) -> Option<Card> {
        None
    }
    fn present_result(&self, _: &Value, _: &Json) -> Option<Card> {
        None
    }
}

/// An extension named at run time.
struct Mod(String);

impl DynExtension for Mod {
    fn name(&self) -> Name {
        Name::from(self.0.as_str())
    }
    fn version(&self) -> u32 {
        3
    }
    fn contribute(&self, r: &mut Registrar) {
        r.dyn_tool(Arc::new(Declared(String::from("bash"))));
    }
}

#[test]
fn an_extension_and_its_tools_may_be_named_at_run_time() {
    let registry = Registry::default()
        .with(&Mine)
        .and_then(|r| r.with_dyn(Tier::User, &Mod(format!("lua-{}", 1))))
        .unwrap();
    let extensions: Vec<_> = registry.extensions().collect();
    assert_eq!(extensions, [("mine", 1), ("lua-1", 3)]);
    let bash = registry.tools().get("bash").unwrap();
    assert_eq!(
        (bash.spec().description, bash.route()),
        ("declared".into(), Route::Owner)
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
    assert_eq!(
        gate(&registry, "b", &mut decided),
        point::Gate::Refuse("policy denied this call".into())
    );
    assert!(matches!(&decided[..], [(name, Verdict::Permission { .. })] if &**name == "policy"));
    assert_eq!(
        gate(&registry, "a", &mut decided),
        point::Gate::Refuse("other denied this call".into())
    );
}

#[test]
fn handlers_merge_in_registration_order() {
    let registry = Registry::default()
        .with(&Other)
        .and_then(|r| r.with(&DenyA))
        .unwrap();
    let mut decided = Decided::new();
    assert_eq!(
        gate(&registry, "a", &mut decided),
        point::Gate::Refuse("other denied this call".into())
    );
    let registry = Registry::default()
        .with(&DenyA)
        .and_then(|r| r.with(&Other))
        .unwrap();
    assert_eq!(
        gate(&registry, "a", &mut decided),
        point::Gate::Refuse("policy denied this call".into())
    );
}

#[test]
fn texts_follow_the_tools_offered_in_their_place() {
    let without = Registry::default().with(&Guide).unwrap();
    assert_eq!(without.system_prompt(), "I am.");
    let with = without.with(&Mine).unwrap();
    assert_eq!(with.system_prompt(), "I am.\n\nUse bash well.");
    // Made once per registry, as are the tool specs.
    assert!(std::ptr::eq(with.system_prompt(), with.system_prompt()));
    assert!(std::ptr::eq(with.specs(), with.specs()));
    let facts = Facts {
        workspace: Path::new("/w"),
        dir: Path::new("/w/sub"),
    };
    assert_eq!(with.context(&facts).as_deref(), Some("in /w/sub"));
}

extension!(Loose, "policy", 9, |r| deny(r, "nothing"));
extension!(Watcher, "watcher", 1, |r| deny(r, "c"));
extension!(Shadow, "shadow", 1, |r| r.tool(Jobs));

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
    let registry = Registry::default().with_in(Tier::Guarded, &DenyA).unwrap();
    assert!(registry.with(&Loose).is_err());

    // A user's command of a guarded one's name leaves it in place.
    let registry = Registry::default()
        .with_in(Tier::Guarded, &Commands)
        .and_then(|r| r.with(&Shadow))
        .unwrap();
    assert_eq!(registry.tools.0.len(), 1);
    assert!(Arc::ptr_eq(
        &registry.tools.0[0],
        &registry.installed[0].made.tools[0]
    ));
}

#[test]
fn commands_and_agents_are_found_by_name() {
    let registry = Registry::default()
        .with(&Commands)
        .and_then(|r| r.with(&Agents))
        .unwrap();
    let commands: Vec<_> = registry.commands().map(DynTool::description).collect();
    assert_eq!(commands, ["Lists jobs."]);
    assert!(
        registry.tools().specs().is_empty(),
        "a command is not the model's"
    );
    assert_eq!(registry.agents().collect::<Vec<_>>(), [crate::LOOP, "idle"]);
    let hi = || crate::Input::User { text: "hi".into() };
    let said = registry.agent(Some("idle")).step(hi(), registry.hooks());
    assert!(said.is_empty());
    // An unknown name runs the default loop.
    let said = registry.agent(Some("nope")).step(hi(), registry.hooks());
    assert!(matches!(&said[..], [crate::Action::CallModel { .. }]));
}
