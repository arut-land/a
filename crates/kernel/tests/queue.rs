//! The session's one queue: what is taken up where, a forced steer, the
//! calls detached into the background, and what the session sends itself.

mod common;

use common::*;
use kernel::{Boundary, Event, Load, Replied, Take, Ticket};
use protocol::{Body, CallId, EnvelopeId, Outcome, OutputStream, Source};
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
    let placeholder = OutputStream { call: id(4) }.placeholder();
    assert!(s.bodies().contains(&&Body::Detached { call: id(4) }));
    assert!(s.bodies().contains(&&tool_result(4, placeholder)));
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
        let placeholder = tool_result(2, OutputStream { call: id(2) }.placeholder());
        let results = (s.bodies().into_iter())
            .filter(|b| matches!(b, Body::ToolResult { .. }))
            .collect::<Vec<_>>();
        assert_eq!(results, [&placeholder], "{command}");
        let reruns = work(&fx)
            .filter(|w| matches!(w, Work::RunTool { .. }))
            .count();
        let reported = (fx.appends.iter())
            .filter(|e| matches!(&e.body, Body::Enqueue { envelope } if envelope.id.purpose == EnvelopeId::DETACHED))
            .count();
        assert_eq!(
            (reruns, reported),
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
        outcome: done(json!("ok")),
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

#[test]
fn a_restart_runs_a_safe_detached_call_again_and_reports_an_unsafe_one_cut_off() {
    let detached = |command: &str, call: Body| {
        log([
            input(command),
            call,
            Body::Detached { call: id(2) },
            tool_result(2, OutputStream { call: id(2) }.placeholder()),
        ])
    };
    let (s, fx) = Session::resume(
        Reg::echo().arc(),
        detached("sleep 9", sleep_call(9)),
        &Load::default(),
    );
    assert!(work(&fx).any(|w| *w == run_tool(2, "sleep", json!({ "seconds": 9 }))));
    assert_ne!(s.core.ledger().detached(), []);

    let (s, fx) = Session::resume(
        Reg::echo().arc(),
        detached("$ make", bash("make")),
        &Load::default(),
    );
    assert!(!work(&fx).any(|w| matches!(w, Work::RunTool { .. })));
    assert_eq!(s.core.ledger().detached(), []);
    let cut = fx
        .appends
        .iter()
        .find_map(|e| match &e.body {
            Body::Enqueue { envelope } => Outcome::from_payload(&envelope.payload),
            _ => None,
        })
        .unwrap();
    let Outcome::Interrupted {
        output: Some(output),
    } = cut
    else {
        panic!("{cut:?}");
    };
    assert!(output.contains("may not have completed"), "{output}");
    assert!(output.contains(r#"{"call":2}"#), "{output}");
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
    });
    let job = s.log.last().unwrap().seq;
    let fx = s.step(withdraw(job, 3));
    assert!(work(&fx).any(|w| matches!(w, Work::Reply { result: Err(_), .. })));

    // Once taken up, it is too late.
    s.step(user("again"));
    let again = s.log.last().unwrap().seq;
    s.step(tool_done(2, done(json!("held"))));
    assert!(takes(&s).contains(&(job.get(), None)));
    assert!(takes(&s).contains(&(again.get(), None)));
    let fx = s.step(withdraw(again, 4));
    assert!(work(&fx).any(|w| matches!(w, Work::Reply { result: Err(_), .. })));
}

#[test]
fn a_background_jobs_end_reaches_an_idle_agent_once() {
    let mut s = echo();
    s.step(user("hi"));
    let ended = || Event::JobEnded {
        call: id(1),
        text: "job 7 ended: exit 0".into(),
    };
    s.step(ended());
    s.step(ended());
    let queued: Vec<_> = (s.log.iter())
        .filter_map(|e| match &e.body {
            Body::Enqueue { envelope } => Some(envelope.source()),
            _ => None,
        })
        .collect();
    assert_eq!(queued, [Source::Job(id(1))]);
    assert_eq!(s.log.last().unwrap().body, said("job 7 ended: exit 0"));
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
        outcome: done(json!("ok")),
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
