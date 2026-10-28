use serde_json::json;

use super::*;
use crate::Error;
use crate::protocol::{Auth, Decoder, Encoded, Protocol, Provider};
use crate::spec::{Assistant, Meta, Origin, Reasoning, Request, Text, ToolCall, User, UserPart};

struct Short;

impl Protocol for Short {
    fn key(&self) -> ProtocolKey {
        ProtocolKey::OpenaiChat
    }

    fn encode(&self, _: &Target<'_>, _: &Request) -> Result<Encoded, Error> {
        unreachable!()
    }

    fn decoder(&self, _: &Target<'_>, _: &Request) -> Box<dyn Decoder> {
        unreachable!()
    }

    fn tool_id(&self, id: &str) -> Option<String> {
        (id.len() > 4).then(|| id[..4].to_owned())
    }
}

fn provider() -> Provider {
    Provider::new("p", "https://example.test", Auth::None, Short)
}

fn origin(model: &str) -> Origin {
    Origin {
        protocol: ProtocolKey::OpenaiChat,
        provider: "p".into(),
        model: model.into(),
    }
}

fn call(id: &str) -> AssistantPart {
    AssistantPart::ToolCall(ToolCall {
        id: id.into(),
        name: "f".into(),
        input: crate::Json::encode(&serde_json::Map::new()).unwrap(),
        invalid: None,
        provider_executed: false,
        meta: Meta::from([("short".into(), json!({"x": 1}))]),
    })
}

fn result(id: &str) -> ToolResult {
    ToolResult {
        id: id.into(),
        name: "f".into(),
        output: Output::Text("ok".into()),
        error: false,
        meta: Meta::new(),
    }
}

fn thinking() -> AssistantPart {
    AssistantPart::Reasoning(Reasoning {
        text: "hmm".into(),
        redacted: false,
        meta: Meta::from([("short".into(), json!({"signature": "s"}))]),
    })
}

fn run(messages: Vec<Message>) -> Vec<Message> {
    let provider = provider();
    let request = Request {
        model: "m".into(),
        messages: messages.into(),
        ..Request::default()
    };
    let target = Target {
        provider: &provider,
        model: "m",
    };
    prepare(&request, &target)
        .messages
        .canonical()
        .unwrap()
        .to_vec()
}

#[test]
fn native_turns_keep_reasoning_and_state() {
    let out = run(vec![
        Message::user("hi"),
        Message::Assistant(Assistant {
            content: vec![thinking(), AssistantPart::Text(Text::new("yo"))],
            origin: Some(origin("m")),
        }),
    ]);
    let Message::Assistant(assistant) = &out[1] else {
        panic!()
    };
    assert_eq!(assistant.content.len(), 2);
    assert_eq!(
        assistant.content[0].meta().get("short").unwrap()["signature"],
        "s"
    );
}

#[test]
fn foreign_turns_lose_reasoning_state_and_long_ids() {
    let out = run(vec![
        Message::user("hi"),
        Message::Assistant(Assistant {
            content: vec![thinking(), call("call_123456"), call("call_999")],
            origin: Some(origin("other-model")),
        }),
        Message::Tool(ToolMessage {
            results: vec![result("call_123456"), result("call_999")],
        }),
    ]);
    let Message::Assistant(assistant) = &out[1] else {
        panic!()
    };
    let ids: Vec<_> = assistant
        .tool_calls()
        .map(|call| (call.id.as_str(), call.meta.is_empty()))
        .collect();
    // Both fit to "call"; the second is kept distinct.
    assert_eq!(ids, vec![("call", true), ("2_ca", true)]);
    assert!(
        !assistant
            .content
            .iter()
            .any(|part| matches!(part, AssistantPart::Reasoning(_)))
    );
    let Message::Tool(tool) = &out[2] else {
        panic!()
    };
    let ids: Vec<_> = tool
        .results
        .iter()
        .map(|result| result.id.as_str())
        .collect();
    assert_eq!(ids, vec!["call", "2_ca"]);
}

#[test]
fn unanswered_calls_get_error_results_and_stray_results_go() {
    let out = run(vec![
        Message::user("hi"),
        Message::Assistant(Assistant {
            content: vec![call("a"), call("b")],
            origin: None,
        }),
        Message::system("be brief"),
        Message::Tool(ToolMessage {
            results: vec![result("a"), result("zzz")],
        }),
        Message::User(User {
            content: vec![UserPart::Text(Text::new("again"))],
        }),
    ]);
    let kinds: Vec<_> = out
        .iter()
        .map(|message| match message {
            Message::System(_) => "system",
            Message::User(_) => "user",
            Message::Assistant(_) => "assistant",
            Message::Tool(_) => "tool",
        })
        .collect();
    assert_eq!(kinds, vec!["user", "assistant", "tool", "system", "user"]);
    let Message::Tool(tool) = &out[2] else {
        panic!()
    };
    let answered: Vec<_> = tool
        .results
        .iter()
        .map(|result| (result.id.as_str(), result.error))
        .collect();
    assert_eq!(answered, vec![("a", false), ("b", true)]);
    assert_eq!(tool.results[1].output, Output::Text(NO_RESULT.into()));
}

#[test]
fn foreign_reasoning_stays_as_context_naming_its_source_without_its_state() {
    let out = run(vec![
        Message::user("hi"),
        Message::Assistant(Assistant {
            content: vec![thinking(), AssistantPart::Text(Text::new("yo"))],
            origin: Some(origin("other")),
        }),
    ]);
    let Message::Assistant(assistant) = &out[1] else {
        panic!()
    };
    let [AssistantPart::Text(reasoning), AssistantPart::Text(answer)] = &assistant.content[..]
    else {
        panic!("{:?}", assistant.content)
    };
    assert_eq!(
        reasoning.text,
        "<reasoning source=\"p/other\">\nhmm\n</reasoning>"
    );
    assert!(reasoning.meta.is_empty());
    assert_eq!(answer.text, "yo");
}

