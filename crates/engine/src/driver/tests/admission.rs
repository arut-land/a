//! Ordering, publication and replay-safe admission across flush epochs.

use super::*;
use crate::store::{Faulty, SyncFailure};
use rt::Runtime;

struct NoTimers(rt::Threads);

impl Runtime for NoTimers {
    fn spawn(&self, task: rt::BoxFuture<'static, ()>) {
        self.0.spawn(task);
    }
    fn spawn_blocking(
        &self,
        class: rt::BlockingClass,
        bytes: usize,
        work: Box<dyn FnOnce() + Send>,
    ) -> rt::BoxFuture<'static, io::Result<()>> {
        self.0.spawn_blocking(class, bytes, work)
    }
    fn spawn_flush(
        &self,
        work: Box<dyn FnOnce() + Send>,
    ) -> rt::BoxFuture<'static, io::Result<()>> {
        self.0.spawn_flush(work)
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
            fixture = fixture.configure(|shared| {
                shared.rt = Arc::new(NoTimers(rt::Threads::new(1)));
                shared.barriers = barriers;
            });
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
    pipeline_on(Arc::new(Memory::default()), registry)
}

fn pipeline_on(
    store: Arc<dyn LogStore>,
    registry: Registry,
) -> (Fixture, Arc<Faulty>, Arc<Script>) {
    let faulty = Arc::new(Faulty::new(store));
    let script = Script::new(&cassette(
        "openai/streaming_grammar/tool_then_followup_text.yaml",
    ));
    let mut fixture = Fixture::on(
        Arc::clone(&faulty) as _,
        ext::LOOP,
        registry,
        openai(&script),
    );
    fixture = fixture.configure(|shared| shared.rt = Arc::new(NoTimers(rt::Threads::new(1))));
    (fixture, faulty, script)
}

fn enqueue(session: &Live, text: &str) -> oneshot::Receiver<Result<(), String>> {
    let (reply, received) = oneshot::channel();
    session.send(Msg::Request {
        request: Request::Input {
            segments: text.into(),
            mode: Mode::Queue,
            force: false,
        },
        origin: None,
        reply,
    });
    received
}

