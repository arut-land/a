//! Request bodies over a corpus of conversations: every host and quirk of
//! every protocol, sending conversations made of the replies recorded in
//! `fixtures/rig/cassettes` (its own and other protocols'), grown a message
//! at a time as a session grows them. The bodies are pinned by digest in
//! `tests/encode.golden`, made by the encoder before it kept encoded
//! messages; `UPDATE_GOLDEN=1` rewrites it.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

use llm::http::{BoxFuture, Http, Payload, Response, body};
use llm::protocols::{chat, gemini, messages, responses};
use llm::{
    AssistantPart, Media, Message, Origin, Output, Provider, Request, Source, Text, Tool,
    ToolChoice, ToolMessage, ToolResult, User, UserPart,
};
use serde_json::json;

struct Recorded(String);

impl Http for Recorded {
    fn send(&self, _: http::Request<Payload>) -> BoxFuture<'static, io::Result<Response>> {
        let chunks: Vec<Vec<u8>> = self.0.as_bytes().chunks(97).map(<[u8]>::to_vec).collect();
        let response = http::Response::builder()
            .status(200)
            .body(body(chunks))
            .expect("recorded response");
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

/// The protocol a recorded path speaks.
fn protocol_of(path: &str) -> Option<&'static str> {
    if path.ends_with("/responses") {
        Some(responses::KEY)
    } else if path.ends_with("/chat/completions") {
        Some(chat::KEY)
    } else if path.contains(":streamGenerateContent") {
        Some(gemini::KEY)
    } else if path.ends_with("/messages") {
        Some(messages::KEY)
    } else {
        None
    }
}

/// A provider speaking `key` at its first host, for decoding.
fn decoder(key: &str) -> Provider {
    let url = "http://recorded";
    let auth = llm::Auth::None;
    match key {
        chat::KEY => Provider::new("openai", url, auth, chat::Chat::new(chat::HOSTS[0].1)),
        messages::KEY => Provider::new(
            "anthropic",
            url,
            auth,
            messages::Messages::new(messages::HOSTS[0].1),
        ),
        responses::KEY => Provider::new(
            "openai",
            url,
            auth,
            responses::Responses::new(responses::HOSTS[0].1),
        ),
        _ => Provider::new("gemini", url, auth, gemini::Gemini::new(gemini::HOSTS[0].1)),
    }
}

/// Every reply recorded that decodes, by the protocol that produced it, in
/// cassette order. The ids the writer issues itself are random per process,
/// so they are renumbered.
fn replies() -> Vec<(&'static str, llm::Response)> {
    let mut files = Vec::new();
    walk(&root(), &mut files);
    files.sort();
    let mut out = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("cassette");
        for doc in serde_yaml_ng::Deserializer::from_str(&text) {
            let doc: serde_yaml_ng::Value = serde::Deserialize::deserialize(doc).expect("yaml");
            let Some(key) = doc["when"]["path"].as_str().and_then(protocol_of) else {
                continue;
            };
            if doc["then"]["status"].as_u64() != Some(200) {
                continue;
            }
            let body = doc["then"]["body"].as_str().unwrap_or_default();
            let request = Request {
                model: "recorded".into(),
                messages: vec![Message::user("hi")].into(),
                ..Request::default()
            };
            let decoded = rt::block_on(llm::generate(
                &Recorded(body.to_owned()),
                &decoder(key),
                &request,
            ));
            if let Ok(mut reply) = decoded {
                for part in &mut reply.message.content {
                    if let AssistantPart::ToolCall(call) = part
                        && call.id.len() == 21
                        && call.id.starts_with("call_")
                    {
                        call.id = format!("call_issued_{}", out.len()).into();
                    }
                }
                out.push((key, reply));
            }
        }
    }
    out
}

