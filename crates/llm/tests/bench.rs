//! What decoding and encoding cost, counted by one allocator: the
//! invariants that hold in any build (a streamed token is copied once, a
//! chunk that continues text allocates little), and the benchmarks, ignored
//! by default: `cargo test -p llm --release --test bench -- --ignored
//! --nocapture`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use llm::http::{BoxFuture, Http, Payload, Response, body};
use llm::protocols::{chat, gemini, messages, responses};
use llm::{
    Accumulator, Assistant, AssistantPart, Ended, Event, Flow, Frame, Message, Origin, Output,
    Provider, Request, Target, Text, ToolCall, ToolMessage, ToolResult,
};

/// The system allocator, counting: allocations and bytes per thread, so
/// tests running beside each other do not disturb each other's counts, and
/// the bytes live across threads for the peak.
struct Counting;

thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
    static BYTES: Cell<usize> = const { Cell::new(0) };
}

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn count(size: usize) {
    // A thread being torn down counts nothing.
    let _ = ALLOCS.try_with(|allocs| allocs.set(allocs.get() + 1));
    let _ = BYTES.try_with(|bytes| bytes.set(bytes.get() + size));
    let live = LIVE.fetch_add(size, Relaxed) + size;
    PEAK.fetch_max(live, Relaxed);
}

// Only this test binary counts: the allocator defers to the system's.
#[allow(unsafe_code)]
// SAFETY: every method forwards to `System`, which upholds the contract.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        // SAFETY: the caller upholds `System.alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        // SAFETY: the caller upholds `System.dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        count(new);
        LIVE.fetch_sub(layout.size(), Relaxed);
        // SAFETY: the caller upholds `System.realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// What `f` allocates on this thread: its result, allocations and bytes.
fn allocated<T>(f: impl FnOnce() -> T) -> (T, usize, usize) {
    let (allocs, bytes) = (ALLOCS.with(Cell::get), BYTES.with(Cell::get));
    let out = f();
    (
        out,
        ALLOCS.with(Cell::get) - allocs,
        BYTES.with(Cell::get) - bytes,
    )
}

/// Runs `f` `runs` times and prints its fastest time, which a busy machine
/// disturbs least, and its allocations per run.
fn measure(label: &str, runs: usize, mut f: impl FnMut()) {
    f();
    let (allocs, bytes) = (ALLOCS.with(Cell::get), BYTES.with(Cell::get));
    let base = LIVE.load(Relaxed);
    PEAK.store(base, Relaxed);
    let mut fastest = Duration::MAX;
    for _ in 0..runs {
        let start = Instant::now();
        f();
        fastest = fastest.min(start.elapsed());
    }
    let runs = runs.max(1);
    eprintln!(
        "{label}: {fastest:?}/run, {} allocs/run, {} KiB allocated/run, peak {} KiB",
        (ALLOCS.with(Cell::get) - allocs) / runs,
        (BYTES.with(Cell::get) - bytes) / runs / 1024,
        (PEAK.load(Relaxed) - base) / 1024,
    );
}

struct Recorded {
    status: u16,
    body: Vec<u8>,
}

