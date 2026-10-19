//! Interrupts reaching the model call itself: over a real connection, the
//! provider sees the request go away.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::OnceLock;
use std::sync::mpsc;

use super::*;

/// Well past a socket shutdown's wake-up, well short of any timeout.
const PROMPT: Duration = Duration::from_millis(200);

/// A provider on a local port that takes one request, answers `reply`
/// (nothing at all if empty) and holds the connection. It says on the
/// first channel that it got the request, and on the second that the
/// client closed the connection.
fn provider(reply: String) -> (u16, mpsc::Receiver<()>, mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (got, got_rx) = mpsc::channel();
    let (closed, closed_rx) = mpsc::channel();
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        read_request(&mut reader);
        (&stream).write_all(reply.as_bytes()).unwrap();
        got.send(()).unwrap();
        let _ = reader.read_to_end(&mut Vec::new());
        closed.send(()).unwrap();
    });
    (port, got_rx, closed_rx)
}

fn read_request(reader: &mut BufReader<TcpStream>) {
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap();
        }
        if line == "\r\n" {
            break;
        }
    }
    reader.read_exact(&mut vec![0; length]).unwrap();
}

/// A session on `gpt-5.6` at the provider on `port`, over ureq.
fn over_ureq(port: u16) -> (Fixture, PathBuf) {
    let home = std::env::temp_dir().join(format!("a-engine-port-{port}.toml"));
    let env = format!("[env]\nOPENAI_BASE_URL = \"http://127.0.0.1:{port}/v1\"\n");
    std::fs::write(&home, env).unwrap();
    let models = Models {
        settings: crate::config::Settings {
            process: daemon_env,
            ..crate::config::Settings::new(home.clone(), Some("gpt-5.6".into()), None)
        },
        http: Arc::new(llm::Ureq::new(Arc::new(rt::Threads::new(1)))),
    };
    let fixture = Fixture::new(agent::Kind::Chat, Registry::default(), models);
    (fixture, home)
}

fn model_result(log: &[Entry]) -> Option<&Outcome> {
    log.iter().find_map(|e| match &e.body {
        Body::ModelResult { outcome, .. } => Some(outcome),
        _ => None,
    })
}

#[test]
fn an_interrupt_while_the_model_thinks_closes_its_connection() {
    let (port, got, closed) = provider(String::new());
    let (fixture, home) = over_ureq(port);
    let session = fixture.start(&[]);
    session.submit("hi".into()).unwrap();
    got.recv().unwrap();
    session.interrupt();
    closed
        .recv_timeout(PROMPT)
        .expect("the connection stays open");
    settle(&session);
    let log = log_of(&session);
    session.stop();
    std::fs::remove_file(home).unwrap();
    assert_eq!(
        model_result(&log),
        Some(&Outcome::Interrupted { output: None })
    );
}

#[test]
fn an_interrupt_mid_reply_closes_its_connection_and_keeps_the_text_so_far() {
    // The start of a recorded reply, through its first text delta.
    let recorded = &cassette("openai/streaming_grammar/tool_then_followup_text.yaml")[1];
    let delta = recorded.find("event: response.output_text.delta").unwrap();
    let cut = delta + recorded[delta..].find("\n\n").unwrap() + 2;
    let data = recorded[delta..cut].split_once("data: ").unwrap().1;
    let text = serde_json::from_str::<Value>(data).unwrap()["delta"].clone();
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
    let (port, got, closed) = provider(format!("{head}{}", &recorded[..cut]));
    let (fixture, home) = over_ureq(port);
    let session = fixture.start(&[]);
    let outbox = Outbox::new(Caps::NONE);
    session.attach(&outbox, 0);
    session.submit("hi".into()).unwrap();
    got.recv().unwrap();
    // Interrupted once the delta reached clients, so it is the text so far.
    while let Some(next) = outbox.next() {
        let crate::outbox::Next::Write(frame) = next else {
            continue;
        };
        let msg = protocol::read_frame(&mut frame.as_bytes()).unwrap();
        if let Some(DaemonMsg::Delta { .. }) = msg {
            break;
        }
    }
    session.interrupt();
    closed
        .recv_timeout(PROMPT)
        .expect("the connection stays open");
    settle(&session);
    let log = log_of(&session);
    session.stop();
    std::fs::remove_file(home).unwrap();
    assert_eq!(
        model_result(&log),
        Some(&Outcome::Interrupted {
            output: text.as_str().map(str::to_owned)
        })
    );
}

/// Fails each request, interrupting `session` first.
#[derive(Default)]
struct Interrupting {
    session: OnceLock<Weak<Session>>,
    sent: AtomicUsize,
}

impl Http for Interrupting {
    fn send(&self, _: http::Request<Bytes>) -> BoxFuture<'static, io::Result<llm::http::Response>> {
        self.sent.fetch_add(1, Ordering::SeqCst);
        if let Some(session) = self.session.get().and_then(Weak::upgrade) {
            session.interrupt();
        }
        Box::pin(std::future::ready(Err(io::Error::other("unreachable"))))
    }
}

#[test]
fn a_request_failing_after_an_interrupt_is_not_retried() {
    let http = Arc::new(Interrupting::default());
    let models = Models {
        http: Arc::clone(&http) as Arc<dyn Http>,
        ..openai(&Script::new(&[]))
    };
    let registry = Registry::default().with(&Extras).unwrap();
    let fixture = Fixture::new(agent::Kind::Chat, registry, models);
    let session = fixture.start(&[]);
    http.session.set(Arc::downgrade(&session)).unwrap();
    session.submit("hi".into()).unwrap();
    settle(&session);
    let log = log_of(&session);
    session.stop();
    assert_eq!(http.sent.load(Ordering::SeqCst), 1, "{log:#?}");
    assert!(
        !log.iter().any(|e| matches!(
            e.body,
            Body::Hook {
                decision: Decision::Retry { .. },
                ..
            }
        )),
        "{log:#?}"
    );
    assert!(matches!(
        model_result(&log),
        Some(Outcome::Failed { error }) if error.contains("unreachable")
    ));
}
