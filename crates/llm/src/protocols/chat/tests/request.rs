//! The request body, ported from rig's Chat request and option tests: each
//! message and part's shape, the tool and format fields, the generation
//! options per host, and what the request's own options override.

use std::num::NonZeroU32;

use serde_json::{Value, json};

use super::{gateway, provider, request, sent, sent_to};
use crate::Kind;
use crate::protocols::chat::encode::{
    DOCUMENT_UNSENDABLE, TOOL_IMAGE_ATTACHED, TOOL_IMAGES_HEADING,
};
use crate::protocols::chat::{KEY, Quirks};
use crate::spec::{
    Assistant, AssistantPart, Effort, Format, Generation, Media, Message, Meta, Output, Request,
    Source, Text, Tool, ToolCall, ToolChoice, ToolMessage, ToolResult, User, UserPart,
};

pub(super) fn tool(name: &str) -> Tool {
    Tool {
        name: name.into(),
        description: "a tool".into(),
        schema: json!({"type": "object", "properties": {}}),
        strict: None,
    }
}

pub(super) fn hello(model: &str) -> Request {
    Request {
        model: model.into(),
        messages: vec![Message::user("Hello")].into(),
        ..Request::default()
    }
}

pub(super) fn with(model: &str, generation: Generation) -> Request {
    Request {
        generation,
        ..hello(model)
    }
}

pub(super) fn effort(effort: Effort) -> Generation {
    Generation {
        reasoning: Some(effort),
        ..Generation::default()
    }
}

fn options(request: &mut Request, options: Value) {
    request.options.insert(KEY.into(), options);
}

fn media(media_type: &str, source: Source) -> UserPart {
    UserPart::Media(Media {
        media_type: media_type.into(),
        source,
        meta: Meta::new(),
    })
}

fn data(data: &str) -> Source {
    Source::Data { data: data.into() }
}

fn calling(id: &str) -> Message {
    Message::Assistant(Assistant {
        content: vec![AssistantPart::ToolCall(ToolCall {
            id: id.into(),
            name: "tool".into(),
            input: crate::Json::encode(&serde_json::Map::new()).unwrap(),
            invalid: None,
            provider_executed: false,
            meta: Meta::new(),
        })],
        origin: None,
    })
}

fn answered(id: &str, output: Output) -> Message {
    Message::Tool(ToolMessage {
        results: vec![ToolResult {
            id: id.into(),
            name: "tool".into(),
            output,
            error: false,
            meta: Meta::new(),
        }],
    })
}

/// The bodies a recorded exchange sent, ours to the byte.
#[test]
fn a_recorded_request_is_what_we_send() {
    let recorded = |name: &str| -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/rig/cassettes/openai/raw_stream_capture_matrix")
            .join(name);
        let text = std::fs::read_to_string(path).expect("the cassette");
        let doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text).expect("yaml");
        let body = doc["when"]["body"].as_str().expect("a recorded body");
        serde_json::from_str(body).expect("a JSON body")
    };
    let openai = provider("openai");
    let prompt = |text: &str, max: u32| Request {
        model: "gpt-4.1-nano".into(),
        messages: vec![Message::user(text)].into(),
        generation: Generation {
            temperature: Some(0.0),
            max_output_tokens: NonZeroU32::new(max),
            ..Generation::default()
        },
        ..Request::default()
    };
    assert_eq!(
        sent(
            &openai,
            &prompt("Reply with exactly the single word: pong", 16)
        ),
        recorded("chat_stream_raw_round_trips_typed.yaml")
    );
    let mut call = prompt("Call ping exactly once with no arguments.", 64);
    call.tools = vec![Tool {
        name: "ping".into(),
        description: "Matrix tool ping".into(),
        schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        strict: None,
    }]
    .into();
    call.tool_choice = ToolChoice::Required;
    assert_eq!(
        sent(&openai, &call),
        recorded("chat_tool_call_stream_raw_round_trips_typed.yaml")
    );
}