impl Http for Recorded {
    fn send(&self, _: http::Request<Payload>) -> BoxFuture<'static, io::Result<Response>> {
        let chunks: Vec<_> = self.body.chunks(1024).map(<[u8]>::to_vec).collect();
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
                    body: body.as_bytes().to_vec(),
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
fn long_chat(deltas: usize) -> Vec<u8> {
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

/// A chat completions stream of one tool call in `fragments` argument
/// fragments that name neither the call's index nor its id, as some
/// compatible servers stream them: each is judged against the arguments
/// so far.
fn long_chat_call(fragments: usize) -> Vec<u8> {
    let mut body = String::from(
        "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"content\\\":\\\"\"}}]}}]}\n\n",
    );
    for n in 0..fragments {
        let _ = write!(
            body,
            "data: {{\"id\":\"c1\",\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"function\":{{\"arguments\":\"line {n} of the file \"}}}}]}}}}]}}\n\n"
        );
    }
    body.push_str("data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"function\":{\"arguments\":\"\\\"}\"}}]}}]}\n\n");
    body.push_str("data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":20}}\n\ndata: [DONE]\n\n");
    body.into()
}

/// A messages stream of `deltas` thinking deltas and as many text deltas.
fn long_messages(deltas: usize) -> Vec<u8> {
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
    let call_body = Recorded {
        status: 200,
        body: long_chat_call(4_000),
    };
    measure("chat call in 4k unindexed fragments", 5, || {
        let reply = rt::block_on(llm::generate(&call_body, &chat, &request)).expect("reply");
        assert_eq!(reply.message.tool_calls().count(), 1);
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
                    input: llm::Json::encode(&serde_json::json!({"command": "cat notes.md"}))
                        .unwrap(),
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
    let text = serde_json::to_string(&reply).expect("a reply writes");
    measure("from_json 150 parts", 200, || {
        let _ = std::hint::black_box(llm::Response::from_json(&text));
    });
    measure("derived Deserialize from text 150 parts", 200, || {
        let _ = std::hint::black_box(serde_json::from_str::<llm::Response>(&text));
    });
    measure("text to a tree 150 parts", 200, || {
        let _ = std::hint::black_box(serde_json::from_str::<serde_json::Value>(&text));
    });
}

/// The replies a session's log at `A_SESSION_LOG` holds, as their text.
fn logged_replies() -> Option<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Entry<'a> {
        #[serde(borrow)]
        kind: std::borrow::Cow<'a, str>,
        #[serde(borrow)]
        outcome: Option<Outcome<'a>>,
    }
    #[derive(serde::Deserialize)]
    struct Outcome<'a> {
        #[serde(borrow)]
        output: Option<&'a serde_json::value::RawValue>,
    }
    let text = std::fs::read_to_string(std::env::var_os("A_SESSION_LOG")?).expect("a log");
    let replies = text.lines().skip(1).filter_map(|line| {
        let entry: Entry<'_> = serde_json::from_str(line).ok()?;
        let output = entry.outcome?.output?;
        (entry.kind == "model_result").then(|| output.get().to_owned())
    });
    Some(replies.collect())
}

/// Every reply of a session's log read as the engine reads it at load and
/// on each turn.
#[test]
#[ignore = "a benchmark"]
fn read_session_replies() {
    let Some(replies) = logged_replies() else {
        return;
    };
    let bytes: usize = replies.iter().map(String::len).sum();
    eprintln!("{} replies, {} KiB", replies.len(), bytes / 1024);
    measure("session replies from_json", 10, || {
        for reply in &replies {
            let _ = std::hint::black_box(llm::Response::from_json(reply).expect("a reply"));
        }
    });
    measure("session replies text to a tree", 10, || {
        for reply in &replies {
            let _ = std::hint::black_box(serde_json::from_str::<serde_json::Value>(reply));
        }
    });
}

/// The heap a session's typed transcript holds: the log at `A_SESSION_LOG`
/// read as the engine reads it (replies, tool outputs, user text).
#[test]
#[ignore = "a benchmark"]
fn transcript_memory() {
    let Some(path) = std::env::var_os("A_SESSION_LOG") else {
        return;
    };
    let text = std::fs::read_to_string(path).expect("a log");
    let base = LIVE.load(Relaxed);
    let mut messages = llm::Messages::new();
    for line in text.lines().skip(1) {
        let entry: serde_json::Value = serde_json::from_str(line).expect("an entry");
        let message = match entry["kind"].as_str() {
            Some("user" | "context") => Message::user(entry["text"].as_str().unwrap_or_default()),
            Some("model_result") => {
                match llm::Response::from_json(&entry["outcome"]["output"].to_string()) {
                    Ok(reply) => Message::Assistant(reply.message),
                    Err(_) => continue,
                }
            }
            Some("tool_result") => Message::Tool(ToolMessage {
                results: vec![ToolResult {
                    id: "call".into(),
                    name: "bash".into(),
                    output: Output::Text(entry["outcome"]["output"]["output"].to_string()),
                    error: false,
                    meta: llm::Meta::new(),
                }],
            }),
            _ => continue,
        };
        messages.push(message);
    }
    eprintln!(
        "{} messages from {} KiB of log: {} KiB held",
        messages.len(),
        text.len() / 1024,
        (LIVE.load(Relaxed) - base) / 1024
    );
    // A request for it, as the session's model takes it: encoded whole,
    // then grown by a turn.
    let (host, quirks) = responses::HOSTS[0];
    let provider = Provider::new(
        host.id,
        "http://x",
        llm::Auth::None,
        responses::Responses::new(quirks),
    );
    let reply = Recorded {
        status: 200,
        body: long_chat(1),
    };
    let request = |messages: llm::Messages| Request {
        model: "gpt-6-luna".into(),
        messages,
        ..Request::default()
    };
    let mut fresh: Vec<llm::Messages> = (0..=5).map(|_| messages.to_vec().into()).collect();
    measure("session request, whole", 5, || {
        let _ = llm::stream(&reply, &provider, &request(fresh.pop().expect("a copy")));
    });
    let _ = llm::stream(&reply, &provider, &request(messages.clone()));
    measure("session request, grown by a turn", 20, || {
        messages.push(Message::user("and then?"));
        let _ = llm::stream(&reply, &provider, &request(messages.clone()));
    });
}

