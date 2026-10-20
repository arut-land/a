//! What clients are shown against what a power cut keeps.

use crate::store::{Faulty, SyncFailure};

use super::*;

/// The entries `outbox` is sent, until `done` holds of them.
fn shown(outbox: &Outbox, done: impl Fn(&[Entry]) -> bool) -> Vec<Entry> {
    let mut seen = Vec::new();
    let reading = async {
        while !done(&seen) {
            let frame = outbox.next().await.expect("the follower is open");
            if let Ok(Some(DaemonMsg::Entry(e))) =
                protocol::read_frame::<DaemonMsg>(&mut frame.as_bytes())
            {
                seen.push(e);
            }
        }
    };
    rt::block_on(or(reading, async {
        rt::Reactor::AsyncIo.sleep(PATIENCE).await;
        panic!("never shown");
    }));
    seen
}

/// A seq a client has seen is never given out again: nothing is shown
/// before it is durable, so what a power cut loses was never seen. Output
/// a command streams is written without a sync, and shown once synced.
#[test]
fn a_seq_shown_to_a_client_survives_a_power_cut() {
    let faulty = Arc::new(Faulty::new(Arc::new(Memory::default())));
    let registry = Registry::default().with(&ext_shell::Shell).unwrap();
    let store: Arc<dyn LogStore> = Arc::clone(&faulty) as _;
    let fixture = Fixture::on(
        store,
        ext_fixtures::ECHO,
        registry,
        openai(&Script::new(&[])),
    );
    let session = fixture.create();
    let outbox = Outbox::new();
    session.follow(&outbox, 0);
    session.submit("$ echo one; sleep 30").unwrap();
    let seen = shown(&outbox, |seen| {
        seen.iter().any(|e| matches!(e.body, Body::Progress { .. }))
    });
    faulty.cut();
    session.halt();

    let kept = faulty.entries(session.id).unwrap();
    for e in &seen {
        let same = kept.iter().find(|k| k.seq == e.seq);
        assert_eq!(
            same.map(|k| &k.body),
            Some(&e.body),
            "seq {} vanished",
            e.seq
        );
    }
}

#[test]
fn a_failed_deferred_sync_recovers_before_accepting_more_work() {
    for failure in [SyncFailure::Discard, SyncFailure::Retain] {
        let mut harness = ManualSession::new();
        let before = harness.session.frames.seq();
        let mut waited = harness.note();
        harness.poll();
        assert_eq!(
            harness.session.frames.seq(),
            before,
            "unsynced bytes must stay unseen"
        );
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::pin::Pin::new(&mut waited).poll(&mut cx).is_pending());
        harness.faulty.fail_next_sync(failure);
        harness.session.send(Msg::Sync);
        harness.poll();
        let acknowledged = rt::block_on(waited);
        harness.request("after");
        let kept = harness.finish();
        assert!(
            acknowledged.is_none(),
            "a failed sync cannot acknowledge the note"
        );
        assert_retained(&kept, failure);
        assert!(
            kept.iter()
                .any(|e| matches!(&e.body, Body::Assistant { text } if text == "after"))
        );
    }
}

/// The actor runs only when polled by the test; I/O completes inline and
/// deferred timers never fire, so queued messages define exact bursts.
struct Manual {
    background: rt::Threads,
    captured: AtomicBool,
    actor: Mutex<Option<rt::BoxFuture<'static, ()>>>,
}

impl rt::Runtime for Manual {
    fn spawn(&self, task: rt::BoxFuture<'static, ()>) {
        if self.captured.swap(true, Ordering::SeqCst) {
            self.background.spawn(task);
        } else {
            *lock(&self.actor) = Some(task);
        }
    }

    fn spawn_blocking(&self, work: Box<dyn FnOnce() + Send>) {
        work();
    }

    fn reactor(&self) -> rt::Reactor {
        rt::Reactor::AsyncIo
    }

    fn sleep(&self, _: Duration) -> rt::BoxFuture<'static, ()> {
        Box::pin(std::future::pending())
    }
}

struct ControlledSleep(Arc<channel::Sender<oneshot::Sender<()>>>);

impl Tool for ControlledSleep {
    type In = ext_fixtures::Seconds;
    type Out = String;
    const NAME: &'static str = "sleep";
    const DESCRIPTION: &'static str = "Wait for the test to release this call.";

    async fn call(&self, _: &Self::In, cx: &Cx) -> Result<String, String> {
        let (release, released) = oneshot::channel();
        self.0.send(release).ok().unwrap();
        unless(&cx.cancel, released)
            .await
            .ok_or_else(|| "stopped".to_owned())?;
        Ok("released".into())
    }
}

impl Extension for ControlledSleep {
    const NAME: &'static str = "controlled";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(ControlledSleep(self.0.clone()));
    }
}

struct ManualSession {
    session: Arc<Live>,
    actor: rt::BoxFuture<'static, ()>,
    faulty: Arc<Faulty>,
    release: oneshot::Sender<()>,
    call: CallId,
}

