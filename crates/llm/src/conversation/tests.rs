use serde_json::{Value, json};

use super::*;
use crate::protocol::{Auth, Provider};
use crate::protocols::{chat, gemini, messages, responses};
use crate::spec::{
    Assistant, AssistantPart, Output, Request, Text, ToolCall, ToolMessage, ToolResult, Tools,
};

fn providers() -> [Provider; 4] {
    let url = "https://example.test";
    [
        Provider::new("openai", url, Auth::None, chat::Chat::new(chat::HOSTS[0].1)),
        Provider::new(
            "anthropic",
            url,
            Auth::None,
            messages::Messages::new(messages::HOSTS[0].1),
        ),
        Provider::new(
            "openai",
            url,
            Auth::None,
            responses::Responses::new(responses::HOSTS[0].1),
        ),
        Provider::new(
            "gemini",
            url,
            Auth::None,
            gemini::Gemini::new(gemini::HOSTS[0].1),
        ),
    ]
}

/// A tool round `provider` produced for `model`: its call and the result.
fn round(provider: &Provider, model: &str, turn: usize) -> [Message; 2] {
    let id: SmolStr = format!("call_{turn}").into();
    let call = ToolCall {
        id: id.clone(),
        name: "bash".into(),
        input: crate::Json::encode(&json!({"command": "ls"})).unwrap(),
        invalid: None,
        provider_executed: false,
        meta: crate::Meta::new(),
    };
    let reply = Assistant {
        content: vec![
            AssistantPart::Text(Text::new(format!("step {turn}"))),
            AssistantPart::ToolCall(call),
        ],
        origin: Some(crate::origin(provider, model)),
    };
    let result = ToolResult {
        id,
        name: "bash".into(),
        output: Output::Text(format!("file {turn}\n").repeat(100)),
        error: false,
        meta: crate::Meta::new(),
    };
    [
        Message::Assistant(reply),
        Message::Tool(ToolMessage {
            results: vec![result],
        }),
    ]
}

fn body(provider: &Provider, messages: &Messages) -> Vec<u8> {
    let request = Request {
        model: "claude-sonnet-4-5".into(),
        messages: messages.clone().into(),
        generation: crate::Generation {
            max_output_tokens: std::num::NonZeroU32::new(64),
            ..crate::Generation::default()
        },
        ..Request::default()
    };
    let sent = provider.request(&request.model, &request);
    sent.expect("the request encodes").body().to_vec()
}

fn folded() -> usize {
    FOLDED.with(std::cell::Cell::take)
}

#[test]
fn large_settled_runs_share_chunks_without_reencoding_the_prefix_or_changing_request_bytes() {
    for provider in providers() {
        let mut canonical = Messages::from(vec![
            Message::system("terse"),
            Message::user("go"),
            Message::assistant("🦀 text ".repeat(200_000)),
            Message::user("next"),
        ]);
        let mut tail = canonical.clone();
        let mut resident = Resident::default();
        let stored = StoredMessages::new(&canonical);
        let reads = Arc::new(AtomicU64::new(0));
        let request = stored.request(&resident, &tail, &reads);
        let actual = provider
            .request(&request.model, &request)
            .unwrap()
            .into_body();
        folded();
        drop(request);
        resident.retain(&mut tail);
        assert!(actual.pieces().len() > 4);
        assert!(actual.pieces().iter().all(|piece| piece.len() <= 256 << 10));
        assert_eq!(
            actual.to_vec(),
            body(&provider, &Messages::from(canonical.to_vec()))
        );
        folded();
        canonical.push(Message::user("one more"));
        tail.push(Message::user("one more"));
        stored.write(&canonical);
        let request = stored.request(&resident, &tail, &reads);
        let next = provider
            .request(&request.model, &request)
            .unwrap()
            .into_body();
        assert_eq!(folded(), 1);
        assert_eq!(reads.load(Ordering::Relaxed), 0);
        assert_eq!(
            next.to_vec(),
            body(&provider, &Messages::from(canonical.to_vec()))
        );
        folded();
        assert_eq!(
            actual.to_vec(),
            body(
                &provider,
                &Messages::from(canonical[..canonical.len() - 1].to_vec())
            )
        );
        folded();
    }
}

