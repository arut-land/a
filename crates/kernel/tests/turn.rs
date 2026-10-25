//! The turn and call lifecycle as tables of `(event, effects)`: the
//! orderings the engine's session tests waited and polled for, each a row.

mod common;

use std::time::Duration;

use common::*;
use kernel::{Event, Gated, Later, Load, Note, Owner, Recover, Replied, Ticket};
use protocol::{
    Audience, Body, CallId, Installed, Json, Mode, Outcome, Permission, Seq, SessionId, Verdict,
};
use serde_json::json;

fn echo() -> Session {
    Session::new(Reg::echo().arc())
}

fn modeled() -> Session {
    Session::new(Reg::modeled().arc())
}

/// A call the model made, under its id.
fn model_tool_call(tool: &str, input: &serde_json::Value, id: &str) -> Body {
    Body::ToolCall {
        invoke: None,
        tool: tool.to_owned(),
        input: Json::encode(input).unwrap(),
        id: Some(id.to_owned()),
        card: None,
        by: Audience::Model,
    }
}

#[test]
fn input_with_nothing_running_starts_a_turn_at_once() {
    echo().table(vec![(
        user("hi"),
        (vec![entry(1, input("hi")), entry(2, said("hi"))], vec![]),
    )]);
}

#[test]
fn a_tool_runs_after_its_intent_and_its_result_ends_the_turn() {
    echo().table(vec![
        (
            user("$ ls"),
            (
                vec![entry(1, input("$ ls")), entry(2, bash("ls"))],
                vec![BUSY, run_bash(2, "ls")],
            ),
        ),
        (
            tool_done(2, done(json!("a\n"))),
            (
                vec![entry(3, tool_result(2, done(json!("a\n"))))],
                vec![IDLE],
            ),
        ),
    ]);
}

#[test]
fn a_denied_call_is_answered_after_its_intent_without_running() {
    let mut s = Session::new(Reg::echo().policy(Policy::Deny(&["bash"])).arc());
    s.table(vec![(
        user("$ rm"),
        (
            vec![
                entry(1, input("$ rm")),
                entry(2, bash("rm")),
                entry(3, permission(2, Permission::Deny)),
                entry(4, tool_result(2, failed("policy denied this call"))),
            ],
            vec![],
        ),
    )]);
}

fn asking() -> Session {
    Session::new(Reg::echo().policy(Policy::Ask(&["bash"])).arc())
}

fn asked() -> Expected {
    (
        vec![
            entry(1, input("$ echo")),
            entry(2, bash("echo")),
            entry(
                3,
                permission(
                    2,
                    Permission::Ask(protocol::ApprovalReason::Mode { mode: "ask".into() }),
                ),
            ),
        ],
        vec![],
    )
}

fn answer(call: u64, approved: bool, ticket: u64) -> Event {
    Event::Answer {
        call: id(call),
        value: Json::encode(&approved).unwrap(),
        origin: None,
        ticket: Some(Ticket(ticket)),
    }
}

fn answered(call: u64, approved: bool) -> Body {
    Body::Answer {
        call: id(call),
        value: Json::encode(&approved).unwrap(),
        origin: None,
    }
}

/// Waiting for an approval is not work; the first answer wins.
#[test]
fn an_ask_waits_and_runs_once_approved() {
    let ok = |ticket| Work::Reply {
        ticket: Ticket(ticket),
        result: Ok(()),
    };
    asking().table(vec![
        (user("$ echo"), asked()),
        (
            answer(1, true, 6),
            (
                vec![],
                vec![Work::Reply {
                    ticket: Ticket(6),
                    result: Err("call 1 is not waiting for an answer".into()),
                }],
            ),
        ),
        (
            answer(2, true, 7),
            (
                vec![entry(4, answered(2, true))],
                vec![BUSY, run_bash(2, "echo"), ok(7)],
            ),
        ),
        (
            answer(2, false, 8),
            (
                vec![],
                vec![Work::Reply {
                    ticket: Ticket(8),
                    result: Err("call 2 is not waiting for an answer".into()),
                }],
            ),
        ),
        (
            tool_done(2, done(json!("echo\n"))),
            (
                vec![entry(5, tool_result(2, done(json!("echo\n"))))],
                vec![IDLE],
            ),
        ),
    ]);
}

#[test]
fn a_refused_ask_answers_the_call_without_running_it() {
    asking().table(vec![
        (user("$ echo"), asked()),
        (
            approve(2, false),
            (
                vec![
                    entry(4, answered(2, false)),
                    entry(
                        5,
                        tool_result(2, failed("the user did not approve this call")),
                    ),
                ],
                vec![],
            ),
        ),
    ]);
}

#[test]
fn an_interrupt_while_asked_settles_the_call_without_running_it() {
    asking().table(vec![
        (user("$ echo"), asked()),
        (
            interrupt(),
            (
                vec![
                    entry(4, tool_result(2, INTERRUPTED)),
                    entry(5, Body::INTERRUPT),
                ],
                vec![],
            ),
        ),
        // Too late: nothing runs.
        (approve(2, true), (vec![], vec![])),
    ]);
}

/// A gate that answers later holds its call, not the session: an
/// interrupt settles the call, and the answer coming after starts nothing.
#[test]
fn an_interrupt_while_a_call_is_gated_starts_nothing() {
    let mut s = Session::new(Reg::echo().policy(Policy::Later).arc());
    let later = Later::new(async { Gated::run(Json::null()) });
    s.table(vec![
        (
            user("$ x"),
            (
                vec![entry(1, input("$ x")), entry(2, bash("x"))],
                vec![
                    BUSY,
                    Work::Await {
                        call: id(2),
                        answer: later,
                    },
                ],
            ),
        ),
        (
            interrupt(),
            (
                vec![
                    entry(3, tool_result(2, INTERRUPTED)),
                    entry(4, Body::INTERRUPT),
                ],
                vec![cancel(2), IDLE],
            ),
        ),
        (
            Event::Gated {
                call: id(2),
                gated: Gated::run(Json::null()),
            },
            (vec![], vec![]),
        ),
    ]);
}

