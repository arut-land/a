//! Model calls recovered by the `retry` extension: each attempt a fact,
//! streamed on its own, and bounded however often the daemon restarts.

use std::pin::Pin;
use std::task::Context;

use ext_retry::Retry;

use super::*;

/// A reply a [`Flaky`] provider gives.
#[derive(Clone)]
enum Reply {
    /// A failure, with its headers.
    Status(u16, &'static [(&'static str, &'static str)]),
    /// These SSE events, then the end of the stream.
    Events(String),
    /// These SSE events, then nothing ever again.
    Hang(String),
}

/// A provider answering each request with the next of its replies.
#[derive(Default)]
struct Flaky {
    replies: Mutex<VecDeque<Reply>>,
    sent: AtomicUsize,
}

impl Flaky {
    fn new(replies: impl IntoIterator<Item = Reply>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into_iter().collect()),
            sent: AtomicUsize::new(0),
        })
    }

    fn sent(&self) -> usize {
        self.sent.load(Ordering::SeqCst)
    }
}

/// A body that yields its bytes, then never ends.
struct Hanging(Option<Bytes>);

impl futures_core::Stream for Hanging {
    type Item = io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0
            .take()
            .map_or(Poll::Pending, |b| Poll::Ready(Some(Ok(b))))
    }
}

impl Http for Flaky {
    fn send(
        &self,
        _: http::Request<llm::http::Payload>,
    ) -> BoxFuture<'static, io::Result<llm::http::Response>> {
        self.sent.fetch_add(1, Ordering::SeqCst);
        let reply = lock(&self.replies).pop_front();
        let response = |status, body: llm::http::Body| {
            http::Response::builder()
                .status(status)
                .header("content-type", "text/event-stream")
                .body(body)
                .expect("scripted response")
        };
        let response = match reply {
            Some(Reply::Status(status, headers)) => {
                let mut r = response(status, llm::http::body([Bytes::from_static(b"{}")]));
                for (name, value) in headers {
                    r.headers_mut().insert(*name, value.parse().unwrap());
                }
                Ok(r)
            }
            Some(Reply::Events(events)) => Ok(response(200, llm::http::body([events.into()]))),
            Some(Reply::Hang(events)) => Ok(response(200, Box::pin(Hanging(Some(events.into()))))),
            None => Err(io::Error::other("no more replies")),
        };
        Box::pin(std::future::ready(response))
    }
}

/// The text reply of the rig's recordings, its events whole.
fn text_reply() -> String {
    cassette("openai/streaming_grammar/tool_then_followup_text.yaml").swap_remove(1)
}

/// The first `n` events of `reply`: a reply cut off.
fn cut(reply: &str, n: usize) -> String {
    reply.split_inclusive("\n\n").take(n).collect()
}

/// Retries waiting a millisecond, but the provider's waits.
fn quick() -> Registry {
    let retry = Retry {
        base: Duration::from_millis(1),
        ..Retry::default()
    };
    Registry::default().with(&retry).unwrap()
}

fn fixture(registry: Registry, http: &Arc<Flaky>) -> Fixture {
    let models = Models {
        http: Arc::clone(http) as Arc<dyn Http>,
        ..openai(&Script::new(&[]))
    };
    Fixture::new(ext_chat::CHAT, registry, models)
}

/// The attempts each retry decision starts, by call.
fn retries(log: &[Entry]) -> Vec<(u64, u32)> {
    log.iter()
        .filter_map(|e| match &e.body {
            Body::Hook {
                decision: Decision::Retry { call, attempt, .. },
                ..
            } => Some((*call, *attempt)),
            _ => None,
        })
        .collect()
}

fn model_result(log: &[Entry]) -> Option<&Outcome> {
    log.iter().find_map(|e| match &e.body {
        Body::ModelResult { outcome, .. } => Some(outcome),
        _ => None,
    })
}

#[test]
fn a_transient_error_is_retried_then_succeeds() {
    let http = Flaky::new([
        Reply::Status(429, &[("retry-after-ms", "30")]),
        Reply::Status(503, &[]),
        Reply::Events(text_reply()),
    ]);
    let session = fixture(quick(), &http).start(&[]);
    let began = Instant::now();
    session.submit("hi".into()).unwrap();
    settle(&session);
    let log = log_of(&session);
    session.stop();

    assert!(
        began.elapsed() >= Duration::from_millis(30),
        "the provider's wait"
    );
    assert_eq!(http.sent(), 3);
    assert_eq!(retries(&log), [(2, 1), (2, 2)]);
    assert!(
        matches!(model_result(&log), Some(Outcome::Done { output })
            if output.value()["message"]["content"][1]["text"] == "The harbor label is crimson-harbor."),
        "{log:#?}"
    );
}