/// Live text a reply streams while its call waits for its flush reaches
/// followers once the call is shown, in order and after it: a stream that
/// outruns durability, as concurrent streams sharing the flush lane do,
/// loses none of it.
#[test]
fn no_delta_is_lost_when_a_stream_outruns_its_calls_flush() {
    let (fixture, faulty, script) = pipeline(Registry::default().with(&Harbor).unwrap());
    let session = fixture.create();
    settle(&session);
    let outbox = Outbox::new();
    session.follow(&outbox, 0);
    let held = faulty.hold_next_flush();
    let ack = enqueue(&session, "hello");
    held.started();
    session.wait("both replies streamed", PATIENCE, || {
        lock(&script.sent).len() == 2 && !session.has_work()
    });
    held.release();
    assert_eq!(rt::block_on(ack), Some(Ok(())));
    settle(&session);
    let mut shown = Vec::new();
    let mut streamed = String::new();
    while let Some(frame) = outbox.try_next() {
        match protocol::read_frame::<DaemonMsg>(&mut frame.as_bytes()) {
            Ok(Some(DaemonMsg::Head { head })) => shown.push(head.cut.seq),
            Ok(Some(DaemonMsg::Delta { call, text, .. })) => {
                assert!(
                    shown.iter().any(|seq| *seq >= call.get()),
                    "a delta of call {call:?} preceded it"
                );
                streamed.push_str(&text);
            }
            _ => {}
        }
    }
    let recorded: String = (retry::text_reply().split_inclusive("\n\n"))
        .filter(|event| event.starts_with("event: response.output_text.delta"))
        .map(|event| {
            let data = event.split_once("data: ").unwrap().1;
            let data: Value = serde_json::from_str(data.trim()).unwrap();
            data["delta"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(streamed, recorded);
    session.halt();
}

#[test]
fn publication_never_crosses_the_durable_prefix() {
    let (fixture, faulty, script) = pipeline(Registry::default().with(&Harbor).unwrap());
    let session = fixture.create();
    settle(&session);
    let before = session.frames.seq();
    let outbox = Outbox::new();
    session.follow(&outbox, 0);
    loop {
        let frame = rt::block_on(outbox.next(&rt::Threads::new(1))).unwrap();
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
    let reader = rt::Threads::new(1);
    let mut next = std::pin::pin!(outbox.next(&reader));
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
    session.send(Msg::Completed {
        fence: 0,
        completion: Completion::Event(Event::ToolDone {
            call: old.call,
            outcome: Outcome::Done {
                output: Json::encode(&"stale result").unwrap().into(),
            },
            card: None,
            held: Vec::new(),
        }),
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
    let frame = rt::block_on(outbox.next(&rt::Threads::new(1))).unwrap();
    assert!(matches!(
        protocol::read_frame::<DaemonMsg>(&mut frame.as_bytes()),
        Ok(Some(DaemonMsg::Error { .. }))
    ));
    session.halt();
}

struct RetainedObserver(Arc<channel::Sender<Cx>>);
impl Extension for RetainedObserver {
    const NAME: &'static str = "retained_observer";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        let contexts = Arc::clone(&self.0);
        r.on::<point::Observe>(move |cx, entry| {
            if !matches!(entry.body, Body::ToolCall { .. }) {
                return ext::Reaction::NONE;
            }
            let cx = cx.cx().unwrap();
            let contexts = Arc::clone(&contexts);
            ext::Reaction::later(Requirement::Ordered, async move {
                contexts.send(cx).ok().unwrap();
            })
        });
    }
}

#[derive(Clone, Copy)]
enum RetainedProbe {
    Side,
    Child,
    Reaction,
    Mailbox,
}

fn stale_request<'a>(
    probe: RetainedProbe,
    old: &'a Cx,
    session: &'a Session,
    fence: u64,
) -> rt::BoxFuture<'a, ()> {
    if matches!(probe, RetainedProbe::Mailbox) {
        Box::pin(async move {
            // A queued request must be fenced even independently of revocation.
            let origin = WorkHandle::new(
                old.call,
                fence,
                Admission {
                    through: old.call.get(),
                    requirement: Requirement::Ordered,
                },
                Cancel::default(),
            );
            let (reply, _) = oneshot::channel();
            origin.send(
                session,
                ContextRequest::Side {
                    registry: session.shared.registry.get(),
                    side: ext::Conversation::turn::<HeldHarbor>("stale side", "")
                        .per_call()
                        .key("initial")
                        .request(old.call),
                    reply,
                },
            );
            let (reply, _) = oneshot::channel();
            origin.send(
                session,
                ContextRequest::Child {
                    request: ext::Conversation::task::<HeldHarbor>("stale", "stale child")
                        .kind(ext::LOOP)
                        .depth(1)
                        .spawn(String::new),
                    reply: spawn::SpawnReply::Awaited(reply),
                },
            );
            origin.send(
                session,
                ContextRequest::Note {
                    note: Note::Progress("stale note".into()),
                    done: None,
                },
            );
            origin.send(
                session,
                ContextRequest::Own {
                    process: (Arc::new(StaleProcess) as Arc<dyn ext::RunningProcess>).into(),
                    role: ext::Role::Work,
                },
            );
        })
    } else if matches!(probe, RetainedProbe::Child) {
        Box::pin(async move {
            let _ = old
                .conversations()
                .unwrap()
                .task(
                    old,
                    ext::Conversation::task::<HeldHarbor>("stale", "stale child")
                        .kind(ext::LOOP)
                        .depth(1)
                        .spawn(String::new),
                )
                .await;
        })
    } else {
        Box::pin(async move {
            let side = ext::Conversation::turn::<HeldHarbor>("stale side", "stale request")
                .per_call()
                .key("initial")
                .request(old.call);
            let _ = old.conversations().unwrap().turn(old, side).await;
        })
    }
}

fn settled_context_cannot_submit_after_recovery(probe: RetainedProbe) {
    let (started, starts) = channel::unbounded();
    let (observed, observations) = channel::unbounded();
    let registry = Registry::default()
        .with(&HeldHarbor(Arc::new(started)))
        .unwrap()
        .with(&RetainedObserver(Arc::new(observed)))
        .unwrap();
    let (fixture, faulty, script) = pipeline(registry);
    let turn = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    *lock(&script.replies) = [
        turn[0].clone(),
        turn[1].clone(),
        turn[0].clone(),
        turn[1].clone(),
        turn[1].clone(),
    ]
    .into();
    let session = fixture.create();
    settle(&session);
    let fence = session.session.fence.load(Ordering::SeqCst);
    let flush = faulty.hold_next_flush();
    let rejected = enqueue(&session, "before failure");
    flush.started();
    let (old, release) = started_harbor(&starts);
    let observed = rt::block_on(observations.recv()).unwrap();
    release.send("old result".into());
    session.wait("settled speculative turn", PATIENCE, || {
        lock(&script.sent).len() == 2 && !session.has_work()
    });
    let _ = positions(&session);
    faulty.fail_next_sync(SyncFailure::Discard);
    flush.release();
    assert!(matches!(rt::block_on(rejected), Some(Err(_))));
    let _ = positions(&session);
    let accepted = enqueue(&session, "after failure");
    let (current, release) = started_harbor(&starts);
    assert_eq!(old.call, current.call, "exercise a reused call id");
    let _current_observer = rt::block_on(observations.recv()).unwrap();
    let old = if matches!(probe, RetainedProbe::Reaction) {
        observed
    } else {
        old
    };
    let mut request = stale_request(probe, &old, &session.session, fence);
    let _ = request
        .as_mut()
        .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()));
    let _ = positions(&session);
    let admitted = faulty.pending(session.id).iter().any(|entry| {
        matches!(&entry.body, Body::ModelCall { purpose: Some(p), .. } if p == "stale side")
            || serde_json::to_string(entry)
                .unwrap()
                .contains("stale child")
    }) || log_of(&session)
        .iter()
        .any(|entry| serde_json::to_string(entry).unwrap().contains("stale"));
    drop(request);
    assert!(
        !admitted,
        "old context admitted work into the recovered generation"
    );
    assert!(
        lock(&session.session.jobs).is_empty(),
        "stale process ownership was admitted"
    );
    release.send("fresh result".into());
    assert_eq!(rt::block_on(accepted), Some(Ok(())));
    settle(&session);
    session.halt();
    assert!(
        old.cancel.is_set(),
        "settlement must revoke retained contexts"
    );
}

#[test]
fn a_settled_context_cannot_submit_side_calls_into_a_recovered_generation() {
    settled_context_cannot_submit_after_recovery(RetainedProbe::Side);
}

#[test]
fn a_settled_context_cannot_submit_children_into_a_recovered_generation() {
    settled_context_cannot_submit_after_recovery(RetainedProbe::Child);
}

#[test]
fn a_settled_observer_context_cannot_submit_reactions_into_a_recovered_generation() {
    settled_context_cannot_submit_after_recovery(RetainedProbe::Reaction);
}

#[test]
fn a_queued_context_request_keeps_its_original_fence_without_a_cancel_signal() {
    settled_context_cannot_submit_after_recovery(RetainedProbe::Mailbox);
}

#[test]
fn an_unadmitted_side_call_cannot_outlive_its_cancelled_parent() {
    let (started, starts) = channel::unbounded();
    let registry = Registry::default()
        .with(&HeldHarbor(Arc::new(started)))
        .unwrap();
    let (fixture, faulty, script) = pipeline(registry);
    let session = fixture.create();
    settle(&session);
    let input = faulty.hold_next_flush();
    let side_flush = faulty.hold_next_flush();
    let ack = enqueue(&session, "hello");
    input.started();
    let (cx, release) = started_harbor(&starts);
    let side = ext::Conversation::turn::<HeldHarbor>("cancelled side", "Say hi.")
        .per_call()
        .key("initial")
        .request(cx.call);
    let mut answer = cx.conversations().unwrap().turn(&cx, side);
    assert!(
        answer
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    );
    let _ = positions(&session);
    let (reply, interrupted) = oneshot::channel();
    session.send(Msg::Request {
        request: Request::Interrupt,
        origin: None,
        reply,
    });
    let _ = positions(&session);
    release.send("cancelled result".into());
    input.release();
    side_flush.started();
    side_flush.release();
    assert_eq!(rt::block_on(ack), Some(Ok(())));
    assert_eq!(rt::block_on(interrupted), Some(Ok(())));
    let refused = rt::block_on(answer).is_err();
    settle(&session);
    session.halt();
    assert!(
        refused,
        "cancellation must revoke side work still awaiting admission"
    );
    assert_eq!(lock(&script.sent).len(), 1);
}

#[test]
fn submitted_input_is_cancelled_while_its_publication_flush_is_held() {
    let (started, starts) = channel::unbounded();
    let (fixture, faulty, script) = pipeline(
        Registry::default()
            .with(&HeldHarbor(Arc::new(started)))
            .unwrap(),
    );
    let session = fixture.create();
    settle(&session);
    let before = session.frames.seq();
    let mut client = client::SessionClient::new(client::Env::Keep);
    client.open(protocol::Target::Id(session.id));
    let conn = client.connection();
    let feed = |client: &mut client::SessionClient, kind| {
        client.step(client::Event { conn, kind });
    };
    feed(
        &mut client,
        client::EventKind::Connected {
            welcome: client::Welcome {
                proto: protocol::PROTO,
                caps: protocol::Caps::ALL,
            },
            remote: false,
            commands: Vec::new(),
            machine: protocol::MachineId([0; 16]),
            upgrade: None,
        },
    );
    feed(
        &mut client,
        client::EventKind::Msg(DaemonMsg::Attached {
            session: session.id,
            workspace: protocol::HostPath::lossy(&session.workspace),
        }),
    );
    for entry in log_of(&session) {
        feed(&mut client, client::EventKind::Msg(DaemonMsg::Entry(entry)));
    }
    feed(
        &mut client,
        client::EventKind::Msg(DaemonMsg::Activity {
            seq: before,
            activity: Activity::Idle,
        }),
    );
    feed(
        &mut client,
        client::EventKind::Msg(DaemonMsg::Synced { seq: before }),
    );
    let send = |client: &mut client::SessionClient, cmd| {
        let mut effects = client.run(cmd);
        let request = match effects.remove(0) {
            client::Effect::Io(client::Io::Send(protocol::ClientMsg::Input {
                segments,
                mode,
                ..
            })) => Request::Input {
                segments,
                mode,
                force: false,
            },
            client::Effect::Io(client::Io::Send(protocol::ClientMsg::Interrupt { .. })) => {
                Request::Interrupt
            }
            effect => panic!("submitted input must remain interruptible: {effect:?}"),
        };
        let (reply, ack) = oneshot::channel();
        session.send(Msg::Request {
            request,
            origin: None,
            reply,
        });
        ack
    };
    let flush = faulty.hold_next_flush();
    let ack = send(&mut client, client::Cmd::Input("hello".into()));
    flush.started();
    let (cx, release) = started_harbor(&starts);
    assert_eq!(session.frames.seq(), before);
    assert!(!client.turn().running);
    let interrupted = send(&mut client, client::Cmd::Interrupt);
    let _ = positions(&session);
    assert!(cx.cancel.is_set(), "cancellation must precede durability");
    release.send("finished".into());
    session.wait("cancelled unpublished turn", PATIENCE, || {
        !session.has_work()
    });
    assert_eq!(session.frames.seq(), before);
    assert_eq!(
        lock(&script.sent).len(),
        1,
        "the interrupted turn must not ask for a follow-up"
    );
    flush.release();
    assert_eq!(rt::block_on(ack), Some(Ok(())));
    assert_eq!(rt::block_on(interrupted), Some(Ok(())));
    settle(&session);
    session.halt();
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct AdoptionTotal(u64);
impl ext::Fold for AdoptionTotal {
    const NAME: &'static str = "total";
    type Change = u64;
    fn apply(&mut self, change: &u64) {
        self.0 += change;
    }
}
struct AdoptionState;
impl Extension for AdoptionState {
    const NAME: &'static str = "adoption";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.fold::<AdoptionTotal>();
    }
}

#[test]
fn registry_adoption_waits_for_rest_and_the_complete_durable_prefix() {
    let (started, starts) = channel::unbounded();
    let registry = Registry::default()
        .with(&HeldHarbor(Arc::new(started)))
        .unwrap()
        .with(&AdoptionState)
        .unwrap();
    let (fixture, faulty, script) = pipeline(registry);
    let session = fixture.create();
    settle(&session);
    let old = session.session.state_view();
    let old_registry = fixture.0.registry.get();
    let old_bound = bind(&old_registry, &session.session);
    let fence = session.session.fence.load(Ordering::SeqCst);
    let flush = faulty.hold_next_flush();
    let ack = enqueue(&session, "hello");
    flush.started();
    let (cx, release) = started_harbor(&starts);
    let first = cx.update::<AdoptionTotal>(&1);
    let _ = positions(&session);
    fixture.0.registry.replace(
        Registry::default()
            .with(&Agents)
            .unwrap()
            .with(&Harbor)
            .unwrap()
            .with(&AdoptionState)
            .unwrap(),
    );
    let second = cx.update::<AdoptionTotal>(&1);
    let _ = positions(&session);
    assert!(
        Arc::ptr_eq(&old, &session.session.state_view()),
        "a running turn keeps its generation"
    );
    release.send("finished".into());
    session.wait("speculative turn at rest", PATIENCE, || {
        lock(&script.sent).len() == 2 && !session.has_work()
    });
    let (ordered, durable) = positions(&session);
    assert!(
        ordered > durable,
        "the test must retain an unpublished suffix"
    );
    assert_eq!(lock(&old).get::<AdoptionTotal>().unwrap().0, 2);
    assert!(
        Arc::ptr_eq(&old, &session.session.state_view()),
        "rest cannot split an outstanding flush epoch"
    );
    assert_eq!(session.session.fence.load(Ordering::SeqCst), fence);
    flush.release();
    assert_eq!(rt::block_on(ack), Some(Ok(())));
    rt::block_on(first).unwrap();
    rt::block_on(second).unwrap();
    session.wait("generation adopted", PATIENCE, || {
        !Arc::ptr_eq(&old, &session.session.state_view())
    });
    let (after_ordered, after_durable) = positions(&session);
    assert!(
        after_ordered >= ordered && after_durable >= ordered,
        "adoption preserves admission watermarks"
    );
    assert_eq!(
        lock(&session.session.state_view())
            .get::<AdoptionTotal>()
            .unwrap()
            .0,
        2,
        "ordered reductions replay exactly once"
    );
    assert!(
        old_bound.point().about(cx.call).cx().is_none(),
        "old generation views cannot mint new context authority"
    );
    cx.progress("stale generation");
    let _ = positions(&session);
    assert!(!log_of(&session).iter().any(|e| {
        serde_json::to_string(e)
            .unwrap()
            .contains("stale generation")
    }));
    session.halt();
}

fn messages(outbox: &Outbox) -> impl Iterator<Item = DaemonMsg> + '_ {
    std::iter::from_fn(|| outbox.try_next()).map(|frame| {
        protocol::read_frame::<DaemonMsg>(&mut frame.as_bytes())
            .unwrap()
            .unwrap()
    })
}

