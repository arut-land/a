//! Tool definitions and choices: strict schemas, eager input, the
//! choice a request may send.

use super::*;
use crate::spec::ToolChoice;

/// Hosts that do not implement Anthropic's constrained tool schemas get a
/// strict tool unchanged.
#[test]
fn a_strict_tool_is_unchanged_on_a_gateway() {
    let mut lookup = tool("lookup");
    lookup.strict = Some(true);
    let body = sent_with(
        &provider("zai"),
        "some-model",
        vec![Message::user("Hello")],
        vec![lookup],
    );
    assert!(body["tools"][0].get("strict").is_none());
    assert!(
        body["tools"][0]["input_schema"]
            .get("additionalProperties")
            .is_none()
    );
}

#[test]
fn a_strict_tool_is_marked_and_sanitized_and_provider_tools_are_not() {
    let mut lookup = tool("lookup");
    lookup.strict = Some(true);
    lookup.schema = json!({
        "type": "object",
        "additionalProperties": true,
        "properties": {
            "query": {"type": "string", "minLength": 2, "maxLength": 20,
                "pattern": "^[a-z]+$", "format": "uuid"},
            "kind": {"type": "string", "const": "lookup"},
            "legacy_filter": {"$ref": "#/definitions/LegacyFilter"},
            "options": {
                "type": "object",
                "additionalProperties": true,
                "properties": {
                    "limit": {"type": ["integer", "null"], "minimum": 1, "maximum": 100,
                        "format": "uint32"}
                }
            }
        },
        "definitions": {
            "LegacyFilter": {"type": "object", "properties": {"term": {"type": "string"}}}
        },
        "required": ["query"]
    });
    let mut request = request(
        SONNET_4_6,
        vec![Message::system("System prompt"), Message::user("Hello")],
    );
    request.tools = vec![lookup];
    request.options.insert(
        KEY.to_owned(),
        json!({"tools": [{"type": "mcp_toolset", "name": "remote_tools"}]}),
    );
    let body = sent(&provider("anthropic"), &request);
    let tool = &body["tools"][0];
    let schema = &tool["input_schema"];
    assert_eq!(tool["strict"], true);
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["required"], json!(["query"]));
    assert_eq!(
        schema["properties"]["options"]["additionalProperties"],
        false
    );
    assert!(schema["properties"]["options"].get("required").is_none());
    let query = &schema["properties"]["query"];
    assert_eq!(query["format"], "uuid");
    for keyword in ["minLength", "maxLength", "pattern"] {
        assert!(query.get(keyword).is_none());
    }
    let guidance = query["description"]
        .as_str()
        .expect("constraints become guidance");
    for hint in ["minLength: 2", "maxLength: 20", "pattern: ^[a-z]+$"] {
        assert!(guidance.contains(hint), "{guidance}");
    }
    assert_eq!(schema["properties"]["kind"]["const"], "lookup");
    assert_eq!(
        schema["properties"]["legacy_filter"]["$ref"],
        "#/definitions/LegacyFilter"
    );
    assert_eq!(
        schema["definitions"]["LegacyFilter"]["additionalProperties"],
        false
    );
    let limit = &schema["properties"]["options"]["properties"]["limit"];
    for keyword in ["format", "minimum", "maximum"] {
        assert!(limit.get(keyword).is_none());
    }
    let guidance = limit["description"]
        .as_str()
        .expect("constraints become guidance");
    for hint in ["minimum: 1", "maximum: 100", "format: uint32"] {
        assert!(guidance.contains(hint), "{guidance}");
    }
    assert_eq!(
        body["tools"][1],
        json!({"type": "mcp_toolset", "name": "remote_tools"})
    );
}