#[test]
fn a_gate_answering_later_runs_the_call_when_it_answers() {
    let mut s = Session::new(Reg::echo().policy(Policy::Later).arc());
    s.step(user("$ x"));
    let gated = Gated::run(Json::encode(&json!({ "command": "x" })).unwrap());
    s.table(vec![(
        Event::Gated { call: id(2), gated },
        (vec![], vec![run_bash(2, "x")]),
    )]);
}

/// B2: an interrupt with nothing running stops nothing, not the next turn.
#[test]
fn an_interrupt_between_turns_is_not_kept_for_the_next() {
    echo().table(vec![
        (interrupt(), (vec![], vec![])),
        (
            user("hi"),
            (vec![entry(1, input("hi")), entry(2, said("hi"))], vec![]),
        ),
    ]);
}

#[test]
fn an_interrupt_stops_the_model_call_and_ends_the_turn() {
    modeled().table(vec![
        (
            user("hi"),
            (
                vec![entry(1, input("hi")), entry(2, model_call(0))],
                vec![BUSY, prepare(2, 0)],
            ),
        ),
        (interrupt(), (vec![], vec![cancel(2)])),
        // Interrupting again, before the call reports, does nothing more.
        (interrupt(), (vec![], vec![])),
        (
            Event::ModelDone {
                call: id(2),
                replied: Replied::Interrupted(Some("par".into())),
            },
            (
                vec![
                    entry(
                        3,
                        model_result(
                            2,
                            Outcome::Interrupted {
                                output: Some("par".into()),
                            },
                        ),
                    ),
                    entry(4, Body::INTERRUPT),
                ],
                vec![IDLE],
            ),
        ),
    ]);
}

/// The shared calls of a reply run together, and an interrupt cancels
/// every one; the turn ends once all have reported.
#[test]
fn an_interrupt_cancels_every_call_of_the_reply_running() {
    let a = json!({ "seconds": 1 });
    let b = json!({ "seconds": 2 });
    let calls = [("sleep", a.clone()), ("sleep", b.clone())];
    modeled().table(vec![
        (
            user("go"),
            (
                vec![entry(1, input("go")), entry(2, model_call(0))],
                vec![BUSY, prepare(2, 0)],
            ),
        ),
        (
            model_done(2, &calls),
            (
                vec![
                    entry(
                        3,
                        model_result(
                            2,
                            Outcome::Done {
                                output: reply(&calls),
                            },
                        ),
                    ),
                    entry(4, model_tool_call("sleep", &a, "c0")),
                    entry(5, model_tool_call("sleep", &b, "c1")),
                ],
                vec![
                    run_tool(4, "sleep", a.clone()),
                    run_tool(5, "sleep", b.clone()),
                ],
            ),
        ),
        (interrupt(), (vec![], vec![cancel(4), cancel(5)])),
        (
            tool_done(5, INTERRUPTED),
            (vec![entry(6, tool_result(5, INTERRUPTED))], vec![]),
        ),
        (
            tool_done(4, INTERRUPTED),
            (
                vec![
                    entry(7, tool_result(4, INTERRUPTED)),
                    entry(8, Body::INTERRUPT),
                ],
                vec![IDLE],
            ),
        ),
    ]);
}

/// Each call the model made is answered, the ones not started as
/// interrupted, and the model is not asked again: an exclusive call
/// waits for the one before it, so the interrupt leaves it unstarted.
#[test]
fn an_interrupted_turn_answers_the_calls_it_leaves_and_goes_no_further() {
    let a = json!({ "command": "a" });
    let b = json!({ "command": "b" });
    modeled().table(vec![
        (
            user("go"),
            (
                vec![entry(1, input("go")), entry(2, model_call(0))],
                vec![BUSY, prepare(2, 0)],
            ),
        ),
        (
            model_done(2, &[("bash", a.clone()), ("bash", b.clone())]),
            (
                vec![
                    entry(
                        3,
                        model_result(
                            2,
                            Outcome::Done {
                                output: reply(&[("bash", a.clone()), ("bash", b.clone())]),
                            },
                        ),
                    ),
                    entry(4, model_tool_call("bash", &a, "c0")),
                ],
                vec![run_bash(4, "a")],
            ),
        ),
        (interrupt(), (vec![], vec![cancel(4)])),
        (
            tool_done(4, failed("cut")),
            (
                vec![
                    entry(5, tool_result(4, failed("cut"))),
                    entry(6, model_tool_call("bash", &b, "c1")),
                    entry(7, tool_result(6, INTERRUPTED)),
                    entry(8, Body::INTERRUPT),
                ],
                vec![IDLE],
            ),
        ),
    ]);
}