#[test]
fn a_foreign_turn_that_was_only_redacted_reasoning_disappears() {
    let mut redacted = thinking();
    if let AssistantPart::Reasoning(reasoning) = &mut redacted {
        reasoning.redacted = true;
    }
    let out = run(vec![
        Message::user("hi"),
        Message::Assistant(Assistant {
            content: vec![redacted],
            origin: Some(origin("other")),
        }),
    ]);
    assert_eq!(out.len(), 1);
}

#[test]
fn legal_and_normalised_call_ids_remain_distinct_and_results_follow_them() {
    for ids in [["a.b", "a_b"], ["a_b", "a.b"]] {
        let mut provider = provider();
        provider.protocol = std::sync::Arc::new(gemini::Gemini::default());
        let request = Request {
            messages: vec![
                Message::Assistant(Assistant {
                    content: ids.iter().map(|id| call(id)).collect(),
                    origin: Some(origin("foreign")),
                }),
                Message::Tool(ToolMessage {
                    results: ids.iter().map(|id| result(id)).collect(),
                }),
            ]
            .into(),
            ..Request::default()
        };
        let target = Target {
            provider: &provider,
            model: "m",
        };
        let out = prepare(&request, &target);
        let Message::Assistant(assistant) = &out.messages.canonical().unwrap()[0] else {
            panic!()
        };
        let calls: Vec<_> = assistant.tool_calls().map(|c| c.id.clone()).collect();
        assert_ne!(calls[0], calls[1]);
        let Message::Tool(tool) = &out.messages.canonical().unwrap()[1] else {
            panic!()
        };
        assert_eq!(
            calls,
            tool.results
                .iter()
                .map(|r| r.id.clone())
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn separate_turns_keep_distinct_ids_and_all_results_even_with_native_calls() {
    for ids in [["a.b", "a_b"], ["a_b", "a.b"]] {
        for native_at in [None, Some(0), Some(1)] {
            let mut provider = provider();
            provider.protocol = std::sync::Arc::new(gemini::Gemini::default());
            let mut messages = Vec::new();
            for (at, id) in ids.iter().enumerate() {
                let mut part = call(id);
                part.meta_mut().clear();
                messages.push(Message::Assistant(Assistant {
                    content: vec![part],
                    origin: (native_at == Some(at)).then(|| Origin {
                        protocol: provider.protocol.key(),
                        provider: provider.id.clone(),
                        model: "m".into(),
                    }),
                }));
                messages.push(Message::Tool(ToolMessage {
                    results: vec![result(id)],
                }));
            }
            let request = Request {
                messages: messages.into(),
                ..Request::default()
            };
            let out = prepare(
                &request,
                &Target {
                    provider: &provider,
                    model: "m",
                },
            );
            let mut ids = Vec::new();
            for pair in out.messages.canonical().unwrap().as_chunks::<2>().0 {
                let Message::Assistant(assistant) = &pair[0] else {
                    panic!()
                };
                let id = assistant.tool_calls().next().unwrap().id.clone();
                assert_eq!(provider.protocol.tool_id(&id), None);
                let Message::Tool(tool) = &pair[1] else {
                    panic!()
                };
                assert_eq!(tool.results.len(), 1);
                assert_eq!(tool.results[0].id, id);
                assert!(!tool.results[0].error);
                assert_eq!(tool.results[0].output, Output::Text("ok".into()));
                ids.push(id);
            }
            assert_ne!(ids[0], ids[1]);
        }
    }
}

#[test]
fn a_models_reasoning_reads_as_the_same_kind_over_every_wire_it_came_by() {
    let reasoning = Reasoning {
        text: "weighing it".into(),
        redacted: false,
        meta: Meta::new(),
    };
    let kind = |protocol, model: &str| {
        let origin = Origin {
            protocol,
            provider: "p".into(),
            model: model.into(),
        };
        foreign(&reasoning, &origin).map(|thought| thought.tag(&origin).name)
    };
    for model in ["claude-sonnet-4-5", "anthropic/claude-sonnet-4.5"] {
        assert_eq!(
            kind(ProtocolKey::OpenaiChat, model),
            kind(ProtocolKey::AnthropicMessages, model)
        );
        assert_eq!(
            kind(ProtocolKey::OpenaiChat, model),
            Some(crate::tag::Name::ReasoningSummary)
        );
    }
    assert_eq!(
        kind(ProtocolKey::OpenaiChat, "deepseek-reasoner"),
        Some(crate::tag::Name::Reasoning)
    );
}

#[test]
fn hosted_calls_do_not_consume_local_call_ids() {
    for hosted_first in [false, true] {
        let mut hosted = call("hosted");
        if let AssistantPart::ToolCall(call) = &mut hosted {
            call.provider_executed = true;
        }
        let mut content = vec![call("local-long"), hosted];
        if hosted_first {
            content.reverse();
        }
        let out = run(vec![Message::Assistant(Assistant {
            content,
            origin: Some(origin("m")),
        })]);
        let Message::Assistant(assistant) = &out[0] else {
            panic!()
        };
        let ids: Vec<_> = assistant
            .content
            .iter()
            .filter_map(|part| match part {
                AssistantPart::ToolCall(call) => Some((call.id.as_str(), call.provider_executed)),
                _ => None,
            })
            .collect();
        assert!(ids.contains(&("hosted", true)));
        assert!(ids.contains(&("loca", false)));
        let Message::Tool(tool) = &out[1] else {
            panic!()
        };
        assert_eq!(tool.results.len(), 1);
        assert_eq!(tool.results[0].id, "loca");
    }
}