/// The cassette at `path` under `fixtures/rig/cassettes`.
fn cassette(path: &str) -> String {
    std::fs::read_to_string(root().join(path)).expect("cassette")
}

/// The `data:` lines of a recorded body.
fn frames(body: &str) -> Vec<String> {
    let data = body.lines().filter_map(|line| line.strip_prefix("data: "));
    data.map(str::to_owned).collect()
}

/// The streams of every exchange in `cassettes` to `path`: the model
/// asked, and the frames.
fn streams(cassettes: &[PathBuf], path: &str) -> Vec<(String, Vec<String>)> {
    cassettes
        .iter()
        .flat_map(|file| {
            let text = std::fs::read_to_string(file).expect("cassette");
            serde_yaml_ng::Deserializer::from_str(&text)
                .filter_map(|doc| {
                    let doc: serde_yaml_ng::Value = serde::Deserialize::deserialize(doc).ok()?;
                    let when = doc.get("when")?;
                    if !when.get("path")?.as_str()?.contains(path) {
                        return None;
                    }
                    let sent: serde_json::Value =
                        serde_json::from_str(when.get("body")?.as_str().unwrap_or("{}")).ok()?;
                    let body = doc.get("then")?.get("body")?.as_str()?;
                    let model = sent["model"].as_str().unwrap_or("recorded-model");
                    Some((model.to_owned(), frames(body)))
                })
                .filter(|(_, frames)| !frames.is_empty())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// A recorded reply of four paragraphs, streamed in some 350 deltas.
fn text_delta_stop() -> Recorded {
    let text = cassette("openai/corpus_breadth/text_delta_stop.yaml");
    let doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text).expect("yaml");
    let body = doc["then"]["body"].as_str().expect("a body");
    Recorded {
        status: 200,
        body: body.as_bytes().to_vec(),
    }
}

/// A streamed token is lent to the reader as a piece and moved to the
/// reply whole with its end: neither the writer nor the fold copies it.
#[test]
fn a_streamed_token_is_copied_once() {
    let (host, quirks) = responses::HOSTS[0];
    let provider = Provider::new(
        host.id,
        "http://recorded",
        llm::Auth::None,
        responses::Responses::new(quirks),
    );
    let request = Request {
        model: "gpt-4o".into(),
        messages: vec![Message::user("hi")].into(),
        ..Request::default()
    };
    let mut events = llm::stream(&text_delta_stop(), &provider, &request);
    let (mut read, mut pieces, mut ended) = (Vec::new(), String::new(), None);
    while let Some(event) = rt::block_on(events.next()) {
        let event = event.expect("an event");
        match event {
            Event::Delta { text, .. } => pieces.push_str(text),
            Event::End {
                ended: Ended::Text { text },
                ..
            } => ended = Some(text.as_ptr()),
            _ => {}
        }
        read.push(event.clone());
    }
    let reply = events.finish(None).expect("the reply");
    let [AssistantPart::Text(answer)] = reply.message.content.as_slice() else {
        panic!("one text part: {:?}", reply.message.content);
    };
    assert!(read.len() > 300 && answer.text.len() > 2000);
    assert_eq!(answer.text, pieces, "the end states what streamed");
    // The buffer the text grew in while it streamed is the reply's: moved
    // on at its end, not copied.
    assert_eq!(ended, Some(answer.text.as_ptr()));
    // Folding the events into a reply copies none of the text that
    // streamed: a piece lent to the reader is only read.
    let mut reply = Accumulator::new();
    for event in read {
        let delta = matches!(event, Event::Delta { .. });
        let (pushed, _, bytes) = allocated(|| reply.push(event));
        pushed.expect("in order");
        assert!(!delta || bytes == 0, "a delta folded with {bytes} bytes");
    }
}

/// A Gemini chunk that continues a text run parses into types that borrow
/// its text, so it allocates little beyond what its events own; parsing it
/// as a `Value`, as the decoder once did first, allocates several times
/// more.
#[test]
fn a_gemini_chunk_that_continues_text_allocates_a_fraction_of_its_value() {
    let mut cassettes = Vec::new();
    walk(&root().join("gemini"), &mut cassettes);
    let streams = streams(&cassettes, ":streamGenerateContent");
    let (host, quirks) = gemini::HOSTS[0];
    let provider = Provider::new(
        host.id,
        "http://recorded",
        llm::Auth::None,
        gemini::Gemini::new(quirks),
    );
    let (mut chunks, mut typed, mut parsed) = (0, 0, 0);
    let mut events = Vec::with_capacity(64);
    for (model, frames) in &streams {
        let target = Target {
            provider: &provider,
            model,
        };
        let mut decoder = provider.protocol.decoder(&target, &Request::default());
        for (index, data) in frames.iter().enumerate() {
            let frame = Frame { event: "", data };
            let (flow, made, _) = allocated(|| decoder.frame(frame, &mut events));
            if index > 0 && data.contains("\"text\"") && !data.contains("functionCall") {
                chunks += 1;
                typed += made;
                parsed += allocated(|| drop(serde_json::from_str::<serde_json::Value>(data))).1;
            }
            events.clear();
            if !matches!(flow, Ok(Flow::More)) {
                break;
            }
        }
    }
    eprintln!(
        "{chunks} later text chunks: decoding allocates {typed} times, parsing them as \
         values alone {parsed}"
    );
    assert!(chunks >= 20, "{chunks} text chunks");
    assert!(typed * 3 < parsed, "{typed} allocations against {parsed}");
}

/// The Chat Completions decoder's time per frame on recorded streams, the
/// fastest of several rounds.
#[test]
#[ignore = "a benchmark"]
fn chat_time_per_frame() {
    for (host, cassette) in [
        (
            "openai",
            "openai/streaming_grammar_chat/long_text_stream.yaml",
        ),
        ("venice", "venice/streaming/streaming_smoke.yaml"),
        (
            "deepseek",
            "deepseek/agent_tool_sessions/sequential_complex_tool_calls_streaming.yaml",
        ),
    ] {
        let provider = provider(host, "/chat/completions").expect("a chat host");
        let exchanges = streams(&[root().join(cassette)], "/chat/completions");
        let frames: usize = exchanges.iter().map(|(_, frames)| frames.len()).sum();
        let mut events = Vec::with_capacity(4096);
        let (rounds, runs) = (7, 100);
        let mut fastest = Duration::MAX;
        for _ in 0..rounds {
            let start = Instant::now();
            for _ in 0..runs {
                for (model, frames) in &exchanges {
                    let target = Target {
                        provider: &provider,
                        model,
                    };
                    let mut decoder = provider.protocol.decoder(&target, &Request::default());
                    events.clear();
                    for data in frames {
                        let frame = Frame {
                            event: "message",
                            data,
                        };
                        if decoder.frame(frame, &mut events).expect("decodes") == Flow::Done {
                            break;
                        }
                    }
                }
            }
            fastest = fastest.min(start.elapsed());
        }
        let per_frame = fastest / (u32::try_from(frames).expect("frames") * runs);
        eprintln!("{cassette}: {frames} frames, {per_frame:?} per frame");
    }
}