#[test]
fn a_steer_reaches_the_running_turn_before_the_model_is_next_asked() {
    let sleep = json!({ "seconds": 1 });
    let steered = Body::Input {
        segments: "twice".into(),
        mode: Mode::Steer,
        taken: false,
        origin: None,
        force: false,
    };
    modeled().table(vec![
        (
            user("go"),
            (
                vec![entry(1, input("go")), entry(2, model_call(0))],
                vec![BUSY, prepare(2, 0)],
            ),
        ),
        (
            model_done(2, &[("sleep", sleep.clone())]),
            (
                vec![
                    entry(
                        3,
                        model_result(
                            2,
                            Outcome::Done {
                                output: reply(&[("sleep", sleep.clone())]),
                            },
                        ),
                    ),
                    entry(4, model_tool_call("sleep", &sleep, "c0")),
                ],
                vec![run_tool(4, "sleep", sleep)],
            ),
        ),
        (steer("twice"), (vec![entry(5, steered)], vec![])),
        (user("then"), (vec![entry(6, queued("then"))], vec![])),
        (
            tool_done(4, done(json!({ "slept": 1 }))),
            (
                vec![
                    entry(7, tool_result(4, done(json!({ "slept": 1 })))),
                    entry(8, take(5)),
                    entry(9, model_call(0)),
                ],
                vec![prepare(9, 0)],
            ),
        ),
        (
            model_done(9, &[]),
            (
                vec![
                    entry(10, model_result(9, Outcome::Done { output: reply(&[]) })),
                    entry(11, take(6)),
                    entry(12, model_call(0)),
                ],
                vec![prepare(12, 0)],
            ),
        ),
    ]);
}

#[test]
fn queued_input_is_taken_up_in_order_once_the_turn_ends() {
    let sleep = json!({ "seconds": 1 });
    echo().table(vec![
        (
            user("sleep 1"),
            (
                vec![entry(1, input("sleep 1")), entry(2, sleep_call(1))],
                vec![BUSY, run_tool(2, "sleep", sleep)],
            ),
        ),
        (user("a"), (vec![entry(3, queued("a"))], vec![])),
        (user("b"), (vec![entry(4, queued("b"))], vec![])),
        (
            tool_done(2, done(json!({ "slept": 1 }))),
            (
                vec![
                    entry(5, tool_result(2, done(json!({ "slept": 1 })))),
                    entry(6, take(3)),
                    entry(7, said("a")),
                    entry(8, take(4)),
                    entry(9, said("b")),
                ],
                vec![IDLE],
            ),
        ),
    ]);
}

#[test]
fn input_waits_its_turn_behind_a_call_waiting_for_approval() {
    asking().table(vec![
        (user("$ echo"), asked()),
        (user("next"), (vec![entry(4, queued("next"))], vec![BUSY])),
        (
            approve(2, true),
            (vec![entry(5, answered(2, true))], vec![run_bash(2, "echo")]),
        ),
        (
            tool_done(2, done(json!("echo\n"))),
            (
                vec![
                    entry(6, tool_result(2, done(json!("echo\n")))),
                    entry(7, take(4)),
                    entry(8, said("next")),
                ],
                vec![IDLE],
            ),
        ),
    ]);
}

fn retrying() -> Session {
    Session::new(Reg::modeled().retry(Duration::from_secs(1)).arc())
}

fn retried() -> Expected {
    let retry = Verdict::Retry {
        attempt: 1,
        error: "slow down".into(),
    };
    (
        vec![entry(3, decision("retry", Some(2), retry))],
        vec![Work::Sleep {
            call: id(2),
            after: Duration::from_secs(1),
        }],
    )
}

/// The `request_error` point retries: the next attempt is logged on the
/// call, which is sent again once the backoff is over and streams as that
/// attempt; the agent hears only of the reply.
#[test]
fn a_failed_model_call_is_retried_as_the_request_error_point_asks() {
    let mut s = retrying();
    s.table(vec![
        (
            user("hi"),
            (
                vec![entry(1, input("hi")), entry(2, model_call(0))],
                vec![BUSY, prepare(2, 0)],
            ),
        ),
        (model_failed(2), retried()),
        (Event::Woke { call: id(2) }, (vec![], vec![prepare(2, 1)])),
        (
            model_done(2, &[]),
            (
                vec![entry(
                    4,
                    model_result(2, Outcome::Done { output: reply(&[]) }),
                )],
                vec![IDLE],
            ),
        ),
    ]);
}

/// What the `step` point decided about a request is logged before the
/// request is sent: the kernel has it prepared, logs, then sends.
#[test]
fn a_step_decision_is_logged_before_the_request_is_sent() {
    let mut s = modeled();
    s.step(user("hi"));
    let fallback = Verdict::PrepareStep {
        model: Some("cheap".into()),
    };
    s.table(vec![
        (
            Event::Prepared {
                call: id(2),
                decisions: vec![("fallback".into(), fallback.clone())],
                answered: None,
            },
            (
                vec![entry(3, decision("fallback", Some(2), fallback))],
                vec![send(2)],
            ),
        ),
        (
            model_done(2, &[]),
            (
                vec![entry(
                    4,
                    model_result(2, Outcome::Done { output: reply(&[]) }),
                )],
                vec![IDLE],
            ),
        ),
    ]);
}

/// A handler's own answer at `step` stands as the model's reply, logged
/// as its decision; nothing is sent.
#[test]
fn a_step_answer_stands_as_the_reply_and_nothing_is_sent() {
    let mut s = modeled();
    s.step(user("hi"));
    s.table(vec![(
        Event::Prepared {
            call: id(2),
            decisions: Vec::new(),
            answered: Some(("cache".into(), Replied::Done(reply(&[])))),
        },
        (
            vec![
                entry(3, decision("cache", Some(2), Verdict::Answered)),
                entry(4, model_result(2, Outcome::Done { output: reply(&[]) })),
            ],
            vec![IDLE],
        ),
    )]);
}

/// A request prepared for a turn interrupted meanwhile is never sent:
/// the call ends interrupted, and the turn with it.
#[test]
fn a_request_prepared_after_an_interrupt_is_not_sent() {
    let mut s = modeled();
    s.step(user("hi"));
    s.table(vec![
        (interrupt(), (vec![], vec![cancel(2)])),
        (
            prepared(2),
            (
                vec![
                    entry(3, model_result(2, INTERRUPTED)),
                    entry(4, Body::INTERRUPT),
                ],
                vec![IDLE],
            ),
        ),
    ]);
}

