//! The design's six scenarios, each a combination of the SDK's seven
//! verbs (callables, points, services, folds, text, agents, tiers) and no
//! API of its own:
//!
//! 1. swap `bash` for a sandboxed one: [`a_users_bash_replaces_the_shells_in_place`]
//!    here, `ext-shell`'s `every_process_goes_through_the_spawn_hooks`,
//!    and [`super::fails`]'s `a_failing_spawn_hook_refuses_the_process`;
//! 2. approval modes, the judge among them: `ext-policy`'s
//!    `modes_decide_what_is_asked_about`, `a_judge_that_fails_asks_the_user`
//!    and `the_floor_cannot_be_loosened`;
//! 3. a model fallback after rate limits that keeps the cached prefix:
//!    [`a_step_rewrite_keeps_the_prefix`];
//! 4. reminders mid-session, appended at the tail: [`a_reminder_is_a_tail_message`];
//! 5. compaction against retry: `ext-compact`'s
//!    `compact_wins_overflow_and_retry_wins_429_in_any_order` and
//!    [`super::side`]'s `a_context_overflow_is_compacted_and_sent_again`;
//! 6. a custom loop: `ext`'s `a_turn_point_can_answer_without_the_model`,
//!    `a_reply_point_rewrites_what_the_loop_runs` and, wholesale,
//!    `commands_and_agents_are_found_by_name`.

use ext::{Answer, Cx, Fold, Later, Recover, Tool};

use super::retry::{Flaky, Reply, fixture, text_reply};
use super::*;

/// A sandboxed `bash`: the user's, of the shell's name.
struct Sandboxed;

impl Tool for Sandboxed {
    type In = ext_shell::bash::Command;
    type Out = String;

    const NAME: &'static str = "bash";
    const DESCRIPTION: &'static str = "Runs a command in a sandbox.";

    fn call(&self, input: &ext_shell::bash::Command, _: &Cx) -> Result<String, String> {
        Ok(format!("sandboxed: {}", input.command))
    }
}

struct Sandbox;

impl Extension for Sandbox {
    const NAME: &'static str = "sandbox";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(Sandboxed);
    }
}

/// A tool of the shell's name at the user's tier replaces the shell's in
/// place: the shell's services and text stay, `edit` (which needs them)
/// stays, and the policy, which keys on the name, still asks.
#[test]
fn a_users_bash_replaces_the_shells_in_place() {
    let registry = echoing(&Policy::ask(&["bash"])).with(&Sandbox).unwrap();
    let names: Vec<_> = registry
        .tools()
        .specs()
        .iter()
        .map(|t| t.name.to_string())
        .collect();
    assert_eq!(names[..2], ["bash", "edit"], "{names:?}");
    assert_eq!(
        registry.tools().get("bash").unwrap().description(),
        "Runs a command in a sandbox."
    );
    assert!(
        registry
            .system_prompt()
            .contains("`bash` runs one command per call")
    );
    assert!(registry.service::<dyn ext::CwdOf>().is_some());
    let fixture = Fixture::new(ext_fixtures::ECHO, registry, openai(&Script::new(&[])));
    let session = fixture.start(&[]);
    session.submit("$ echo hi").unwrap();
    settle(&session);
    let log = log_of(&session);
    assert_eq!(inbox(&log), [2], "still asked about");
    session.answer(2, true).unwrap();
    let log = finished(&session, 2);
    session.halt();
    assert!(log.iter().any(|e| matches!(&e.body,
        Body::ToolResult { outcome: Outcome::Done { output }, .. } if output.raw().contains("sandboxed: echo hi"))));
}

/// The model a rate-limited call falls back to, until the turn ends.
#[derive(Debug, Default)]
struct FallbackTo(Option<String>);

impl Fold for FallbackTo {
    const NAME: &'static str = "fallback";
    type Change = Option<String>;

    fn apply(&mut self, to: &Option<String>) {
        self.0.clone_from(to);
    }
}

/// On a rate limit, sends the call again at once to a cheaper model of
/// the same provider: `request_error` before retry (it is the user's),
/// the `fallback` fold, and a `step` rewrite of the model alone.
struct Fallback;

