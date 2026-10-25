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
    let protocol::Carried::Detached(Outcome::Done {
        output: protocol::Payload::Inline(output),
    }) = &back.payload
    else {
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

/// Runs a slow command through `bash` from inside its own call, as a Lua
/// script does: the call waits on it, however long it runs.
struct Nest;

impl Tool for Nest {
    type In = Nothing;
    type Out = protocol::ToolOutcome;

    const NAME: &'static str = "nest";
    const DESCRIPTION: &'static str = "Runs a slow command through bash.";

    async fn call(&self, _: &Nothing, cx: &Cx) -> Result<protocol::ToolOutcome, String> {
        let input = Json::encode(&json!({ "command": "sleep 30", "timeout": 1 })).unwrap();
        Ok(cx
            .invoke("slow", ext_shell::Bash::NAME, &input)
            .await
            .outcome)
    }
}

impl Extension for Nest {
    const NAME: &'static str = "nest";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(Nest);
    }
}

#[test]
fn a_command_another_call_invoked_is_still_stopped_at_its_timeout() {
    let registry = echoing(&Policy::default()).with(&Nest).unwrap();
    let fixture = Fixture::new(ext_fixtures::ECHO, registry, openai(&Script::new(&[])));
    let session = fixture.start(&[]);
    let started = Instant::now();
    session.submit("call nest {}").unwrap();
    let log = until(&session, "the outer call ended", |log| {
        log.iter().any(|e| {
            matches!(&e.body, Body::ToolResult { call, .. }
                if log.iter().any(|c| c.seq == call.0
                    && matches!(&c.body, Body::ToolCall { tool, .. } if tool == "nest")))
        })
    });
    settle(&session);
    session.halt();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "it was stopped"
    );
    assert!(
        !log.iter().any(|e| matches!(e.body, Body::Detached { .. })),
        "{log:#?}"
    );
    let inner = (log.iter())
        .find_map(|e| match &e.body {
            Body::ToolResult {
                outcome:
                    Outcome::Done {
                        output: protocol::Payload::Inline(output),
                    },
                ..
            } => output.parse::<ext_shell::bash::Ran>().ok(),
            _ => None,
        })
        .unwrap();
    assert_eq!(inner.timed_out, Some(1), "{inner:?}");
}

/// A group that has ended already, whose end holds what `sealing` lets
/// through once released.
struct Gone;

impl ext::RunningProcess for Gone {
    fn id(&self) -> u32 {
        0
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
        Box::pin(std::future::ready(Err(io::ErrorKind::Unsupported.into())))
    }
    fn output_drained(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(std::future::ready(Ok(())))
    }
    fn ended(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::ready(()))
    }
}

/// Leaves the session a group whose end's holdings wait on `release`.
struct Leaves {
    sealing: Arc<Flag>,
    release: Arc<Flag>,
}

impl Tool for Leaves {
    type In = Nothing;
    type Out = String;

    const NAME: &'static str = "leaves";
    const DESCRIPTION: &'static str = "Leaves a group to the session.";

    fn call(&self, _: &Nothing, cx: &Cx) -> Result<String, String> {
        let (sealing, release) = (Arc::clone(&self.sealing), Arc::clone(&self.release));
        let lease = ext::ProcessLease::from(Arc::new(Gone) as Arc<dyn ext::RunningProcess>);
        cx.own(lease.holding(move || {
            sealing.set();
            rt::block_on(release.wait());
            Vec::new()
        }));
        Ok("left".into())
    }
}

impl Extension for Leaves {
    const NAME: &'static str = "leaves";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(Self {
            sealing: Arc::clone(&self.sealing),
            release: Arc::clone(&self.release),
        });
    }
}

#[test]
fn a_job_holds_its_session_until_its_end_is_queued() {
    let (sealing, release) = (Arc::new(Flag::default()), Arc::new(Flag::default()));
    let leaves = Leaves {
        sealing: Arc::clone(&sealing),
        release: Arc::clone(&release),
    };
    let registry = echoing(&Policy::default()).with(&leaves).unwrap();
    let fixture = Fixture::new(ext_fixtures::ECHO, registry, openai(&Script::new(&[])));
    let session = fixture.start(&[]);
    session.submit("call leaves {}").unwrap();
    session.wait("its end is being made", PATIENCE, || sealing.is_set());
    assert!(
        session.has_jobs(),
        "the job is gone before its end is queued"
    );
    release.set();
    until(&session, "its end was queued", |log| {
        log.iter().any(|e| matches!(e.body, Body::Enqueue { .. }))
    });
    session.wait("the job ended", PATIENCE, || !session.has_jobs());
    session.halt();
}