#[test]
fn building_a_request_reserializes_nothing() {
    for provider in providers() {
        let mut messages = Messages::from(vec![Message::system("terse"), Message::user("go")]);
        body(&provider, &messages);
        assert_eq!(folded(), 2);
        for turn in 0..20 {
            for message in round(&provider, "claude-sonnet-4-5", turn) {
                messages.push(message);
            }
            let sent = body(&provider, &messages);
            // Only the messages added since the last request are encoded.
            assert_eq!(folded(), 2, "{}", provider.protocol.key());
            let whole = body(&provider, &Messages::from(messages.to_vec()));
            assert_eq!(folded(), messages.len());
            assert_eq!(sent, whole, "{}", provider.protocol.key());
        }
        // The same conversation again encodes nothing.
        body(&provider, &messages);
        assert_eq!(folded(), 0);
    }
}

#[test]
fn a_change_before_the_end_is_encoded_again() {
    for provider in providers() {
        let mut messages = Messages::from(vec![Message::system("terse"), Message::user("go")]);
        for message in round(&provider, "claude-sonnet-4-5", 0) {
            messages.push(message);
        }
        body(&provider, &messages);
        folded();
        // The last message changed: folded again from the start.
        if let Some(Message::Tool(tool)) = messages.last_mut() {
            tool.results[0].output = Output::Text("changed".into());
        }
        let sent = body(&provider, &messages);
        assert_eq!(folded(), messages.len());
        assert_eq!(sent, body(&provider, &Messages::from(messages.to_vec())));
        folded();
        // A message inserted before the end, as a session's context is.
        messages.insert(1, Message::user("<context/>"));
        let sent = body(&provider, &messages);
        assert_eq!(folded(), messages.len());
        assert_eq!(sent, body(&provider, &Messages::from(messages.to_vec())));
        folded();
        // A message changed before the end: every prefix past it is
        // encoded again, though the last message is the same.
        messages.push(Message::user("next"));
        body(&provider, &messages);
        folded();
        let Some(Message::Tool(tool)) = messages.get_mut(4) else {
            panic!("the tool message");
        };
        tool.results[0].output = Output::Text("changed again".into());
        let sent = body(&provider, &messages);
        assert_eq!(folded(), messages.len());
        assert_eq!(sent, body(&provider, &Messages::from(messages.to_vec())));
        folded();
        // A copy that grows apart keeps what the shared prefix encoded.
        let mut copy = messages.clone();
        copy.push(Message::user("and then"));
        let sent = body(&provider, &copy);
        assert_eq!(folded(), 1);
        assert_eq!(sent, body(&provider, &Messages::from(copy.to_vec())));
    }
}

