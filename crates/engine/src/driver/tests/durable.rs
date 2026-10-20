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