impl ManualSession {
    fn new() -> Self {
        let faulty = Arc::new(Faulty::new(Arc::new(Memory::default())));
        let (started, starts) = channel::unbounded();
        let mut fixture = Fixture::on(
            Arc::clone(&faulty) as _,
            ext_fixtures::ECHO,
            Registry::default()
                .with(&ControlledSleep(Arc::new(started)))
                .unwrap(),
            openai(&Script::new(&[])),
        );
        let manual = Arc::new(Manual {
            background: rt::Threads::new(1),
            captured: AtomicBool::new(false),
            actor: Mutex::default(),
        });
        Arc::get_mut(&mut fixture.0).unwrap().rt = Arc::clone(&manual) as _;
        let session = fixture.create();
        let mut actor = lock(&manual.actor).take().unwrap();
        let (reply, received) = oneshot::channel();
        session.send(Msg::Request {
            request: Request::Input {
                text: "sleep 0".into(),
                mode: Mode::Queue,
            },
            origin: None,
            reply,
        });
        poll_actor(&mut actor);
        assert_eq!(rt::block_on(received), Some(Ok(())));
        let release = rt::block_on(starts.recv()).unwrap();
        let call = id(session
            .frames
            .entries()
            .find(|e| matches!(e.body, Body::ToolCall { .. }))
            .unwrap()
            .seq
            .get());
        Self {
            session,
            actor,
            faulty,
            release,
            call,
        }
    }

    fn poll(&mut self) {
        poll_actor(&mut self.actor);
    }

    fn note(&self) -> oneshot::Receiver<()> {
        let (done, waited) = oneshot::channel();
        self.session.send(Msg::Note {
            call: self.call,
            note: Note::State {
                ext: "test".into(),
                state: "value".into(),
                data: Json::encode(&"pending").unwrap(),
            },
            done: Some(done),
        });
        waited
    }

    fn request(&mut self, text: &str) {
        let (reply, received) = oneshot::channel();
        self.session.send(Msg::Request {
            request: Request::Input {
                text: text.into(),
                mode: Mode::Queue,
            },
            origin: None,
            reply,
        });
        self.poll();
        assert_eq!(rt::block_on(received), Some(Ok(())));
    }

    fn finish(self) -> Vec<Entry> {
        self.release.send(());
        self.session.send(Msg::Stop);
        rt::block_on(self.actor);
        let kept = self.faulty.entries(self.session.id).unwrap();
        assert_eq!(log_of(&self.session), kept);
        for (seq, entry) in (1..).zip(&kept) {
            assert_eq!(entry.seq.get(), seq, "recovery must not leave a hole");
        }
        kept
    }
}

#[test]
fn a_failed_sync_drops_acknowledgements_from_the_same_burst() {
    for failure in [SyncFailure::Discard, SyncFailure::Retain] {
        let mut harness = ManualSession::new();
        harness.faulty.fail_next_sync(failure);
        let waited = harness.note();
        harness.session.send(Msg::Sync);
        harness.poll();
        let acknowledged = rt::block_on(waited);
        harness.request("after");
        let kept = harness.finish();
        assert_retained(&kept, failure);
        assert!(
            acknowledged.is_none(),
            "a failed sync cannot acknowledge the burst's note"
        );
    }
}

#[test]
fn a_failed_write_drops_acknowledgements_from_the_same_burst() {
    let mut harness = ManualSession::new();
    harness.faulty.fail_next_write();
    let waited = harness.note();
    harness.session.send(Msg::Sync);
    harness.poll();
    let acknowledged = rt::block_on(waited);
    harness.request("after");
    let kept = harness.finish();
    assert_retained(&kept, SyncFailure::Discard);
    assert!(
        kept.iter()
            .any(|e| matches!(&e.body, Body::Assistant { text } if text == "after"))
    );
    assert!(
        acknowledged.is_none(),
        "a failed write cannot acknowledge the burst's lost note"
    );
}

fn assert_retained(kept: &[Entry], failure: SyncFailure) {
    let notes: Vec<_> = kept
        .iter()
        .filter_map(|e| match &e.body {
            Body::Ext { data, .. } => Some(data.parse::<String>().unwrap()),
            _ => None,
        })
        .collect();
    let expected = match failure {
        SyncFailure::Discard => vec![],
        SyncFailure::Retain => vec!["pending".to_owned()],
    };
    assert_eq!(notes, expected);
}

fn poll_actor(actor: &mut rt::BoxFuture<'static, ()>) {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
}

#[test]
fn a_barrier_without_its_flush_neither_publishes_nor_starts_an_external_effect() {
    let faulty = Arc::new(Faulty::new(Arc::new(Memory::default())));
    let (started, starts) = channel::unbounded();
    let fixture = Fixture::on(
        Arc::clone(&faulty) as _,
        ext_fixtures::ECHO,
        Registry::default()
            .with(&ControlledSleep(Arc::new(started)))
            .unwrap(),
        openai(&Script::new(&[])),
    );
    let session = fixture.create();
    settle(&session);
    let before = log_of(&session);
    faulty.cut_before_next_flush();
    assert!(session.submit("sleep 0").is_err());
    session.halt();
    assert_eq!(faulty.entries(session.id).unwrap(), before);
    assert_eq!(
        log_of(&session),
        before,
        "a failed promise cannot publish a seq"
    );
    assert!(starts.try_recv().is_none());
}
