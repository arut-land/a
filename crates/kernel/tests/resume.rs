//! Recovery: every phase a crash can leave a call in, picked up by the one
//! recovery path, [`kernel::Core::resume`].

mod common;

use std::time::Duration;

use common::*;
use kernel::{Load, Owner};
use protocol::{Body, CallId, Json, Outcome, Permission, Seq, SessionId, Verdict};
use serde_json::json;

/// A session the daemon was running when it stopped.
const BUSY_LOAD: Load<'static> = Load {
    parent: None,
    fresh: true,
    owner: Owner::None,
    abandoned: &[],
    busy: true,
    created: false,
    session: None,
    adopted: &[],
    reheld: &[],
};

#[test]
fn a_replayed_user_input_cannot_reactivate_cancelled_child_ownership() {
    let parent = SessionId::new();
    let child = SessionId::new();
    let author = protocol::Author {
        client: protocol::ClientId::new(),
        seq: protocol::ClientSeq::new(1).unwrap(),
    };
    let (mut session, _) = resume(
        Reg::echo(),
        [
            Body::Input {
                segments: "old input".into(),
                mode: protocol::Mode::Queue,
                taken: true,
                origin: Some(author),
                force: false,
                resume: None,
            },
            Body::Assistant {
                text: "old input".into(),
            },
            Body::INTERRUPT,
        ],
        &Load {
            session: Some(child),
            parent: Some(parent),
            owner: Owner::Cancelled,
            ..Load::default()
        },
    );
    let replay = session.step(kernel::Event::Input {
        segments: "old input".into(),
        mode: protocol::Mode::Queue,
        force: false,
        origin: Some(author),
        ticket: None,
    });
    assert_eq!(replay.appends, [] as [protocol::Entry; 0]);
    let delivery = session.step(kernel::Event::Enqueue {
        envelope: protocol::Envelope::new(
            protocol::EnvelopeId {
                sender: parent,
                call: CallId::new(1).unwrap(),
                purpose: "chat".into(),
            },
            child,
            protocol::Carried::Message("still cancelled".into()),
            Vec::new(),
        )
        .unwrap(),
        ticket: kernel::Ticket(1),
    });
    assert!(work(&delivery).any(|work| matches!(
        work,
        Work::Delivery {
            result: Err(protocol::InboxRefusal::Unavailable),
            ..
        }
    )));
}

