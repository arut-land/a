use protocol::{CallId, Json, Outcome, Seq};

use super::*;
use crate::host::Registry;
use crate::point::Rewrite;

fn user(text: &str) -> Input {
    Input::User { text: text.into() }
}

/// A model reply holding `calls`, as the engine stores it.
fn reply(calls: &[(&str, Option<&str>)]) -> Input {
    let content = calls
        .iter()
        .map(|(id, invalid)| {
            llm::AssistantPart::ToolCall(llm::ToolCall {
                id: (*id).into(),
                name: "sleep".into(),
                input: Json::encode(&serde_json::Map::new()).unwrap(),
                invalid: invalid.map(str::to_owned),
                provider_executed: false,
                meta: llm::Meta::new(),
            })
        })
        .chain([llm::AssistantPart::Text(llm::Text::new("ok"))])
        .collect();
    let response = llm::Response {
        message: llm::Assistant {
            content,
            origin: None,
        },
        finish: llm::Finish::new(llm::Reason::Stop, None),
        usage: llm::Usage::default(),
        id: None,
        model: None,
        meta: llm::Meta::new(),
    };
    Input::ModelDone {
        call: call(),
        outcome: Outcome::Done {
            output: Json::encode(&response).unwrap().into(),
        },
    }
}

fn call() -> CallId {
    CallId::new(1).unwrap()
}

fn done() -> Input {
    Input::ToolDone {
        call: call(),
        outcome: Outcome::Interrupted { output: None },
    }
}

const NOTHING: [Action; 0] = [];
const AUTO: Action = Action::CallModel {
    choice: ToolChoice::Auto,
};
const NONE: Action = Action::CallModel {
    choice: ToolChoice::None,
};

#[test]
fn the_default_loop_is_what_runs_with_nothing_registered() {
    let registry = Registry::default();
    let mut agent = registry.agent(None);
    assert_eq!(agent.step(user("hi"), registry.hooks())[..], [AUTO]);
    assert_eq!(registry.agents().collect::<Vec<_>>(), [LOOP]);
    // Named, it is the same loop.
    let mut named = registry.agent(Some(LOOP));
    assert_eq!(named.step(user("hi"), &Defaults)[..], [AUTO]);
}

#[test]
fn the_loop_hears_a_childs_result() {
    let done = |output: &str| Input::ChildDone {
        watch: Seq::new(3).unwrap(),
        outcome: Outcome::Done {
            output: Json::encode(output).unwrap().into(),
        },
    };
    let mut agent = Loop::default();
    assert_eq!(agent.step(done("hi"), &Defaults)[..], [AUTO]);
    assert_eq!(agent.step(reply(&[]), &Defaults)[..], NOTHING);
}