/// Each tool's `eager_input_streaming` and the `anthropic-beta` header, for
/// a request with `tools` and one provider tool, on `provider`.
fn tool_input_streaming(
    provider: &Provider,
    tools: Vec<Tool>,
    betas: &[&str],
) -> (Vec<Option<Value>>, Option<String>) {
    let mut request = request(SONNET_4_6, vec![Message::user("Hello")]);
    request.tools = tools;
    request.options.insert(
        KEY.to_owned(),
        json!({"tools": [{"type": "web_search_20250305", "name": "web_search"}], "betas": betas}),
    );
    let eager = sent(provider, &request)["tools"]
        .as_array()
        .map_or_else(Vec::new, |tools| {
            tools
                .iter()
                .map(|tool| tool.get("eager_input_streaming").cloned())
                .collect()
        });
    (eager, beta(provider, &request))
}

/// A request with tools asks every host to stream their input as it is
/// written; a provider tool is left as given, and a request without tools
/// of its own asks nothing.
#[test]
fn a_request_with_tools_asks_for_eager_tool_input_on_every_host() {
    for (host, _) in HOSTS {
        let provider = provider(host.id);
        assert_eq!(
            tool_input_streaming(&provider, vec![tool("lookup")], &[]),
            (vec![Some(json!(true)), None], None),
            "{}",
            host.id
        );
        assert_eq!(
            tool_input_streaming(&provider, Vec::new(), &[]),
            (vec![None], None),
            "{}",
            host.id
        );
    }
}

/// A host that rejects the per-tool field gets the beta flag instead, after
/// the caller's own flags and once, and one that rejects both gets neither.
#[test]
fn tool_input_streaming_falls_back_to_the_beta_flag_or_off() {
    let with = |streaming| {
        provider_with(
            "anthropic",
            Quirks {
                tool_input_streaming: streaming,
                ..Quirks::ANTHROPIC
            },
        )
    };
    let header = with(ToolInputStreaming::BetaHeader);
    let files = "files-api-2025-04-14";
    assert_eq!(
        tool_input_streaming(&header, vec![tool("lookup")], &[files]),
        (
            vec![None, None],
            Some(format!("{files},{FINE_GRAINED_TOOL_STREAMING_BETA}"))
        )
    );
    assert_eq!(
        tool_input_streaming(&header, Vec::new(), &[files]),
        (vec![None], Some(files.to_owned()))
    );
    assert_eq!(
        tool_input_streaming(
            &header,
            vec![tool("lookup")],
            &[FINE_GRAINED_TOOL_STREAMING_BETA]
        )
        .1,
        Some(FINE_GRAINED_TOOL_STREAMING_BETA.to_owned())
    );
    let off = with(ToolInputStreaming::Off);
    assert_eq!(
        tool_input_streaming(&off, vec![tool("lookup")], &[files]),
        (vec![None, None], Some(files.to_owned()))
    );
}

/// A tool choice goes only with tools, `auto` when tools go without one; a
/// forced choice a model rejects is left to the model.
#[test]
fn a_tool_choice_goes_only_with_tools() {
    let provider = provider("anthropic");
    let mut request = request(OPUS_4_8, vec![Message::user("Hi")]);
    request.tool_choice = ToolChoice::Required;
    assert!(sent(&provider, &request).get("tool_choice").is_none());
    request.tools = vec![tool("add")];
    assert_eq!(
        sent(&provider, &request)["tool_choice"],
        json!({"type": "any"})
    );
    request.tool_choice = ToolChoice::Tool("add".into());
    assert_eq!(
        sent(&provider, &request)["tool_choice"],
        json!({"type": "tool", "name": "add"})
    );
    request.tool_choice = ToolChoice::None;
    assert_eq!(
        sent(&provider, &request)["tool_choice"],
        json!({"type": "none"})
    );
    request.tool_choice = ToolChoice::Auto;
    assert_eq!(
        sent(&provider, &request)["tool_choice"],
        json!({"type": "auto"})
    );
    assert_eq!(sent(&provider, &request)["stream"], true);

    assert!(rejects_forced_tool_choice(OPUS_5_5));
    request.model = OPUS_5_5.into();
    request.tool_choice = ToolChoice::Required;
    assert_eq!(
        sent(&provider, &request)["tool_choice"],
        json!({"type": "auto"})
    );
}
