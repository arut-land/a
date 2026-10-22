//! Model calls recovered by the `retry` extension: each attempt a fact,
//! streamed on its own, and bounded however often the daemon restarts.

use std::pin::Pin;
use std::task::Context;

use ext_retry::Retry;

use super::*;

/// A reply a [`Flaky`] provider gives.
#[derive(Clone)]
pub(super) enum Reply {
    /// A failure, with its headers.
    Status(u16, &'static [(&'static str, &'static str)]),
    /// These SSE events, then the end of the stream.
    Events(String),
    /// These SSE events, then nothing ever again.
    Hang(String),
}

/// A provider answering each request with the next of its replies.
#[derive(Default)]
pub(super) struct Flaky {
    replies: Mutex<VecDeque<Reply>>,
    sent: Mutex<usize>,
    /// Notified at each request sent.
    went: std::sync::Condvar,
    /// The bodies it was sent, in order.
    bodies: Mutex<Vec<Value>>,
}

impl Flaky {
    pub(super) fn new(replies: impl IntoIterator<Item = Reply>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into_iter().collect()),
            sent: Mutex::new(0),
            went: std::sync::Condvar::new(),
            bodies: Mutex::default(),
        })
    }

    /// The bodies of the requests sent, in order.
    pub(super) fn bodies(&self) -> Vec<Value> {
        lock(&self.bodies).clone()
    }

    pub(super) fn sent(&self) -> usize {
        *lock(&self.sent)
    }

    /// Waits until `n` requests have been sent: a request's call is logged
    /// before it goes out.
    pub(super) fn wait_sent(&self, n: usize) {
        let sent = lock(&self.sent);
        let (sent, waited) = self
            .went
            .wait_timeout_while(sent, PATIENCE, |sent| *sent < n)
            .unwrap();
        drop(sent);
        assert!(!waited.timed_out(), "{n} requests were never sent");
    }
}

/// A body that yields its bytes, then never ends.
struct Hanging(Option<Vec<u8>>);

impl futures_core::Stream for Hanging {
    type Item = io::Result<Vec<u8>>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0
            .take()
            .map_or(Poll::Pending, |b| Poll::Ready(Some(Ok(b))))
    }
}

