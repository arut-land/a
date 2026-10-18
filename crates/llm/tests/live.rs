//! Calls against real endpoints, ignored by default: `mise exec -- cargo
//! nextest run -p llm --features ureq --run-ignored only --test live`. They read
//! keys and base URLs from the environment.

#![cfg(feature = "ureq")]

use std::sync::Arc;

use llm::protocols::{chat, messages, responses};
use llm::{
    Assistant, AssistantPart, Message, Output, Provider, Reason, Request, Tool, ToolMessage,
    ToolResult, Ureq,
};
use serde_json::json;

fn http() -> Ureq {
    Ureq::new(Arc::new(rt::Threads::new(1)))
}

fn openai() -> Provider {
    let (host, quirks) = responses::HOSTS[0];
    host.provider(responses::Responses::new(quirks))
        .expect("OPENAI_API_KEY")
}

fn model() -> llm::SmolStr {
    std::env::var("A_MODEL").map_or_else(|_| "gpt-6-luna".into(), Into::into)
}

#[test]
#[ignore = "calls a real endpoint"]
fn responses_streams_text_and_usage() {
    let request = Request {
        model: model(),
        messages: vec![
            Message::system("Answer in one word."),
            Message::user("Capital of France?"),
        ],
        ..Request::default()
    };
    let reply = rt::block_on(llm::generate(&http(), &openai(), &request)).expect("a reply");
    eprintln!("{reply:#?}");
    assert!(reply.message.text().to_lowercase().contains("paris"));
    assert!(reply.usage.input.is_some_and(|n| n > 0));
    assert_eq!(reply.finish.reason, Reason::Stop);
}

#[test]
#[ignore = "calls a real endpoint"]
fn responses_tool_round_trip_replays_its_items() {
    let tool = Tool {
        name: "lookup_harbor_label".into(),
        description: "Returns the label of a harbor by its number.".into(),
        schema: json!({"type": "object", "properties": {"number": {"type": "integer"}}, "required": ["number"]}),
        strict: None,
    };
    let mut request = Request {
        model: model(),
        messages: vec![Message::user(
            "Call lookup_harbor_label for harbor 7, then tell me the label in one sentence.",
        )],
        tools: vec![tool],
        ..Request::default()
    };
    let provider = openai();
    let first = rt::block_on(llm::generate(&http(), &provider, &request)).expect("first reply");
    eprintln!("first: {first:#?}");
    let call = first
        .message
        .tool_calls()
        .next()
        .expect("a tool call")
        .clone();
    assert_eq!(call.name, "lookup_harbor_label");
    assert!(
        first
            .message
            .content
            .iter()
            .all(|part| !part.meta().is_empty() || matches!(part, AssistantPart::Text(_)))
    );
    request
        .messages
        .push(Message::Assistant(first.message.clone()));
    request.messages.push(Message::Tool(ToolMessage {
        results: vec![ToolResult {
            id: call.id.clone(),
            name: call.name.clone(),
            output: Output::Text("Blue Gull".into()),
            error: false,
            meta: llm::Meta::new(),
        }],
    }));
    let second = rt::block_on(llm::generate(&http(), &provider, &request)).expect("second reply");
    eprintln!("second: {second:#?}");
    assert!(second.message.text().contains("Blue Gull"));
    let _: &Assistant = &second.message;
}

fn anthropic() -> Provider {
    let (host, quirks) = messages::HOSTS[0];
    host.provider(messages::Messages::new(quirks))
        .expect("ANTHROPIC_API_KEY")
}

fn claude() -> llm::SmolStr {
    std::env::var("A_CLAUDE").map_or_else(|_| "claude-sonnet-4-6".into(), Into::into)
}

#[test]
#[ignore = "calls a real endpoint"]
fn messages_streams_text_and_usage() {
    let request = Request {
        model: claude(),
        messages: vec![
            Message::system("Answer in one word."),
            Message::user("Capital of France?"),
        ],
        ..Request::default()
    };
    let reply = rt::block_on(llm::generate(&http(), &anthropic(), &request)).expect("a reply");
    eprintln!("{reply:#?}");
    assert!(reply.message.text().to_lowercase().contains("paris"));
    assert!(reply.usage.input.is_some_and(|n| n > 0));
}