/// The request's tools are written from the bytes the tool set keeps, in
/// the same place and shape as the body's own field, and an escape-hatch
/// `tools` is the one sent.
#[test]
fn tools_are_spliced_as_the_bodys_field_and_an_escape_hatch_replaces_them() {
    let tools = Tools::from(vec![crate::Tool {
        name: "bash".into(),
        description: "Runs a command.".into(),
        schema: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        strict: None,
    }]);
    for provider in providers() {
        let request = Request {
            model: "claude-sonnet-4-5".into(),
            messages: vec![Message::user("go")].into(),
            tools: tools.clone(),
            generation: crate::Generation {
                max_output_tokens: std::num::NonZeroU32::new(64),
                ..crate::Generation::default()
            },
            ..Request::default()
        };
        let sent = |request: &Request| -> Value {
            let http = provider.request(&request.model, request).expect("encodes");
            serde_json::from_slice(&http.body().to_vec()).expect("a JSON body")
        };
        let body = sent(&request);
        let tools = body["tools"].as_array().expect("tools");
        let text = serde_json::to_string(&body).unwrap();
        assert!(
            text.contains(r#""bash""#),
            "{}: {text}",
            provider.protocol.key()
        );
        assert_eq!(tools.len(), 1, "{}: {tools:?}", provider.protocol.key());
        let again = sent(&request);
        assert_eq!(again, body, "{}", provider.protocol.key());
        let overlay = Request {
            body: json!({"tools": [{"type": "hosted"}]}).as_object().cloned(),
            ..request.clone()
        };
        assert_eq!(sent(&overlay)["tools"], json!([{"type": "hosted"}]));
    }
}

#[test]
fn complete_coverage_never_drops_an_exchange_or_an_oversized_newest_message() {
    let current = vec![Message::user("summarise all supplied context")].into();
    let policy = ContextPolicy {
        exchanges: 8,
        bytes: 65536,
    };
    let mut prefix = Vec::new();
    for n in 0..20 {
        prefix.push(Message::user(format!("request {n}")));
        prefix.push(Message::assistant(format!("answer {n}")));
    }
    let large = Message::user("newest 🦀\\\"\n".repeat(20_000));
    prefix.push(large.clone());
    let mut covered = Vec::new();
    let mut fragments = String::new();
    let mut inputs = 0;
    let mut bytes = 0;
    for chunk in policy.cover(&prefix, &current) {
        let chunk = chunk.unwrap();
        inputs += 1;
        let encoded = crate::Json::encode(&chunk).unwrap();
        bytes += encoded.raw().len();
        assert!(encoded.raw().len() <= policy.bytes as usize);
        assert_eq!(chunk.last(), current.last());
        if matches!(chunk.first(), Some(Message::System(system)) if system.text().starts_with("The next message is a consecutive fragment"))
        {
            let Message::User(user) = &chunk[1] else {
                panic!("fragment text")
            };
            let crate::UserPart::Text(text) = &user.content[0] else {
                panic!("fragment text")
            };
            fragments.push_str(&text.text);
        } else {
            covered.extend_from_slice(&chunk[..chunk.len() - 1]);
        }
    }
    assert_eq!(covered, prefix[..prefix.len() - 1]);
    assert_eq!(fragments, crate::Json::encode(&[large]).unwrap().raw());
    assert!(inputs > 3);
    eprintln!(
        "complete prefix: {} source bytes, {inputs} summary inputs, {bytes} total input bytes",
        crate::Json::encode(&prefix).unwrap().raw().len()
    );
}

#[test]
fn oversized_exchanges_keep_native_media_and_all_tool_data() {
    let media = crate::Media {
        media_type: "image/png".into(),
        source: crate::Source::Bytes {
            data: Arc::from([1, 2, 3]),
        },
        meta: crate::Meta::new(),
    };
    let messages = [Message::Tool(ToolMessage {
        results: vec![ToolResult {
            id: "call".into(),
            name: "read".into(),
            output: Output::Content(vec![
                crate::UserPart::Text(Text::new("large output".repeat(20_000))),
                crate::UserPart::Media(media.clone()),
            ]),
            error: false,
            meta: crate::Meta::new(),
        }],
    })];
    let current = vec![Message::user("summary")].into();
    let chunks: Vec<_> = ContextPolicy {
        exchanges: 8,
        bytes: 65536,
    }
    .cover(&messages, &current)
    .collect::<Result<_, _>>()
    .unwrap();
    assert!(chunks.iter().any(|chunk| chunk.iter().any(|m| matches!(m,
        Message::User(user) if user.content.contains(&crate::UserPart::Media(media.clone()))))));
    let text: String = chunks
        .iter()
        .filter_map(|chunk| match chunk.first() {
            Some(Message::System(_)) => match &chunk[1] {
                Message::User(user) => match &user.content[0] {
                    crate::UserPart::Text(text) => Some(text.text.as_str()),
                    crate::UserPart::Media(_) => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect();
    let restored = Messages::from_json(&text).unwrap();
    let Message::Tool(tool) = &restored[0] else {
        panic!("tool result preserved")
    };
    assert_eq!(tool.results[0].id, "call");
    let Output::Content(content) = &tool.results[0].output else {
        panic!("tool content preserved")
    };
    assert_eq!(
        content[0],
        crate::UserPart::Text(Text::new("large output".repeat(20_000)))
    );
    assert_eq!(
        content[1],
        crate::UserPart::Text(Text::new("[media supplied separately]"))
    );
}

#[test]
fn serialized_summary_fragments_never_expose_private_provider_replay_data() {
    let mut meta = crate::Meta::new();
    meta.insert(
        "responses",
        &json!({"encrypted_content": "private-ciphertext"}),
    );
    let prefix = [Message::Assistant(Assistant {
        content: vec![AssistantPart::Reasoning(crate::Reasoning {
            text: "visible reasoning ".repeat(10_000),
            redacted: false,
            meta,
        })],
        origin: None,
    })];
    let current = vec![Message::user("summary")].into();
    let mut fragments = String::new();
    for chunk in (ContextPolicy {
        exchanges: 8,
        bytes: 65536,
    })
    .cover(&prefix, &current)
    {
        let chunk = chunk.unwrap();
        let Message::User(user) = &chunk[1] else {
            panic!("serialized fragment")
        };
        let crate::UserPart::Text(text) = &user.content[0] else {
            panic!("serialized text")
        };
        fragments.push_str(&text.text);
    }
    assert!(!fragments.contains("private-ciphertext"));
    let restored = Messages::from_json(&fragments).unwrap();
    let Message::Assistant(reply) = &restored[0] else {
        panic!("assistant context")
    };
    let AssistantPart::Reasoning(reasoning) = &reply.content[0] else {
        panic!("visible reasoning")
    };
    assert_eq!(reasoning.text, "visible reasoning ".repeat(10_000));
    let Message::Assistant(original) = &prefix[0] else {
        panic!("original assistant")
    };
    assert!(original.content[0].meta().get("responses").is_some());
}

#[test]
fn oversized_exchanges_keep_opaque_provider_items_native() {
    let mut meta = crate::Meta::new();
    meta.insert(
        "responses",
        &json!({"hosted_result": "source content", "id": "native-id"}),
    );
    let opaque = AssistantPart::Opaque(crate::Opaque { meta });
    let origin = crate::origin(&providers()[2], "model");
    let prefix = [Message::Assistant(Assistant {
        content: vec![
            AssistantPart::Text(Text::new("large answer".repeat(10_000))),
            opaque.clone(),
        ],
        origin: Some(origin.clone()),
    })];
    let current = vec![Message::user("summary")].into();
    let chunks: Vec<_> = ContextPolicy {
        exchanges: 8,
        bytes: 65536,
    }
    .cover(&prefix, &current)
    .collect::<Result<_, _>>()
    .unwrap();
    let expected = Message::Assistant(Assistant {
        content: vec![opaque],
        origin: Some(origin),
    });
    assert!(chunks.iter().any(|chunk| chunk.contains(&expected)));
    for chunk in chunks {
        for message in chunk.iter() {
            if let Message::User(user) = message {
                assert!(!format!("{user:?}").contains("native-id"));
            }
        }
    }
}

struct StoredMessages(std::path::PathBuf);

impl StoredMessages {
    fn new(messages: &Messages) -> Self {
        let stored = Self(std::env::temp_dir().join(format!(
            "a-resident-{}-{}",
            std::process::id(),
            mark()
        )));
        stored.write(messages);
        stored
    }

    fn write(&self, messages: &Messages) {
        std::fs::write(&self.0, crate::Json::encode(messages).unwrap().raw()).unwrap();
    }

    fn request(&self, resident: &Resident, tail: &Messages, reads: &Arc<AtomicU64>) -> Request {
        let path = self.0.clone();
        let reads = Arc::clone(reads);
        Request {
            model: "claude-sonnet-4-5".into(),
            messages: Conversation::retained(resident.clone(), tail.clone(), move || {
                reads.fetch_add(1, Ordering::Relaxed);
                let json = std::fs::read_to_string(&path)
                    .map_err(|error| Error::new(crate::Kind::Other, error.to_string()))?;
                Messages::from_json(&json)
                    .map_err(|error| Error::new(crate::Kind::Decode, error.to_string()))
            }),
            generation: crate::Generation {
                max_output_tokens: std::num::NonZeroU32::new(64),
                ..crate::Generation::default()
            },
            ..Request::default()
        }
    }
}

impl Drop for StoredMessages {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn settled_prefix_is_shared_without_reconstruction() {
    for provider in providers() {
        let mut canonical = Messages::from(vec![Message::system("terse"), Message::user("go")]);
        let mut tail = canonical.clone();
        let mut resident = Resident::default();
        let stored = StoredMessages::new(&canonical);
        let reads = Arc::new(AtomicU64::new(0));
        let first = stored.request(&resident, &tail, &reads);
        provider.request(&first.model, &first).unwrap();
        drop(first);
        folded();
        resident.retain(&mut tail);
        let mut previous = None;
        for turn in 0..20 {
            for message in round(&provider, "claude-sonnet-4-5", turn) {
                canonical.push(message.clone());
                tail.push(message);
            }
            stored.write(&canonical);
            let mut request = stored.request(&resident, &tail, &reads);
            let mut whole = Messages::from(canonical.to_vec());
            for extra in 0..turn % 3 {
                let message = Message::user(format!("request-local reminder {extra}"));
                request.messages.push(message.clone());
                whole.push(message);
            }
            let sent = provider
                .request(&request.model, &request)
                .unwrap()
                .into_body();
            assert_eq!(folded(), 2 + turn % 3, "{}", provider.protocol.key());
            if let Some((payload, bytes)) = previous {
                assert_eq!(crate::http::Payload::to_vec(&payload), bytes);
            }
            let expected = body(&provider, &whole);
            folded();
            assert_eq!(sent.to_vec(), expected, "{}", provider.protocol.key());
            previous = Some((sent, expected));
            drop(request);
            resident.retain(&mut tail);
            assert_eq!(tail.len(), 0);
        }
        assert_eq!(reads.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn settled_canonical_payloads_are_released() {
    for provider in providers() {
        let data: Arc<[u8]> = vec![7; 5 * 1024 * 1024].into();
        let weak = Arc::downgrade(&data);
        let canonical = Messages::from(vec![
            Message::User(crate::User {
                content: vec![crate::UserPart::Media(crate::Media {
                    media_type: "image/png".into(),
                    source: crate::Source::Bytes { data },
                    meta: crate::Meta::new(),
                })],
            }),
            Message::assistant("seen"),
            Message::user("continue"),
        ]);
        let mut tail = canonical.clone();
        let mut resident = Resident::default();
        let stored = StoredMessages::new(&canonical);
        let request = stored.request(&resident, &tail, &Arc::new(AtomicU64::new(0)));
        let prepared = crate::prepare_request(&provider, &request);
        drop(request);
        drop(canonical);
        resident.retain(&mut tail);
        assert!(
            weak.upgrade().is_none(),
            "{} retains canonical payload",
            provider.protocol.key()
        );
        assert_eq!(tail.len(), 0);
        std::hint::black_box(prepared.origin());
    }
}

#[test]
fn mutable_results_and_target_changes_match_canonical_replay() {
    for mut provider in providers() {
        let mut canonical = Messages::from(vec![Message::system("terse"), Message::user("go")]);
        for message in round(&provider, "claude-sonnet-4-5", 0) {
            canonical.push(message);
        }
        let mut tail = canonical.clone();
        let mut resident = Resident::default();
        let stored = StoredMessages::new(&canonical);
        let reads = Arc::new(AtomicU64::new(0));
        let request = stored.request(&resident, &tail, &reads);
        provider.request(&request.model, &request).unwrap();
        drop(request);
        resident.retain(&mut tail);
        for message in round(&provider, "claude-sonnet-4-5", 1) {
            canonical.push(message.clone());
            tail.push(message);
        }
        for messages in [&mut canonical, &mut tail] {
            let Some(Message::Tool(tool)) = messages.last_mut() else {
                panic!("tool tail")
            };
            tool.results[0].output = Output::Text("late output".into());
        }
        stored.write(&canonical);
        for changed in [false, true] {
            if changed {
                provider.id = "another-provider".into();
            }
            let mut request = stored.request(&resident, &tail, &reads);
            request
                .messages
                .push(Message::user("request-local reminder"));
            let sent = provider
                .request(&request.model, &request)
                .unwrap()
                .into_body()
                .to_vec();
            let mut whole = Messages::from(canonical.to_vec());
            whole.push(Message::user("request-local reminder"));
            assert_eq!(sent, body(&provider, &whole), "{}", provider.protocol.key());
        }
        assert_eq!(reads.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn fitting_an_encoded_prefix_changes_only_the_request() {
    for provider in providers() {
        let canonical = Messages::from(vec![
            Message::User(crate::User {
                content: vec![crate::UserPart::Media(crate::Media {
                    media_type: "image/png".into(),
                    source: crate::Source::Data {
                        data: "AA==".into(),
                    },
                    meta: crate::Meta::new(),
                })],
            }),
            Message::assistant("seen"),
            Message::user("continue"),
        ]);
        let mut tail = canonical.clone();
        let mut resident = Resident::default();
        let stored = StoredMessages::new(&canonical);
        let reads = Arc::new(AtomicU64::new(0));
        let request = stored.request(&resident, &tail, &reads);
        provider.request(&request.model, &request).unwrap();
        drop(request);
        resident.retain(&mut tail);
        let request = stored.request(&resident, &tail, &reads);
        assert_eq!(request.messages.images(), 1);
        let target = Target {
            provider: &provider,
            model: &request.model,
        };
        let limits = crate::fit::Limits {
            images: 0,
            bytes: 4096,
        };
        let (_, fitted) = crate::fit::fit(&request, limits, |input| {
            provider.protocol.encode(&target, input)
        })
        .unwrap();
        let whole = Request {
            messages: canonical.clone().into(),
            ..request.clone()
        };
        let (_, expected) = crate::fit::fit(&whole, limits, |input| {
            provider.protocol.encode(&target, input)
        })
        .unwrap();
        assert_eq!(
            fitted.to_vec(),
            expected.to_vec(),
            "{}",
            provider.protocol.key()
        );
        assert_eq!(reads.load(Ordering::Relaxed), 1);
        assert_eq!(request.messages.images(), 1);
    }
}

#[test]
fn diverging_sources_never_reuse_each_others_prefix() {
    for provider in providers() {
        let canonical = Messages::from(vec![
            Message::system("terse"),
            Message::user("shared"),
            Message::assistant("base"),
            Message::user("last"),
        ]);
        let stored = StoredMessages::new(&canonical);
        let mut tail = canonical.clone();
        let mut resident = Resident::default();
        let reads = Arc::new(AtomicU64::new(0));
        let request = stored.request(&resident, &tail, &reads);
        provider.request(&request.model, &request).unwrap();
        drop(request);
        resident.retain(&mut tail);
        let mut left = (resident.clone(), tail.clone(), canonical.clone());
        let mut right = (resident, tail, canonical);
        for (branch, text) in [(&mut left, "left branch"), (&mut right, "right branch")] {
            for message in [Message::assistant(text), Message::user("next")] {
                branch.1.push(message.clone());
                branch.2.push(message);
            }
            stored.write(&branch.2);
            let request = stored.request(&branch.0, &branch.1, &reads);
            provider.request(&request.model, &request).unwrap();
            drop(request);
            branch.0.retain(&mut branch.1);
        }
        stored.write(&left.2);
        let request = stored.request(&left.0, &left.1, &reads);
        let sent = provider
            .request(&request.model, &request)
            .unwrap()
            .into_body()
            .to_vec();
        assert_eq!(
            sent,
            body(&provider, &left.2),
            "{}",
            provider.protocol.key()
        );
    }
}

#[test]
fn canonical_exports_cannot_cache_encoded_bytes_in_the_resident_tail() {
    for provider in providers() {
        let tail = Messages::from(vec![
            Message::user("input"),
            Message::assistant("reply"),
            Message::user("continue"),
        ]);
        let stored = StoredMessages::new(&tail);
        let request = stored.request(&Resident::default(), &tail, &Arc::new(AtomicU64::new(0)));
        let fragment = {
            let canonical = Request {
                messages: request.messages.canonical().unwrap().into_owned().into(),
                ..request.clone()
            };
            let target = Target {
                provider: &provider,
                model: &canonical.model,
            };
            let encoded = provider.protocol.encode(&target, &canonical).unwrap();
            Arc::downgrade(
                encoded
                    .history
                    .as_ref()
                    .unwrap()
                    .settled
                    .iter()
                    .next()
                    .unwrap(),
            )
        };
        drop(request);
        assert!(
            fragment.upgrade().is_none(),
            "{} cached through a canonical export",
            provider.protocol.key()
        );
        assert_eq!(tail.len(), 3);
    }
}

#[test]
fn a_fork_reuses_settled_chunks_without_refolding_its_parent() {
    for provider in providers() {
        let canonical = Messages::from(vec![
            Message::system("terse"),
            Message::user("begin"),
            Message::assistant("settled answer"),
            Message::user("next"),
        ]);
        let stored = StoredMessages::new(&canonical);
        let mut tail = canonical.clone();
        let mut resident = Resident::default();
        let reads = Arc::new(AtomicU64::new(0));
        let request = stored.request(&resident, &tail, &reads);
        provider.request(&request.model, &request).unwrap();
        folded();
        let prefix = request.messages.prefix().unwrap();
        let root = Arc::clone(prefix.0.0.settled.0.first().unwrap());
        let references = Arc::strong_count(&root);
        drop(request);
        resident.retain(&mut tail);
        assert!(tail.is_empty());
        let branch = Messages::from(vec![
            Message::assistant("private answer"),
            Message::user("private prompt"),
        ]);
        let whole: Messages = canonical.iter().chain(branch.iter()).cloned().collect();
        let expected = body(&provider, &whole);
        folded();
        stored.write(&whole);
        let child = Resident::from(prefix.clone());
        let request = stored.request(&child, &branch, &reads);
        let actual = provider
            .request(&request.model, &request)
            .unwrap()
            .body()
            .to_vec();
        assert_eq!(
            folded(),
            branch.len(),
            "{} refolded ancestry",
            provider.protocol.key()
        );
        assert_eq!(reads.load(Ordering::Relaxed), 0);
        assert_eq!(actual, expected);
        let held = child
            .encoding
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .0
            .settled
            .clone();
        assert!(
            held.iter().any(|bytes| Arc::ptr_eq(bytes, &root)),
            "branch replaced its prefix bytes"
        );
        assert_eq!(
            Arc::strong_count(&root),
            references,
            "branch copied settled fragment references"
        );
    }
}

#[test]
fn a_frozen_encoding_never_observes_later_parent_appends() {
    for provider in providers() {
        let canonical = Messages::from(vec![
            Message::system("terse"),
            Message::user("begin"),
            Message::assistant("old answer"),
        ]);
        let stored = StoredMessages::new(&canonical);
        let mut tail = canonical.clone();
        let mut resident = Resident::default();
        let reads = Arc::new(AtomicU64::new(0));
        let request = stored.request(&resident, &tail, &reads);
        provider.request(&request.model, &request).unwrap();
        let prefix = request.messages.prefix().unwrap();
        drop(request);
        resident.retain(&mut tail);
        tail.push(Message::user("later parent input"));
        let parent: Messages = canonical.iter().chain(tail.iter()).cloned().collect();
        stored.write(&parent);
        let request = stored.request(&resident, &tail, &reads);
        provider.request(&request.model, &request).unwrap();
        let private = Messages::from(vec![Message::user("child input")]);
        let expected: Messages = canonical.iter().chain(private.iter()).cloned().collect();
        let child = Resident::from(prefix);
        let request = stored.request(&child, &private, &reads);
        assert_eq!(
            provider
                .request(&request.model, &request)
                .unwrap()
                .body()
                .to_vec(),
            body(&provider, &expected)
        );
        assert_eq!(reads.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn different_instruction_generations_never_reuse_a_prefix() {
    for provider in providers() {
        let canonical = Messages::from(vec![
            Message::system("terse"),
            Message::user("begin"),
            Message::assistant("answer"),
        ]);
        let stored = StoredMessages::new(&canonical);
        let resident = Resident::default();
        let reads = Arc::new(AtomicU64::new(0));
        let mut request = stored.request(&resident, &canonical, &reads);
        request.messages = request
            .messages
            .with_scope(crate::Json::encode(&"generation one").unwrap());
        provider.request(&request.model, &request).unwrap();
        let child = Resident::from(request.messages.prefix().unwrap());
        let mut request = stored.request(&child, &Messages::default(), &reads);
        request.messages = request
            .messages
            .with_scope(crate::Json::encode(&"generation two").unwrap());
        assert_eq!(
            provider
                .request(&request.model, &request)
                .unwrap()
                .body()
                .to_vec(),
            body(&provider, &canonical)
        );
        assert_eq!(reads.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn changed_wire_quirks_never_reuse_a_prefix() {
    for provider in providers() {
        let canonical = Messages::from(vec![Message::user("begin"), Message::assistant("answer")]);
        let stored = StoredMessages::new(&canonical);
        let reads = Arc::new(AtomicU64::new(0));
        let request = stored.request(&Resident::default(), &canonical, &reads);
        provider.request(&request.model, &request).unwrap();
        let child = Resident::from(request.messages.prefix().unwrap());
        let url = "https://example.test";
        let changed = match provider.protocol.key() {
            crate::ProtocolKey::OpenaiChat => Provider::new(
                "openai",
                url,
                Auth::None,
                chat::Chat::new(chat::Quirks {
                    path: "/changed",
                    ..chat::Quirks::OPENAI
                }),
            ),
            crate::ProtocolKey::OpenaiResponses => Provider::new(
                "openai",
                url,
                Auth::None,
                responses::Responses::new(responses::Quirks {
                    path: "/changed",
                    ..responses::Quirks::OPENAI
                }),
            ),
            crate::ProtocolKey::AnthropicMessages => Provider::new(
                "anthropic",
                url,
                Auth::None,
                messages::Messages::new(messages::Quirks {
                    strict_tool_schemas: false,
                    ..messages::Quirks::ANTHROPIC
                }),
            ),
            crate::ProtocolKey::GoogleGemini => Provider::new(
                "gemini",
                url,
                Auth::None,
                gemini::Gemini::new(gemini::Quirks {
                    models_path: "/changed",
                    ..gemini::Quirks::GEMINI
                }),
            ),
        };
        let request = stored.request(&child, &Messages::default(), &reads);
        assert_eq!(
            changed
                .request(&request.model, &request)
                .unwrap()
                .body()
                .to_vec(),
            body(&changed, &canonical),
        );
        assert_eq!(
            reads.load(Ordering::Relaxed),
            1,
            "{}",
            provider.protocol.key()
        );
    }
}

#[test]
fn releasing_a_deep_prefix_never_recurses_through_its_ancestry() {
    let mut chunks = Fragments::default();
    let byte: Run = vec![Shared::from(vec![b'0'])].into();
    for _ in 0..100_000 {
        chunks.extend([Arc::clone(&byte)]);
    }
    assert_eq!(Arc::strong_count(&byte), 100_001);
    drop(chunks);
    assert_eq!(Arc::strong_count(&byte), 1);
}

#[test]
fn an_uncompiled_tail_never_claims_a_new_prefix_frontier() {
    let provider = &providers()[0];
    let canonical = Messages::from(vec![Message::user("first")]);
    let stored = StoredMessages::new(&canonical);
    let resident = Resident::default();
    let reads = Arc::new(AtomicU64::new(0));
    let request = stored.request(&resident, &canonical, &reads);
    provider.request(&request.model, &request).unwrap();
    assert!(request.messages.prefix().is_some());
    let mut tail = canonical.clone();
    tail.push(Message::assistant("not yet compiled"));
    assert!(
        stored
            .request(&resident, &tail, &reads)
            .messages
            .prefix()
            .is_none()
    );
}

#[test]
fn changed_offers_and_fitting_never_reuse_a_prefix() {
    for provider in providers() {
        let canonical = Messages::from(vec![Message::user("begin"), Message::assistant("answer")]);
        let stored = StoredMessages::new(&canonical);
        let resident = Resident::default();
        let reads = Arc::new(AtomicU64::new(0));
        let request = stored.request(&resident, &canonical, &reads);
        provider.request(&request.model, &request).unwrap();
        let prefix = request.messages.prefix().unwrap();
        let child = Resident::from(prefix.clone());
        let mut request = stored.request(&child, &Messages::default(), &reads);
        request.tools = vec![crate::Tool {
            name: "added".into(),
            description: "new generation".into(),
            schema: json!({"type":"object"}),
            strict: None,
        }]
        .into();
        provider.request(&request.model, &request).unwrap();
        assert_eq!(reads.swap(0, Ordering::Relaxed), 1);
        let child = Resident::from(prefix);
        let request = stored.request(&child, &Messages::default(), &reads);
        let limits = crate::fit::Limits {
            bytes: 1_000_000,
            images: 2,
        };
        let target = Target {
            provider: &provider,
            model: &request.model,
        };
        crate::fit::fit(&request, limits, |request| {
            provider.protocol.encode(&target, request)
        })
        .unwrap();
        assert_eq!(reads.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn a_repaired_owner_history_never_claims_a_context_prefix() {
    for provider in providers() {
        let pending = round(&provider, "claude-sonnet-4-5", 0)[0].clone();
        let canonical = Messages::from(vec![
            Message::user("begin"),
            pending,
            Message::user("after interruption"),
        ]);
        let stored = StoredMessages::new(&canonical);
        let request = stored.request(
            &Resident::default(),
            &canonical,
            &Arc::new(AtomicU64::new(0)),
        );
        provider.request(&request.model, &request).unwrap();
        assert!(
            request.messages.prefix().is_none(),
            "a repaired owner prefix differs from context-only semantics"
        );
    }
}

#[test]
fn reclaiming_a_parent_preserves_only_the_childs_prefix_lease() {
    for provider in providers() {
        let canonical = Messages::from(vec![Message::user("begin"), Message::assistant("settled")]);
        let stored = StoredMessages::new(&canonical);
        let resident = Resident::default();
        let request = stored.request(&resident, &canonical, &Arc::new(AtomicU64::new(0)));
        provider.request(&request.model, &request).unwrap();
        let prefix = request.messages.prefix().unwrap();
        let bytes = Arc::downgrade(prefix.0.0.settled.0.first().unwrap());
        let child = Resident::from(prefix);
        drop(request);
        drop(resident);
        assert!(bytes.upgrade().is_some());
        drop(child);
        assert!(bytes.upgrade().is_none());
    }
}