/// Every host and quirk to encode for, with the models it is asked for.
fn targets() -> Vec<(String, Provider, &'static [&'static str])> {
    let url = "http://recorded";
    let auth = || llm::Auth::None;
    let mut out = Vec::new();
    for (host, quirks) in chat::HOSTS {
        let provider = Provider::new(host.id, url, auth(), chat::Chat::new(*quirks));
        out.push((
            format!("chat/{}", host.id),
            provider,
            &["gpt-4o", "deepseek-reasoner"][..],
        ));
    }
    for (host, quirks) in messages::HOSTS {
        let provider = Provider::new(host.id, url, auth(), messages::Messages::new(*quirks));
        out.push((
            format!("messages/{}", host.id),
            provider,
            &["claude-opus-5-5", "claude-sonnet-4-5"][..],
        ));
    }
    for (host, quirks) in responses::HOSTS {
        let provider = Provider::new(host.id, url, auth(), responses::Responses::new(*quirks));
        out.push((
            format!("responses/{}", host.id),
            provider,
            &["gpt-5", "gpt-4o"][..],
        ));
    }
    let all = responses::Quirks {
        instructions: responses::Instructions::All,
        ..responses::HOSTS[0].1
    };
    out.push((
        "responses/all-instructions".into(),
        Provider::new("openai", url, auth(), responses::Responses::new(all)),
        &["gpt-5"][..],
    ));
    for (host, quirks) in gemini::HOSTS {
        let provider = Provider::new(host.id, url, auth(), gemini::Gemini::new(*quirks));
        out.push((
            format!("gemini/{}", host.id),
            provider,
            &["gemini-3-pro-preview", "gemini-2.5-flash"][..],
        ));
    }
    let current = gemini::Quirks {
        thought_replay: gemini::ThoughtReplay::CurrentTurn,
        ..gemini::HOSTS[0].1
    };
    out.push((
        "gemini/current-turn".into(),
        Provider::new("gemini", url, auth(), gemini::Gemini::new(current)),
        &["gemini-3-pro-preview"][..],
    ));
    out
}

/// The answers to every call of `reply`'s message.
fn answers(message: &llm::Assistant, turn: usize) -> Option<Message> {
    let results: Vec<ToolResult> = message
        .tool_calls()
        .enumerate()
        .map(|(n, call)| ToolResult {
            id: call.id.clone(),
            name: call.name.clone(),
            output: match (turn + n) % 3 {
                0 => Output::Text(format!("result {turn}.{n}")),
                1 => Output::Json(json!({"ok": true, "turn": turn})),
                _ => Output::Content(vec![
                    UserPart::Text(Text::new("see the chart")),
                    UserPart::Media(image()),
                ]),
            },
            error: n % 4 == 3,
            meta: llm::Meta::new(),
        })
        .collect();
    (!results.is_empty()).then_some(Message::Tool(ToolMessage { results }))
}

fn image() -> Media {
    Media {
        media_type: "image/png".into(),
        source: Source::Data {
            data: "iVBORw0KGgo=".into(),
        },
        meta: llm::Meta::new(),
    }
}

/// A conversation of `replies`, each answered, sent as `origin` produced
/// them when it is given, else as their own protocol did.
fn conversation(replies: &[&llm::Response], origin: Option<&Origin>, edges: bool) -> Vec<Message> {
    let mut out = vec![
        Message::system("You are terse."),
        Message::user("<environment>cwd: /w</environment>"),
        Message::user("hi"),
    ];
    for (turn, reply) in replies.iter().enumerate() {
        let mut message = reply.message.clone();
        if let Some(origin) = origin {
            message.origin = Some(origin.clone());
        }
        let answered = answers(&message, turn);
        out.push(Message::Assistant(message));
        if edges && turn == 1 {
            // A turn interrupted before its calls ran, then a steer.
            out.push(Message::user("stop, do this instead"));
            continue;
        }
        out.extend(answered);
        match (edges, turn % 3) {
            (true, 0) => out.push(Message::system("Mind the budget.")),
            (true, 2) => out.push(Message::User(User {
                content: vec![
                    UserPart::Text(Text::new("and this")),
                    UserPart::Media(image()),
                ],
            })),
            _ => {}
        }
        out.push(Message::user(format!("go on {turn}")));
    }
    out
}

fn bash() -> Tool {
    Tool {
        name: "bash".into(),
        description: "Runs a command.".into(),
        schema: json!({"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}),
        strict: None,
        encoded: llm::Encodings::default(),
    }
}

/// A request for `messages`, as variant `variant` asks.
fn request(model: &str, key: &str, variant: usize, messages: impl Into<llm::Messages>) -> Request {
    let mut request = Request {
        model: model.into(),
        messages: messages.into(),
        generation: llm::Generation {
            max_output_tokens: std::num::NonZeroU32::new(1024),
            ..llm::Generation::default()
        },
        ..Request::default()
    };
    if variant % 2 == 0 {
        request.tools = vec![bash()].into();
    }
    match (key, variant) {
        (_, 3) => request.tool_choice = ToolChoice::Required,
        (messages::KEY, 1) => {
            request.options = llm::Options::from([(key.into(), json!({"prompt_caching": true}))]);
        }
        (messages::KEY, 5) => {
            let ttl = json!({"static_prefix_cache_ttl": "1h", "cache_control": {"type": "ephemeral", "ttl": "1h"}});
            request.options = llm::Options::from([(key.into(), ttl)]);
        }
        (responses::KEY, 1) => {
            request.options = llm::Options::from([(key.into(), json!({"store": true}))]);
        }
        (_, 5) => {
            request.body = json!({"temperature": 0.5}).as_object().cloned();
        }
        _ => {}
    }
    request
}

/// FNV-1a, fed as a writer.
struct Fnv(u64);

impl Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0 ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
        }
    }
}

