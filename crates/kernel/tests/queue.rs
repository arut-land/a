//! The session's one queue: what is taken up where, a forced steer, the
//! calls detached into the background, and what the session sends itself.

mod common;

use common::*;
use kernel::{Boundary, CutOff, Event, Load, Replied, StepPlan, StreamEnd, Take, Ticket};
use protocol::{
    BlobId, BlobOrigin, BlobRef, Body, CallId, EnvelopeId, Held, Outcome, OutputStream, SessionId,
    Source,
};
use serde_json::json;

fn modeled() -> Session {
    Session::new(Reg::modeled().arc())
}

fn echo() -> Session {
    Session::new(Reg::echo().arc())
}

/// The seq of the last entry of the log that `is`.
fn last(s: &Session, is: impl Fn(&Body) -> bool) -> u64 {
    s.log.iter().rev().find(|e| is(&e.body)).unwrap().seq.get()
}

fn takes(s: &Session) -> Vec<(u64, Option<CallId>)> {
    (s.log.iter())
        .filter_map(|e| match e.body {
            Body::Take { input, into } => Some((input.get(), into)),
            _ => None,
        })
        .collect()
}

/// A modeled session that called `sleep` as call 4, which runs.
fn sleeping() -> Session {
    let mut s = modeled();
    s.step(user("work"));
    s.step(prepared(2));
    let fx = s.step(model_done(2, &[("sleep", json!({ "seconds": 9 }))]));
    assert!(work(&fx).any(|w| matches!(w, Work::RunTool { call, .. } if *call == id(4))));
    s
}

#[test]
fn a_forced_steer_cuts_the_model_call_short_and_its_turn_goes_on_from_the_steer() {
    let mut s = modeled();
    s.step(user("work"));
    s.step(prepared(2));
    let fx = s.step(forced("now"));
    assert!(work(&fx).any(|w| *w == Work::Cancel { call: id(2) }));
    assert!(matches!(
        s.log.last().unwrap().body,
        Body::Input { force: true, .. }
    ));
    let steer = s.log.last().unwrap().seq.get();
    let fx = s.step(Event::ModelDone {
        call: id(2),
        replied: Replied::Interrupted(Some("half a rep".into())),
    });
    assert_eq!(takes(&s), [(steer, None)]);
    let asked = last(&s, |b| matches!(b, Body::ModelCall { .. }));
    assert!(asked > last(&s, |b| matches!(b, Body::Take { .. })));
    assert!(work(&fx).any(|w| matches!(w, Work::Prepare { call, .. } if call.get() == asked)));
}

#[test]
fn a_plain_steer_never_cuts_anything_short() {
    let mut s = modeled();
    s.step(user("work"));
    s.step(prepared(2));
    let fx = s.step(steer("later"));
    assert!(!work(&fx).any(|w| matches!(w, Work::Cancel { .. } | Work::Detach { .. })));
}