/// A result's parts are one string, joined, unless the host asks for parts
/// or a part is an image the host reads in a `tool` message (llama.cpp);
/// elsewhere the image moves to a user message after the results.
#[test]
fn a_result_is_a_string_or_its_parts() {
    let parts = Output::Content(vec![
        UserPart::Text(Text::new("first")),
        UserPart::Text(Text::new("{\"status\":\"ok\"}")),
    ]);
    let history = || {
        vec![
            Message::user("q"),
            calling("call-id"),
            answered("call-id", parts.clone()),
        ]
    };
    let string = sent(&provider("openai"), &request("gpt-4o-mini", history()));
    assert_eq!(
        string["messages"][2]["content"],
        "first\n{\"status\":\"ok\"}"
    );
    let array = sent(
        &gateway(Quirks {
            tool_result_parts: true,
            ..Quirks::OPENAI
        }),
        &request("gpt-4o-mini", history()),
    );
    assert_eq!(
        array["messages"][2]["content"],
        json!([{"type": "text", "text": "first"}, {"type": "text", "text": "{\"status\":\"ok\"}"}])
    );

    let image = || {
        vec![
            Message::user("q"),
            calling("call-id"),
            answered(
                "call-id",
                Output::Content(vec![media("image/png", data("iVBORw0KGgo="))]),
            ),
        ]
    };
    let llamacpp = sent(
        &provider("llamacpp"),
        &request("Qwen3-VL-2B-Instruct-Q8_0", image()),
    );
    assert_eq!(
        llamacpp["messages"][2]["content"][0]["image_url"]["url"],
        "data:image/png;base64,iVBORw0KGgo="
    );
    let openai = sent(&provider("openai"), &request("gpt-4o", image()));
    assert_eq!(openai["messages"][2]["content"], TOOL_IMAGE_ATTACHED);
    assert_eq!(
        openai["messages"][3],
        json!({"role": "user", "content": [
            {"type": "text", "text": TOOL_IMAGES_HEADING},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo=",
                "detail": "auto"}},
        ]})
    );
}

/// User parts: a PDF as a file part with its data or the URL that names
/// it, a file id where the host takes one, an image with `detail: auto`,
/// and a video by URL.
#[test]
fn user_parts_take_their_wire_shapes() {
    let content = vec![
        UserPart::Text(Text::new("look")),
        media("application/pdf", data("JVBERi0xLjQK")),
        media(
            "application/pdf",
            Source::Url {
                url: "https://example.com/x.pdf".into(),
            },
        ),
        media(
            "text/plain",
            Source::File {
                id: "file_abc".into(),
            },
        ),
        media("image/png", data("iVBORw0KGgo=")),
        media(
            "video/mp4",
            Source::Url {
                url: "https://example.com/v.mp4".into(),
            },
        ),
    ];
    let user = |content| Request {
        model: "google/gemini-2.5-flash".into(),
        messages: vec![Message::User(User { content })].into(),
        ..Request::default()
    };
    let openrouter = sent(&provider("openrouter"), &user(content));
    assert_eq!(
        openrouter["messages"][0]["content"],
        json!([
            {"type": "text", "text": "look"},
            {"type": "file", "file": {"file_data": "data:application/pdf;base64,JVBERi0xLjQK",
                "filename": "document.pdf"}},
            {"type": "file", "file": {"file_data": "https://example.com/x.pdf",
                "filename": "document.pdf"}},
            {"type": "text", "text": DOCUMENT_UNSENDABLE},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo=",
                "detail": "auto"}},
            {"type": "video_url", "video_url": {"url": "https://example.com/v.mp4"}},
        ]),
        "OpenRouter takes no file ids"
    );
    let file = vec![
        UserPart::Text(Text::new("read")),
        media(
            "text/plain",
            Source::File {
                id: "file_abc".into(),
            },
        ),
    ];
    let openai = sent(&provider("openai"), &user(file));
    assert_eq!(
        openai["messages"][0]["content"][1],
        json!({"type": "file", "file": {"file_id": "file_abc"}})
    );
}