/// The point's attempts run out: the call fails.
#[test]
fn a_call_fails_once_the_retries_run_out() {
    let mut s = retrying();
    s.step(user("hi"));
    for attempt in 1..=RETRIES {
        s.step(model_failed(2));
        assert_plan(
            &s.step(Event::Woke { call: id(2) }),
            (vec![], vec![prepare(2, attempt)]),
        );
    }
    let fx = s.step(model_failed(2));
    let seq = s.log.len() as u64;
    assert_plan(
        &fx,
        (
            vec![entry(seq, model_result(2, failed("slow down")))],
            vec![IDLE],
        ),
    );
}

#[test]
fn an_interrupt_in_backoff_sends_nothing_again() {
    retrying().table(vec![
        (
            user("hi"),
            (
                vec![entry(1, input("hi")), entry(2, model_call(0))],
                vec![BUSY, prepare(2, 0)],
            ),
        ),
        (model_failed(2), retried()),
        (
            interrupt(),
            (
                vec![
                    entry(4, model_result(2, INTERRUPTED)),
                    entry(5, Body::INTERRUPT),
                ],
                vec![cancel(2), IDLE],
            ),
        ),
        (Event::Woke { call: id(2) }, (vec![], vec![])),
    ]);
}

/// A handler that answers later how to recover holds the call meanwhile:
/// nothing is sent until it answers.
fn recovering() -> (Session, Work) {
    let mut s = Session::new(Reg::modeled().resend().arc());
    s.step(user("hi"));
    let mut fx = into_work(s.step(model_failed(2))).collect::<Vec<_>>();
    assert_eq!(fx.len(), 1, "{fx:?}");
    (s, fx.remove(0))
}

/// `resend` sends the call again at once, as its next attempt, with the
/// request made from the transcript as it is then: the effect names no
/// request of its own.
#[test]
fn a_resend_rebuilds_from_the_transcript() {
    let (mut s, recover) = recovering();
    assert_eq!(
        recover,
        Work::Recover {
            call: id(2),
            answer: Later::new(async { None }),
        }
    );
    let retry = Verdict::Retry {
        attempt: 1,
        error: "slow down".into(),
    };
    s.table(vec![(
        Event::Recovered {
            call: id(2),
            recovered: Some(("compact".into(), Recover::Resend)),
        },
        (
            vec![entry(3, decision("compact", Some(2), retry))],
            vec![prepare(2, 1)],
        ),
    )]);
    // An answer of none fails the call with its error.
    let (mut s, _) = recovering();
    let recovered = Event::Recovered {
        call: id(2),
        recovered: None,
    };
    s.table(vec![(
        recovered,
        (
            vec![entry(3, model_result(2, failed("slow down")))],
            vec![IDLE],
        ),
    )]);
}

#[test]
fn an_interrupt_in_recovering_sends_nothing() {
    let (mut s, _) = recovering();
    s.table(vec![
        (
            interrupt(),
            (
                vec![
                    entry(3, model_result(2, INTERRUPTED)),
                    entry(4, Body::INTERRUPT),
                ],
                vec![cancel(2), IDLE],
            ),
        ),
        // An answer that comes anyway changes nothing.
        (
            Event::Recovered {
                call: id(2),
                recovered: Some(("compact".into(), Recover::Resend)),
            },
            (vec![], vec![]),
        ),
    ]);
}

#[test]
fn a_failure_no_extension_retries_fails_the_turn() {
    modeled().table(vec![
        (
            user("hi"),
            (
                vec![entry(1, input("hi")), entry(2, model_call(0))],
                vec![BUSY, prepare(2, 0)],
            ),
        ),
        (
            model_failed(2),
            (
                vec![entry(3, model_result(2, failed("slow down")))],
                vec![IDLE],
            ),
        ),
    ]);
}

fn command(args: &str) -> Body {
    Body::ToolCall {
        invoke: None,
        tool: "jobs".into(),
        input: Json::encode(&json!({ "args": args })).unwrap(),
        id: None,
        card: None,
        by: Audience::User,
    }
}

/// A command is a callable the user calls: no model turn, the same gate.
#[test]
fn slash_commands_run_without_a_turn() {
    echo().table(vec![
        (
            user("/jobs now"),
            (
                vec![entry(1, input("/jobs now")), entry(2, command("now"))],
                vec![BUSY, run_tool(2, "jobs", json!({ "args": "now" }))],
            ),
        ),
        (
            tool_done(2, done(json!([]))),
            (vec![entry(3, tool_result(2, done(json!([]))))], vec![IDLE]),
        ),
    ]);
}

/// A pasted `/jobs` line is what the user pasted, never a command, taken
/// at once or after waiting.
#[test]
fn a_pasted_slash_line_is_input_not_a_command() {
    let pasted = || protocol::Segments(vec![protocol::Segment::Paste("/jobs now".into())]);
    let paste = || Event::Input {
        segments: pasted(),
        mode: Mode::Queue,
        origin: None,
        ticket: None,
        force: false,
    };
    let logged = |taken| Body::Input {
        segments: pasted(),
        mode: Mode::Queue,
        taken,
        origin: None,
        force: false,
    };
    echo().table(vec![
        (
            paste(),
            (
                vec![entry(1, logged(true)), entry(2, said("/jobs now"))],
                vec![],
            ),
        ),
        (
            user("$ ls"),
            (
                vec![entry(3, input("$ ls")), entry(4, bash("ls"))],
                vec![BUSY, run_bash(4, "ls")],
            ),
        ),
        (paste(), (vec![entry(5, logged(false))], vec![])),
        (
            tool_done(4, done(json!("a\n"))),
            (
                vec![
                    entry(6, tool_result(4, done(json!("a\n")))),
                    entry(7, take(5)),
                    entry(8, said("/jobs now")),
                ],
                vec![IDLE],
            ),
        ),
    ]);
}