#[test]
fn a_forced_steer_detaches_only_the_calls_that_can_go_on_in_the_background() {
    let mut s = sleeping();
    let fx = s.step(forced("now"));
    let steer = s.log.last().unwrap().seq.get();
    assert_eq!(
        work(&fx)
            .filter(|w| matches!(w, Work::Detach { .. }))
            .count(),
        1
    );
    assert!(work(&fx).any(|w| *w == Work::Detach { call: id(4) }));
    s.step(Event::Detached { call: id(4) });
    assert!(s.bodies().contains(&&Body::Detached { call: id(4) }));
    assert!(s.bodies().contains(&&placeholder(4)));
    // The reply's calls are answered, so the steer is read before the
    // model is asked again.
    assert_eq!(takes(&s), [(steer, None)]);
    let asked = last(&s, |b| matches!(b, Body::ModelCall { .. }));
    assert!(asked > steer);
    assert_eq!(
        (s.core.ledger().detached().iter())
            .map(|d| d.call)
            .collect::<Vec<_>>(),
        [id(4)]
    );

    // The session works while the call does: no idle exit, no drain.
    assert_eq!(s.core.ledger().activity(), protocol::Activity::Running);
    s.step(model_done(asked, &[]));
    assert!(!s.core.at_rest());

    // Its real outcome comes back as the session's own envelope, hidden.
    s.step(tool_done(4, done(json!("slept"))));
    let back = s
        .log
        .iter()
        .find_map(|e| match &e.body {
            Body::Enqueue { envelope } if envelope.id.purpose == EnvelopeId::DETACHED => {
                Some((e.seq.get(), envelope.source()))
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(back.1, Source::Job(id(4)));
    assert_eq!(s.core.ledger().detached(), []);
    // Idle, the turn's end takes it up.
    assert!(takes(&s).contains(&(back.0, None)));
}

#[test]
fn a_call_detached_twice_or_reporting_twice_comes_back_once() {
    let mut s = sleeping();
    s.step(forced("now"));
    s.step(Event::Detached { call: id(4) });
    let before = s.log.len();
    s.step(Event::Detached { call: id(4) });
    assert_eq!(s.log.len(), before);
    s.step(tool_done(4, done(json!("slept"))));
    s.step(tool_done(4, done(json!("slept again"))));
    let back = (s.bodies().into_iter())
        .filter(|b| matches!(b, Body::Enqueue { .. }))
        .count();
    assert_eq!(back, 1);
}

#[test]
fn an_interrupt_ends_the_calls_in_the_background_and_starts_nothing() {
    let mut s = sleeping();
    s.step(forced("now"));
    s.step(Event::Detached { call: id(4) });
    let asked = last(&s, |b| matches!(b, Body::ModelCall { .. }));
    s.step(model_done(asked, &[]));
    let before = takes(&s);
    let fx = s.step(interrupt());
    assert!(work(&fx).any(|w| *w == Work::Cancel { call: id(4) }));
    // The interrupt's entry ends it: nothing is queued that a torn write
    // could leave behind.
    assert_eq!(s.log.last().unwrap().body, Body::INTERRUPT);
    assert!(!s.bodies().iter().any(|b| matches!(b, Body::Enqueue { .. })));
    assert_eq!(s.core.ledger().detached(), []);
    assert_eq!(s.core.ledger().untaken(), []);
    // What it reports once cancelled changes nothing.
    let after = s.log.len();
    s.step(tool_done(4, Outcome::Interrupted { output: None }));
    assert_eq!(s.log.len(), after);
    assert_eq!(takes(&s), before);
    assert!(s.core.at_rest());
}

#[test]
fn a_call_in_the_background_reporting_while_an_interrupt_stops_the_turn_queues_nothing() {
    let mut s = sleeping();
    s.step(forced("now"));
    s.step(Event::Detached { call: id(4) });
    // The turn's model call runs as the interrupt comes.
    let asked = last(&s, |b| matches!(b, Body::ModelCall { .. }));
    s.step(interrupt());
    s.step(tool_done(4, Outcome::Interrupted { output: None }));
    s.step(Event::ModelDone {
        call: id(asked),
        replied: Replied::Interrupted(None),
    });
    assert!(!s.bodies().iter().any(|b| matches!(b, Body::Enqueue { .. })));
    assert_eq!(s.core.ledger().detached(), []);
    assert!(s.core.at_rest());
}

#[test]
fn a_crash_between_detached_and_its_placeholder_recovers_the_call_once() {
    for (command, call, safe) in [
        ("sleep 9", sleep_call(9), true),
        ("$ make", bash("make"), false),
    ] {
        let torn = log([input(command), call, Body::Detached { call: id(2) }]);
        let (s, fx) = Session::resume(Reg::echo().arc(), torn, &Load::default());
        let placeholder = placeholder(2);
        let results = (s.bodies().into_iter())
            .filter(|b| matches!(b, Body::ToolResult { .. }))
            .collect::<Vec<_>>();
        assert_eq!(results, [&placeholder], "{command}");
        let reruns = work(&fx)
            .filter(|w| matches!(w, Work::RunTool { .. }))
            .count();
        let ended = work(&fx)
            .filter(|w| {
                matches!(
                    w,
                    Work::EndStream {
                        cut: CutOff::Detached,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            (reruns, ended),
            if safe { (1, 0) } else { (0, 1) },
            "{command}"
        );
    }
}

#[test]
fn a_childs_result_never_jumps_ahead_of_the_users_queued_input() {
    let mut s = echo();
    s.step(user("task go"));
    s.step(Event::Spawn {
        call: id(2),
        child: child(),
        input: "go".into(),
        intent: protocol::Json::null(),
        wait: false,
    });
    s.step(Event::ChildSettled {
        child: child(),
        outcome: done(json!("ok")).into(),
    });
    s.step(user("next"));
    s.step(tool_done(2, done(json!(null))));
    let said: Vec<_> = (s.bodies().into_iter())
        .filter_map(|b| match b {
            Body::Assistant { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(said, ["next", "child: \"ok\""]);
}

/// A blob, as an ended stream's bytes are sealed into.
fn sealed(call: u64) -> BlobRef {
    BlobRef {
        id: BlobId([7; 32]),
        size: 6,
        media_type: "text/plain; charset=utf-8".into(),
        origin: BlobOrigin::ToolOutput(id(call)),
    }
}

/// Each envelope the session queued itself: what it carries, and holds.
fn queued(s: &Session) -> Vec<(protocol::Carried, Vec<Held>)> {
    (s.bodies().into_iter())
        .filter_map(|b| match b {
            Body::Enqueue { envelope } => Some((envelope.payload.clone(), envelope.held.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn a_restart_runs_a_safe_detached_call_again_and_reports_an_unsafe_one_cut_off() {
    let detached = |command: &str, call: Body| {
        log([
            input(command),
            call,
            Body::Detached { call: id(2) },
            placeholder(2),
        ])
    };
    let (s, fx) = Session::resume(
        Reg::echo().arc(),
        detached("sleep 9", sleep_call(9)),
        &Load::default(),
    );
    assert!(work(&fx).any(|w| *w == run_tool(2, "sleep", json!({ "seconds": 9 }))));
    assert_ne!(s.core.ledger().detached(), []);

    let (mut s, fx) = Session::resume(
        Reg::echo().arc(),
        detached("$ make", bash("make")),
        &Load::default(),
    );
    assert!(!work(&fx).any(|w| matches!(w, Work::RunTool { .. })));
    let cut = CutOff::Detached;
    assert!(work(&fx).any(|w| *w
        == Work::EndStream {
            stream: STREAM,
            cut
        }));
    assert_eq!(queued(&s), [], "its outcome waits for what it wrote");
    let fx = s.step(Event::StreamEnded {
        stream: STREAM,
        cut,
        end: StreamEnd::Copied(sealed(2)),
    });
    assert_eq!(s.core.ledger().detached(), []);
    let [
        (
            protocol::Carried::Detached(Outcome::Interrupted {
                output: Some(output),
            }),
            held,
        ),
    ] = &queued(&s)[..]
    else {
        panic!("{:?}", queued(&s));
    };
    assert!(output.contains("may not have completed"), "{output}");
    assert_eq!(*held, [Held::Blob(sealed(2))], "its output so far");
    assert_eq!(discarded_once_logged(&fx), [STREAM]);
}

/// Call 2's stream, as a session with no id names it.
const STREAM: OutputStream = OutputStream {
    session: SessionId::NIL,
    call: CallId::new(2).unwrap(),
};

/// The streams `fx` removes, each only once what it held is durable.
fn discarded_once_logged(fx: &StepPlan) -> Vec<OutputStream> {
    assert!(
        !fx.immediate
            .iter()
            .any(|w| matches!(w, Work::DiscardStream { .. })),
        "{fx:?}"
    );
    (fx.admitted.iter())
        .filter_map(|guarded| match guarded.work {
            Work::DiscardStream { stream } => {
                assert_eq!(guarded.admission.requirement, ext::Requirement::Durable);
                Some(stream)
            }
            _ => None,
        })
        .collect()
}

/// A stream copied at recovery is held as what it held: its blob, or, if
/// it could not be read, the stream as it is, left in place; one empty or
/// gone is held by nothing.
#[test]
fn a_cut_off_calls_outcome_holds_its_stream_as_it_was_copied() {
    for (end, held, discarded) in [
        (StreamEnd::Unreadable, vec![Held::Stream(STREAM)], vec![]),
        (StreamEnd::Empty, vec![], vec![STREAM]),
        (StreamEnd::Missing, vec![], vec![]),
    ] {
        let (mut s, _) = Session::resume(Reg::echo().arc(), cut_off(), &Load::default());
        let cut = CutOff::Detached;
        let fx = s.step(Event::StreamEnded {
            stream: STREAM,
            cut,
            end,
        });
        let [(protocol::Carried::Detached(_), queued)] = &queued(&s)[..] else {
            panic!("{:?}", queued(&s));
        };
        assert_eq!(*queued, held);
        assert_eq!(discarded_once_logged(&fx), discarded);
    }
}

/// A log a crash cut off with call 2 gone on in the background.
fn cut_off() -> Vec<protocol::Entry> {
    log([
        input("$ make"),
        bash("make"),
        Body::Detached { call: id(2) },
        placeholder(2),
    ])
}

/// While a cut-off stream is being copied, the session is at work in the
/// background: nothing may unload it, or drain or idle-exit the daemon,
/// before what it held is logged.
#[test]
fn a_stream_being_copied_holds_its_session() {
    let fresh = Load {
        fresh: true,
        ..Load::default()
    };
    let (mut s, fx) = Session::resume(Reg::echo().arc(), job(2, 8), &fresh);
    assert!(work(&fx).any(|w| *w == Work::MarkBusy(true)));
    assert!(!s.core.at_rest());
    let fx = s.step(Event::StreamEnded {
        stream: STREAM,
        cut: CutOff::Job { pgid: 8 },
        end: StreamEnd::Copied(sealed(2)),
    });
    assert!(work(&fx).any(|w| *w == Work::MarkBusy(false)));
    assert!(s.core.at_rest());
}

/// An interrupt that drops a cut-off call before its stream is copied
/// leaves nothing to report: the copy that comes back queues nothing, and
/// the stream is removed.
#[test]
fn a_cut_off_call_an_interrupt_dropped_reports_nothing_when_its_stream_is_copied() {
    let (mut s, _) = Session::resume(Reg::echo().arc(), cut_off(), &Load::default());
    s.step(interrupt());
    let fx = s.step(Event::StreamEnded {
        stream: STREAM,
        cut: CutOff::Detached,
        end: StreamEnd::Copied(sealed(2)),
    });
    assert_eq!(queued(&s), []);
    assert_eq!(discarded_once_logged(&fx), [STREAM]);
}

/// An orphan drops what it ran in the background as it resumes, and
/// removes those calls' streams with them, copying nothing.
#[test]
fn an_orphan_removes_the_streams_of_the_calls_it_drops() {
    let load = Load {
        owner: kernel::Owner::Gone,
        ..Load::default()
    };
    let (s, fx) = Session::resume(Reg::echo().arc(), cut_off(), &load);
    assert!(!work(&fx).any(|w| matches!(w, Work::EndStream { .. })));
    assert_eq!(discarded_once_logged(&fx), [STREAM]);
    assert_eq!(s.core.ledger().detached(), []);
}

/// Call `call`, which left group `pgid` to the session.
fn left(call: u64, pgid: u32) -> [Body; 4] {
    [
        input("$ serve &"),
        bash("serve &"),
        Body::ToolProcess {
            call: id(call),
            pgid,
            boot: "boot-1".into(),
            background: true,
        },
        tool_result(call, done(json!(0))),
    ]
}

/// A log whose call `call` left group `pgid` to the session.
fn job(call: u64, pgid: u32) -> Vec<protocol::Entry> {
    log(left(call, pgid))
}

/// A group left to the session that a crash cut off has its stream copied
/// on the first load after, and its end queued holding what it wrote, or,
/// with no stream, holding nothing, which ends it all the same. One whose
/// end was queued, one handed over to go on, and one whose call is still
/// open, which its call's run again starts over, are left alone.
#[test]
fn a_job_a_crash_cut_off_has_its_end_queued_holding_what_it_wrote() {
    let ended = Body::Enqueue {
        envelope: protocol::Envelope::own(
            SessionId::NIL,
            id(6),
            protocol::Carried::Job("ended".into()),
        ),
    };
    let bodies = || {
        log(left(2, 8)
            .into_iter()
            .chain(left(6, 9))
            .chain([ended.clone()]))
    };
    let fresh = Load {
        fresh: true,
        ..Load::default()
    };
    let (mut s, fx) = Session::resume(Reg::echo().arc(), bodies(), &fresh);
    let cut = CutOff::Job { pgid: 8 };
    let ends: Vec<_> = work(&fx)
        .filter(|w| matches!(w, Work::EndStream { .. }))
        .collect();
    assert_eq!(
        ends,
        [&Work::EndStream {
            stream: STREAM,
            cut
        }]
    );
    let fx = s.step(Event::StreamEnded {
        stream: STREAM,
        cut,
        end: StreamEnd::Copied(sealed(2)),
    });
    assert_eq!(discarded_once_logged(&fx), [STREAM]);
    let jobs = |s: &Session| -> Vec<_> {
        (s.log.iter())
            .filter_map(|e| match &e.body {
                Body::Enqueue { envelope } => Some((envelope.source(), envelope.held.clone())),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        jobs(&s),
        [
            (Source::Job(id(6)), vec![]),
            (Source::Job(id(2)), vec![Held::Blob(sealed(2))]),
        ]
    );

    let (mut s, _) = Session::resume(Reg::echo().arc(), bodies(), &fresh);
    s.step(Event::StreamEnded {
        stream: STREAM,
        cut,
        end: StreamEnd::Missing,
    });
    assert_eq!(
        jobs(&s)[1],
        (Source::Job(id(2)), vec![]),
        "logged all the same"
    );
    let (_, fx) = Session::resume(Reg::echo().arc(), s.log.clone(), &fresh);
    assert!(
        !work(&fx).any(|w| matches!(w, Work::EndStream { .. })),
        "once ended, never again"
    );

    let handed = Load {
        reheld: &[id(2)],
        ..fresh
    };
    let (_, fx) = Session::resume(Reg::echo().arc(), bodies(), &handed);
    assert!(!work(&fx).any(|w| matches!(w, Work::EndStream { .. })));
    let open = log(left(2, 8).into_iter().take(3));
    let (_, fx) = Session::resume(Reg::echo().arc(), open, &fresh);
    assert!(!work(&fx).any(|w| matches!(w, Work::EndStream { .. })));
    // Within one daemon, the session's groups are its own still.
    let (_, fx) = Session::resume(Reg::echo().arc(), bodies(), &Load::default());
    assert!(!work(&fx).any(|w| matches!(w, Work::EndStream { .. })));
}

/// A call whose group the previous build handed over was never cut off:
/// it goes on, still detached, its outcome still to come.
#[test]
fn an_adopted_detached_call_goes_on_and_is_never_settled_as_interrupted() {
    let log = log([
        input("$ make"),
        bash("make"),
        Body::Detached { call: id(2) },
        placeholder(2),
    ]);
    let load = Load {
        adopted: &[id(2)],
        ..Load::default()
    };
    let (s, fx) = Session::resume(Reg::echo().arc(), log, &load);
    assert!(
        work(&fx).any(
            |w| matches!(w, Work::Adopt { call, tool, .. } if *call == id(2) && tool == "bash")
        )
    );
    assert!(
        !fx.appends
            .iter()
            .any(|e| matches!(e.body, Body::Enqueue { .. }))
    );
    assert_ne!(s.core.ledger().detached(), []);
}

#[test]
fn a_detached_calls_outcome_holds_what_its_result_would_have() {
    let mut s = sleeping();
    s.step(forced("now"));
    s.step(Event::Detached { call: id(4) });
    let blob = Held::Blob(BlobRef {
        id: BlobId([7; 32]),
        size: 1,
        media_type: "text/plain".into(),
        origin: BlobOrigin::ToolOutput(id(4)),
    });
    s.step(Event::ToolDone {
        call: id(4),
        outcome: done(json!("slept")).into(),
        card: None,
        held: vec![blob.clone()],
    });
    let envelope = (s.bodies().into_iter())
        .find_map(|b| match b {
            Body::Enqueue { envelope } if envelope.id.purpose == EnvelopeId::DETACHED => {
                Some(envelope)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(envelope.held, [blob]);
}

#[test]
fn only_the_users_queued_input_can_be_withdrawn_and_an_earlier_take_wins() {
    let mut s = echo();
    s.step(user("$ hold"));
    s.step(user("later"));
    let later = s.log.last().unwrap().seq;
    let withdraw = |input, ticket| Event::Withdraw {
        input,
        origin: None,
        ticket: Some(Ticket(ticket)),
    };
    let fx = s.step(withdraw(later, 1));
    assert!(work(&fx).any(|w| matches!(w, Work::Reply { result: Ok(()), .. })));
    assert_eq!(s.core.ledger().untaken(), []);
    let fx = s.step(withdraw(later, 2));
    assert!(work(&fx).any(|w| matches!(w, Work::Reply { result: Err(_), .. })));

    // Hidden items are not the user's to take back.
    s.step(Event::JobEnded {
        call: id(2),
        text: "job 7 ended".into(),
        held: Vec::new(),
    });
    let job = s.log.last().unwrap().seq;
    let fx = s.step(withdraw(job, 3));
    assert!(work(&fx).any(|w| matches!(w, Work::Reply { result: Err(_), .. })));

    // Once taken up, it is too late.
    s.step(user("again"));
    let again = s.log.last().unwrap().seq;
    s.step(tool_done(2, done(json!("held"))));
    assert!(takes(&s).contains(&(again.get(), None)));
    let fx = s.step(withdraw(again, 4));
    assert!(work(&fx).any(|w| matches!(w, Work::Reply { result: Err(_), .. })));
}

#[test]
fn a_background_jobs_end_starts_no_turn_and_is_read_with_the_next_model_call() {
    let mut s = modeled();
    s.step(user("work"));
    s.step(prepared(2));
    s.step(model_done(2, &[]));
    assert!(s.core.at_rest());
    let blob = Held::Blob(BlobRef {
        id: BlobId([7; 32]),
        size: 3,
        media_type: "text/plain".into(),
        origin: BlobOrigin::ToolOutput(id(1)),
    });
    let ended = || Event::JobEnded {
        call: id(1),
        text: "job 7 ended: exit 0".into(),
        held: vec![blob.clone()],
    };
    s.step(ended());
    s.step(ended());
    let queued: Vec<_> = (s.log.iter())
        .filter_map(|e| match &e.body {
            Body::Enqueue { envelope } => Some((envelope.source(), envelope.held.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(queued, [(Source::Job(id(1)), vec![blob.clone()])]);
    let job = last(&s, |b| matches!(b, Body::Enqueue { .. }));
    assert_eq!(takes(&s), [], "no turn of its own");
    assert!(s.core.at_rest(), "news waiting is no work");
    assert_eq!(s.core.ledger().activity(), protocol::Activity::Idle);

    s.step(user("next"));
    let next = job + 1;
    assert_eq!(
        takes(&s),
        [(next, None), (job, None)],
        "the user's input starts the turn; the end is read before the model is asked"
    );
    let taken = last(&s, |b| matches!(b, Body::Take { .. }));
    assert!(last(&s, |b| matches!(b, Body::ModelCall { .. })) > taken);
}

/// Delivers what is queued into each tool's result, and nothing elsewhere:
/// a policy as a `dequeue` handler.
struct IntoEachResult;

impl ext::Extension for IntoEachResult {
    const NAME: &'static str = "into";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut ext::Registrar) {
        r.on::<ext::point::Dequeue>(|(), (boundary, queue)| match boundary {
            Boundary::ToolDone(_) => Some(Take::IntoResult(
                queue.items().iter().map(|q| q.seq).collect(),
            )),
            Boundary::BeforeModel => Some(Take::None),
            Boundary::TurnEnd => None,
        });
    }
}

#[test]
fn a_dequeue_handler_decides_what_is_taken_and_a_result_carries_it_beside_its_call() {
    let reg = Reg::looping(|r| r.with(&IntoEachResult));
    let mut s = Session::new(reg.arc());
    s.step(user("go"));
    let asked = last(&s, |b| matches!(b, Body::ModelCall { .. }));
    s.step(prepared(asked));
    s.step(Event::ModelDone {
        call: id(asked),
        replied: Replied::Done(response(&[("sleep", json!({ "seconds": 1 }))])),
    });
    let call = last(&s, |b| matches!(b, Body::ToolCall { .. }));
    s.step(steer("also this"));
    let also = s.log.last().unwrap().seq.get();
    s.step(tool_done(call, done(json!("slept"))));
    let result = last(&s, |b| matches!(b, Body::ToolResult { .. }));
    assert_eq!(takes(&s), [(also, Some(id(call)))]);
    let taken = last(&s, |b| matches!(b, Body::Take { .. }));
    assert_eq!(taken, result + 1, "taken right behind the result it joins");
    // The steer did not start the next model call's tail too.
    assert!(last(&s, |b| matches!(b, Body::ModelCall { .. })) > taken);
    assert_eq!(s.core.ledger().untaken(), []);
}

#[test]
fn a_child_no_call_waits_on_is_queued_by_its_watch_once() {
    let mut s = echo();
    s.step(user("task go"));
    s.step(Event::Spawn {
        call: id(2),
        child: child(),
        input: "go".into(),
        intent: protocol::Json::null(),
        wait: false,
    });
    let settled = || Event::ChildSettled {
        child: child(),
        outcome: done(json!("ok")).into(),
    };
    s.step(settled());
    s.step(settled());
    let queued: Vec<_> = (s.log.iter())
        .filter_map(|e| match &e.body {
            Body::Enqueue { envelope } => Some(envelope.source()),
            _ => None,
        })
        .collect();
    let watch = last(&s, |b| matches!(b, Body::Watch { .. }));
    assert_eq!(queued, [Source::Child(seq(watch))]);
}
