//! A shell command past its timeout, through the daemon: it goes on in the
//! background as a forced steer sends it, and its whole output comes back
//! on the queue.

use std::fmt::Write as _;
use std::io::Read as _;

use protocol::{BlobRef, EnvelopeId, Held, OutputStream};

use super::*;

/// What `blob` holds, read from the fixture's store.
pub(super) fn read(fixture: &Fixture, blob: &BlobRef) -> String {
    let mut text = String::new();
    (fixture.0.blobs.open(&blob.id).unwrap())
        .read_to_string(&mut text)
        .unwrap();
    text
}

/// No stream of `session`'s is left growing.
pub(super) fn nothing_open(fixture: &Fixture, session: SessionId) {
    let open = fixture.0.blobs.streams().join(session.to_string());
    let left: Vec<_> = (std::fs::read_dir(open).unwrap())
        .map(|e| e.unwrap().path())
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

/// The output the commands below write: cut for the model, so kept whole.
const COMMAND: &str = "seq 1 100000; sleep 2; echo done";

fn written() -> String {
    (1..=100_000).fold(String::new(), |mut text, n| {
        let _ = writeln!(text, "{n}");
        text
    }) + "done\n"
}

/// How a call `call` went, as the log tells it: its placeholder result,
/// then its outcome on the queue, holding what it wrote.
fn backgrounded(fixture: &Fixture, session: &Live, log: &[Entry], call: CallId) -> String {
    assert!(
        log.iter().any(|e| e.body == Body::Detached { call }),
        "{log:#?}"
    );
    let stream = OutputStream {
        session: session.id,
        call,
    };
    assert!(log.iter().any(|e| matches!(&e.body,
        Body::ToolResult { call: c, outcome, held, .. } if *c == call
            && *outcome == Outcome::detached()
            && *held == [Held::Stream(stream)])));
    let back = (log.iter())
        .find_map(|e| match &e.body {
            Body::Enqueue { envelope } if envelope.id.purpose == EnvelopeId::DETACHED => {
                Some(envelope)
            }
            _ => None,
        })
        .unwrap();
    let Some(Outcome::Done { output }) = Outcome::from_payload(&back.payload) else {
        panic!("{back:?}")
    };
    let ran: ext_shell::bash::Ran = output.parse().unwrap();
    assert_eq!((ran.exit, ran.timed_out), (Some(0), None), "{ran:?}");
    let [Held::Blob(blob)] = &back.held[..] else {
        panic!("{back:?}")
    };
    read(fixture, blob)
}

fn call_of(log: &[Entry]) -> CallId {
    (log.iter())
        .find_map(|e| matches!(e.body, Body::ToolCall { .. }).then_some(CallId(e.seq)))
        .unwrap()
}

fn came_back(log: &[Entry]) -> bool {
    log.iter().any(
        |e| matches!(&e.body, Body::Assistant { text } if text.contains("ended in the background")),
    )
}

#[test]
fn a_command_past_its_timeout_goes_on_in_the_background_and_its_whole_output_comes_back() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        echoing(&Policy::default()),
        openai(&Script::new(&[])),
    );
    let session = fixture.start(&[]);
    let input = json!({ "command": COMMAND, "timeout": 1 });
    session.submit(&format!("call bash {input}")).unwrap();
    let log = until(&session, "the call was answered", |log| {
        log.iter()
            .any(|e| matches!(e.body, Body::ToolResult { .. }))
    });
    // Answered at its timeout, while the command still runs.
    assert!(
        !log.iter().any(|e| matches!(e.body, Body::Enqueue { .. })),
        "{log:#?}"
    );
    let log = until(&session, "its outcome came back", came_back);
    settle(&session);
    session.halt();
    let call = call_of(&log);
    assert_eq!(backgrounded(&fixture, &session, &log, call), written());
    nothing_open(&fixture, session.id);
}

#[test]
fn a_forced_steer_and_a_timeout_send_a_command_to_the_background_the_same_way() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        echoing(&Policy::default()),
        openai(&Script::new(&[])),
    );
    let session = fixture.start(&[]);
    let input = json!({ "command": COMMAND });
    session.submit(&format!("call bash {input}")).unwrap();
    until(&session, "the command runs", |log| {
        log.iter()
            .any(|e| matches!(e.body, Body::ToolProcess { .. }))
    });
    let force = Request::Input {
        segments: "now".into(),
        mode: Mode::Steer,
        force: true,
    };
    rt::block_on(session.request(force, None)).unwrap();
    let log = until(&session, "its outcome came back", came_back);
    settle(&session);
    session.halt();
    let call = call_of(&log);
    assert_eq!(backgrounded(&fixture, &session, &log, call), written());
    nothing_open(&fixture, session.id);
}