#[test]
fn commands_obey_permission() {
    let mut s = Session::new(Reg::echo().policy(Policy::Deny(&["jobs"])).arc());
    s.table(vec![(
        user("/jobs"),
        (
            vec![
                entry(1, input("/jobs")),
                entry(2, command("")),
                entry(3, permission(2, Permission::Deny)),
                entry(4, tool_result(2, failed("policy denied this call"))),
            ],
            vec![],
        ),
    )]);
}

#[test]
fn the_input_point_rewrites_or_refuses_input_before_it_is_logged() {
    echo().table(vec![
        (
            Event::Input {
                segments: "secret".into(),
                mode: Mode::Queue,
                origin: None,
                ticket: Some(Ticket(1)),
                force: false,
            },
            (
                vec![],
                vec![Work::Reply {
                    ticket: Ticket(1),
                    result: Err("input: no secrets".into()),
                }],
            ),
        ),
        (
            user("shout hi"),
            (
                vec![
                    entry(
                        1,
                        decision(
                            "input",
                            None,
                            Verdict::Input {
                                original: "shout hi".into(),
                            },
                        ),
                    ),
                    entry(2, input("HI")),
                    entry(3, said("HI")),
                ],
                vec![],
            ),
        ),
    ]);
}

#[test]
fn the_on_stop_point_has_the_agent_go_on() {
    let mut s = Session::new(Reg::echo().on_stop("and again").arc());
    s.table(vec![(
        user("hi"),
        (
            vec![
                entry(1, input("hi")),
                entry(2, said("hi")),
                entry(
                    3,
                    decision(
                        "policy",
                        None,
                        Verdict::OnStop {
                            text: "and again".into(),
                        },
                    ),
                ),
                entry(4, said("and again")),
            ],
            vec![],
        ),
    )]);
}

/// A registry offered while busy is discarded; the driver retries at rest.
#[test]
fn a_generation_is_adopted_only_at_rest() {
    let sleep = json!({ "seconds": 1 });
    let newer = Reg::echo().extensions(&["fixtures", "extras"]).arc();
    let installed = ["fixtures", "extras"].map(|name| Installed {
        name: name.into(),
        version: 1,
        image: None,
    });
    echo().table(vec![
        (
            user("sleep 1"),
            (
                vec![entry(1, input("sleep 1")), entry(2, sleep_call(1))],
                vec![BUSY, run_tool(2, "sleep", sleep)],
            ),
        ),
        (Event::Registry(newer.clone()), (vec![], vec![])),
        (
            tool_done(2, done(json!(1))),
            (vec![entry(3, tool_result(2, done(json!(1))))], vec![IDLE]),
        ),
        (
            user("old"),
            (vec![entry(4, input("old")), entry(5, said("old"))], vec![]),
        ),
        (
            Event::Registry(newer),
            (
                vec![entry(
                    6,
                    Body::Extensions {
                        installed: installed.to_vec(),
                    },
                )],
                vec![],
            ),
        ),
        (
            user("new"),
            (vec![entry(7, input("new")), entry(8, said("new"))], vec![]),
        ),
    ]);
}

fn spawn(wait: bool) -> Event {
    Event::Spawn {
        intent: Json::null(),
        call: id(2),
        child: child(),
        input: "go".into(),
        wait,
    }
}

fn spawned() -> Expected {
    (
        vec![entry(
            3,
            Body::Link {
                intent: Json::null(),
                child: child(),
                input: "go".into(),
            },
        )],
        vec![],
    )
}

fn started() -> Work {
    Work::StartChild {
        link: seq(3),
        child: child(),
        input: "go".into(),
    }
}

fn tasked() -> Expected {
    let task = json!({ "input": "go" });
    (
        vec![
            entry(1, input("task go")),
            entry(2, tool_call("task", task.clone())),
        ],
        vec![BUSY, run_tool(2, "task", task)],
    )
}

fn fired(seq: u64, watch: u64, outcome: Outcome) -> protocol::Entry {
    entry(
        seq,
        Body::Fired {
            watch: Seq::new(watch).unwrap(),
            outcome,
        },
    )
}

/// A call waiting on its child goes on with the child's result.
#[test]
fn a_call_waits_on_the_child_it_links_and_resumes_with_its_result() {
    let mut waits = spawned();
    waits.0.extend([
        entry(4, wait_child(2, 3)),
        entry(5, Body::Watch { child: child() }),
    ]);
    waits.1.push(started());
    let ok = done(json!("ok"));
    echo().table(vec![
        (user("task go"), tasked()),
        (spawn(true), waits),
        (
            Event::ChildSettled {
                child: child(),
                outcome: ok.clone(),
            },
            (
                vec![fired(6, 5, ok.clone())],
                vec![Work::Resume {
                    call: id(2),
                    tool: "task".into(),
                    input: Json::encode(&json!({ "input": "go" })).unwrap(),
                    value: Json::encode(&done(json!("ok"))).unwrap(),
                }],
            ),
        ),
        // A child reports again after a restart: its watch fired already.
        (
            Event::ChildSettled {
                child: child(),
                outcome: ok.clone(),
            },
            (vec![], vec![]),
        ),
        (
            tool_done(2, ok.clone()),
            (vec![entry(7, tool_result(2, ok))], vec![IDLE]),
        ),
    ]);
}