#[test]
fn delayed_previous_outcomes_cannot_settle_a_resumed_runs_watch() {
    let child = SessionId::new();
    let notice = protocol::Envelope::new(
        protocol::EnvelopeId {
            sender: child,
            call: CallId::new(9).unwrap(),
            purpose: protocol::EnvelopeId::RESUMED.into(),
        },
        SessionId::new(),
        protocol::Carried::Resumed,
        Vec::new(),
    )
    .unwrap();
    let (mut s, _) = resume(
        Reg::echo(),
        [Body::Watch { child }, Body::Enqueue { envelope: notice }],
        &Load::default(),
    );
    for text in ["previous", "resumed"] {
        s.step(kernel::Event::ChildSettled {
            run: if text == "previous" {
                protocol::RunId::Initial
            } else {
                protocol::RunId::Resumed(Seq::new(9).unwrap())
            },
            child,
            outcome: Outcome::Done {
                output: Json::encode(&text).unwrap(),
            }
            .into(),
        });
    }
    let outcomes: Vec<_> = s
        .log
        .iter()
        .filter_map(|entry| match &entry.body {
            Body::Fired { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        outcomes,
        [
            Outcome::Done {
                output: Json::encode(&"previous").unwrap()
            }
            .into(),
            Outcome::Done {
                output: Json::encode(&"resumed").unwrap()
            }
            .into()
        ]
    );
}

fn pending_resume() -> (Session, protocol::Envelope) {
    let child = SessionId::new();
    let parent = SessionId::new();
    let (mut s, _) = resume(
        Reg::echo(),
        [
            input("original"),
            Body::Assistant {
                text: "original".into(),
            },
        ],
        &Load {
            session: Some(child),
            parent: Some(parent),
            owner: Owner::Present,
            ..Load::default()
        },
    );
    let fx = s.step(user("resumed"));
    let envelope = work(&fx)
        .find_map(|work| match work {
            Work::Deliver { envelope, .. } => Some(envelope.clone()),
            _ => None,
        })
        .unwrap();
    (s, envelope)
}

#[test]
fn a_full_parent_inbox_never_releases_a_pending_resume() {
    let (mut s, envelope) = pending_resume();
    let refused = s.step(kernel::Event::Delivered {
        id: envelope.id().clone(),
        recipient: envelope.destination,
        result: Err(protocol::InboxRefusal::Full),
        ticket: None,
    });
    assert!(
        s.log
            .iter()
            .all(|entry| !matches!(&entry.body, Body::Assistant { text } if text == "resumed"))
    );
    assert!(s.core.ledger().resumption().is_some());
    assert!(work(&refused).any(
        |work| matches!(work, Work::Deliver { envelope: retry, .. } if retry.id() == envelope.id())
    ));
    assert!(s.core.ledger().acknowledged(envelope.id()).is_none());
    let load = Load {
        session: Some(envelope.id().sender),
        parent: Some(envelope.destination),
        owner: Owner::Present,
        ..Load::default()
    };
    let (mut restarted, retry) = Session::resume(Reg::echo().arc(), s.log, &load);
    assert!(work(&retry).any(
        |work| matches!(work, Work::Deliver { envelope: retry, .. } if retry.id() == envelope.id())
    ));
    restarted.step(kernel::Event::Delivered {
        id: envelope.id().clone(),
        recipient: envelope.destination,
        result: Ok(()),
        ticket: None,
    });
    assert_eq!(
        restarted
            .log
            .iter()
            .filter(|entry| matches!(&entry.body, Body::Assistant { text } if text == "resumed"))
            .count(),
        1
    );
}

#[test]
fn stopping_a_pending_resume_prevents_execution_after_acknowledgement_and_replay() {
    let (mut s, envelope) = pending_resume();
    s.step(user("also queued"));
    s.step(interrupt());
    let pending = s.log.clone();
    let load = Load {
        session: Some(envelope.id().sender),
        parent: Some(envelope.destination),
        owner: Owner::Present,
        ..Load::default()
    };
    let (restarted, _) = Session::resume(Reg::echo().arc(), pending, &load);
    s = restarted;
    let mut view = protocol::view::SessionView::default();
    for entry in &s.log {
        view.fold(&protocol::DaemonMsg::from(entry.clone()));
    }
    assert_eq!(view.state.queued().count(), 0);
    s.step(kernel::Event::Delivered {
        id: envelope.id().clone(),
        recipient: envelope.destination,
        result: Ok(()),
        ticket: None,
    });
    assert!(
        s.log
            .iter()
            .all(|entry| !matches!(&entry.body, Body::Assistant { text } if text == "resumed"))
    );
    let (replayed, _) = Session::resume(Reg::echo().arc(), s.log, &Load::default());
    assert_eq!(replayed.core.ledger().untaken(), []);
}

#[test]
fn an_unavailable_parent_settles_the_resume_without_running_queued_work() {
    let (mut s, envelope) = pending_resume();
    let before = s.log.len();
    let fx = s.step(kernel::Event::Delivered {
        id: envelope.id().clone(),
        recipient: envelope.destination,
        result: Err(protocol::InboxRefusal::Unavailable),
        ticket: None,
    });
    assert_eq!(s.core.ledger().untaken(), []);
    assert!(s.core.ledger().resumption().is_none());
    assert!(matches!(
        s.core.ledger().result(),
        Some(Outcome::Interrupted { .. })
    ));
    assert!(work(&fx).all(|work| !matches!(work, Work::RunTool { .. } | Work::Send { .. })));
    for cut in before..=s.log.len() {
        let load = Load {
            session: Some(envelope.id().sender),
            parent: Some(envelope.destination),
            owner: Owner::Present,
            ..Load::default()
        };
        let (mut replayed, _) = Session::resume(Reg::echo().arc(), s.log[..cut].to_vec(), &load);
        replayed.step(kernel::Event::Delivered {
            id: envelope.id().clone(),
            recipient: envelope.destination,
            result: Err(protocol::InboxRefusal::Unavailable),
            ticket: None,
        });
        assert_eq!(replayed.core.ledger().untaken(), []);
        assert!(
            replayed
                .log
                .iter()
                .all(|entry| !matches!(&entry.body, Body::Assistant { text } if text == "resumed"))
        );
    }
}

#[test]
fn withdrawing_an_unexecuted_resume_reports_cancellation_for_that_run_after_replay() {
    let (mut s, envelope) = pending_resume();
    let input = envelope.id().call.0;
    s.step(kernel::Event::Withdraw {
        input,
        origin: None,
        ticket: None,
    });
    let load = Load {
        session: Some(envelope.id().sender),
        parent: Some(envelope.destination),
        owner: Owner::Present,
        ..Load::default()
    };
    let (mut restarted, _) = Session::resume(Reg::echo().arc(), s.log, &load);
    let fx = restarted.step(kernel::Event::Delivered {
        id: envelope.id().clone(),
        recipient: envelope.destination,
        result: Ok(()),
        ticket: None,
    });
    assert!(work(&fx).any(|work| matches!(work, Work::Report { run: protocol::RunId::Resumed(run), outcome: Outcome::Interrupted { .. } } if *run == input)), "{fx:?}");
    assert!(
        !restarted
            .log
            .iter()
            .any(|entry| matches!(&entry.body, Body::Assistant { text } if text == "resumed"))
    );
}

fn resume(
    reg: Reg,
    bodies: impl IntoIterator<Item = Body>,
    load: &Load<'_>,
) -> (Session, StepPlan) {
    let (s, fx) = Session::resume(reg.arc(), log(bodies), load);
    (s, fx)
}

fn progress(call: u64, text: &str) -> Body {
    Body::Progress {
        call: CallId::new(call).unwrap(),
        text: text.into(),
    }
}

/// A daemon that keeps dying during a model call: each restart counts the
/// attempt it cut off, and past [`kernel::RESTART_CAP`] the call fails,
/// whatever the `request_error` point would say.
#[test]
fn a_crash_loop_cannot_retry_forever() {
    let reg = || Reg::modeled().forever().arc();
    let mut log = log([input("hi"), model_call(0)]);
    for _ in 0..kernel::RESTART_CAP {
        let (s, fx) = Session::resume(reg(), log, &BUSY_LOAD);
        assert!(
            matches!(work(&fx).last(), Some(Work::Sleep { .. })),
            "{fx:?}"
        );
        log = s.log;
    }
    let (s, fx) = Session::resume(reg(), log, &BUSY_LOAD);
    assert!(!work(&fx).any(|e| matches!(e, Work::Sleep { .. } | Work::Prepare { .. })));
    let last = &s.log.last().unwrap().body;
    assert!(
        matches!(last, Body::ModelResult { outcome: Outcome::Failed { error }, .. } if error.contains("gave up after 10 attempts")),
        "{last:?}"
    );
}

/// A call the agent refused is answered so after a crash cut it off
/// before its answer committed: never run, whatever of it committed.
#[test]
fn a_refused_call_is_never_run_after_a_crash() {
    let whole = {
        let mut s = Session::new(Reg::echo().arc());
        s.step(user("refuse bash"));
        s.log.clone()
    };
    let refused = |e: &&protocol::Entry| matches!(&e.body, Body::ToolCall { .. });
    let at = whole.iter().position(|e| refused(&e)).unwrap();
    for cut in at + 1..whole.len() {
        let (s, fx) = Session::resume(Reg::echo().arc(), whole[..cut].to_vec(), &Load::default());
        assert!(
            !work(&fx).any(|e| matches!(e, Work::RunTool { .. })),
            "cut after {cut}: {fx:?}"
        );
        let failed = Outcome::Failed {
            error: "not now".into(),
        };
        assert_eq!(s.log.last().map(|e| &e.body), Some(&tool_result(2, failed)));
        let blocks = (s.log.iter())
            .filter(|e| {
                matches!(
                    e.body,
                    Body::Decision {
                        d: Verdict::Block { .. },
                        ..
                    }
                )
            })
            .count();
        assert_eq!(blocks, 1, "cut after {cut}: {:#?}", s.log);
    }
}

/// A safe call whose gate a crash cut off is gated and run again; an
/// interrupt before it runs stops it.
#[test]
fn a_safe_call_cut_off_runs_again_and_an_interrupt_before_it_stops_it() {
    let (mut s, fx) = resume(
        Reg::echo(),
        [input("sleep 1"), sleep_call(1)],
        &Load::default(),
    );
    assert_plan(
        &fx,
        (
            vec![],
            vec![BUSY, run_tool(2, "sleep", json!({ "seconds": 1 }))],
        ),
    );
    s.table(vec![
        (interrupt(), (vec![], vec![cancel(2)])),
        (
            tool_done(2, INTERRUPTED),
            (
                vec![
                    entry(3, tool_result(2, INTERRUPTED)),
                    entry(4, Body::INTERRUPT),
                ],
                vec![IDLE],
            ),
        ),
    ]);
}

#[test]
fn an_unsafe_call_cut_off_reports_the_output_it_committed() {
    let bodies = [
        input("$ make"),
        bash("make"),
        progress(2, "one\n"),
        progress(2, "two\n"),
    ];
    let (_, fx) = resume(Reg::echo(), bodies, &BUSY_LOAD);
    let cut = Outcome::Interrupted {
        output: Some("one\ntwo\n".into()),
    };
    assert_plan(&fx, (vec![entry(5, tool_result(2, cut))], vec![IDLE]));
}

/// An unsafe call gated before the crash may have started: it is not run
/// twice.
#[test]
fn an_unsafe_call_gated_before_a_crash_is_never_run_twice() {
    let (_, fx) = resume(Reg::echo(), [input("$ rm"), bash("rm")], &BUSY_LOAD);
    assert_plan(
        &fx,
        (vec![entry(3, tool_result(2, INTERRUPTED))], vec![IDLE]),
    );
}

/// The gate asked again decides as it did: what it logged before the
/// crash is not logged again.
#[test]
fn a_gate_cut_off_by_a_crash_logs_its_decisions_once() {
    let rewritten = Json::encode(&json!({ "rewritten": true })).unwrap();
    let pre = decision("policy", Some(2), Verdict::PreTool { input: rewritten });
    let reg = Reg::echo().policy(Policy::RewriteThenDeny);
    let (_, fx) = resume(reg, [input("$ echo hi"), bash("echo hi"), pre], &BUSY_LOAD);
    assert_plan(
        &fx,
        (
            vec![
                entry(4, permission(2, Permission::Deny)),
                entry(5, tool_result(2, failed("policy denied this call"))),
            ],
            vec![IDLE],
        ),
    );
}

/// A wait is in the log, not in memory: it outlives the daemon.
#[test]
fn a_wait_survives_a_restart_and_runs_once_answered() {
    let reg = Reg::echo().policy(Policy::Ask(&["bash"]));
    let bodies = [
        input("$ echo"),
        bash("echo"),
        permission(
            2,
            Permission::Ask(protocol::ApprovalReason::Mode { mode: "ask".into() }),
        ),
    ];
    let (mut s, fx) = resume(reg, bodies, &BUSY_LOAD);
    assert_plan(&fx, (vec![], vec![IDLE]));
    let answered = Body::Answer {
        call: id(2),
        value: Json::encode(&protocol::QuestionAnswer::confirm(
            Seq::new(3).unwrap(),
            protocol::Response::accept(),
        ))
        .unwrap(),
        origin: None,
    };
    s.table(vec![(
        approve(2, true),
        (vec![entry(4, answered)], vec![BUSY, run_bash(2, "echo")]),
    )]);
}

/// A model call a restart cut off: the attempt it was on failed, and the
/// `request_error` point retries it as it would any failure.
#[test]
fn a_model_call_cut_off_is_retried_as_a_failed_attempt() {
    let reg = Reg::modeled().retry(Duration::ZERO);
    let (_, fx) = resume(reg, [input("hi"), model_call(0)], &BUSY_LOAD);
    let retry = Verdict::Retry {
        attempt: 1,
        error: "the daemon stopped during the call".into(),
    };
    assert_plan(
        &fx,
        (
            vec![entry(3, decision("retry", Some(2), retry))],
            vec![Work::Sleep {
                call: id(2),
                after: Duration::ZERO,
            }],
        ),
    );
    let (_, fx) = resume(Reg::modeled(), [input("hi"), model_call(0)], &BUSY_LOAD);
    let cut = failed("the daemon stopped during the call");
    assert_plan(&fx, (vec![entry(3, model_result(2, cut))], vec![IDLE]));
}

/// Restarts count as attempts, in the log: a session that crashes during
/// every attempt fails the call once the retries run out.
#[test]
fn a_crash_loop_fails_once_the_retries_run_out() {
    let reg = || Reg::modeled().retry(Duration::ZERO).arc();
    let mut log = log([input("hi"), model_call(0)]);
    for _ in 0..RETRIES {
        let (s, fx) = Session::resume(reg(), log, &BUSY_LOAD);
        assert!(
            matches!(work(&fx).last(), Some(Work::Sleep { .. })),
            "{fx:?}"
        );
        log = s.log;
    }
    let (_, fx) = Session::resume(reg(), log.clone(), &BUSY_LOAD);
    let seq = log.len() as u64 + 1;
    let cut = failed("the daemon stopped during the call");
    assert_plan(&fx, (vec![entry(seq, model_result(2, cut))], vec![IDLE]));
}

/// What the agent asked for that the log does not show is carried out.
#[test]
fn what_the_agent_asked_for_and_the_log_does_not_show_is_carried_out() {
    let (_, fx) = resume(Reg::echo(), [input("hi")], &Load::default());
    assert_plan(&fx, (vec![entry(2, said("hi"))], vec![]));
}

/// Input still queued is taken up once, in order, before input that
/// arrives after the load.
#[test]
fn input_queued_before_a_restart_runs_once_in_order() {
    let bodies = [input("one"), said("one"), queued("left")];
    let (mut s, fx) = resume(Reg::echo(), bodies, &BUSY_LOAD);
    assert_plan(
        &fx,
        (vec![entry(4, take(3)), entry(5, said("left"))], vec![IDLE]),
    );
    s.table(vec![(
        user("two"),
        (vec![entry(6, input("two")), entry(7, said("two"))], vec![]),
    )]);
}

#[test]
fn an_orphan_does_not_resume_what_it_was_doing() {
    let load = Load {
        owner: Owner::Gone,
        ..BUSY_LOAD
    };
    let (_, fx) = resume(Reg::echo(), [input("sleep 1"), sleep_call(1)], &load);
    assert_plan(
        &fx,
        (
            vec![
                entry(3, tool_result(2, INTERRUPTED)),
                entry(4, Body::INTERRUPT),
            ],
            vec![IDLE],
        ),
    );
}

/// Recorded process ids grant no lease. A fresh load ends their streams;
/// it never attempts to signal processes recovered from a log.
#[test]
fn replayed_process_ids_never_confer_process_ownership() {
    let end_job = |pgid| Work::EndStream {
        stream: protocol::OutputStream {
            session: SessionId::NIL,
            call: CallId::new(2).unwrap(),
        },
        cut: kernel::CutOff::Job { pgid },
    };
    let process = |pgid, boot: &str, background| Body::ToolProcess {
        call: CallId::new(2).unwrap(),
        pgid,
        boot: boot.into(),
        background,
    };
    let bodies = || {
        [
            input("$ make"),
            bash("make"),
            process(7, "boot-1", false),
            process(8, "boot-1", true),
            process(9, "boot-0", false),
        ]
    };
    let (_, fx) = resume(Reg::echo(), bodies(), &BUSY_LOAD);
    assert_plan(
        &fx,
        (vec![entry(6, tool_result(2, INTERRUPTED))], vec![IDLE]),
    );
    // A background group outlives its call, until the session ends.
    let ended = [
        input("$ serve &"),
        bash("serve &"),
        process(8, "boot-1", true),
        tool_result(2, done(json!(0))),
    ];
    let (_, fx) = resume(Reg::echo(), ended.clone(), &BUSY_LOAD);
    // Busy still, until what it wrote is back.
    assert_plan(&fx, (vec![], vec![end_job(8)]));
    let again = Load {
        fresh: false,
        ..BUSY_LOAD
    };
    let (_, fx) = resume(Reg::echo(), ended, &again);
    assert_plan(&fx, (vec![], vec![IDLE]));
}

/// Links are re-armed: a link not yet watched is watched, and each child
/// still watched is started again, to run to its end and report.
#[test]
fn links_are_rearmed() {
    let link = || Body::Link {
        call: CallId::new(2).unwrap(),
        intent: Json::null(),
        child: child(),
        input: "go".into(),
    };
    let start = || Work::StartChild {
        link: seq(1),
        child: child(),
        input: "go".into(),
    };
    let (_, fx) = resume(Reg::echo(), [link()], &Load::default());
    assert_plan(
        &fx,
        (
            vec![entry(2, Body::Watch { child: child() })],
            vec![BUSY, start()],
        ),
    );
    let watch = Body::Watch { child: child() };
    let (_, fx) = resume(Reg::echo(), [link(), watch.clone()], &Load::default());
    assert_plan(&fx, (vec![], vec![BUSY, start()]));
    let fired = Body::Fired {
        watch: Seq::new(2).unwrap(),
        outcome: done(json!("ok")).into(),
    };
    let queued = Body::Enqueue {
        envelope: protocol::Envelope::own(
            SessionId::NIL,
            id(2),
            protocol::Carried::Child(done(json!("ok")).into()),
        ),
    };
    let (_, fx) = resume(
        Reg::echo(),
        [link(), watch, fired, queued],
        &Load::default(),
    );
    // The agent is yet to take the child's result up; the child is not
    // started.
    assert_plan(
        &fx,
        (
            vec![entry(5, take(4)), entry(6, said("child: \"ok\""))],
            vec![],
        ),
    );
}

/// A call waiting on a child whose result came in before the crash goes on
/// with it.
#[test]
fn a_call_whose_child_settled_before_a_crash_resumes_with_its_result() {
    let task = json!({ "input": "go" });
    let ok = done(json!("ok"));
    let bodies = [
        input("task go"),
        tool_call("task", task.clone()),
        Body::Link {
            call: CallId::new(2).unwrap(),
            intent: Json::null(),
            child: child(),
            input: "go".into(),
        },
        wait_child(2, 3),
        Body::Watch { child: child() },
        Body::Fired {
            watch: Seq::new(5).unwrap(),
            outcome: ok.clone().into(),
        },
    ];
    let (_, fx) = resume(Reg::echo(), bodies, &BUSY_LOAD);
    assert_plan(
        &fx,
        (
            vec![],
            vec![Work::Resume {
                call: id(2),
                tool: "task".into(),
                input: Json::encode(&task).unwrap(),
                value: Json::encode(&ok).unwrap(),
            }],
        ),
    );
}

#[test]
fn a_command_cut_off_is_not_run_again() {
    let command = Body::ToolCall {
        invoke: None,
        tool: "jobs".into(),
        input: Json::encode(&json!({ "args": "" })).unwrap(),
        id: None,
        card: None,
        by: protocol::Audience::User,
    };
    let (_, fx) = resume(Reg::echo(), [input("/jobs"), command], &BUSY_LOAD);
    assert_plan(
        &fx,
        (vec![entry(3, tool_result(2, INTERRUPTED))], vec![IDLE]),
    );
}

/// A failed commit is a resume from what committed: the intent that did
/// commit is carried on, none is made twice.
#[test]
fn a_failed_commit_is_resume() {
    let mut s = Session::new(Reg::echo().arc());
    s.step(user("sleep 1"));
    let committed = s.log.clone();
    // The result's commit fails: the driver resumes from what committed.
    s.step(tool_done(2, done(json!({ "slept": 1 }))));
    let (mut s, fx) = Session::resume(Reg::echo().arc(), committed, &BUSY_LOAD);
    assert_plan(
        &fx,
        (vec![], vec![run_tool(2, "sleep", json!({ "seconds": 1 }))]),
    );
    s.step(tool_done(2, done(json!({ "slept": 1 }))));
    let calls = s
        .bodies()
        .into_iter()
        .filter(|b| matches!(b, Body::ToolCall { .. }))
        .count();
    assert_eq!(calls, 1, "{:#?}", s.log);
    assert_eq!(*s.core.turn(), kernel::Turn::Idle);
}

#[test]
fn abandoned_inbox_work_is_terminal_for_every_owned_conversation() {
    let envelope = protocol::Envelope::new(
        protocol::EnvelopeId {
            sender: SessionId::new(),
            call: CallId::new(1).unwrap(),
            purpose: "task".into(),
        },
        SessionId::new(),
        "hi".into(),
        Vec::new(),
    )
    .unwrap();
    for taken in [false, true] {
        let mut bodies = vec![Body::Enqueue {
            envelope: envelope.clone(),
        }];
        if taken {
            bodies.push(Body::Take {
                input: Seq::new(1).unwrap(),
                into: None,
            });
        }
        let entries = log(bodies);
        let fact = log(vec![Body::ModelResult {
            call: envelope.id().call,
            outcome: Outcome::Interrupted { output: None },
            r#for: None,
            failure: None,
        }]);
        let cancelled = kernel::CancelledInput::interrupted(&entries[0], &fact[0]).unwrap();
        let load = Load {
            owner: Owner::Present,
            abandoned: &[cancelled],
            ..Load::default()
        };
        let (session, fx) = Session::resume(Reg::modeled().arc(), entries, &load);
        assert!(!work(&fx).any(|w| matches!(w, Work::Prepare { .. } | Work::Send { .. })));
        assert!(
            session
                .log
                .iter()
                .any(|e| matches!(e.body, Body::Interrupt { .. }))
        );
        let (mut session, fx) = Session::resume(
            Reg::modeled().arc(),
            session.log,
            &Load {
                owner: Owner::Present,
                ..Load::default()
            },
        );
        assert!(!work(&fx).any(|w| matches!(w, Work::Prepare { .. } | Work::Send { .. })));
        let fx = session.step(kernel::Event::Enqueue {
            envelope: envelope.clone(),
            ticket: kernel::Ticket(1),
        });
        assert!(!work(&fx).any(|w| matches!(w, Work::Prepare { .. } | Work::Send { .. })));
    }
}