/// `max_tokens` stays where the host wants it; `OpenAI`'s reasoning models
/// get `max_completion_tokens`, a caller's own cap wins, and a
/// vendor-prefixed id (a proxy behind the base URL) keeps the legacy field.
#[test]
fn the_output_cap_is_spelled_for_the_model() {
    let capped = |model: &str, max: Option<u32>, own: Option<Value>| {
        let mut request = with(
            model,
            Generation {
                max_output_tokens: max.and_then(NonZeroU32::new),
                ..Generation::default()
            },
        );
        if let Some(own) = own {
            options(&mut request, own);
        }
        sent(&provider("openai"), &request)
    };
    let legacy = capped("gpt-4o-mini", Some(4096), None);
    assert_eq!(legacy["max_tokens"], 4096);
    assert!(legacy.get("max_completion_tokens").is_none());
    let modern = capped("gpt-5", Some(4096), Some(json!({"top_p": 0.5})));
    assert_eq!(modern["max_completion_tokens"], 4096);
    assert!(modern.get("max_tokens").is_none());
    assert_eq!(modern["top_p"], 0.5);
    let theirs = capped(
        "gpt-5",
        Some(4096),
        Some(json!({"max_completion_tokens": 48})),
    );
    assert_eq!(theirs["max_completion_tokens"], 48);
    let upgraded = capped("gpt-5", None, Some(json!({"max_tokens": 48})));
    assert_eq!(upgraded["max_completion_tokens"], 48);
    assert!(upgraded.get("max_tokens").is_none());
    let none = capped("gpt-5", None, None);
    assert!(none.get("max_tokens").is_none() && none.get("max_completion_tokens").is_none());
    for model in ["openai/gpt-5", "openai/o3-mini", "litellm/gpt-5.4"] {
        assert_eq!(capped(model, Some(16), None)["max_tokens"], 16, "{model}");
    }
    let mistral = sent(
        &provider("mistral"),
        &with(
            "gpt-5",
            Generation {
                max_output_tokens: NonZeroU32::new(16),
                ..Generation::default()
            },
        ),
    );
    assert_eq!(mistral["max_tokens"], 16, "only OpenAI renames it");
}

