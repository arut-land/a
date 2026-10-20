//! Ordering, publication and replay-safe admission across flush epochs.

use super::*;
use crate::store::{Faulty, SyncFailure};

struct NoTimers(rt::Threads);

impl rt::Runtime for NoTimers {
    fn spawn(&self, task: rt::BoxFuture<'static, ()>) {
        self.0.spawn(task);
    }
    fn spawn_blocking(&self, work: Box<dyn FnOnce() + Send>) {
        self.0.spawn_blocking(work);
    }
    fn reactor(&self) -> rt::Reactor {
        self.0.reactor()
    }
    fn sleep(&self, _: Duration) -> rt::BoxFuture<'static, ()> {
        Box::pin(std::future::pending())
    }
}

struct UnsafeHarbor;
impl Tool for UnsafeHarbor {
    type In = Nothing;
    type Out = String;
    const NAME: &'static str = HarborLabel::NAME;
    const DESCRIPTION: &'static str = HarborLabel::DESCRIPTION;
    fn call(&self, _: &Nothing, _: &Cx) -> Result<String, String> {
        Ok("crimson-harbor".into())
    }
    fn to_model(value: &Json) -> llm::Output {
        HarborLabel::to_model(value)
    }
}
impl Extension for UnsafeHarbor {
    const NAME: &'static str = "harbor";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.tool(Self);
    }
}

#[test]
fn turn_flushes_are_input_unsafe_work_and_final_publication() {
    for barriers in [kernel::Barriers::Step, kernel::Barriers::Effects] {
        let mut counts = Vec::new();
        for kind in ["safe", "unsafe", "echo"] {
            let faulty = Arc::new(Faulty::new(Arc::new(Memory::default())));
            let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
            let registry = match kind {
                "safe" => Registry::default().with(&Harbor).unwrap(),
                "unsafe" => Registry::default().with(&UnsafeHarbor).unwrap(),
                _ => Registry::default(),
            };
            let mut fixture = Fixture::on(
                Arc::clone(&faulty) as _,
                if kind == "echo" {
                    ext_fixtures::ECHO
                } else {
                    ext::LOOP
                },
                registry,
                openai(&Script::new(&replies)),
            );
            let shared = Arc::get_mut(&mut fixture.0).unwrap();
            shared.rt = Arc::new(NoTimers(rt::Threads::new(1)));
            shared.barriers = barriers;
            let session = fixture.create();
            settle(&session);
            let before = faulty.flushes();
            session.submit("hello").unwrap();
            settle(&session);
            counts.push(faulty.flushes() - before);
            session.halt();
        }
        assert_eq!(counts, [2, 3, 1], "{barriers:?}: safe, unsafe, echo");
    }
}

fn pipeline(registry: Registry) -> (Fixture, Arc<Faulty>, Arc<Script>) {
    let faulty = Arc::new(Faulty::new(Arc::new(Memory::default())));
    let script = Script::new(&cassette(
        "openai/streaming_grammar/tool_then_followup_text.yaml",
    ));
    let mut fixture = Fixture::on(
        Arc::clone(&faulty) as _,
        ext::LOOP,
        registry,
        openai(&script),
    );
    Arc::get_mut(&mut fixture.0).unwrap().rt = Arc::new(NoTimers(rt::Threads::new(1)));
    (fixture, faulty, script)
}

fn enqueue(session: &Live, text: &str) -> oneshot::Receiver<Result<(), String>> {
    let (reply, received) = oneshot::channel();
    session.send(Msg::Request {
        request: Request::Input {
            text: text.into(),
            mode: Mode::Queue,
        },
        origin: None,
        reply,
    });
    received
}