#[test]
fn activity_waits_for_the_current_ledgers_flush_on_live_snapshot_and_since_replay() {
    let (fixture, faulty, _) = pipeline(Registry::default().with(&Harbor).unwrap());
    let history: Vec<_> = (1..=129).map(|seq| entry(seq, echo("earlier"))).collect();
    let session = fixture.start(&history);
    let before = session.frames.seq();
    let live = Outbox::new();
    session.follow(&live, before);
    while live.try_next().is_some() {}
    let input = faulty.hold_next_flush();
    let final_flush = faulty.hold_next_flush();
    let ack = enqueue(&session, "hello");
    input.started();
    session.wait("safe turn awaiting publication", PATIENCE, || {
        !session.has_work() && positions(&session).0 > before
    });
    input.release();
    final_flush.started();
    let captured = faulty
        .entries(session.id)
        .unwrap()
        .last()
        .unwrap()
        .seq
        .get();
    session.wait("input prefix published", PATIENCE, || {
        session.frames.seq() >= captured
    });
    let (ordered, durable) = positions(&session);
    assert!(ordered > durable);
    let mut view = SessionView::from_log(
        session.id,
        protocol::HostPath::lossy(&session.workspace),
        history,
    );
    for msg in messages(&live) {
        assert!(
            !matches!(msg, DaemonMsg::Activity { .. }),
            "current idle activity cannot certify an earlier durable prefix: {msg:?}"
        );
        view.fold(&msg);
    }
    assert_eq!(view.activity, None);
    let mut replays = Vec::new();
    for since in [0, before] {
        let outbox = Outbox::new();
        let (done, attached) = oneshot::channel();
        session.send(Msg::Attach(outbox.clone(), since, done));
        replays.push((since, outbox, attached));
    }
    assert_eq!(positions(&session), (ordered, durable));
    for (_, outbox, _) in &replays {
        assert!(outbox.try_next().is_none(), "attach waits for durability");
    }
    final_flush.release();
    assert_eq!(rt::block_on(ack), Some(Ok(())));
    settle(&session);
    for msg in messages(&live) {
        if let DaemonMsg::Activity { seq, .. } = &msg {
            assert!(*seq <= positions(&session).1);
            assert_eq!(*seq, ordered);
        }
        view.fold(&msg);
    }
    assert_eq!(view.seq, ordered);
    assert_eq!(
        view.activity,
        Some(Activity::Idle),
        "flush completion ends the turn"
    );
    for (since, outbox, attached) in replays {
        assert_eq!(rt::block_on(attached), Some(()));
        let mut snapshot = false;
        let mut activity = None;
        let mut certifications = 0;
        for msg in messages(&outbox) {
            match msg {
                DaemonMsg::Head { .. } => snapshot = true,
                DaemonMsg::Activity {
                    seq,
                    activity: current,
                } => {
                    assert!(seq <= positions(&session).1);
                    activity = Some((seq, current));
                    certifications += 1;
                }
                _ => {}
            }
        }
        assert!(
            snapshot,
            "every attach receives its public cut, since {since}"
        );
        assert_eq!(activity, Some((ordered, Activity::Idle)));
        assert_eq!(
            certifications, 1,
            "flush admits attach after certifying live followers"
        );
    }
    session.halt();
}

