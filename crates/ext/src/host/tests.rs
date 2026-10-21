use protocol::{Card, Json, Permission, Verdict};
use serde_json::{Value, json};

use super::*;
use crate::point::{self, PointCx, View};
use crate::{Args, Concurrency, Need, Replay, Tool};

/// A tool named `bash`, described as `mine`.
struct Named(&'static str);

impl Tool for Named {
    type In = Value;
    type Out = Value;
    const NAME: &'static str = "bash";
    const DESCRIPTION: &'static str = "mine";

    fn call(&self, _: &Value, _: &Cx) -> Result<Value, String> {
        Ok(self.0.into())
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

    fn call(&self, _: &Args, _: &Cx) -> Result<Value, String> {
        Ok("none".into())
    }
}

macro_rules! extension {
    ($ty:ident, $name:literal, $version:literal, |$r:ident| $body:expr) => {
        extension!($ty, $name, $version, needs(), |$r| $body);
    };
    ($ty:ident, $name:literal, $version:literal, needs $needs:ty, |$r:ident| $body:expr) => {
        struct $ty;
        impl Extension for $ty {
            const NAME: &'static str = $name;
            const VERSION: u32 = $version;
            type Needs = $needs;
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
extension!(Editor, "editor", 1, needs(Need<dyn CwdOf>, ()), |r| r
    .text(Text::system("editing", 1, "Edit.")));

impl crate::Provides<dyn CwdOf> for Shell {}
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
    fn step(&mut self, _: crate::Input, _: &dyn LoopPoints) -> crate::Actions {
        crate::Actions::new()
    }
}

/// A session no handler reads.
struct Bare;

impl View for Bare {
    fn session(&self) -> SessionId {
        SessionId::new()
    }
    fn workspace(&self) -> &Path {
        Path::new("/w")
    }
    fn model(&self) -> String {
        String::new()
    }
    fn states(&self, read: &mut dyn FnMut(&States)) {
        read(&States::default());
    }
}

fn gate(registry: &Registry, tool: &str, decided: &mut Decided) -> point::Gate {
    let view = Bare;
    let cx = PointCx::new(&view);
    let gated = (registry
        .hooks()
        .gate(&cx, tool, &Json::encode(&json!({})).unwrap())
        .now())
    .unwrap();
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
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    fn spec(&self) -> llm::Tool {
        llm::Tool {
            name: self.0.as_str().into(),
            description: "declared".into(),
            schema: json!({ "type": "object" }),
            strict: None,
        }
    }
    fn parse(&self, _: &Json) -> Result<Parsed, String> {
        Ok(Parsed::new(()))
    }
    fn call<'a>(&'a self, _: Parsed, _: &'a Cx) -> BoxFuture<'a, Called> {
        Box::pin(std::future::ready(Called {
            outcome: Outcome::Done {
                output: Json::encode(&json!("declared")).unwrap(),
            },
            card: None,
        }))
    }
    fn to_model(&self, output: &Json) -> llm::Output {
        llm::Output::Json(output.clone())
    }
    fn present(&self, _: &Parsed) -> Option<Card> {
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
    assert_eq!(bash.spec().description, "declared");
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

extension!(Theirs, "theirs", 1, |r| r.tool(Named("theirs")));

/// Two extensions of one tier naming the same tool: the later wins, and
/// the registry says so, naming both. Across tiers it is an override,
/// said nothing of.
#[test]
fn a_same_tier_clash_warns_and_the_later_wins() {
    let registry = Registry::default()
        .with(&Mine)
        .and_then(|r| r.with(&Theirs))
        .unwrap();
    assert_eq!(registry.tools().specs().len(), 1);
    assert_eq!(
        registry.warnings(),
        ["tool bash of extension theirs replaces the one of mine, in the same tier"]
    );
    let overridden = Registry::default()
        .with_in(Tier::Pre, &Mine)
        .and_then(|r| r.with(&Theirs))
        .unwrap();
    assert_eq!(overridden.warnings(), [] as [String; 0]);
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

#[test]
fn a_non_inheritable_extension_contributes_nothing_to_children() {
    struct RootOnly;
    impl Extension for RootOnly {
        const NAME: &'static str = "root-only";
        const VERSION: u32 = 1;
        const INHERIT: bool = false;

        fn contribute(&self, r: &mut Registrar) {
            r.tool(Named("root"))
                .tool(Jobs)
                .text(Text::system("root", 0, "root prompt"));
            deny(r, "bash");
        }
    }
    let parent = Registry::default().with(&RootOnly).unwrap();
    let child = parent.inherited().unwrap();
    assert!(child.tools().get("bash").is_none());
    assert_eq!(child.commands().count(), 0);
    assert_eq!(child.system_prompt(), "");
    assert_eq!(child.extensions().count(), 0);
    assert_eq!(child.installed(), parent.installed());
    assert_eq!(child.inherited().unwrap().extensions().count(), 0);
}

#[test]
fn inheritance_rejects_a_missing_service_at_registration() {
    trait Required: Send + Sync {}
    struct Provider;
    impl Required for Provider {}
    impl Extension for Provider {
        const NAME: &'static str = "provider";
        const VERSION: u32 = 1;
        const INHERIT: bool = false;
        fn contribute(&self, r: &mut Registrar) {
            r.provide::<dyn Required>(Arc::new(Self));
        }
    }
    struct Consumer;
    impl Extension for Consumer {
        const NAME: &'static str = "consumer";
        const VERSION: u32 = 1;
        type Needs = (Need<dyn Required>, ());
        fn contribute(&self, _: &mut Registrar) {}
    }
    assert!(
        Registry::default()
            .with(&Provider)
            .unwrap()
            .with(&Consumer)
            .is_err()
    );
}

#[test]
fn a_child_never_widens_its_parents_set_and_never_beats_an_extensions_never() {
    extension!(Kept, "kept", 1, |_r| {});
    extension!(Dropped, "dropped", 1, |_r| {});
    struct Never;
    impl Extension for Never {
        const NAME: &'static str = "never";
        const VERSION: u32 = 1;
        const INHERIT: bool = false;
        fn contribute(&self, _: &mut Registrar) {}
    }
    let root = Registry::default()
        .with(&Kept)
        .unwrap()
        .with(&Dropped)
        .unwrap()
        .with(&Never)
        .unwrap();
    let child = root
        .select(crate::Inheritance::Except(&[Dropped::NAME]), None)
        .unwrap();
    let grandchild = child
        .select(
            crate::Inheritance::Only(&[Kept::NAME, Dropped::NAME, Never::NAME]),
            None,
        )
        .unwrap();
    assert_eq!(
        grandchild.extensions().collect::<Vec<_>>(),
        [(Kept::NAME, 1)]
    );
    let only = root
        .select(crate::Inheritance::Only(&[Never::NAME]), None)
        .unwrap();
    assert_eq!(only.extensions().count(), 0);
}

#[test]
fn a_kind_cannot_drop_a_service_its_inherited_extensions_require() {
    trait Required: Send + Sync {}
    struct Provider;
    impl Required for Provider {}
    impl Extension for Provider {
        const NAME: &'static str = "provider";
        const VERSION: u32 = 1;
        fn contribute(&self, r: &mut Registrar) {
            r.provide::<dyn Required>(Arc::new(Self));
        }
    }
    struct Broken;
    impl crate::AgentKind for Broken {
        const NAME: &'static str = "broken";
        const INHERIT: crate::Inheritance = crate::Inheritance::Except(&[Provider::NAME]);
        fn make() -> Box<dyn Agent> {
            Box::new(Loop::default())
        }
    }
    extension!(
        Consumer,
        "consumer",
        1,
        needs(Need<dyn Required>, ()),
        |r| {
            r.agent_kind::<Broken>();
        }
    );
    let result = Registry::default().with(&Provider).unwrap().with(&Consumer);
    assert!(matches!(result, Err(Refused(why)) if why.contains("needs")));
}