impl Extension for Fallback {
    const NAME: &'static str = "fallback";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.fold::<FallbackTo>()
            .on::<point::RequestError>(|cx, failed| {
                if failed.error.kind != llm::Kind::RateLimit {
                    return Answer::Now(None);
                }
                let cx = cx.cx();
                Answer::Later(Later::new(async move {
                    cx?.update::<FallbackTo>(Some("gpt-5.6-mini".into()));
                    Some(Recover::Resend)
                }))
            })
            .on::<point::Step>(|cx, request| match cx.state(|f: &FallbackTo| f.0.clone()) {
                Some(model) => Rewrite::Replace(llm::Request {
                    model: model.into(),
                    ..request.clone()
                }),
                None => Rewrite::Pass,
            });
    }
}

/// Everything of `body` but its model.
fn but_model(body: &Value) -> Value {
    let mut body = body.clone();
    body.as_object_mut().unwrap().remove("model");
    body
}

/// A fallback that changes only the request's model keeps every other
/// byte of it: the instructions, the tools and the conversation are those
/// the call first sent, so the provider's cache of that prefix serves the
/// fallback's model, and the first model's entry is untouched for when
/// the fallback ends. The rewrite is logged with the model it went to.
#[test]
fn a_step_rewrite_keeps_the_prefix() {
    let http = Flaky::new([Reply::Status(429, &[]), Reply::Events(text_reply())]);
    let registry = Registry::default()
        .with(&Fallback)
        .and_then(|r| r.with_in(ext::Tier::Post, &ext_retry::Retry::default()))
        .unwrap();
    let session = fixture(registry, &http).start(&[]);
    session.submit("hi").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();

    let bodies = http.bodies();
    assert_eq!(bodies.len(), 2, "{log:#?}");
    assert_eq!(bodies[0]["model"], "gpt-5.6");
    assert_eq!(bodies[1]["model"], "gpt-5.6-mini");
    assert_eq!(but_model(&bodies[0]), but_model(&bodies[1]));
    assert!(log.iter().any(|e| matches!(&e.body,
        Body::Decision { by, d: Verdict::PrepareStep { model: Some(m) }, .. } if by == "fallback" && m == "gpt-5.6-mini")));
    assert!(log.iter().any(|e| matches!(&e.body,
        Body::Decision { by, d: Verdict::Retry { .. }, .. } if by == "fallback")));
}

/// A reminder of the extension's, as a user's message.
const REMINDER: &str = "<system-reminder>Keep replies short.</system-reminder>";

/// Appends a reminder to every request at its tail: never in the system
/// text, which is the cached prefix of every session.
struct Reminders;

impl Extension for Reminders {
    const NAME: &'static str = "reminders";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.on::<point::Step>(|_, request| {
            let mut messages = request.messages.clone();
            messages.push(llm::Message::user(REMINDER));
            Rewrite::Replace(llm::Request {
                messages,
                ..request.clone()
            })
        });
    }
}

/// A reminder appended at the tail leaves every earlier byte as it was:
/// what a turn's first request sent (but its own reminder) is what the
/// next one sends first, so each cache breakpoint holds.
#[test]
fn a_reminder_is_a_tail_message() {
    let http = Flaky::new([Reply::Events(text_reply()), Reply::Events(text_reply())]);
    let registry = Registry::default().with(&Reminders).unwrap();
    let session = fixture(registry, &http).start(&[]);
    session.submit("hi").unwrap();
    settle(&session);
    session.submit("and again").unwrap();
    settle(&session);
    session.halt();

    let bodies = http.bodies();
    assert_eq!(bodies.len(), 2);
    let input = |n: usize| bodies[n]["input"].as_array().unwrap().clone();
    let (first, second) = (input(0), input(1));
    let last = |input: &[Value]| input.last().unwrap().to_string();
    assert!(last(&first).contains("system-reminder"), "{first:?}");
    assert!(last(&second).contains("system-reminder"), "{second:?}");
    // The first request but its reminder is how the second begins.
    assert_eq!(first[..first.len() - 1], second[..first.len() - 1]);
    assert_eq!(bodies[0]["instructions"], bodies[1]["instructions"]);
    assert_eq!(bodies[0]["tools"], bodies[1]["tools"]);
}