#[test]
#[ignore = "calls a real endpoint"]
fn messages_thinking_tool_round_trip_replays_its_signature() {
    let tool = Tool {
        name: "lookup_harbor_label".into(),
        description: "Returns the label of a harbor by its number.".into(),
        schema: json!({"type": "object", "properties": {"number": {"type": "integer"}}, "required": ["number"]}),
        strict: None,
    };
    let mut request = Request {
        model: claude(),
        messages: vec![Message::user(
            "Call lookup_harbor_label for harbor 7, then tell me the label in one sentence.",
        )],
        tools: vec![tool],
        ..Request::default()
    };
    request.generation.max_output_tokens = std::num::NonZeroU32::new(4096);
    request.generation.reasoning_budget = std::num::NonZeroU32::new(1024);
    let provider = anthropic();
    let first = rt::block_on(llm::generate(&http(), &provider, &request)).expect("first reply");
    eprintln!("first: {first:#?}");
    assert!(
        first
            .message
            .content
            .iter()
            .any(|part| matches!(part, AssistantPart::Reasoning(_))),
        "thinking was asked for"
    );
    let call = first
        .message
        .tool_calls()
        .next()
        .expect("a tool call")
        .clone();
    request
        .messages
        .push(Message::Assistant(first.message.clone()));
    request.messages.push(Message::Tool(ToolMessage {
        results: vec![ToolResult {
            id: call.id.clone(),
            name: call.name.clone(),
            output: Output::Text("Blue Gull".into()),
            error: false,
            meta: llm::Meta::new(),
        }],
    }));
    let second = rt::block_on(llm::generate(&http(), &provider, &request)).expect("second reply");
    eprintln!("second: {second:#?}");
    assert!(second.message.text().contains("Blue Gull"));
}

fn openai_chat() -> Provider {
    let (host, quirks) = chat::HOSTS
        .iter()
        .find(|(host, _)| host.id == "openai")
        .copied()
        .expect("an openai chat host");
    host.provider(chat::Chat::new(quirks))
        .expect("OPENAI_API_KEY")
}

fn tool_round_trip(provider: &Provider, model: &str) {
    let tool = Tool {
        name: "lookup_harbor_label".into(),
        description: "Returns the label of a harbor by its number.".into(),
        schema: json!({"type": "object", "properties": {"number": {"type": "integer"}}, "required": ["number"]}),
        strict: None,
    };
    let mut request = Request {
        model: model.into(),
        messages: vec![Message::user(
            "Call lookup_harbor_label for harbor 7, then tell me the label in one sentence.",
        )],
        tools: vec![tool],
        ..Request::default()
    };
    let first = rt::block_on(llm::generate(&http(), provider, &request)).expect("first reply");
    eprintln!("first: {first:#?}");
    let call = first
        .message
        .tool_calls()
        .next()
        .expect("a tool call")
        .clone();
    request
        .messages
        .push(Message::Assistant(first.message.clone()));
    request.messages.push(Message::Tool(ToolMessage {
        results: vec![ToolResult {
            id: call.id.clone(),
            name: call.name.clone(),
            output: Output::Text("Blue Gull".into()),
            error: false,
            meta: llm::Meta::new(),
        }],
    }));
    let second = rt::block_on(llm::generate(&http(), provider, &request)).expect("second reply");
    eprintln!("second: {second:#?}");
    assert!(second.message.text().contains("Blue Gull"));
    assert!(second.usage.input.is_some_and(|n| n > 0));
}

#[test]
#[ignore = "calls a real endpoint"]
fn chat_tool_round_trip_on_gpt() {
    tool_round_trip(&openai_chat(), &model());
}

#[test]
#[ignore = "calls a real endpoint"]
fn chat_tool_round_trip_on_claude() {
    tool_round_trip(&openai_chat(), &claude());
}