/// Cancelling runs bottom-up: the child is interrupted, and the parent's
/// turn ends only once the child has settled.
#[test]
fn an_interrupt_stops_the_child_first() {
    let mut waits = spawned();
    waits.0.extend([
        entry(4, wait_child(2, 3)),
        entry(5, Body::Watch { child: child() }),
    ]);
    waits.1.push(started());
    echo().table(vec![
        (user("task go"), tasked()),
        (spawn(true), waits),
        (
            interrupt(),
            (vec![], vec![Work::Interrupt { child: child() }]),
        ),
        (
            Event::ChildSettled {
                child: child(),
                outcome: INTERRUPTED,
            },
            (
                vec![
                    fired(6, 5, INTERRUPTED),
                    entry(7, tool_result(2, INTERRUPTED)),
                    entry(8, Body::INTERRUPT),
                ],
                vec![IDLE],
            ),
        ),
    ]);
}

/// A child no call waits on hands its result to the agent, between turns.
#[test]
fn a_background_childs_result_starts_a_turn_of_its_own() {
    let ok = done(json!("ok"));
    echo().table(vec![
        (user("task go"), tasked()),
        (spawn(false), {
            let mut fx = spawned();
            fx.0.push(entry(4, Body::Watch { child: child() }));
            fx.1.push(started());
            fx
        }),
        // The turn runs: the result is queued, and waits for it to end.
        (
            Event::ChildSettled {
                child: child(),
                outcome: ok.clone(),
            },
            (
                vec![fired(5, 4, ok.clone()), entry(6, own_child(4, &ok))],
                vec![],
            ),
        ),
        (
            tool_done(2, done(json!(null))),
            (
                vec![
                    entry(7, tool_result(2, done(json!(null)))),
                    entry(8, take(6)),
                    entry(9, said("child: \"ok\"")),
                ],
                vec![IDLE],
            ),
        ),
    ]);
}

#[test]
fn a_child_reports_each_time_it_settles() {
    let load = Load {
        owner: Owner::Present,
        ..Load::default()
    };
    let (mut s, fx) = Session::resume(Reg::echo().arc(), log([input("hi")]), &load);
    let report = |text: &str| Work::Report {
        outcome: Outcome::Done {
            output: Json::encode(&text).unwrap(),
        },
    };
    assert_plan(&fx, (vec![entry(2, said("hi"))], vec![report("hi")]));
    s.table(vec![(
        user("again"),
        (
            vec![entry(3, input("again")), entry(4, said("again"))],
            vec![report("again")],
        ),
    )]);
}

#[test]
fn a_settled_call_cannot_write_and_stray_reports_change_nothing() {
    let progress = || Event::Note {
        call: id(2),
        note: Note::Progress("one\n".into()),
    };
    echo().table(vec![
        (
            user("$ make"),
            (
                vec![entry(1, input("$ make")), entry(2, bash("make"))],
                vec![BUSY, run_bash(2, "make")],
            ),
        ),
        (
            progress(),
            (
                vec![entry(
                    3,
                    Body::Progress {
                        call: CallId::new(2).unwrap(),
                        text: "one\n".into(),
                    },
                )],
                vec![],
            ),
        ),
        (
            tool_done(2, done(json!(0))),
            (vec![entry(4, tool_result(2, done(json!(0))))], vec![IDLE]),
        ),
        (progress(), (vec![], vec![])),
        (tool_done(2, done(json!(0))), (vec![], vec![])),
        (tool_done(9, done(json!(0))), (vec![], vec![])),
        (model_done(2, &[]), (vec![], vec![])),
        (Event::Woke { call: id(2) }, (vec![], vec![])),
    ]);
}

/// The same mutation sent twice (after a reconnect, or a restart) is done
/// once and answered as the first was.
#[test]
fn a_resend_runs_once() {
    let hi = |ticket| Event::Input {
        segments: "hi".into(),
        mode: Mode::Queue,
        origin: Some(author(1)),
        ticket: Some(Ticket(ticket)),
        force: false,
    };
    let ok = |ticket| Work::Reply {
        ticket: Ticket(ticket),
        result: Ok(()),
    };
    let logged = Body::Input {
        segments: "hi".into(),
        mode: Mode::Queue,
        taken: true,
        origin: Some(author(1)),
        force: false,
    };
    echo().table(vec![
        // Answered once its input is logged, before the turn it starts.
        (
            hi(1),
            (vec![entry(1, logged), entry(2, said("hi"))], vec![ok(1)]),
        ),
        (hi(2), (vec![], vec![ok(2)])),
    ]);
}

#[test]
fn an_agents_refusal_is_logged_as_a_block() {
    echo().table(vec![(
        user("refuse bash"),
        (
            vec![
                entry(1, input("refuse bash")),
                entry(2, tool_call("bash", json!(null))),
                entry(
                    3,
                    decision(
                        "agent",
                        Some(2),
                        Verdict::Block {
                            reason: "not now".into(),
                        },
                    ),
                ),
                entry(4, tool_result(2, failed("not now"))),
            ],
            vec![],
        ),
    )]);
}