#[test]
fn safe_work_uses_ordered_transcripts_without_publishing_past_its_flush() {
    let (fixture, faulty, script) = pipeline(Registry::default().with(&Harbor).unwrap());
    let session = fixture.create();
    settle(&session);
    let before = session.frames.seq();
    let outbox = Outbox::new();
    session.follow(&outbox, 0);
    loop {
        let frame = rt::block_on(outbox.next()).unwrap();
        if matches!(
            protocol::read_frame::<DaemonMsg>(&mut frame.as_bytes()),
            Ok(Some(DaemonMsg::Synced { .. }))
        ) {
            break;
        }
    }
    let input = faulty.hold_next_flush();
    let final_flush = faulty.hold_next_flush();
    let mut ack = enqueue(&session, "ordered hello");
    input.started();
    session.wait("speculative safe turn", PATIENCE, || {
        lock(&script.sent).len() == 2 && !session.has_work()
    });
    let sent = lock(&script.sent);
    assert!(
        sent[0].to_string().contains("ordered hello"),
        "cold transcript includes unpublished input"
    );
    assert!(
        sent[1].to_string().contains("crimson-harbor"),
        "follow-up includes unpublished tool output"
    );
    drop(sent);
    assert_eq!(session.frames.seq(), before);
    let mut next = std::pin::pin!(outbox.next());
    let mut next_cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        next.as_mut().poll(&mut next_cx).is_pending(),
        "a speculative delta cannot expose an unflushed call id"
    );
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(std::pin::Pin::new(&mut ack).poll(&mut cx).is_pending());
    input.release();
    final_flush.started();
    let captured = faulty
        .entries(session.id)
        .unwrap()
        .last()
        .unwrap()
        .seq
        .get();
    session.wait("captured prefix published", PATIENCE, || {
        session.frames.seq() >= captured
    });
    assert_eq!(
        session.frames.seq(),
        captured,
        "later writes need a later successful epoch"
    );
    assert_eq!(rt::block_on(ack), Some(Ok(())));
    final_flush.release();
    settle(&session);
    assert_eq!(log_of(&session), faulty.entries(session.id).unwrap());
    session.halt();
}

struct SpiedHarbor(Arc<AtomicUsize>);
impl Tool for SpiedHarbor {
    type In = Nothing;
    type Out = String;
    const NAME: &'static str = HarborLabel::NAME;
    const DESCRIPTION: &'static str = HarborLabel::DESCRIPTION;
    fn call(&self, _: &Nothing, _: &Cx) -> Result<String, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok("crimson-harbor".into())
    }
}
impl Extension for SpiedHarbor {
    const NAME: &'static str = "harbor";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.tool(Self(Arc::clone(&self.0)));
    }
}

#[test]
fn an_unsafe_tool_waits_for_the_epoch_covering_its_own_intent() {
    unsafe_turn(None);
}

#[test]
fn a_failed_publication_never_runs_a_durable_unsafe_intent_twice() {
    for failure in [SyncFailure::Discard, SyncFailure::Retain] {
        unsafe_turn(Some(failure));
    }
}

fn unsafe_turn(failure: Option<SyncFailure>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let (fixture, faulty, script) = pipeline(
        Registry::default()
            .with(&SpiedHarbor(Arc::clone(&calls)))
            .unwrap(),
    );
    let session = fixture.create();
    settle(&session);
    let input = faulty.hold_next_flush();
    let tool = faulty.hold_next_flush();
    let final_flush = faulty.hold_next_flush();
    let ack = enqueue(&session, "hello");
    input.started();
    session.wait("unsafe intent ordered", PATIENCE, || {
        faulty
            .pending(session.id)
            .iter()
            .any(|e| matches!(e.body, Body::ToolCall { .. }))
    });
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    input.release();
    tool.started();
    assert_eq!(rt::block_on(ack), Some(Ok(())));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "input durability cannot admit a newer unsafe intent"
    );
    tool.release();
    final_flush.started();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(lock(&script.sent).len(), 2);
    if let Some(failure) = failure {
        let reply = cassette("openai/streaming_grammar/tool_then_followup_text.yaml")[1].clone();
        lock(&script.replies).push_back(reply);
        faulty.fail_next_sync(failure);
    }
    final_flush.release();
    settle(&session);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    session.halt();
}