struct DelegatedHarbor;
impl Tool for DelegatedHarbor {
    type In = Nothing;
    type Out = bool;
    const NAME: &'static str = "delegated_harbor";
    const DESCRIPTION: &'static str = "finish before an earlier flush";
    const REPLAY: Replay = Replay::Safe;
    fn call(&self, _: &Nothing, _: &Cx) -> Result<bool, String> {
        Ok(true)
    }
}
impl Extension for DelegatedHarbor {
    const NAME: &'static str = "delegated_harbor";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.tool(Self);
    }
}

#[test]
fn delegated_results_wait_for_their_own_durable_prefix() {
    let (started, starts) = channel::unbounded();
    let (fixture, faulty, _) = pipeline(
        Registry::default()
            .with(&HeldHarbor(Arc::new(started)))
            .unwrap()
            .with(&DelegatedHarbor)
            .unwrap(),
    );
    let session = fixture.create();
    settle(&session);
    let input = faulty.hold_next_flush();
    let intent = faulty.hold_next_flush();
    let result = faulty.hold_next_flush();
    let accepted = enqueue(&session, "hello");
    input.started();
    let (cx, release) = started_harbor(&starts);
    let origin = session.session.origin(cx.call, 0).unwrap();
    let invoke = || {
        let (reply, received) = oneshot::channel();
        origin.send(
            &session.session,
            ContextRequest::Invoke {
                context: cx.invocation().clone(),
                name: "lookup".into(),
                tool: <DelegatedHarbor as Tool>::NAME.into(),
                input: Json::encode(&serde_json::json!({})).unwrap(),
                reply,
            },
        );
        received
    };
    let mut first = invoke();
    let mut completed = 0;
    session.wait("speculative delegated result", PATIENCE, || {
        completed = faulty
            .pending(session.id)
            .iter()
            .find_map(|e| {
                matches!(e.body, Body::ToolResult { call, .. } if call != cx.call)
                    .then_some(e.seq.get())
            })
            .unwrap_or(0);
        completed != 0
    });
    let mut repeated = invoke();
    let (_, before) = positions(&session);
    assert!(before < completed);
    input.release();
    intent.started();
    let captured = faulty
        .entries(session.id)
        .unwrap()
        .last()
        .unwrap()
        .seq
        .get();
    intent.release();
    session.wait("intent durable", PATIENCE, || {
        session.frames.seq() > captured
    });
    let (ordered, durable) = positions(&session);
    assert!(ordered >= completed && durable < completed);
    let mut task = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(std::pin::Pin::new(&mut first).poll(&mut task).is_pending());
    assert!(
        std::pin::Pin::new(&mut repeated)
            .poll(&mut task)
            .is_pending()
    );
    session.send(Msg::Sync);
    result.started();
    assert!(std::pin::Pin::new(&mut first).poll(&mut task).is_pending());
    result.release();
    let first = rt::block_on(first).unwrap();
    assert_eq!(first, rt::block_on(repeated).unwrap());
    assert_eq!(
        first.outcome,
        Outcome::Done {
            output: Json::encode(&true).unwrap().into()
        }
    );
    assert_eq!(rt::block_on(accepted), Some(Ok(())));
    release.send("parent finished".into());
    settle(&session);
    assert_eq!(
        log_of(&session)
            .iter()
            .filter(|e| matches!(
                e.body,
                Body::ToolCall {
                    invoke: Some(_),
                    ..
                }
            ))
            .count(),
        1
    );
    session.halt();
}

