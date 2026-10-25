//! A session's side of a handover: frozen, it takes nothing up, so no turn
//! starts between the daemon's asking every session and its commit.

use super::*;

/// Input that comes while the session is frozen for a handover starts no
/// turn: it waits, and is taken up in order once the handover is called
/// off. A session at work is never frozen.
#[test]
fn a_turn_never_starts_while_a_session_is_frozen_for_a_handover() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        echoing(&Policy::default()),
        openai(&Script::new(&[])),
    );
    let session = Arc::new(fixture.start(&[]));
    session.submit("before").unwrap();
    settle(&session);
    rt::block_on(session.freeze()).unwrap();

    let submitting = Arc::clone(&session);
    let submitted = thread::spawn(move || submitting.submit("while frozen"));
    thread::sleep(Duration::from_millis(200));
    let said = |log: &[Entry]| {
        log.iter().any(|e| {
            e.body
                == Body::Assistant {
                    text: "while frozen".into(),
                }
        })
    };
    assert!(!said(&log_of(&session)), "a turn ran while frozen");
    session.thaw();
    submitted.join().unwrap().unwrap();
    until(&session, "the turn after the thaw", said);

    session.submit("$ sleep 3").unwrap();
    session.wait("the command to run", PATIENCE, || session.has_work());
    let refused = rt::block_on(session.freeze());
    assert!(refused.is_err_and(|why| why.contains("at work")));
    session.halt();
}

/// What comes while a session is frozen is refused as it commits, never
/// swallowed: the client is told to send it again, to the newer build.
#[test]
fn input_held_back_by_a_handover_is_refused_as_it_commits() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        echoing(&Policy::default()),
        openai(&Script::new(&[])),
    );
    let session = Arc::new(fixture.start(&[]));
    rt::block_on(session.freeze()).unwrap();
    let submitting = Arc::clone(&session);
    let submitted = thread::spawn(move || submitting.submit("while frozen"));
    thread::sleep(Duration::from_millis(200));
    assert!(rt::block_on(session.commit()).is_empty());
    let refused = submitted.join().unwrap();
    assert_eq!(refused, Err(crate::upgrade::DRAINING.to_owned()));
    assert_eq!(
        session.submit("after the commit"),
        Err(crate::upgrade::DRAINING.to_owned())
    );
    session.halt();
}
