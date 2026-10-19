//! Time and allocations of decoding every recorded stream and of encoding a
//! long transcript, ignored by default: `cargo test -p llm --release --test
//! bench -- --ignored --nocapture`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

use bytes::Bytes;
use llm::http::{BoxFuture, Http, Payload, Response, body};
use llm::protocols::{chat, gemini, messages, responses};
use llm::{
    Assistant, AssistantPart, Message, Origin, Output, Provider, Request, Text, ToolCall,
    ToolMessage, ToolResult,
};

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// Only this test binary counts: the allocator defers to the system's.
#[allow(unsafe_code)]
// SAFETY: every method forwards to `System`, which upholds the contract.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(layout.size(), Relaxed);
        let live = LIVE.fetch_add(layout.size(), Relaxed) + layout.size();
        PEAK.fetch_max(live, Relaxed);
        // SAFETY: the caller upholds `System.alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        // SAFETY: the caller upholds `System.dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(new, Relaxed);
        let live = LIVE.fetch_add(new, Relaxed) + new;
        LIVE.fetch_sub(layout.size(), Relaxed);
        PEAK.fetch_max(live, Relaxed);
        // SAFETY: the caller upholds `System.realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Runs `f` `runs` times and prints its fastest time, which a busy machine
/// disturbs least, and its allocations per run.
fn measure(label: &str, runs: usize, mut f: impl FnMut()) {
    f();
    let (allocs, bytes) = (ALLOCS.load(Relaxed), BYTES.load(Relaxed));
    let base = LIVE.load(Relaxed);
    PEAK.store(base, Relaxed);
    let mut fastest = std::time::Duration::MAX;
    for _ in 0..runs {
        let start = Instant::now();
        f();
        fastest = fastest.min(start.elapsed());
    }
    let runs = runs.max(1);
    eprintln!(
        "{label}: {fastest:?}/run, {} allocs/run, {} KiB allocated/run, peak {} KiB",
        (ALLOCS.load(Relaxed) - allocs) / runs,
        (BYTES.load(Relaxed) - bytes) / runs / 1024,
        (PEAK.load(Relaxed) - base) / 1024,
    );
}

struct Recorded {
    status: u16,
    body: Bytes,
}

impl Http for Recorded {
    fn send(&self, _: http::Request<Payload>) -> BoxFuture<'static, io::Result<Response>> {
        let chunks: Vec<Bytes> = (0..self.body.len())
            .step_by(1024)
            .map(|at| self.body.slice(at..self.body.len().min(at + 1024)))
            .collect();
        let response = http::Response::builder()
            .status(self.status)
            .body(body(chunks))
            .expect("response");
        Box::pin(std::future::ready(Ok(response)))
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/rig/cassettes")
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("cassettes") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "yaml") {
            out.push(path);
        }
    }
}

fn provider(dir: &str, path: &str) -> Option<Provider> {
    let auth = llm::Auth::None;
    let url = "http://recorded";
    if path.ends_with("/responses") {
        let (host, quirks) = responses::HOSTS.iter().find(|(host, _)| host.id == dir)?;
        return Some(Provider::new(
            host.id,
            url,
            auth,
            responses::Responses::new(*quirks),
        ));
    }
    if path.ends_with("/chat/completions") {
        let (host, quirks) = chat::HOSTS.iter().find(|(host, _)| host.id == dir)?;
        return Some(Provider::new(host.id, url, auth, chat::Chat::new(*quirks)));
    }
    if path.contains(":streamGenerateContent") {
        let (host, quirks) = gemini::HOSTS.iter().find(|(host, _)| host.id == dir)?;
        return Some(Provider::new(
            host.id,
            url,
            auth,
            gemini::Gemini::new(*quirks),
        ));
    }
    if path.ends_with("/messages") {
        let (host, quirks) = messages::HOSTS.first()?;
        return Some(Provider::new(
            host.id,
            url,
            auth,
            messages::Messages::new(*quirks),
        ));
    }
    None
}