#[test]
fn metrics_add_no_flushes_to_provider_execution() {
    let mut counts = Vec::new();
    for metrics in [false, true] {
        let faulty = Arc::new(Faulty::new(Arc::new(Memory::default())));
        let mut registry = Registry::default().with(&Harbor).unwrap();
        if metrics {
            registry = registry.with(&ext_metrics::Metrics).unwrap();
        }
        let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
        let fixture = Fixture::on(
            Arc::clone(&faulty) as _,
            ext::LOOP,
            registry,
            openai(&Script::new(&replies)),
        );
        let session = fixture.create();
        settle(&session);
        let before = faulty.flushes();
        session.submit("hello").unwrap();
        settle(&session);
        counts.push(faulty.flushes() - before);
        session.halt();
    }
    assert_eq!(counts[1], counts[0], "without/with metrics: {counts:?}");
}

#[test]
fn cancelled_before_provider_send_adds_no_executed_attempt() {
    struct CancelBeforeSend;
    impl Extension for CancelBeforeSend {
        const NAME: &'static str = "cancel-before-send";
        const VERSION: u32 = 1;
        fn contribute(&self, r: &mut Registrar) {
            r.on::<point::Step>(|cx, _| {
                cx.cx().unwrap().cancel.set();
                Rewrite::Pass
            });
        }
    }
    let script = Script::new(&[retry::text_reply()]);
    let fixture = Fixture::new(
        ext::LOOP,
        Registry::default()
            .with(&ext_metrics::Metrics)
            .and_then(|r| r.with(&CancelBeforeSend))
            .unwrap(),
        openai(&script),
    );
    let session = fixture.create();
    session.submit("hello").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    assert!(lock(&script.sent).is_empty());
    assert!(log.iter().all(|e| e.execution.is_none()));
    let mut projection = ext_metrics::Projection::default();
    projection.fold(&ext::LogPrefix {
        session: session.id,
        entries: log,
        cursor: ext::LogCursor::default(),
        reset: true,
        inherited_through: 0,
    });
    let prices = ext_metrics::Prices {
        version: "unpriced".into(),
        currency: "USD".into(),
        models: Vec::new(),
    };
    assert_eq!(projection.own(session.id, &prices).attempts.executed, 0);
}

pub(super) struct StaleProcess;
impl ext::RunningProcess for StaleProcess {
    fn id(&self) -> u32 {
        42
    }
    fn signal(&self, _: ext::ProcessSignal) -> io::Result<()> {
        Ok(())
    }
    fn alive(&self) -> bool {
        false
    }
    fn try_wait(&self) -> io::Result<Option<std::process::ExitStatus>> {
        Ok(None)
    }
    fn wait(&self) -> BoxFuture<'_, io::Result<std::process::ExitStatus>> {
        Box::pin(std::future::pending())
    }
    fn output_drained(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(std::future::ready(Ok(())))
    }
    fn ended(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

#[path = "../../../../kernel/tests/schedules.rs"]
mod schedules;

// Only the actor's explicit polls consume ingress. All other async jobs are
// polled by the script; native flush syscalls still run on the real runtime.
struct ScheduledRuntime {
    background: rt::Threads,
    captured: AtomicBool,
    actor: Mutex<Option<rt::BoxFuture<'static, ()>>>,
    tasks: Mutex<Vec<rt::BoxFuture<'static, ()>>>,
    flushes: Mutex<Vec<oneshot::Receiver<()>>>,
}

impl Runtime for ScheduledRuntime {
    fn spawn(&self, task: rt::BoxFuture<'static, ()>) {
        if self.captured.swap(true, Ordering::SeqCst) {
            lock(&self.tasks).push(task);
        } else {
            *lock(&self.actor) = Some(task);
        }
    }
    fn spawn_blocking(
        &self,
        _: rt::BlockingClass,
        _: usize,
        work: Box<dyn FnOnce() + Send>,
    ) -> rt::BoxFuture<'static, io::Result<()>> {
        work();
        Box::pin(std::future::ready(Ok(())))
    }
    fn spawn_flush(
        &self,
        work: Box<dyn FnOnce() + Send>,
    ) -> rt::BoxFuture<'static, io::Result<()>> {
        let (done, finished) = oneshot::channel();
        lock(&self.flushes).push(finished);
        self.background.spawn_flush(Box::new(move || {
            work();
            done.send(());
        }))
    }
    fn reactor(&self) -> rt::Reactor {
        self.background.reactor()
    }
    fn sleep(&self, _: Duration) -> rt::BoxFuture<'static, ()> {
        Box::pin(std::future::pending())
    }
}

impl ScheduledRuntime {
    fn poll_tasks(&self) -> usize {
        let tasks = std::mem::take(&mut *lock(&self.tasks));
        let count = tasks.len();
        let pending: Vec<_> = tasks
            .into_iter()
            .filter_map(|mut task| {
                task.as_mut()
                    .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                    .is_pending()
                    .then_some(task)
            })
            .collect();
        let mut tasks = lock(&self.tasks);
        let advanced = count - pending.len() + tasks.len();
        tasks.extend(pending);
        advanced
    }
    fn wait_flushes(&self) {
        let flushes = std::mem::take(&mut *lock(&self.flushes));
        for finished in flushes {
            assert_eq!(rt::block_on(finished), Some(()));
        }
    }
}