#[test]
fn a_call_waits_on_a_dialog_and_resumes_with_its_answer() {
    let dialog = protocol::WaitOn::Dialog {
        ui: Json::encode(&json!({ "ask": "name?" })).unwrap(),
    };
    echo().table(vec![
        (
            user("$ ask"),
            (
                vec![entry(1, input("$ ask")), entry(2, bash("ask"))],
                vec![BUSY, run_bash(2, "ask")],
            ),
        ),
        (
            Event::Waits {
                call: id(2),
                on: dialog.clone(),
            },
            (
                vec![entry(
                    3,
                    Body::Wait {
                        call: id(2),
                        on: dialog,
                    },
                )],
                vec![IDLE],
            ),
        ),
        (
            Event::Answer {
                call: id(2),
                value: Json::encode("ada").unwrap(),
                origin: None,
                ticket: None,
            },
            (
                vec![entry(
                    4,
                    Body::Answer {
                        call: id(2),
                        value: Json::encode("ada").unwrap(),
                        origin: None,
                    },
                )],
                vec![
                    BUSY,
                    Work::Resume {
                        call: id(2),
                        tool: "bash".into(),
                        input: Json::encode(&json!({ "command": "ask" })).unwrap(),
                        value: Json::encode("ada").unwrap(),
                    },
                ],
            ),
        ),
    ]);
}

fn outgoing(envelope: &protocol::Envelope) -> protocol::Outgoing {
    protocol::Outgoing {
        id: envelope.id.clone(),
        recipients: vec![envelope.destination],
        payload: match &envelope.payload {
            protocol::Carried::Text(text) => text.clone(),
            other => panic!("{other:?}"),
        },
    }
}

/// An envelope from the child to this session.
fn envelope(purpose: &str) -> protocol::Envelope {
    protocol::Envelope {
        id: protocol::EnvelopeId {
            sender: child(),
            call: id(2),
            purpose: purpose.into(),
        },
        destination: SessionId::NIL,
        payload: purpose.into(),
        held: Vec::new(),
    }
}

#[test]
fn inbox_order_and_consumption_survive_duplicate_delivery_and_replay() {
    let mut s = echo();
    s.step(user("$ hold"));
    for purpose in ["one", "two", "one"] {
        s.step(Event::Enqueue {
            envelope: envelope(purpose),
            ticket: Ticket(1),
        });
    }
    assert_eq!(s.core.ledger().inbox_len(), 2);
    assert!(s.log.iter().all(|e| !matches!(e.body, Body::Take { .. })));
    s.step(tool_done(2, done(json!("finished"))));
    let taken: Vec<_> = s
        .log
        .iter()
        .filter_map(|e| match e.body {
            Body::Take { input, .. } => Some(input),
            _ => None,
        })
        .collect();
    let queued: Vec<_> = s
        .log
        .iter()
        .filter_map(|e| matches!(e.body, Body::Enqueue { .. }).then_some(e.seq))
        .collect();
    assert_eq!(taken, queued);
    let (mut restored, _) = Session::resume(Reg::echo().arc(), s.log, &Load::default());
    restored.step(Event::Enqueue {
        envelope: envelope("one"),
        ticket: Ticket(2),
    });
    assert_eq!(restored.core.ledger().inbox_len(), 0);
    assert_eq!(
        restored
            .log
            .iter()
            .filter(|e| matches!(e.body, Body::Take { .. }))
            .count(),
        2
    );
}

#[test]
fn a_full_inbox_refuses_new_identity_but_acknowledges_duplicates() {
    let mut s = echo();
    s.step(user("$ hold"));
    for index in 0..protocol::INBOX_CAPACITY {
        s.step(Event::Enqueue {
            envelope: envelope(&index.to_string()),
            ticket: Ticket(1),
        });
    }
    let fx = s.step(Event::Enqueue {
        envelope: envelope("overflow"),
        ticket: Ticket(2),
    });
    assert!(work(&fx).any(|e| matches!(
        e,
        Work::Delivery {
            result: Err(protocol::InboxRefusal::Full),
            ..
        }
    )));
    let fx = s.step(Event::Enqueue {
        envelope: envelope("0"),
        ticket: Ticket(3),
    });
    assert!(work(&fx).any(|e| matches!(e, Work::Delivery { result: Ok(()), .. })));
    assert_eq!(s.core.ledger().inbox_len(), protocol::INBOX_CAPACITY);
}

#[test]
fn an_outbox_replays_the_original_envelope_until_its_durable_ack() {
    let mut s = echo();
    s.step(user("$ hold"));
    let original = envelope("first");
    s.step(Event::SendEnvelope {
        outgoing: outgoing(&original),
        ticket: Ticket(1),
    });
    let (mut restored, fx) = Session::resume(Reg::echo().arc(), s.log.clone(), &Load::default());
    assert!(
        work(&fx).any(|e| matches!(e, Work::Deliver { envelope, .. } if envelope == &original))
    );
    let mut changed = original.clone();
    changed.payload = "different".into();
    let fx = restored.step(Event::SendEnvelope {
        outgoing: outgoing(&changed),
        ticket: Ticket(2),
    });
    assert!(
        work(&fx).any(|e| matches!(e, Work::Deliver { envelope, .. } if envelope == &original))
    );
    restored.step(Event::Delivered {
        id: original.id.clone(),
        result: Ok(()),
        ticket: Some(Ticket(2)),
        recipient: SessionId::NIL,
    });
    let (mut again, fx) = Session::resume(Reg::echo().arc(), restored.log, &Load::default());
    assert!(!work(&fx).any(|e| matches!(e, Work::Deliver { .. })));
    let fx = again.step(Event::SendEnvelope {
        outgoing: outgoing(&original),
        ticket: Ticket(3),
    });
    assert!(!work(&fx).any(|e| matches!(e, Work::Deliver { .. })));
    assert_eq!(
        again
            .log
            .iter()
            .filter(|e| matches!(e.body, Body::Outbox { .. }))
            .count(),
        1
    );
    assert_eq!(
        again
            .log
            .iter()
            .filter(|e| matches!(e.body, Body::Ack { .. }))
            .count(),
        1
    );
}

