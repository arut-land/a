use super::*;
use ext::{QuestionStep, QuestionTool};
use protocol::{Question, QuestionAnswer, Response, WaitOn};

#[derive(Clone)]
struct Questions(Arc<AtomicUsize>);
impl QuestionTool for Questions {
    type In = Nothing;
    type Out = u32;
    type State = u32;
    const NAME: &'static str = "questions";
    const DESCRIPTION: &'static str = "ask twice, without repeating the original effect";
    fn start(
        &self,
        _: &Nothing,
        _: &Cx,
    ) -> impl Future<Output = Result<QuestionStep<u32, u32>, String>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Ok(QuestionStep::Ask {
            question: Question::confirm("first".into()),
            state: 1,
        }))
    }
    fn answer(
        &self,
        _: &Nothing,
        state: u32,
        _: QuestionAnswer,
        _: &Cx,
    ) -> impl Future<Output = Result<QuestionStep<u32, u32>, String>> {
        std::future::ready(Ok(if state == 1 {
            QuestionStep::Ask {
                question: Question::confirm("second".into()),
                state: 2,
            }
        } else {
            QuestionStep::Done(state)
        }))
    }
}
impl Extension for Questions {
    const NAME: &'static str = "questions";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.question_tool(self.clone());
    }
}

fn pending(session: &Live) -> (CallId, Seq) {
    let ledger = Ledger::of(&log_of(session));
    ledger
        .open_calls()
        .find_map(|(call, open)| {
            matches!(
                open.phase,
                protocol::ledger::Phase::Waiting(WaitOn::Question { .. })
            )
            .then(|| (call, open.question_seq.unwrap()))
        })
        .unwrap()
}
fn answer(session: &Live, call: CallId, question: Seq) -> Result<(), String> {
    let answer = QuestionAnswer::confirm(question, Response::accept());
    rt::block_on(session.request(
        Request::Answer {
            call,
            value: Json::encode(&answer).unwrap(),
        },
        None,
    ))
}
fn fixture(calls: &Arc<AtomicUsize>) -> Fixture {
    Fixture::new(
        ext_fixtures::ECHO,
        Registry::default().with(&Questions(calls.clone())).unwrap(),
        openai(&Script::new(&[])),
    )
}

#[test]
fn a_parked_question_survives_restart_and_a_stale_answer_cannot_resolve_the_next_round() {
    let calls = Arc::new(AtomicUsize::new(0));
    let before = fixture(&calls);
    let session = before.create();
    session.submit("call questions {}").unwrap();
    settle(&session);
    let (call, first) = pending(&session);
    let prefix = log_of(&session);
    session.halt();
    let after = fixture(&calls);
    let recovered = after.start(&prefix);
    assert_eq!(pending(&recovered), (call, first));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    answer(&recovered, call, first).unwrap();
    settle(&recovered);
    let (same, second) = pending(&recovered);
    assert_eq!(call, same);
    assert_ne!(first, second);
    assert!(answer(&recovered, call, first).is_err());
    assert_eq!(pending(&recovered), (call, second));
    answer(&recovered, call, second).unwrap();
    settle(&recovered);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(log_of(&recovered).iter().any(|e| matches!(&e.body, Body::ToolResult { call: of, outcome: Outcome::Done { .. }, .. } if *of == call)));
}

#[test]
fn an_answered_continuation_without_a_committed_result_is_interrupted_on_recovery() {
    let calls = Arc::new(AtomicUsize::new(0));
    let before = fixture(&calls);
    let session = before.create();
    session.submit("call questions {}").unwrap();
    settle(&session);
    let (call, first) = pending(&session);
    answer(&session, call, first).unwrap();
    settle(&session);
    let log = log_of(&session);
    let cut = log
        .iter()
        .position(|e| matches!(e.body, Body::Answer { .. }))
        .unwrap()
        + 1;
    session.halt();
    let after = fixture(&calls);
    let recovered = after.start(&log[..cut]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(log_of(&recovered).iter().any(|e| matches!(e.body, Body::ToolResult { call: of, outcome: Outcome::Interrupted { .. }, .. } if of == call)));
}

#[derive(Clone, Default)]
struct Presenting(Arc<Mutex<Option<Arc<dyn ext::host::QuestionPort>>>>);
impl Tool for Presenting {
    type In = Nothing;
    type Out = ();
    const NAME: &'static str = "presenting";
    const DESCRIPTION: &'static str = "grant shared presentation authority";
    async fn call(&self, _: &Nothing, cx: &Cx) -> Result<(), String> {
        *self.0.lock().unwrap() = Some(cx.questions()?);
        cx.cancel.wait().await;
        Ok(())
    }
}
impl Extension for Presenting {
    const NAME: &'static str = "presenting";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.tool(self.clone());
    }
}

#[test]
fn a_shared_question_outlives_the_caller_that_granted_its_presentation_authority() {
    let presenting = Presenting::default();
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        Registry::default().with(&presenting).unwrap(),
        openai(&Script::new(&[])),
    );
    let session = fixture.create();
    session.submit("call presenting {}").unwrap();
    session.wait("presentation port granted", PATIENCE, || {
        presenting.0.lock().unwrap().is_some()
    });
    let port = presenting.0.lock().unwrap().take().unwrap();
    let question = port.ask(Question::confirm(
        "owned by a shared credential flow".into(),
    ));
    session.wait("independent question committed", PATIENCE, || {
        log_of(&session)
            .iter()
            .any(|e| matches!(e.body, Body::Question { .. }))
    });
    session.interrupt_now();
    settle(&session);
    let log = log_of(&session);
    let id = log
        .iter()
        .find(|e| matches!(e.body, Body::Question { .. }))
        .unwrap()
        .seq;
    assert!(Ledger::of(&log).independent_question(CallId(id)).is_some());
    answer(&session, CallId(id), id).unwrap();
    assert!(rt::block_on(question).unwrap().accepted());
}