struct ScheduledActor {
    live: TestingLive,
    actor: Option<rt::BoxFuture<'static, ()>>,
    runtime: Arc<ScheduledRuntime>,
}

impl ScheduledActor {
    fn new(fixture: &Fixture, runtime: Arc<ScheduledRuntime>) -> Self {
        let live = fixture.create();
        let actor = lock(&runtime.actor).take().unwrap();
        let mut scheduled = Self {
            live,
            actor: Some(actor),
            runtime,
        };
        scheduled.poll();
        scheduled.runtime.poll_tasks();
        scheduled.runtime.wait_flushes();
        scheduled.runtime.poll_tasks();
        scheduled.poll();
        scheduled
    }
    fn poll(&mut self) {
        assert!(
            self.actor
                .as_mut()
                .unwrap()
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
    }
    fn position(&mut self) -> (u64, u64) {
        let (done, mut position) = oneshot::channel();
        self.live.send(Msg::Position(done));
        loop {
            self.poll();
            if let Poll::Ready(Some(position)) = std::pin::Pin::new(&mut position)
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
            {
                return position;
            }
            // IO and cancelled jobs may suspend a burst, but no new ingress is
            // released while draining these deterministic continuations.
            let advanced = self.runtime.poll_tasks();
            let flushing = !lock(&self.runtime.flushes).is_empty();
            self.runtime.wait_flushes();
            assert!(
                advanced != 0 || flushing,
                "batch ended with an unbound async continuation"
            );
        }
    }

    fn stop(&mut self) {
        if let Some(actor) = self.actor.take() {
            let tasks = std::mem::take(&mut *lock(&self.runtime.tasks));
            for task in tasks {
                self.runtime.background.spawn(task);
            }
            self.runtime.background.spawn(actor);
            self.live.halt();
        }
    }
}

impl Drop for ScheduledActor {
    fn drop(&mut self) {
        self.stop();
    }
}

#[test]
fn shutdown_consumes_flushes_in_the_suffix_after_a_prefetched_read_fence() {
    let dir = tempfile::tempdir().unwrap();
    let faulty = Arc::new(Faulty::new(Arc::new(
        crate::store::Jsonl::open(dir.path()).unwrap(),
    )));
    let runtime = Arc::new(ScheduledRuntime {
        background: rt::Threads::new(1),
        captured: AtomicBool::new(false),
        actor: Mutex::default(),
        tasks: Mutex::default(),
        flushes: Mutex::default(),
    });
    let fixture = Fixture::on(
        Arc::clone(&faulty) as _,
        ext_fixtures::ECHO,
        Registry::default(),
        openai(&Script::new(&[])),
    )
    .configure(|shared| shared.rt = runtime.clone());
    let mut scheduled = ScheduledActor::new(&fixture, Arc::clone(&runtime));
    let flush = faulty.hold_next_flush();
    scheduled
        .live
        .send(Msg::Step(Event::Environment(Environment {
            tty: true,
            ..Environment::default()
        })));
    scheduled.live.send(Msg::Sync);
    scheduled.poll();
    assert!(!lock(&runtime.flushes).is_empty());
    runtime.poll_tasks();
    flush.started();
    flush.release();
    runtime.wait_flushes();
    scheduled.live.send(Msg::Stop);
    scheduled.live.send(Msg::Sync);
    let (done, _position) = oneshot::channel();
    scheduled.live.send(Msg::Position(done));
    runtime.poll_tasks();
    let ended = scheduled
        .actor
        .as_mut()
        .unwrap()
        .as_mut()
        .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        .is_ready();
    // A failing assertion must not leave TestingLive's shutdown guard waiting.
    scheduled.live.ended.set();
    assert!(ended, "shutdown lost the completion behind the read fence");
    scheduled.actor.take();
}

#[test]
fn prefetched_mail_cannot_cross_read_fences_or_reorder_thawed_inputs() {
    for frozen in [false, true] {
        let runtime = Arc::new(ScheduledRuntime {
            background: rt::Threads::new(1),
            captured: AtomicBool::new(false),
            actor: Mutex::default(),
            tasks: Mutex::default(),
            flushes: Mutex::default(),
        });
        let fixture = Fixture::new(
            ext_fixtures::ECHO,
            Registry::default(),
            openai(&Script::new(&[])),
        )
        .configure(|shared| shared.rt = runtime.clone());
        let mut scheduled = ScheduledActor::new(&fixture, Arc::clone(&runtime));
        if frozen {
            let (done, frozen) = oneshot::channel();
            scheduled.live.send(Msg::Freeze(done));
            scheduled.poll();
            assert_eq!(rt::block_on(frozen), Some(Ok(())));
            for n in 0..BURST + 7 {
                let _ack = enqueue(&scheduled.live, &format!("held {n}"));
            }
            scheduled.poll();
            assert!(
                log_of(&scheduled.live)
                    .iter()
                    .all(|entry| !matches!(entry.body, Body::Input { .. }))
            );
            scheduled.live.thaw();
            scheduled.position();
        }
        let mut positions = Vec::new();
        for n in 0..BURST + 7 {
            let _ack = enqueue(&scheduled.live, &format!("mail {n}"));
            // Several inputs before a fence force the adaptive receive path.
            if n % 4 == 3 {
                let (done, position) = oneshot::channel();
                scheduled.live.send(Msg::Position(done));
                positions.push((n, position));
            }
        }
        scheduled.position();
        let log = log_of(&scheduled.live);
        let inputs: Vec<_> = log
            .iter()
            .filter(|entry| matches!(entry.body, Body::Input { .. }))
            .collect();
        let held = if frozen { BURST + 7 } else { 0 };
        assert_eq!(inputs.len(), held + BURST + 7);
        for (n, position) in positions {
            let (ordered, _) = rt::block_on(position).unwrap();
            assert!(inputs[held + n].seq.get() <= ordered);
            assert!(ordered < inputs[held + n + 1].seq.get());
        }
        for (n, entry) in inputs.iter().enumerate() {
            let Body::Input { segments, .. } = &entry.body else {
                unreachable!();
            };
            let expected = if n < held {
                format!("held {n}")
            } else {
                format!("mail {}", n - held)
            };
            assert_eq!(*segments, expected.as_str());
        }
        scheduled.stop();
    }
}

struct ScheduledSleep(Option<Arc<channel::Sender<StartedHarbor>>>);
impl Tool for ScheduledSleep {
    type In = ext_fixtures::Seconds;
    type Out = String;
    const NAME: &'static str = "sleep";
    const DESCRIPTION: &'static str = "A replay-safe held call.";
    const REPLAY: Replay = Replay::Safe;
    const CONCURRENCY: ext::Concurrency = ext::Concurrency::Shared;
    async fn call(&self, _: &Self::In, cx: &Cx) -> Result<String, String> {
        let Some(starts) = &self.0 else {
            return Ok("finished".into());
        };
        let (done, finished) = oneshot::channel();
        starts.send((cx.clone(), done)).ok().unwrap();
        Ok(finished.await.unwrap_or_default())
    }
}
impl Extension for ScheduledSleep {
    const NAME: &'static str = "scheduled-sleep";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.tool(Self(self.0.clone()));
    }
}