struct Reactions {
    ordered: Arc<AtomicUsize>,
    durable: Arc<AtomicUsize>,
}
impl Extension for Reactions {
    const NAME: &'static str = "reactions";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        let ordered = Arc::clone(&self.ordered);
        let durable = Arc::clone(&self.durable);
        r.on::<point::Observe>(move |_, entry| {
            if !matches!(entry.body, Body::Input { .. }) {
                return ext::Reaction::NONE;
            }
            let ordered = Arc::clone(&ordered);
            ext::Reaction::later(Requirement::Ordered, async move {
                ordered.fetch_add(1, Ordering::SeqCst);
            })
        });
        r.on::<point::Observe>(move |_, entry| {
            if !matches!(entry.body, Body::Input { .. }) {
                return ext::Reaction::NONE;
            }
            let durable = Arc::clone(&durable);
            ext::Reaction::later(Requirement::Durable, async move {
                durable.fetch_add(1, Ordering::SeqCst);
            })
        });
    }
}

#[test]
fn observer_work_obeys_the_same_ordered_and_durable_admission() {
    let ordered = Arc::new(AtomicUsize::new(0));
    let durable = Arc::new(AtomicUsize::new(0));
    let registry = Registry::default()
        .with(&Harbor)
        .unwrap()
        .with(&Reactions {
            ordered: Arc::clone(&ordered),
            durable: Arc::clone(&durable),
        })
        .unwrap();
    let (fixture, faulty, script) = pipeline(registry);
    let session = fixture.create();
    settle(&session);
    let flush = faulty.hold_next_flush();
    let ack = enqueue(&session, "hello");
    flush.started();
    session.wait("safe observer and work", PATIENCE, || {
        ordered.load(Ordering::SeqCst) == 1 && lock(&script.sent).len() == 2
    });
    assert_eq!(durable.load(Ordering::SeqCst), 0);
    flush.release();
    assert_eq!(rt::block_on(ack), Some(Ok(())));
    settle(&session);
    session.wait("durable observer", PATIENCE, || {
        durable.load(Ordering::SeqCst) == 1
    });
    session.halt();
}

fn positions(session: &Live) -> (u64, u64) {
    let (send, receive) = oneshot::channel();
    session.send(Msg::Position(send));
    rt::block_on(receive).unwrap()
}

#[test]
fn cancelling_an_unadmitted_tool_settles_it_without_starting_it() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (fixture, faulty, _) = pipeline(
        Registry::default()
            .with(&SpiedHarbor(Arc::clone(&calls)))
            .unwrap(),
    );
    let session = fixture.create();
    settle(&session);
    let input = faulty.hold_next_flush();
    let tool = faulty.hold_next_flush();
    let ack = enqueue(&session, "hello");
    input.started();
    session.wait("unsafe intent", PATIENCE, || {
        faulty
            .pending(session.id)
            .iter()
            .any(|e| matches!(e.body, Body::ToolCall { .. }))
    });
    let (reply, interrupted) = oneshot::channel();
    session.send(Msg::Request {
        request: Request::Interrupt,
        origin: None,
        reply,
    });
    let _ = positions(&session);
    input.release();
    tool.started();
    tool.release();
    assert_eq!(rt::block_on(ack), Some(Ok(())));
    assert_eq!(rt::block_on(interrupted), Some(Ok(())));
    settle(&session);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    session.halt();
}

#[test]
fn later_epoch_notifications_cannot_skip_an_unresolved_prefix() {
    let (fixture, faulty, script) = pipeline(Registry::default().with(&Harbor).unwrap());
    let session = fixture.create();
    settle(&session);
    let before = session.frames.seq();
    let first = faulty.hold_next_flush();
    let last = faulty.hold_next_flush();
    let ack = enqueue(&session, "hello");
    first.started();
    session.wait("ordered final result", PATIENCE, || {
        lock(&script.sent).len() == 2 && !session.has_work()
    });
    let (ordered, durable) = positions(&session);
    assert!(ordered > durable);
    session.send(Msg::Flushed {
        fence: 0,
        through: ordered,
        result: Ok(()),
    });
    assert_eq!(
        positions(&session).1,
        before,
        "notifications can be scheduled out of epoch order"
    );
    first.release();
    last.started();
    last.release();
    assert_eq!(rt::block_on(ack), Some(Ok(())));
    settle(&session);
    session.halt();
}