/// Every recorded exchange: its provider and reply.
fn exchanges() -> Vec<(Provider, Recorded)> {
    let mut files = Vec::new();
    walk(&root(), &mut files);
    files.sort();
    let mut out = Vec::new();
    for file in files {
        let rel = file.strip_prefix(root()).expect("under root");
        let dir = rel.iter().next().and_then(|dir| dir.to_str()).expect("dir");
        let text = std::fs::read_to_string(&file).expect("cassette");
        for doc in serde_yaml_ng::Deserializer::from_str(&text) {
            let doc: serde_yaml_ng::Value = serde::Deserialize::deserialize(doc).expect("yaml");
            let Some(path) = doc["when"]["path"].as_str() else {
                continue;
            };
            let Some(provider) = provider(dir, path) else {
                continue;
            };
            let status = doc["then"]["status"]
                .as_u64()
                .and_then(|s| u16::try_from(s).ok());
            let body = doc["then"]["body"].as_str().unwrap_or_default();
            out.push((
                provider,
                Recorded {
                    status: status.unwrap_or(200),
                    body: Bytes::copy_from_slice(body.as_bytes()),
                },
            ));
        }
    }
    out
}

fn ask() -> Request {
    Request {
        model: "recorded-model".into(),
        messages: vec![Message::user("hi")].into(),
        generation: llm::Generation {
            max_output_tokens: std::num::NonZeroU32::new(1024),
            ..llm::Generation::default()
        },
        ..Request::default()
    }
}

#[test]
#[ignore = "a benchmark"]
fn decode_recorded() {
    let exchanges = exchanges();
    let request = ask();
    eprintln!("{} exchanges", exchanges.len());
    measure("decode all cassettes", 50, || {
        for (provider, recorded) in &exchanges {
            let _ = rt::block_on(llm::generate(recorded, provider, &request));
        }
    });
}

/// A chat completions stream of `deltas` reasoning deltas and as many text
/// deltas.
fn long_chat(deltas: usize) -> Bytes {
    let mut body = String::new();
    for n in 0..deltas {
        let _ = write!(
            body,
            "data: {{\"id\":\"c1\",\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{\"reasoning_content\":\"thinking step {n} about the problem \"}}}}]}}\n\n"
        );
    }
    for n in 0..deltas {
        let _ = write!(
            body,
            "data: {{\"id\":\"c1\",\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"word{n} and more \"}}}}]}}\n\n"
        );
    }
    body.push_str("data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":20}}\n\ndata: [DONE]\n\n");
    body.into()
}