impl Http for Flaky {
    fn send(
        &self,
        request: http::Request<llm::http::Payload>,
    ) -> llm::http::BoxFuture<'static, io::Result<llm::http::Response>> {
        *lock(&self.sent) += 1;
        self.went.notify_all();
        let body = serde_json::from_slice(&request.body().to_vec()).unwrap_or_default();
        lock(&self.bodies).push(body);
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
                let mut r = response(status, llm::http::body([b"{}".to_vec()]));
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
pub(super) fn text_reply() -> String {
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

pub(super) fn fixture(registry: Registry, http: &Arc<Flaky>) -> Fixture {
    let models = Models {
        http: Arc::clone(http) as Arc<dyn Http>,
        ..openai(&Script::new(&[]))
    };
    Fixture::new(ext::LOOP, registry, models)
}

/// The attempts each retry decision starts, by call.
fn retries(log: &[Entry]) -> Vec<(u64, u16)> {
    log.iter()
        .filter_map(|e| match &e.body {
            Body::Decision {
                call: Some(call),
                d: Verdict::Retry { attempt, .. },
                ..
            } => Some((call.get(), *attempt)),
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
    session.submit("hi").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();

    assert!(
        began.elapsed() >= Duration::from_millis(30),
        "the provider's wait"
    );
    assert_eq!(http.sent(), 3);
    assert_eq!(retries(&log), [(2, 1), (2, 2)]);
    assert!(
        matches!(model_result(&log), Some(Outcome::Done { output })
            if output.parse::<Value>().unwrap()["message"]["content"][1]["text"] == "The harbor label is crimson-harbor."),
        "{log:#?}"
    );
}

#[test]
fn what_cannot_pass_is_not_retried() {
    let http = Flaky::new([Reply::Status(401, &[])]);
    let session = fixture(quick(), &http).start(&[]);
    session.submit("hi").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    assert_eq!(http.sent(), 1);
    assert_eq!(retries(&log), []);
    assert!(matches!(model_result(&log), Some(Outcome::Failed { .. })));
}

/// #42: each restart mid-call is an attempt that failed, logged as the
/// retry it starts; once the extension's attempts are spent, the call
/// fails with nothing sent, however often the daemon comes back.
#[test]
fn a_crash_loop_cannot_retry_forever() {
    let cap = Retry::default().attempts;
    for logged in 0..=cap {
        let mut entries = vec![
            entry(1, user("hi")),
            entry(
                2,
                Body::ModelCall {
                    request: None,
                    model: "m".into(),
                    attempt: 0,
                    choice: ToolChoice::Auto,
                    r#for: None,
                    purpose: None,
                },
            ),
        ];
        for attempt in 1..=logged {
            let retried = Verdict::Retry {
                attempt,
                error: "the daemon stopped during the call".into(),
            };
            entries.push(entry(
                u64::from(attempt) + 2,
                decision("retry", Some(2), retried),
            ));
        }
        let http = Flaky::new([Reply::Events(text_reply())]);
        let session = fixture(quick(), &http).start(&entries);
        let log = log_of(&session);
        session.halt();
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
    session.submit("hi").unwrap();
    until(&session, "decided to retry", |log| !retries(log).is_empty());
    let began = Instant::now();
    session.interrupt_now();
    settle(&session);
    let log = log_of(&session);
    session.halt();

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

/// #25 `a_retry_streams_one_attempt`: an attempt cut off streams nothing
/// into the next: interrupted, the call keeps only the text of the attempt
/// it cut off.
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
    session.submit("hi").unwrap();
    // The next attempt is out and streaming: a follower has a delta of it.
    let outbox = Outbox::new();
    session.follow(&outbox, 0);
    let streaming = async {
        loop {
            let frame = outbox
                .next(&rt::Threads::new(1))
                .await
                .expect("the follower is open");
            if let Ok(Some(DaemonMsg::Delta { attempt: 1, .. })) =
                protocol::read_frame::<DaemonMsg>(&mut frame.as_bytes())
            {
                return;
            }
        }
    };
    rt::block_on(or(streaming, async {
        rt::Reactor::AsyncIo.sleep(PATIENCE).await;
        panic!("the retry never streamed");
    }));
    session.interrupt_now();
    settle(&session);
    let log = log_of(&session);
    session.halt();

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

#[test]
fn every_provider_retry_has_one_durable_start_and_completion() {
    let http = Flaky::new([Reply::Status(429, &[]), Reply::Events(text_reply())]);
    let session = fixture(quick().with(&ext_metrics::Metrics).unwrap(), &http).start(&[]);
    session.submit("hi").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    let facts: Vec<_> = log
        .iter()
        .filter_map(|entry| match &entry.body {
            Body::Fact {
                fact: protocol::ExtensionFact::Call(fact),
            } => Some(fact),
            _ => None,
        })
        .collect();
    assert_eq!(http.sent(), 2);
    assert_eq!(facts.len(), 4);
    for (attempt, pair) in facts.as_chunks::<2>().0.iter().enumerate() {
        assert_eq!(pair[0].id, pair[1].id);
        assert_eq!(usize::from(pair[0].id.attempt), attempt);
        assert_eq!(pair[0].id.executing_session, session.id);
        assert!(pair[0].elapsed_ns.is_none());
        assert!(pair[1].elapsed_ns.is_some());
        assert!(!pair[0].side);
    }
    assert!(facts[1].usage.is_none());
    assert!(
        facts[3]
            .usage
            .as_ref()
            .unwrap()
            .parse::<llm::Usage>()
            .unwrap()
            .input
            .is_some()
    );
}