#[test]
fn the_loop_calls_the_model_again_once_every_call_is_answered() {
    let mut agent = Loop::default();
    assert_eq!(agent.step(user("hi"), &Defaults)[..], [AUTO]);
    let calls = agent.step(reply(&[("a", None), ("b", Some("{\"sec"))]), &Defaults);
    let ids: Vec<_> = calls
        .iter()
        .map(|action| match action {
            Action::Call { id, .. } => (id.as_deref(), false),
            Action::Refuse { id, .. } => (id.as_deref(), true),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(ids, [(Some("a"), false), (Some("b"), true)]);
    assert_eq!(agent.step(done(), &Defaults)[..], NOTHING);
    assert_eq!(agent.step(done(), &Defaults)[..], [AUTO]);
    assert_eq!(agent.step(reply(&[]), &Defaults)[..], NOTHING);
    // A stray result, as an odd log might hold, changes nothing.
    assert_eq!(agent.step(done(), &Defaults)[..], NOTHING);
}

/// With no limit installed the loop goes on as long as the model calls
/// tools.
#[test]
fn without_a_step_limit_the_loop_goes_on() {
    let mut agent = Loop::default();
    agent.step(user("hi"), &Defaults);
    for _ in 0..100 {
        assert_eq!(agent.step(reply(&[("a", None)]), &Defaults).len(), 1);
        assert_eq!(agent.step(done(), &Defaults)[..], [AUTO]);
    }
}

#[test]
fn a_step_limit_sends_a_final_call_with_no_tools() {
    let registry = Registry::default()
        .with_in(crate::Tier::Pre, &StepLimit { max: 3 })
        .unwrap();
    let points = registry.hooks();
    let mut agent = registry.agent(None);
    assert_eq!(agent.step(user("hi"), points)[..], [AUTO]);
    for _ in 1..3 {
        assert_eq!(agent.step(reply(&[("a", None)]), points).len(), 1);
        assert_eq!(agent.step(done(), points)[..], [AUTO]);
    }
    // The third reply's calls run; the call after them is the last, with
    // the tools offered and none callable.
    assert!(matches!(
        &agent.step(reply(&[("a", None)]), points)[..],
        [Action::Call { .. }]
    ));
    assert_eq!(agent.step(done(), points)[..], [NONE]);
    // A model that calls one anyway is refused, with why, and the turn ends.
    let last = agent.step(reply(&[("a", None)]), points);
    assert!(matches!(&last[..], [Action::Refuse { why, .. }] if why.contains("limit")));
    assert_eq!(agent.step(done(), points)[..], NOTHING);
    // The next input starts the count again.
    assert_eq!(agent.step(user("again"), points)[..], [AUTO]);
}

#[test]
fn a_step_limit_answers_only_at_and_past_its_limit() {
    let limit = StepLimit { max: 2 };
    let answers: Vec<_> = (0..4).map(|step| limit.resume(step)).collect();
    assert_eq!(
        answers,
        [None, None, Some(Resume::Final), Some(Resume::Stop)]
    );
}

/// A command answered without a model call: `/help` as a `turn` handler.
struct Help;

impl Extension for Help {
    const NAME: &'static str = "help";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.on::<point::Turn>(|(), (input, _)| match input {
            Input::User { text } if text == "help" => Rewrite::Replace(Actions::from_elem(
                Action::Say("Ask me anything.".into()),
                1,
            )),
            _ => Rewrite::Pass,
        });
    }
}

#[test]
fn a_turn_point_can_answer_without_the_model() {
    let registry = Registry::default().with(&Help).unwrap();
    let mut agent = registry.agent(None);
    let said = agent.step(user("help"), registry.hooks());
    assert_eq!(said[..], [Action::Say("Ask me anything.".into())]);
    assert_eq!(agent.step(user("hi"), registry.hooks())[..], [AUTO]);
}

/// Sequential calls only: a `reply` handler that runs a reply's first call
/// and refuses the rest.
struct OneAtATime;

impl Extension for OneAtATime {
    const NAME: &'static str = "one";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.on::<point::Reply>(|(), replied| {
            let mut actions = replied.actions.clone();
            for action in actions.iter_mut().skip(1) {
                if let Action::Call { tool, input, id } = action {
                    *action = Action::Refuse {
                        tool: tool.clone(),
                        input: input.clone(),
                        id: id.clone(),
                        why: "one call at a time".into(),
                    };
                }
            }
            Rewrite::Replace(actions)
        });
    }
}

#[test]
fn a_reply_point_rewrites_what_the_loop_runs() {
    let registry = Registry::default().with(&OneAtATime).unwrap();
    let points = registry.hooks();
    let mut agent = registry.agent(None);
    agent.step(user("hi"), points);
    let actions = agent.step(reply(&[("a", None), ("b", None)]), points);
    assert!(matches!(
        &actions[..],
        [Action::Call { .. }, Action::Refuse { why, .. }] if why == "one call at a time"
    ));
    // Both are answered before the model is called again.
    assert_eq!(agent.step(done(), points)[..], NOTHING);
    assert_eq!(agent.step(done(), points)[..], [AUTO]);
}