#[test]
fn cancel_wins_once_over_a_detached_result_even_after_reload() {
    let mut s = echo();
    s.step(user("task go"));
    s.step(spawn(false));
    s.step(tool_done(2, done(json!(null))));
    let fx = s.step(Event::Interrupt {
        origin: None,
        ticket: None,
    });
    assert!(work(&fx).any(|e| matches!(e, Work::Interrupt { child: c } if *c == child())));
    s.step(Event::ChildSettled {
        child: child(),
        outcome: done(json!("late")),
    });
    let (mut restored, fx) = Session::resume(Reg::echo().arc(), s.log, &Load::default());
    assert!(!work(&fx).any(|e| matches!(e, Work::StartChild { .. })));
    restored.step(Event::ChildSettled {
        child: child(),
        outcome: done(json!("later")),
    });
    let results: Vec<_> = restored
        .log
        .iter()
        .filter_map(|e| match &e.body {
            Body::Fired { outcome, .. } => Some(outcome),
            _ => None,
        })
        .collect();
    assert_eq!(results, [&Outcome::Interrupted { output: None }]);
}

#[test]
fn compaction_never_removes_owned_watches() {
    let mut s = echo();
    s.step(user("task go"));
    s.step(spawn(false));
    s.step(Event::Note {
        call: id(2),
        note: Note::Edit {
            target: seq(1),
            upto: Some(seq(4)),
            replace: None,
        },
    });
    assert_eq!(
        s.core.ledger().armed().collect::<Vec<_>>(),
        [(seq(4), child())]
    );
}

#[test]
fn detached_results_and_messages_enter_before_the_next_provider_step() {
    let mut s = modeled();
    s.step(user("work"));
    s.step(model_done(2, &[("task", json!({"prompt":"go"}))]));
    let call = s
        .log
        .iter()
        .find_map(|e| matches!(e.body, Body::ToolCall { .. }).then_some(CallId(e.seq)))
        .unwrap();
    s.step(Event::Spawn {
        call,
        child: child(),
        input: "go".into(),
        intent: Json::null(),
        wait: false,
    });
    s.step(Event::ChildSettled {
        child: child(),
        outcome: done(json!("result")),
    });
    s.step(Event::Enqueue {
        envelope: envelope("update"),
        ticket: Ticket(1),
    });
    // Both are queued at once, and taken up only at the boundary.
    assert!(s.log.iter().all(|e| !matches!(e.body, Body::Take { .. })));
    s.step(Event::ToolDone {
        call,
        outcome: done(json!("handle")).into(),
        card: None,
        held: Vec::new(),
    });
    let request = s
        .log
        .iter()
        .rposition(|e| matches!(e.body, Body::ModelCall { .. }))
        .unwrap();
    let fired = s
        .log
        .iter()
        .position(|e| matches!(e.body, Body::Fired { .. }))
        .unwrap();
    let taken = s
        .log
        .iter()
        .position(|e| matches!(e.body, Body::Take { .. }))
        .unwrap();
    assert!(fired < request && taken < request);
    assert_eq!(
        s.log
            .iter()
            .filter(|e| matches!(e.body, Body::ModelCall { .. }))
            .count(),
        2
    );
}

#[test]
fn an_unacknowledged_outbox_keeps_its_owner_recoverable() {
    let mut s = echo();
    s.step(user("$ hold"));
    let message = envelope("pending");
    s.step(Event::SendEnvelope {
        outgoing: outgoing(&message),
        ticket: Ticket(1),
    });
    s.step(tool_done(2, done(json!("finished"))));
    assert_eq!(s.core.ledger().activity(), protocol::Activity::Running);
    assert!(!s.core.at_rest());
    let (mut restored, effects) = Session::resume(Reg::echo().arc(), s.log, &Load::default());
    assert!(
        work(&effects).any(|e| matches!(e, Work::Deliver { envelope, .. } if envelope == &message))
    );
    restored.step(Event::Delivered {
        id: message.id,
        result: Ok(()),
        ticket: None,
        recipient: SessionId::NIL,
    });
    assert_eq!(restored.core.ledger().activity(), protocol::Activity::Idle);
    assert!(restored.core.at_rest());
}

#[test]
fn child_and_inbox_recovery_keep_their_original_identity() {
    let mut session = echo();
    session.step(user("task go"));
    let child = child();
    session.step(Event::Spawn {
        call: id(2),
        child,
        input: "original child input".into(),
        intent: Json::null(),
        wait: true,
    });
    let message = envelope("identity");
    session.step(Event::Enqueue {
        envelope: message.clone(),
        ticket: Ticket(1),
    });
    session.step(Event::SendEnvelope {
        outgoing: outgoing(&message),
        ticket: Ticket(2),
    });
    let link = session
        .core
        .ledger()
        .links()
        .iter()
        .find(|link| link.child == child)
        .unwrap()
        .seq;
    let (mut recovered, effects) =
        Session::resume(Reg::echo().arc(), session.log, &Load::default());
    assert!(work(&effects).any(|effect| matches!(effect, Work::StartChild { link: recovered, child: id, input } if *recovered == link && *id == child && input == "original child input")));
    assert!(
        work(&effects)
            .any(|effect| matches!(effect, Work::Deliver { envelope, .. } if envelope == &message))
    );
    recovered.step(Event::Enqueue {
        envelope: message.clone(),
        ticket: Ticket(3),
    });
    assert_eq!(
        recovered
            .log
            .iter()
            .filter(
                |entry| matches!(&entry.body, Body::Enqueue { envelope } if envelope == &message)
            )
            .count(),
        1
    );
    assert_eq!(recovered.core.ledger().links().len(), 1);
    assert_eq!(recovered.core.ledger().links()[0].seq, link);
}
