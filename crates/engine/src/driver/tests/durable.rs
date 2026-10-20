//! What clients are shown against what a power cut keeps.

use crate::store::Faulty;

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
    let faulty = Arc::new(Faulty::new(Arc::new(Memory::default())));
    let fixture = Fixture::on(
        Arc::clone(&faulty) as _,
        ext_fixtures::ECHO,
        Registry::default().with(&ext_fixtures::Fixtures).unwrap(),
        openai(&Script::new(&[])),
    );
    let session = fixture.create();
    session.submit("sleep 1").unwrap();
    let log = until(&session, "sleep started", tool_running);
    let call = log
        .iter()
        .find(|e| matches!(e.body, Body::ToolCall { .. }))
        .unwrap()
        .seq
        .get();
    let before = session.frames.seq();
    faulty.fail_next_sync();
    let (done, waited) = oneshot::channel();
    session
        .session
        .mail
        .send(Msg::Note {
            call: id(call),
            note: Note::State {
                ext: "test".into(),
                state: "value".into(),
                data: Json::encode(&"lost").unwrap(),
            },
            done: Some(done),
        })
        .ok()
        .unwrap();
    assert!(
        rt::block_on(waited).is_none(),
        "a failed sync cannot acknowledge the note"
    );
    session.barrier();
    session.interrupt_now();
    session.submit("after").unwrap();
    session.barrier();
    settle(&session);
    let kept = faulty.entries(session.id).unwrap();
    assert!(kept.iter().all(|e| !matches!(e.body, Body::Ext { .. })));
    assert!(kept.len() as u64 > before);
    for (seq, entry) in (1..).zip(&kept) {
        assert_eq!(entry.seq.get(), seq, "recovery must not leave a hole");
    }
    assert_eq!(log_of(&session), kept);
    session.halt();
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
        if !self.captured.swap(true, Ordering::SeqCst) {
            *lock(&self.actor) = Some(task);
        } else {
            self.background.spawn(task);
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

    fn finish(self) -> Vec<Entry> {
        self.release.send(());
        self.session.send(Msg::Stop);
        rt::block_on(self.actor);
        let kept = self.faulty.entries(self.session.id).unwrap();
        assert_eq!(log_of(&self.session), kept);
        kept
    }
}

#[test]
fn a_failed_sync_drops_acknowledgements_from_the_same_burst() {
    let mut harness = ManualSession::new();
    harness.faulty.fail_next_sync();
    let waited = harness.note();
    harness.session.send(Msg::Sync);
    harness.poll();
    let acknowledged = rt::block_on(waited);
    let kept = harness.finish();
    assert!(kept.iter().all(|e| !matches!(e.body, Body::Ext { .. })));
    assert!(
        acknowledged.is_none(),
        "a failed sync cannot acknowledge the burst's note"
    );
}

fn poll_actor(actor: &mut rt::BoxFuture<'static, ()>) {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
}