/// A messages stream of `deltas` thinking deltas and as many text deltas.
fn long_messages(deltas: usize) -> Bytes {
    let mut body = String::from(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"claude\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n\
         event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
    );
    for n in 0..deltas {
        let _ = write!(
            body,
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"thinking_delta\",\"thinking\":\"thinking step {n} about the problem \"}}}}\n\n"
        );
    }
    body.push_str("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig\"}}\n\n");
    body.push_str(
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    body.push_str("event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n");
    for n in 0..deltas {
        let _ = write!(
            body,
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{{\"type\":\"text_delta\",\"text\":\"word{n} and more \"}}}}\n\n"
        );
    }
    body.push_str(
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    );
    body.push_str("event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":20}}\n\n");
    body.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    body.into()
}

#[test]
#[ignore = "a benchmark"]
fn decode_long_streams() {
    let request = ask();
    let (host, quirks) = chat::HOSTS[0];
    let chat = Provider::new(
        host.id,
        "http://x",
        llm::Auth::None,
        chat::Chat::new(quirks),
    );
    let (host, quirks) = messages::HOSTS[0];
    let messages = Provider::new(
        host.id,
        "http://x",
        llm::Auth::None,
        messages::Messages::new(quirks),
    );
    let chat_body = Recorded {
        status: 200,
        body: long_chat(20_000),
    };
    let messages_body = Recorded {
        status: 200,
        body: long_messages(20_000),
    };
    measure("chat 40k deltas", 20, || {
        let reply = rt::block_on(llm::generate(&chat_body, &chat, &request)).expect("reply");
        assert_eq!(reply.message.content.len(), 2);
    });
    measure("messages 40k deltas", 20, || {
        let reply =
            rt::block_on(llm::generate(&messages_body, &messages, &request)).expect("reply");
        assert_eq!(reply.message.content.len(), 2);
    });
}

/// Tool round `turn`, its result `size` bytes, produced by `origin`.
fn round(turn: usize, size: usize, origin: &Origin) -> [Message; 2] {
    let id: llm::SmolStr = format!("call_{turn}").into();
    [
        Message::Assistant(Assistant {
            content: vec![
                AssistantPart::Text(Text::new("Let me look.")),
                AssistantPart::ToolCall(ToolCall {
                    id: id.clone(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "cat notes.md"})
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                    invalid: None,
                    provider_executed: false,
                    meta: llm::Meta::new(),
                }),
            ],
            origin: Some(origin.clone()),
        }),
        Message::Tool(ToolMessage {
            results: vec![ToolResult {
                id,
                name: "bash".into(),
                output: Output::Text("x".repeat(size)),
                error: false,
                meta: llm::Meta::new(),
            }],
        }),
    ]
}

/// A session of `turns` tool rounds, each result `size` bytes, produced by
/// `origin`.
fn transcript(turns: usize, size: usize, origin: &Origin) -> Request {
    let mut messages = vec![Message::system("You are terse."), Message::user("go")];
    for turn in 0..turns {
        messages.extend(round(turn, size, origin));
    }
    Request {
        model: origin.model.clone(),
        messages: messages.into(),
        generation: llm::Generation {
            max_output_tokens: std::num::NonZeroU32::new(1024),
            ..llm::Generation::default()
        },
        ..Request::default()
    }
}

#[test]
#[ignore = "a benchmark"]
fn encode_long_transcript() {
    let reply = Recorded {
        status: 200,
        body: long_chat(1),
    };
    for (label, provider) in [
        ("chat", {
            let (host, quirks) = chat::HOSTS[0];
            Provider::new(
                host.id,
                "http://x",
                llm::Auth::None,
                chat::Chat::new(quirks),
            )
        }),
        ("messages", {
            let (host, quirks) = messages::HOSTS[0];
            Provider::new(
                host.id,
                "http://x",
                llm::Auth::None,
                messages::Messages::new(quirks),
            )
        }),
        ("responses", {
            let (host, quirks) = responses::HOSTS[0];
            Provider::new(
                host.id,
                "http://x",
                llm::Auth::None,
                responses::Responses::new(quirks),
            )
        }),
        ("gemini", {
            let (host, quirks) = gemini::HOSTS[0];
            Provider::new(
                host.id,
                "http://x",
                llm::Auth::None,
                gemini::Gemini::new(quirks),
            )
        }),
    ] {
        let origin = llm::origin(&provider, "m");
        let mut request = transcript(200, 8 * 1024, &origin);
        // Conversations no request encoded before.
        let mut fresh: Vec<Request> = (0..=20)
            .map(|_| Request {
                messages: request.messages.to_vec().into(),
                ..request.clone()
            })
            .collect();
        measure(
            &format!("{label} encode 200 turns x 8 KiB, whole"),
            20,
            || {
                let request = fresh.pop().expect("a fresh conversation");
                let _ = llm::stream(&reply, &provider, &request);
            },
        );
        // One conversation, grown a round per request as a session's grows.
        let mut grown = std::mem::take(&mut request.messages);
        let _ = llm::stream(
            &reply,
            &provider,
            &Request {
                messages: grown.clone(),
                ..request.clone()
            },
        );
        let mut turn = 200;
        measure(
            &format!("{label} encode 200 turns x 8 KiB, grown"),
            20,
            || {
                for message in round(turn, 8 * 1024, &origin) {
                    grown.push(message);
                }
                turn += 1;
                let sent = Request {
                    messages: grown.clone(),
                    ..request.clone()
                };
                let _ = llm::stream(&reply, &provider, &sent);
            },
        );
    }
}

#[test]
#[ignore = "a benchmark"]
fn read_logged_reply() {
    use serde::Deserialize as _;
    let origin = Origin {
        protocol: "messages".into(),
        provider: "anthropic".into(),
        model: "claude".into(),
    };
    let Message::Assistant(message) = transcript(1, 0, &origin).messages[2].clone() else {
        unreachable!("the assistant turn");
    };
    let mut content = Vec::new();
    for _ in 0..50 {
        content.extend(message.content.iter().cloned());
        content.push(AssistantPart::Text(Text::new("y".repeat(4096))));
    }
    let reply = llm::Response {
        message: Assistant {
            content,
            origin: Some(origin),
        },
        finish: llm::Finish::new(llm::Reason::Stop, None),
        usage: llm::Usage::default(),
        id: None,
        model: None,
        meta: llm::Meta::new(),
    };
    let value = serde_json::to_value(&reply).expect("a value");
    measure("from_value 150 parts", 200, || {
        let _ = std::hint::black_box(llm::Response::from_value(&value));
    });
    measure("derived Deserialize 150 parts", 200, || {
        let _ = std::hint::black_box(llm::Response::deserialize(&value));
    });
}