/// One conversation sent to one target, grown a message at a time.
struct Case {
    label: String,
    provider: Provider,
    model: &'static str,
    variant: usize,
    messages: Vec<Message>,
}

/// `n` of `from`, starting at a place `seed` picks.
fn pick<'a>(from: &[&'a llm::Response], seed: usize, n: usize) -> Vec<&'a llm::Response> {
    let start = seed * 5 % from.len().max(1);
    from.iter().cycle().skip(start).take(n).copied().collect()
}

fn cases() -> Vec<Case> {
    let replies = replies();
    let mut out = Vec::new();
    for (label, provider, models) in targets() {
        let key = provider.protocol.key();
        let own: Vec<&llm::Response> = replies
            .iter()
            .filter(|(by, _)| *by == key)
            .map(|(_, reply)| reply)
            .collect();
        let others: Vec<&llm::Response> = replies
            .iter()
            .filter(|(by, _)| *by != key)
            .map(|(_, reply)| reply)
            .collect();
        let salt = label.bytes().map(usize::from).sum::<usize>();
        for (m, model) in models.iter().enumerate() {
            let origin = llm::origin(&provider, model);
            for variant in 0..6 {
                let pick = |from, n| pick(from, salt + 7 * variant + 13 * m, n);
                let messages = match variant {
                    // Another origin's replies, which the target rebuilds.
                    4 => conversation(&pick(&others, 5), None, false),
                    // Its own, with steers, mid-conversation system
                    // messages and an unanswered call.
                    2 | 5 => conversation(&pick(&own, 5), Some(&origin), true),
                    _ => conversation(&pick(&own, 5), Some(&origin), false),
                };
                out.push(Case {
                    label: format!("{label} {model} v{variant}"),
                    provider: provider.clone(),
                    model,
                    variant,
                    messages,
                });
            }
        }
    }
    out
}

/// The body sent for `request`, or its error.
fn sent(provider: &Provider, request: &Request) -> Vec<u8> {
    match provider.request(&request.model, request) {
        Ok(http) => http.body().to_vec(),
        Err(error) => format!("error: {error}").into_bytes(),
    }
}

fn golden() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/encode.golden")
}

/// Every body is the one pinned, whether its conversation was encoded
/// whole or grew from what earlier requests encoded of it: a tool message
/// a result at a time, as a session's grows.
#[test]
fn every_body_is_the_one_pinned() {
    let mut lines = String::new();
    for case in cases() {
        let key = case.provider.protocol.key();
        let send = |messages: llm::Messages| {
            sent(
                &case.provider,
                &request(case.model, key, case.variant, messages),
            )
        };
        let mut hash = Fnv(0xcbf2_9ce4_8422_2325);
        let mut grown = llm::Messages::new();
        for (at, message) in case.messages.iter().enumerate() {
            match message {
                Message::Tool(tool) if tool.results.len() > 1 => {
                    let first = tool.results[..1].to_vec();
                    grown.push(Message::Tool(ToolMessage { results: first }));
                    send(grown.clone());
                    if let Some(Message::Tool(last)) = grown.last_mut() {
                        last.results.extend_from_slice(&tool.results[1..]);
                    }
                }
                message => grown.push(message.clone()),
            }
            let whole = send(case.messages[..=at].to_vec().into());
            let body = send(grown.clone());
            assert!(
                body == whole,
                "{} at {at}:\n grown: {}\n whole: {}",
                case.label,
                String::from_utf8_lossy(&body),
                String::from_utf8_lossy(&whole)
            );
            hash.write(&(body.len() as u64).to_le_bytes());
            hash.write(&body);
        }
        let _ = writeln!(lines, "{:016x} {}", hash.0, case.label);
    }
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(golden(), &lines).expect("golden");
        return;
    }
    let pinned = std::fs::read_to_string(golden()).expect("golden");
    let differ: Vec<_> = lines
        .lines()
        .zip(pinned.lines())
        .filter(|(ours, pinned)| ours != pinned)
        .map(|(ours, _)| ours)
        .collect();
    assert_eq!(lines.lines().count(), pinned.lines().count());
    assert!(
        differ.is_empty(),
        "{} differ:\n{}",
        differ.len(),
        differ.join("\n")
    );
}