#[test]
fn what_cannot_pass_is_not_retried() {
    let http = Flaky::new([Reply::Status(401, &[])]);
    let session = fixture(quick(), &http).start(&[]);
    session.submit("hi".into()).unwrap();
    settle(&session);
    let log = log_of(&session);
    session.stop();
    assert_eq!(http.sent(), 1);
    assert_eq!(retries(&log), []);
    assert!(matches!(model_result(&log), Some(Outcome::Failed { .. })));
}

/// Each restart mid-call is an attempt that failed, logged as the retry
/// it starts; once the extension's attempts are spent, the call fails
/// with nothing sent, however often the daemon comes back.
#[test]
fn a_crash_loop_cannot_retry_forever() {
    let cap = Retry::default().attempts;
    for logged in 0..=cap {
        let mut entries = vec![
            entry(1, Body::User { text: "hi".into() }),
            entry(2, Body::ModelCall { model: "m".into() }),
        ];
        for attempt in 1..=logged {
            let decision = Decision::Retry {
                call: 2,
                attempt,
                error: "the daemon stopped during the call".into(),
            };
            entries.push(entry(
                u64::from(attempt) + 2,
                Body::Hook {
                    hook: "retry".into(),
                    decision,
                },
            ));
        }
        let http = Flaky::new([Reply::Events(text_reply())]);
        let session = fixture(quick(), &http).start(&entries);
        let log = log_of(&session);
        session.stop();
        let new = &log[entries.len()..];
        if logged < cap {
            assert_eq!(http.sent(), 1, "{logged}");
            assert_eq!(retries(new), [(2, logged + 1)]);
            assert!(matches!(model_result(new), Some(Outcome::Done { .. })));
        } else {
            assert_eq!(http.sent(), 0, "{logged}");
            assert_eq!(retries(new), []);
            assert!(
                matches!(model_result(new), Some(Outcome::Failed { error }) if error.contains("stopped")),
                "{new:#?}"
            );
        }
    }
}

#[test]
fn an_interrupt_cancels_a_pending_retry() {
    let http = Flaky::new([
        Reply::Status(429, &[("retry-after", "20")]),
        Reply::Events(text_reply()),
    ]);
    let session = fixture(quick(), &http).start(&[]);
    session.submit("hi".into()).unwrap();
    until(&session, "decided to retry", |log| !retries(log).is_empty());
    let began = Instant::now();
    session.interrupt();
    settle(&session);
    let log = log_of(&session);
    session.stop();

    assert!(
        began.elapsed() < Duration::from_secs(5),
        "waited out the backoff"
    );
    assert_eq!(http.sent(), 1, "a request sent after the interrupt");
    assert_eq!(
        model_result(&log),
        Some(&Outcome::Interrupted { output: None })
    );
}

/// An attempt cut off streams nothing into the next: interrupted, the
/// call keeps only the text of the attempt it cut off.
#[test]
fn an_interrupted_retry_keeps_only_its_own_text() {
    let full = "The harbor label is crimson-harbor.";
    let reply = text_reply();
    let events = reply.split_inclusive("\n\n").count();
    let http = Flaky::new([
        Reply::Events(cut(&reply, events / 2)),
        Reply::Hang(cut(&reply, events - 2)),
    ]);
    let session = fixture(quick(), &http).start(&[]);
    session.submit("hi".into()).unwrap();
    until(&session, "retried", |log| !retries(log).is_empty());
    while http.sent() < 2 {
        thread::sleep(Duration::from_millis(5));
    }
    thread::sleep(Duration::from_millis(50));
    session.interrupt();
    settle(&session);
    let log = log_of(&session);
    session.stop();

    let Some(Outcome::Interrupted {
        output: Some(output),
    }) = model_result(&log)
    else {
        panic!("{log:#?}")
    };
    assert!(
        !output.is_empty() && full.starts_with(output.as_str()),
        "{output:?}"
    );
}
