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
    starts: channel::Receiver<oneshot::Sender<()>>,
    call: CallId,
}

impl ManualSession {
    fn new() -> Self {
        Self::with_registry(Registry::default())
    }

    fn with_registry(registry: Registry) -> Self {
        let faulty = Arc::new(Faulty::new(Arc::new(Memory::default())));
        let (started, starts) = channel::unbounded();
        let mut fixture = Fixture::on(
            Arc::clone(&faulty) as _,
            ext_fixtures::ECHO,
            registry.with(&ControlledSleep(Arc::new(started))).unwrap(),
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
            starts,
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

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Updates(u64);
impl ext::Fold for Updates {
    const NAME: &'static str = "value";
    type Change = String;
    fn apply(&mut self, _: &String) {
        self.0 += 1;
    }
}
struct CountingState;
impl Extension for CountingState {
    const NAME: &'static str = "test";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.fold::<Updates>();
    }
}

#[test]
fn pending_fold_changes_survive_reload_exactly_once() {
    let make = || Registry::default().with(&CountingState).unwrap();
    let mut harness = ManualSession::with_registry(make());
    let registry = harness.session.session.shared.registry.get();
    let cx = harness
        .session
        .session
        .cx(harness.call, &registry, Cancel::default());
    let _ack = harness.note();
    harness.poll();
    assert_eq!(cx.state(|s: &Updates| s.0), 1);
    harness.session.session.shared.registry.replace(make());
    harness
        .session
        .send(Msg::Step(Event::Environment(Environment::default())));
    harness.poll();
    assert_eq!(
        cx.state(|s: &Updates| s.0),
        1,
        "a running call keeps its state view"
    );
    assert_eq!(
        lock(&harness.session.session.state_view())
            .get::<Updates>()
            .unwrap()
            .0,
        1,
        "reload cannot forget a folded but unsynced change"
    );
    let old = harness.session.session.state_view();
    let _second = harness.note();
    harness.poll();
    harness.session.send(Msg::Step(Event::ToolDone {
        call: harness.call,
        outcome: Outcome::Done {
            output: Json::default(),
        },
        card: None,
    }));
    harness.request("adopt");
    let adopted = harness.session.session.state_view();
    assert!(
        !Arc::ptr_eq(&old, &adopted),
        "adoption switches the state view at the kernel boundary"
    );
    assert_eq!(
        lock(&adopted).get::<Updates>().unwrap().0,
        2,
        "the prepared generation includes each pending change exactly once"
    );
    harness.finish();
}

struct ReloadFold {
    version: u32,
    multiplier: u64,
    reject_second: bool,
}
impl ext::DynExtension for ReloadFold {
    fn name(&self) -> ext::Name {
        "test".into()
    }
    fn version(&self) -> u32 {
        self.version
    }
    fn contribute(&self, r: &mut Registrar) {
        r.dyn_fold(Arc::new(Self { ..*self }));
    }
}
impl ext::DynFold for ReloadFold {
    fn name(&self) -> ext::Name {
        "value".into()
    }
    fn initial(&self) -> Json {
        Json::encode(&0_u64).unwrap()
    }
    fn apply(&self, state: &Json, _: &Json) -> Result<Json, String> {
        let value = state.parse::<u64>().unwrap();
        if self.reject_second && value > 0 {
            return Err("second change rejected".into());
        }
        Ok(Json::encode(&(value + self.multiplier)).unwrap())
    }
}
fn reload_registry(version: u32, multiplier: u64, reject_second: bool) -> Registry {
    Registry::default()
        .with(&Agents)
        .unwrap()
        .with_dyn(
            ext::Tier::User,
            &ReloadFold {
                version,
                multiplier,
                reject_second,
            },
        )
        .unwrap()
}
impl ManualSession {
    fn replace(&self, registry: Registry) {
        self.session.session.shared.registry.replace(registry);
    }
    fn complete(&self) {
        self.session.send(Msg::Step(Event::ToolDone {
            call: self.call,
            outcome: Outcome::Done {
                output: Json::default(),
            },
            card: None,
        }));
    }
    fn value(&self) -> u64 {
        lock(&self.session.session.state_view())
            .erased("test", "value")
            .unwrap()
            .parse()
            .unwrap()
    }
    fn snapshot(&mut self) {
        self.request(&"x".repeat(usize::try_from(SNAPSHOT_EVERY).unwrap()));
    }
    fn assert_replay(&self, registry: &Registry) {
        let mut states = registry.states();
        states.replay(log_of(&self.session));
        assert_eq!(
            self.value(),
            states
                .erased("test", "value")
                .unwrap()
                .parse::<u64>()
                .unwrap()
        );
    }
}

#[test]
fn a_restored_fold_replays_changes_missing_from_the_checkpoint() {
    let mut h = ManualSession::with_registry(reload_registry(1, 1, false));
    let _ack = h.note();
    h.complete();
    h.snapshot();
    h.replace(Registry::default().with(&Agents).unwrap());
    h.snapshot();
    h.replace(reload_registry(1, 1, false));
    h.request("adopt");
    h.assert_replay(&reload_registry(1, 1, false));
    h.finish();
}

#[test]
fn a_changed_reducer_replays_the_checkpoint_prefix() {
    let mut h = ManualSession::with_registry(reload_registry(1, 1, false));
    let _ack = h.note();
    h.complete();
    h.snapshot();
    h.replace(reload_registry(2, 2, false));
    h.request("adopt");
    h.assert_replay(&reload_registry(2, 2, false));
    h.finish();
}

#[test]
fn a_candidate_rejecting_a_later_change_is_never_adopted() {
    let mut h = ManualSession::with_registry(reload_registry(1, 1, false));
    let _first = h.note();
    h.poll();
    h.replace(reload_registry(2, 1, true));
    h.session
        .send(Msg::Step(Event::Environment(Environment::default())));
    h.poll();
    let _second = h.note();
    h.poll();
    h.complete();
    h.request("adopt");
    assert_eq!(h.value(), 2);
    h.request("still valid");
    assert_eq!(h.value(), 2, "a failed candidate cannot remain pending");
    h.finish();
}

#[test]
fn the_old_view_receives_its_final_note_before_adoption() {
    let mut h = ManualSession::with_registry(reload_registry(1, 1, false));
    let _first = h.note();
    h.poll();
    let old = h.session.session.state_view();
    h.replace(reload_registry(2, 1, false));
    h.session
        .send(Msg::Step(Event::Environment(Environment::default())));
    h.poll();
    let _last = h.note();
    h.complete();
    h.request("adopt");
    assert_eq!(h.value(), 2);
    assert_eq!(
        lock(&old)
            .erased("test", "value")
            .unwrap()
            .parse::<u64>()
            .unwrap(),
        2
    );
    h.finish();
}

#[test]
fn a_busy_burst_keeps_one_generation_across_completed_calls() {
    let mut h = ManualSession::with_registry(reload_registry(1, 1, false));
    let old = h.session.session.state_view();
    let (started, starts) = channel::unbounded();
    h.replace(
        reload_registry(2, 1, false)
            .with(&ControlledSleep(Arc::new(started)))
            .unwrap(),
    );
    h.request("sleep 0");
    h.complete();
    h.poll();
    let unchanged = Arc::ptr_eq(&old, &h.session.session.state_view());
    let second = rt::block_on(or(h.starts.recv(), starts.recv())).unwrap();
    h.finish();
    drop(second);
    assert!(
        unchanged,
        "queued work prevents adoption even between calls in one burst"
    );
}

struct Superseding(Weak<Shared>);
impl ext::DynExtension for Superseding {
    fn name(&self) -> ext::Name {
        "test".into()
    }
    fn version(&self) -> u32 {
        2
    }
    fn contribute(&self, r: &mut Registrar) {
        r.dyn_fold(Arc::new(Self(self.0.clone())));
    }
}
impl ext::DynFold for Superseding {
    fn name(&self) -> ext::Name {
        "value".into()
    }
    fn initial(&self) -> Json {
        Json::encode(&0_u64).unwrap()
    }
    fn apply(&self, state: &Json, _: &Json) -> Result<Json, String> {
        self.0
            .upgrade()
            .unwrap()
            .registry
            .replace(reload_registry(3, 3, false));
        Ok(Json::encode(&(state.parse::<u64>().unwrap() + 2)).unwrap())
    }
}

#[test]
fn a_candidate_superseded_during_rebuild_is_never_published() {
    let mut h = ManualSession::with_registry(reload_registry(1, 1, false));
    let _note = h.note();
    h.complete();
    h.request("rest");
    h.replace(
        Registry::default()
            .with(&Agents)
            .unwrap()
            .with_dyn(
                ext::Tier::User,
                &Superseding(Arc::downgrade(&h.session.session.shared)),
            )
            .unwrap(),
    );
    h.request("adopt");
    assert!(!log_of(&h.session).iter().any(|e| matches!(&e.body,
        Body::Extensions { installed } if installed.iter().any(|i| i.name == "test" && i.version == 2))),
        "superseded candidates cannot reach the kernel");
    assert_eq!(h.value(), 3);
    h.finish();
}

#[test]
fn a_session_reopens_after_a_rejected_reduction() {
    let mut h = ManualSession::with_registry(reload_registry(1, 1, true));
    let _first = h.note();
    let _rejected = h.note();
    h.complete();
    h.request("still usable");
    assert_eq!(h.value(), 1);
    let shared = Arc::clone(&h.session.session.shared);
    let header = Header {
        id: h.session.id,
        workspace: h.session.workspace.clone(),
        parent: None,
    };
    h.finish();
    let loaded = Live::start(&header, &shared, Open::Load, Owner::None)
        .unwrap_or_else(|e| panic!("a skipped reduction cannot poison reopen: {e}"));
    loaded.submit("after reopen").unwrap();
    settle(&loaded);
    assert_eq!(
        lock(&loaded.session.state_view())
            .erased("test", "value")
            .unwrap()
            .parse::<u64>()
            .unwrap(),
        1
    );
    assert!(
        log_of(&loaded)
            .iter()
            .any(|e| matches!(&e.body, Body::Assistant { text } if text == "after reopen"))
    );
    loaded.halt();
}

#[test]
fn a_rejected_reduction_does_not_stop_deferred_write_recovery() {
    let mut h = ManualSession::with_registry(reload_registry(1, 1, true));
    let _first = h.note();
    let rejected = h.note();
    h.session.send(Msg::Sync);
    h.poll();
    assert_eq!(rt::block_on(rejected), Some(()));
    assert_eq!(h.value(), 1);
    h.faulty.fail_next_write();
    let lost = h.note();
    // Recovery of this unsafe call settles it instead of starting it again.
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let polled = h.actor.as_mut().poll(&mut cx);
    assert!(
        polled.is_pending(),
        "recovery must keep serving after a skipped reduction"
    );
    assert_eq!(rt::block_on(lost), None);
    assert_eq!(h.value(), 1);
    h.request("after recovery");
    let kept = h.finish();
    assert!(
        kept.iter()
            .any(|e| matches!(&e.body, Body::Assistant { text } if text == "after recovery"))
    );
}

#[test]
fn a_rejected_reduction_has_the_same_outcome_at_every_snapshot_position() {
    let mut h = ManualSession::with_registry(reload_registry(1, 1, true));
    let registry = h.session.session.shared.registry.get();
    let mut entries = Vec::new();
    let mut checkpoints = vec![None];
    // Each turn rests after one reduction, allowing a checkpoint before
    // the rejection, after it, or no checkpoint at all over the same log.
    for _ in 0..2 {
        let _note = h.note();
        h.complete();
        h.request("rest");
        entries = log_of(&h.session);
        let image = Image {
            build: h.session.session.shared.build.clone(),
            extensions: registry.installed(),
            has_input: h.session.has_input(),
            ledger: Ledger::of(&entries),
            states: lock(&h.session.session.state_view()).image().unwrap(),
        };
        checkpoints.push(Some(Snapshot {
            covers: entries.last().unwrap().seq.get(),
            image: Json::encode(&image).unwrap(),
        }));
        if checkpoints.len() == 2 {
            h.request("sleep 0");
            h.call = id(log_of(&h.session)
                .iter()
                .rev()
                .find(|e| matches!(e.body, Body::ToolCall { .. }))
                .unwrap()
                .seq
                .get());
            let release = rt::block_on(h.starts.recv()).unwrap();
            h.release.send(());
            h.release = release;
        }
    }
    let shared = Arc::clone(&h.session.session.shared);
    let binding = bind(&registry, &h.session.session) as Arc<dyn Binding>;
    let header = Header {
        id: h.session.id,
        workspace: h.session.workspace.clone(),
        parent: None,
    };
    h.finish();
    let mut outcomes = Vec::new();
    for snapshot in checkpoints {
        if let Some(snapshot) = snapshot {
            let mut loaded = shared
                .store
                .load(header.id, &mut |_| false, &mut |_| {})
                .unwrap();
            loaded.writer.snapshot(&snapshot).unwrap();
        }
        let rebuilt = Folding::open(&header, &shared, Open::Load, &binding, &registry);
        outcomes.push(
            rebuilt
                .map(|(_, f)| {
                    (
                        f.states
                            .erased("test", "value")
                            .unwrap()
                            .parse::<u64>()
                            .unwrap(),
                        f.core.ledger().seq(),
                        f.has_input,
                    )
                })
                .map_err(|e| e.to_string()),
        );
    }
    let expected = Ok((1, entries.last().unwrap().seq.get(), true));
    assert!(
        outcomes.iter().all(|outcome| outcome == &expected),
        "snapshot position changed replay: {outcomes:?}"
    );
}
