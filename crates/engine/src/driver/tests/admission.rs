//! Ordering, publication and replay-safe admission across flush epochs.

use super::*;
use crate::store::{Faulty, SyncFailure};

struct NoTimers(rt::Threads);

impl rt::Runtime for NoTimers {
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
    let history: Vec<_> = (1..=SNAPSHOT_GAP + 1)
        .map(|seq| entry(seq, echo("earlier")))
        .collect();
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
                DaemonMsg::Snapshot(_) => snapshot = true,
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
        assert_eq!(snapshot, since == 0);
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