fn native_schedule(trace: &schedules::trace::Trace) -> bool {
    use schedules::trace::Decision;
    let at = |decision| trace.0.iter().position(|step| *step == decision);
    let (Some(cancel), Some(complete)) = (at(Decision::Cancel), at(Decision::Complete)) else {
        return false;
    };
    let early = trace
        .0
        .iter()
        .position(|s| matches!(s, Decision::Flush { epoch: 0, .. }));
    let late = trace
        .0
        .iter()
        .position(|s| matches!(s, Decision::Flush { epoch: 1, .. }));
    trace.validate().is_ok()
        && trace.0.starts_with(&[
            Decision::Input,
            Decision::Batch,
            Decision::Progress,
            Decision::Batch,
        ])
        && early
            .zip(late)
            .is_some_and(|(early, late)| cancel < complete && complete < early && late < early)
}

#[expect(
    clippy::too_many_lines,
    reason = "one arm per ingress choice and assertions at its explicit boundary"
)]
fn replay_held_epochs(trace: &schedules::trace::Trace) -> schedules::trace::Checked<Vec<Entry>> {
    use schedules::trace::{Decision, Failure, require};
    if !native_schedule(trace) {
        return Err(Failure::Invalid("unsupported native schedule".into()));
    }
    let dir = tempfile::tempdir().unwrap();
    let faulty = Arc::new(Faulty::new(Arc::new(
        crate::store::Jsonl::open(dir.path()).unwrap(),
    )));
    let (started, starts) = channel::unbounded();
    let runtime = Arc::new(ScheduledRuntime {
        background: rt::Threads::new(1),
        captured: AtomicBool::new(false),
        actor: Mutex::default(),
        tasks: Mutex::default(),
        flushes: Mutex::default(),
    });
    let fixture = Fixture::on(
        Arc::clone(&faulty) as _,
        ext_fixtures::ECHO,
        Registry::default()
            .with(&ScheduledSleep(Some(Arc::new(started))))
            .unwrap(),
        openai(&Script::new(&[])),
    )
    .configure(|shared| shared.rt = runtime.clone());
    let mut scheduled = ScheduledActor::new(&fixture, Arc::clone(&runtime));
    let before = scheduled.live.frames.seq();
    let fence = scheduled.live.session.fence.load(Ordering::SeqCst);
    let first = faulty.hold_next_flush();
    let second = faulty.hold_next_flush();
    let ack = enqueue(&scheduled.live, "sleep 1");
    scheduled.poll();
    runtime.poll_tasks();
    first.started();
    let (context, release) = starts
        .try_recv()
        .expect("input batch must start the safe tool after ordering");
    require(
        scheduled.live.frames.seq() == before,
        "safe work precedes durability",
    )?;
    let mut release = Some(release);
    let mut interrupted = None;
    let mut cancelled = false;
    let mut failed = false;
    let mut boundaries = vec![scheduled.position()];
    for group in trace
        .groups()
        .map_err(Failure::Invalid)?
        .into_iter()
        .skip(1)
    {
        for step in group {
            match *step {
                Decision::Progress => context.progress("ordered"),
                Decision::Cancel => {
                    let (reply, ack) = oneshot::channel();
                    scheduled.live.send(Msg::Request {
                        request: Request::Interrupt,
                        origin: None,
                        reply,
                    });
                    interrupted = Some(ack);
                }
                Decision::Complete => {
                    require(
                        context.cancel.is_set() == cancelled,
                        "cancel delivery crossed an unbound batch",
                    )?;
                    release.take().unwrap().send("finished".into());
                    runtime.poll_tasks();
                }
                Decision::Flush { epoch: 1, .. } => scheduled.live.send(Msg::Flushed {
                    fence,
                    through: boundaries[1].0,
                    result: Ok(()),
                }),
                Decision::Flush { epoch: 0, result } => {
                    failed = result == schedules::trace::Result::Failure;
                    if failed {
                        faulty.fail_next_sync(SyncFailure::Discard);
                    }
                    first.release();
                    second.release();
                    runtime.wait_flushes();
                    runtime.poll_tasks();
                }
                _ => {
                    return Err(Failure::Invalid(format!(
                        "unsupported native decision: {step:?}"
                    )));
                }
            }
        }
        // All messages in this group are queued before the only actor poll.
        if !matches!(group[0], Decision::Flush { .. }) {
            scheduled.live.send(Msg::Sync);
        }
        scheduled.poll();
        if group.contains(&Decision::Cancel) {
            cancelled = true;
            require(
                context.cancel.is_set(),
                "interrupt must revoke the context at its boundary",
            )?;
        }
        let position = scheduled.position();
        if matches!(group[0], Decision::Flush { epoch: 1, .. }) {
            require(
                position.1 == before && scheduled.live.frames.seq() == before,
                "later notification skipped an unresolved prefix",
            )?;
        }
        boundaries.push(position);
    }
    require(
        rt::block_on(ack).unwrap().is_err() == failed,
        "input acknowledgment disagrees with flush result",
    )?;
    require(
        rt::block_on(interrupted.unwrap()).unwrap().is_err() == failed,
        "interrupt acknowledgment disagrees with flush result",
    )?;
    require(!scheduled.live.has_work(), "trace left owned work")?;
    let log = log_of(&scheduled.live);
    require(
        if failed {
            scheduled.live.frames.seq() == before
        } else {
            log.iter()
                .filter(|e| matches!(e.body, Body::ToolResult { .. }))
                .count()
                == 1
        },
        "wrong settled prefix",
    )?;
    faulty.cut();
    scheduled.stop();
    require(
        log == faulty.entries(scheduled.live.id).unwrap(),
        "publication differs from durable prefix",
    )?;
    Ok(log)
}

