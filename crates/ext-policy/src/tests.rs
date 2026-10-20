use std::sync::Arc;

use ext::host::Registry;
use ext::testing::Rig;
use ext::{BoxFuture, Gate, Gated, Gating, Models, Tier};
use protocol::Json;
use serde_json::json;
use sys::config::Effect;

use super::*;

/// A model asked aside that answers `reply`, or fails with none.
struct Judge(Option<&'static str>);

impl Models for Judge {
    fn call<'a>(
        &'a self,
        _: &'a Cx,
        side: Side,
    ) -> BoxFuture<'a, Result<llm::Response, llm::Error>> {
        assert_eq!(&*side.purpose, "judge");
        let reply = self.0.map(|text| llm::Response {
            message: llm::Assistant {
                content: vec![llm::AssistantPart::Text(llm::Text::new(text))],
                origin: None,
            },
            finish: llm::Finish::new(llm::Reason::Stop, None),
            usage: llm::Usage::default(),
            id: None,
            model: None,
            meta: llm::Meta::new(),
        });
        Box::pin(std::future::ready(
            reply.ok_or_else(|| llm::Error::new(llm::Kind::RateLimit, "busy")),
        ))
    }
}

struct Models_(Option<&'static str>);

impl Extension for Models_ {
    const NAME: &'static str = "models";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.provide::<dyn Models>(Arc::new(Judge(self.0)));
    }
}

fn rig(name: &str, registry: Registry) -> Rig {
    Rig::new(&format!("policy-{name}"), registry)
}

/// The gate `rig`'s next call meets, for `tool`, waiting for a later
/// answer if one comes.
#[allow(clippy::needless_pass_by_value)]
fn gate(rig: &Rig, tool: &str, input: serde_json::Value) -> Gate {
    let cx = rig.cx();
    let gating = rig
        .registry
        .hooks()
        .gate(&cx.point(), tool, &Json::encode(&input).unwrap());
    match gating {
        Gating::Now(gated) => gated.gate,
        Gating::Approving(asked) => {
            let answer = rt::block_on(asked.answer.0).map(|ok| (asked.by, ok));
            Gated::approved(asked.input, asked.decisions, answer).gate
        }
    }
}

fn policy() -> Policy {
    Policy::ask(&["bash", "edit"])
}

#[test]
fn modes_decide_what_is_asked_about() {
    let registry = Registry::default().with_in(Tier::Pre, &policy()).unwrap();
    let rig = rig("modes", registry);
    let mode = |name: &str| rig.done("mode", &json!({ "args": name }));
    assert_eq!(rig.done("mode", &json!({})), json!({ "mode": "ask" }));
    assert_eq!(gate(&rig, "bash", json!({})), Gate::Ask);
    assert_eq!(gate(&rig, "read", json!({})), Gate::Run);
    assert_eq!(mode("accept-edits"), json!({ "mode": "accept-edits" }));
    assert_eq!(gate(&rig, "edit", json!({})), Gate::Run);
    assert_eq!(gate(&rig, "bash", json!({})), Gate::Ask);
    mode("auto");
    assert_eq!(gate(&rig, "bash", json!({})), Gate::Run);
    // The mode is the session's state: it survives a restart.
    rig.restart();
    assert_eq!(gate(&rig, "bash", json!({})), Gate::Run);
    let no = rig.call("mode", &json!({ "args": "yolo" }));
    assert!(matches!(no, protocol::Outcome::Failed { error } if error.starts_with("no mode yolo")));
}

/// In `judge` mode a model asked aside approves or refuses; when it fails,
/// or does not say, the user is asked, as in `ask` mode.
#[test]
fn a_judge_that_fails_asks_the_user() {
    let judged = |reply| {
        let registry = Registry::default()
            .with_in(Tier::Pre, &Models_(reply))
            .and_then(|r| {
                r.with_in(
                    Tier::Pre,
                    &Policy {
                        mode: Mode::Judge,
                        ..policy()
                    },
                )
            })
            .unwrap();
        let rig = rig("judge", registry);
        gate(&rig, "bash", json!({ "command": "ls" }))
    };
    assert_eq!(judged(Some("yes")), Gate::Run);
    assert_eq!(
        judged(Some("No.")),
        Gate::Refuse("policy did not approve this call".into())
    );
    assert_eq!(judged(Some("maybe")), Gate::Ask);
    assert_eq!(judged(None), Gate::Ask);
}

/// A policy that allows everything.
struct Lax;

impl Extension for Lax {
    const NAME: &'static str = "policy";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.on::<PermissionPoint>(|_, _| Permission::Allow);
    }
}

fn rule(tool: &str, input: Option<&str>, effect: Effect) -> sys::config::Rule {
    sys::config::Rule {
        tool: tool.to_owned(),
        input: input.map(str::to_owned),
        effect,
    }
}

/// The floor holds whatever mode, or policy, is above it: `permission`
/// takes the strictest answer, and the floor allows nothing.
#[test]
fn the_floor_cannot_be_loosened() {
    let floor = Floor::new([
        rule("bash", Some("rm -rf /"), Effect::Deny),
        rule("*", Some("secret"), Effect::Ask),
    ])
    .unwrap();
    for mode in Mode::ALL {
        let registry = Registry::default()
            .with_in(Tier::Guarded, &floor)
            .and_then(|r| r.with_in(Tier::Pre, &Policy { mode, ..policy() }))
            .unwrap();
        let rig = rig("floor", registry);
        let denied = gate(
            &rig,
            "bash",
            json!({ "command": "rm -rf / --no-preserve-root" }),
        );
        assert_eq!(
            denied,
            Gate::Refuse("policy-floor denied this call".into()),
            "{mode:?}"
        );
        let asked = gate(&rig, "read", json!({ "path": "secret.txt" }));
        assert!(
            matches!(asked, Gate::Ask | Gate::Refuse(_)),
            "{mode:?}: {asked:?}"
        );
    }
    // A policy of the user's that allows everything does not loosen it.
    let registry = Registry::default()
        .with_in(Tier::Guarded, &floor)
        .and_then(|r| r.with(&Lax))
        .unwrap();
    let rig = rig("lax", registry);
    let denied = gate(&rig, "bash", json!({ "command": "rm -rf /" }));
    assert_eq!(denied, Gate::Refuse("policy-floor denied this call".into()));
    // Nor may the floor allow.
    let allows = Floor::new([rule("bash", None, Effect::Allow)]);
    assert!(allows.unwrap_err().contains("may only ask or deny"));
}