fn started_harbor(starts: &channel::Receiver<StartedHarbor>) -> StartedHarbor {
    rt::block_on(or(starts.recv(), async {
        rt::Reactor::AsyncIo.sleep(PATIENCE).await;
        panic!("the expected speculative tool never started");
    }))
    .unwrap()
}

type StartedHarbor = (Cx, oneshot::Sender<String>);
struct HeldHarbor(Arc<channel::Sender<StartedHarbor>>);
impl Tool for HeldHarbor {
    type In = Nothing;
    type Out = String;
    const NAME: &'static str = HarborLabel::NAME;
    const DESCRIPTION: &'static str = HarborLabel::DESCRIPTION;
    const REPLAY: Replay = Replay::Safe;
    async fn call(&self, _: &Nothing, cx: &Cx) -> Result<String, String> {
        let (send, receive) = oneshot::channel();
        self.0.send((cx.clone(), send)).ok().unwrap();
        Ok(receive.await.unwrap_or_default())
    }
}
impl Extension for HeldHarbor {
    const NAME: &'static str = "harbor";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.tool(Self(Arc::clone(&self.0)));
    }
}

#[test]
fn failed_epochs_fence_speculative_reports_before_sequences_are_reused() {
    let (started, starts) = channel::unbounded();
    let (fixture, faulty, script) = pipeline(
        Registry::default()
            .with(&HeldHarbor(Arc::new(started)))
            .unwrap(),
    );
    let turn = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    *lock(&script.replies) = [turn[0].clone(), turn[0].clone(), turn[1].clone()].into();
    let session = fixture.create();
    settle(&session);
    let before = session.frames.seq();
    let first = faulty.hold_next_flush();
    let rejected = enqueue(&session, "before failure");
    first.started();
    let (old, release_old) = started_harbor(&starts);
    faulty.fail_next_sync(SyncFailure::Discard);
    first.release();
    assert!(matches!(rt::block_on(rejected), Some(Err(_))));
    let _ = positions(&session);
    assert_eq!(session.frames.seq(), before);
    let accepted = enqueue(&session, "after failure");
    let (current, release_current) = started_harbor(&starts);
    assert_eq!(current.call, old.call, "exercise the reused sequence");
    old.progress("stale progress");
    session.send(Msg::Fenced {
        fence: 0,
        msg: Box::new(Msg::Step(Event::ToolDone {
            call: old.call,
            outcome: Outcome::Done {
                output: Json::encode(&"stale result").unwrap(),
            },
            card: None,
        })),
    });
    let _ = positions(&session);
    assert_eq!(
        lock(&script.sent).len(),
        2,
        "an old result cannot settle the new call"
    );
    release_current.send("fresh result".into());
    assert_eq!(rt::block_on(accepted), Some(Ok(())));
    settle(&session);
    release_old.send("stale result".into());
    let _ = positions(&session);
    assert!(lock(&script.sent)[2].to_string().contains("fresh result"));
    assert!(
        !log_of(&session)
            .iter()
            .any(|entry| serde_json::to_string(entry).unwrap().contains("stale"))
    );
    session.halt();
}

#[test]
fn a_failed_epoch_answers_an_attach_waiting_for_publication() {
    let (fixture, faulty, script) = pipeline(Registry::default().with(&Harbor).unwrap());
    let session = fixture.create();
    settle(&session);
    let flush = faulty.hold_next_flush();
    let rejected = enqueue(&session, "hello");
    flush.started();
    session.wait("ordered turn", PATIENCE, || {
        lock(&script.sent).len() == 2 && !session.has_work()
    });
    let outbox = Outbox::new();
    let (done, attached) = oneshot::channel();
    session.send(Msg::Attach(outbox.clone(), 0, done));
    let _ = positions(&session);
    faulty.cut_before_next_flush();
    flush.release();
    assert!(matches!(rt::block_on(rejected), Some(Err(_))));
    assert_eq!(rt::block_on(attached), Some(()));
    let frame = rt::block_on(outbox.next()).unwrap();
    assert!(matches!(
        protocol::read_frame::<DaemonMsg>(&mut frame.as_bytes()),
        Ok(Some(DaemonMsg::Error { .. }))
    ));
    session.halt();
}