/// The request's own tools, function or native, join the typed ones in one
/// array; its other options override typed fields.
#[test]
fn the_requests_own_options_join_and_override() {
    let mut joined = Request {
        tools: vec![tool("builder_tool")].into(),
        ..hello("openai/gpt-oss-20b")
    };
    options(
        &mut joined,
        json!({"tools": [
            {"type": "function", "function": {"name": "params_tool",
                "description": "from the options", "parameters": {"type": "object"}}},
            {"type": "browser_search"},
        ]}),
    );
    for id in ["groq", "openai"] {
        let body = sent(&provider(id), &joined);
        let kinds: Vec<&str> = body["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|tool| {
                tool["function"]["name"]
                    .as_str()
                    .or_else(|| tool["type"].as_str())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(
            kinds,
            ["builder_tool", "params_tool", "browser_search"],
            "{id}"
        );
    }
    let mut overridden = with(
        "gpt-4o",
        Generation {
            temperature: Some(0.1),
            ..Generation::default()
        },
    );
    options(&mut overridden, json!({"temperature": 0.9, "top_p": 0.5}));
    let body = sent(&provider("openai"), &overridden);
    assert_eq!(body["temperature"], 0.9);
    assert_eq!(body["top_p"], 0.5);
}

/// A structured format waits for the first tool result unless the host
/// takes both at once, so the model can still call a tool.
#[test]
fn the_response_format_waits_for_a_tool_result() {
    let format = Format::Json {
        schema: Some(json!({"title": "WeatherResponse", "type": "object",
            "properties": {"city": {"type": "string"}}, "required": ["city"]})),
        name: None,
        description: None,
    };
    let first = Request {
        tools: vec![tool("tool")].into(),
        format: Some(format.clone()),
        ..hello("gpt-4o")
    };
    assert!(
        sent(&provider("openai"), &first)
            .get("response_format")
            .is_none()
    );
    let openrouter = sent(&provider("openrouter"), &first);
    assert_eq!(openrouter["response_format"]["type"], "json_schema");
    let later = Request {
        format: Some(format),
        ..request(
            "gpt-4o",
            vec![
                Message::user("Weather in London?"),
                calling("call_1"),
                answered("call_1", Output::Text("fire".into())),
            ],
        )
    };
    let body = sent(&provider("openai"), &later);
    assert_eq!(
        body["response_format"],
        json!({"type": "json_schema", "json_schema": {"name": "WeatherResponse", "strict": true,
            "schema": {"title": "WeatherResponse", "type": "object", "additionalProperties": false,
                "properties": {"city": {"type": "string"}}, "required": ["city"]}}})
    );
    assert!(
        sent(&provider("deepseek"), &later)
            .get("response_format")
            .is_none(),
        "DeepSeek takes no schema"
    );
}

/// Ollama's `OpenAI`-compatible API reads `reasoning_effort`; it ignores
/// `num_ctx` and `options`, which are refused rather than dropped.
#[test]
fn ollama_takes_an_effort_and_refuses_native_options() {
    let ollama = provider("ollama");
    for (level, sent_as) in [
        (Effort::None, "none"),
        (Effort::High, "high"),
        (Effort::Low, "low"),
    ] {
        let body = sent(&ollama, &with("qwen3", effort(level)));
        assert_eq!(body["reasoning_effort"], sent_as);
    }
    for (own, named) in [
        (json!({"num_ctx": 8192}), "`num_ctx`"),
        (json!({"options": {"num_ctx": 8192}}), "`options`"),
    ] {
        let mut request = hello("qwen3");
        options(&mut request, own);
        let refused = sent_to(&ollama, &request).expect_err("refused");
        assert_eq!(refused.kind, Kind::Unsupported);
        assert!(refused.message.contains(named), "{refused:?}");
    }
}

/// llama.cpp and Moonshot refuse a specific-function tool choice; Moonshot
/// coerces `required` to `auto` with a steering message; `DeepSeek` drops a
/// forced choice while its model thinks; Mistral relaxes one beside a
/// structured format.
#[test]
fn hosts_rewrite_or_refuse_a_forced_tool_choice() {
    let forced = |model: &str, choice: ToolChoice| Request {
        tools: vec![tool("f")].into(),
        tool_choice: choice,
        ..hello(model)
    };
    for id in ["llamacpp", "moonshot"] {
        let refused = sent_to(&provider(id), &forced("m", ToolChoice::Tool("f".into())))
            .expect_err("a forced tool is refused");
        assert_eq!(refused.kind, Kind::Unsupported, "{id}");
    }
    let moonshot = sent(
        &provider("moonshot"),
        &forced("kimi-k2.6", ToolChoice::Required),
    );
    assert_eq!(moonshot["tool_choice"], "auto");
    assert_eq!(
        moonshot["messages"]
            .as_array()
            .and_then(|messages| messages.last()),
        Some(
            &json!({"role": "user", "content": "Please select a tool to handle the current issue."})
        )
    );
    let deepseek = provider("deepseek");
    let thinking = sent(
        &deepseek,
        &forced("deepseek-reasoner", ToolChoice::Required),
    );
    assert!(thinking.get("tool_choice").is_none(), "{thinking}");
    let chat = sent(&deepseek, &forced("deepseek-chat", ToolChoice::Required));
    assert_eq!(chat["tool_choice"], "required");
    // A typed format waits for a tool result; the request's own goes now.
    let mut structured = forced("mistral-small-latest", ToolChoice::Required);
    options(
        &mut structured,
        json!({"response_format": {"type": "json_object"}}),
    );
    let mistral = sent(&provider("mistral"), &structured);
    assert_eq!(mistral["tool_choice"], "auto");
    let unstructured = sent(
        &provider("mistral"),
        &forced("mistral-small-latest", ToolChoice::Required),
    );
    assert_eq!(unstructured["tool_choice"], "required");
}

/// Perplexity takes a text-only content array as one string; Mira every
/// array; Mistral's parts are its own chunks.
#[test]
fn hosts_reshape_content() {
    let parts = |model: &str| Request {
        model: model.into(),
        messages: vec![
            Message::system("be brief"),
            Message::User(User {
                content: vec![
                    UserPart::Text(Text::new("a")),
                    UserPart::Text(Text::new("b")),
                ],
            }),
        ]
        .into(),
        ..Request::default()
    };
    let perplexity = sent(&provider("perplexity"), &parts("sonar"));
    assert_eq!(perplexity["messages"][0]["content"], "be brief");
    assert_eq!(perplexity["messages"][1]["content"], "a\nb");
    let openai = sent(&provider("openai"), &parts("gpt-4o"));
    assert_eq!(
        openai["messages"][0]["content"],
        json!([{"type": "text", "text": "be brief"}])
    );
    let mut audio = parts("voxtral-small-latest");
    audio.messages = vec![Message::User(User {
        content: vec![media("audio/wav", data("UklG"))],
    })]
    .into();
    let mistral = sent(&provider("mistral"), &audio);
    assert_eq!(
        mistral["messages"][0]["content"],
        json!([{"type": "input_audio", "input_audio": "UklG"}])
    );
}

/// Mistral takes system text only before the conversation: a later system
/// message joins the leading one.
#[test]
fn mistral_joins_later_system_messages() {
    let request = Request {
        model: "mistral-small-latest".into(),
        messages: vec![
            Message::system("first"),
            Message::user("q"),
            Message::system("second"),
            Message::user("r"),
        ]
        .into(),
        ..Request::default()
    };
    let body = sent(&provider("mistral"), &request);
    assert_eq!(
        body["messages"],
        json!([
            {"role": "system", "content": [{"type": "text", "text": "first\n\nsecond"}]},
            {"role": "user", "content": [{"type": "text", "text": "q"}, {"type": "text", "text": "r"}]},
        ])
    );
}

/// An Azure deployment is in the URL with its `api-version`, and the body
/// names no model.
#[test]
fn an_azure_deployment_is_in_the_url() {
    let (url, body) = sent_to(&provider("azure.openai"), &hello("prod-gpt")).expect("encodes");
    assert_eq!(
        url,
        "https://example.openai.azure.com/openai/deployments/prod-gpt/chat/completions?api-version=2024-10-21"
    );
    assert!(body.get("model").is_none(), "{body}");
    // The default `api-version` predates reasoning efforts.
    let body = sent(
        &provider("azure.openai"),
        &with("prod-gpt", effort(Effort::High)),
    );
    assert!(body.get("reasoning_effort").is_none(), "{body}");
}

/// A stream asks for the usage chunk where the host takes `stream_options`,
/// and the caller's own stand.
#[test]
fn a_stream_asks_for_usage_where_the_host_takes_it() {
    let openai = sent(&provider("openai"), &hello("gpt-4o"));
    assert_eq!(openai["stream"], true);
    assert_eq!(openai["stream_options"], json!({"include_usage": true}));
    for id in ["mistral", "openrouter", "perplexity", "mira"] {
        assert!(
            sent(&provider(id), &hello("m"))
                .get("stream_options")
                .is_none(),
            "{id}"
        );
    }
    let mut own = hello("gpt-4o");
    options(
        &mut own,
        json!({"stream_options": {"include_usage": false}}),
    );
    assert_eq!(
        sent(&provider("openai"), &own)["stream_options"],
        json!({"include_usage": false})
    );
}