fn expected_recovery(
    registry: Arc<dyn Binding>,
    prefix: &[Entry],
    trace: &schedules::trace::Trace,
) -> (Core, Vec<Entry>) {
    let (mut expected, plan) = Core::resume(registry, prefix, &Load::default());
    let mut expected_log = prefix.to_vec();
    expected_log.extend(plan.appends.iter().cloned());
    let mut work: VecDeque<_> = plan
        .immediate
        .into_iter()
        .chain(plan.admitted.into_iter().map(|g| g.work))
        .collect();
    while let Some(effect) = work.pop_front() {
        let Effect::RunTool { call, tool, .. } = effect else {
            assert!(
                matches!(effect, Effect::MarkBusy(_)),
                "unbound recovery effect: {effect:?}; {trace}"
            );
            continue;
        };
        assert_eq!(tool, "sleep", "{trace}");
        let plan = expected.step(Event::ToolDone {
            call,
            outcome: Outcome::Done {
                output: Json::encode(&"finished").unwrap().into(),
            },
            card: None,
            held: Vec::new(),
        });
        expected_log.extend(plan.appends.iter().cloned());
        work.extend(
            plan.immediate
                .into_iter()
                .chain(plan.admitted.into_iter().map(|g| g.work)),
        );
    }
    assert!(
        expected.at_rest(),
        "expected recovery did not settle: {trace}"
    );
    (expected, expected_log)
}

fn assert_native_recovery(fixture: &Fixture, prefix: &[Entry], trace: &schedules::trace::Trace) {
    let session = fixture.start(prefix);
    let final_log = log_of(&session);
    assert!(final_log.len() >= prefix.len(), "prefix lost: {trace}");
    assert_eq!(
        facts(&final_log[..prefix.len()]),
        facts(prefix),
        "prefix changed: {trace}"
    );
    let registry = fixture.0.registry.get();
    let (expected, expected_log) =
        expected_recovery(bind(&registry, &session.session), prefix, trace);
    let inputs = prefix
        .iter()
        .filter(|e| matches!(e.body, Body::Input { .. }))
        .count();
    let calls: Vec<_> = final_log
        .iter()
        .filter_map(|e| matches!(e.body, Body::ToolCall { .. }).then_some(CallId(e.seq)))
        .collect();
    for entry in &final_log {
        if let Body::ToolCall { tool, input, .. } = &entry.body {
            assert_eq!(tool, "sleep", "wrong recovered tool: {trace}");
            assert_eq!(
                *input,
                Json::encode(&json!({"seconds": 1})).unwrap(),
                "wrong recovered input: {trace}"
            );
        }
    }
    let results: Vec<_> = final_log
        .iter()
        .filter_map(|e| match &e.body {
            Body::ToolResult { call, outcome, .. } => Some((*call, outcome)),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls.len(),
        inputs,
        "retained input lost its intent: {trace}"
    );
    assert_eq!(
        results.len(),
        inputs,
        "retained input lost its result: {trace}"
    );
    if inputs != 0 {
        assert_eq!(
            results,
            [(
                calls[0],
                &Outcome::Done {
                    output: Json::encode(&"finished").unwrap().into()
                }
            )],
            "wrong settled call or outcome: {trace}"
        );
    }
    assert_eq!(
        facts(&final_log),
        facts(&expected_log),
        "recovered facts differ from expected trace: {trace}"
    );
    assert_eq!(
        serde_json::to_value(Ledger::of(&final_log)).unwrap(),
        serde_json::to_value(expected.ledger()).unwrap(),
        "wrong final folded state: {trace}"
    );
    let (resumed, plan) = Core::resume(
        bind(&registry, &session.session),
        &final_log,
        &Load::default(),
    );
    assert!(
        resumed.at_rest() && plan.appends.is_empty() && plan.admitted.is_empty(),
        "settled recovery produced work: {trace}"
    );
    session.halt();
}

#[test]
fn scripted_epoch_delivery_and_crash_cuts_preserve_native_admission() {
    use schedules::trace::{Decision, Result, Trace};
    let traces = match std::env::var("A_NATIVE_SCHEDULE_TRACE") {
        Ok(text) => vec![text.parse::<Trace>().unwrap()],
        Err(_) => [Result::Success, Result::Failure]
            .into_iter()
            .flat_map(|result| {
                schedules::trace::held_epochs(result)
                    .into_iter()
                    .filter(native_schedule)
            })
            .collect(),
    };
    for trace in traces {
        trace.validate().unwrap();
        let selected_cut = trace.0.iter().find_map(|s| {
            if let Decision::Crash { cut } = s {
                Some(*cut)
            } else {
                None
            }
        });
        let trace = Trace(
            trace
                .0
                .into_iter()
                .filter(|s| !matches!(s, Decision::Crash { .. }))
                .collect(),
        );
        eprintln!(
            "replay: A_NATIVE_SCHEDULE_TRACE='{trace}' cargo test -p engine scripted_epoch_delivery -- --nocapture"
        );
        let log = replay_held_epochs(&trace).unwrap_or_else(|error| {
            let minimal = schedules::trace::shrink(trace.clone(), |candidate| {
                native_schedule(candidate)
                    && matches!(
                        replay_held_epochs(candidate),
                        Err(schedules::trace::Failure::Invariant(_))
                    )
            });
            panic!("{error}; native replay: {minimal}");
        });
        for cut in selected_cut.map_or(0..=log.len(), |cut| cut..=cut) {
            assert!(cut <= log.len());
            let mut crashed = trace.clone();
            crashed.0.push(Decision::Crash { cut });
            eprintln!(
                "replay: A_NATIVE_SCHEDULE_TRACE='{crashed}' cargo test -p engine scripted_epoch_delivery -- --nocapture"
            );
            let dir = tempfile::tempdir().unwrap();
            let fixture = Fixture::on(
                Arc::new(crate::store::Jsonl::open(dir.path()).unwrap()),
                ext_fixtures::ECHO,
                Registry::default().with(&ScheduledSleep(None)).unwrap(),
                openai(&Script::new(&[])),
            );
            assert_native_recovery(&fixture, &log[..cut], &crashed);
        }
    }
}
