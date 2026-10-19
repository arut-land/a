//! Hosts and their quirks: rig's `wire` tests and its defaults.

use super::*;
use crate::Kind;

#[test]
fn current_model_default_max_tokens_match_anthropic_limits() {
    for (model, tokens) in [
        ("claude-fable-5-1", Some(128_000)),
        ("claude-fable-5", Some(128_000)),
        ("claude-opus-5", Some(128_000)),
        ("claude-sonnet-5", Some(128_000)),
        (OPUS_4_8, Some(128_000)),
        ("claude-opus-4-7", Some(128_000)),
        ("claude-opus-4-6", Some(128_000)),
        (SONNET_4_6, Some(128_000)),
        ("claude-haiku-4-5", Some(64_000)),
        ("claude-sonnet-4-20250514", Some(64_000)),
        // The Models API's limit for Claude Opus 4.1.
        ("claude-opus-4-1-20250805", Some(32_000)),
        ("claude-3-opus", None),
        // An id the table does not list takes its family's default.
        ("claude-opus-4.6", Some(64_000)),
        ("claude-sonnet-4-6@20260101", Some(64_000)),
    ] {
        assert_eq!(default_max_tokens(model), tokens, "{model}");
    }
}

/// A gateway documents one ceiling rather than per-model limits, so an
/// unrecognized model still gets a usable default; Anthropic sends none
/// for a model it does not know, and the request fails here.
#[test]
fn a_gateway_defaults_max_tokens_to_its_one_documented_ceiling() {
    const _: () = assert!(!Quirks::GATEWAY.strict_tool_schemas);
    const _: () = assert!(Quirks::ANTHROPIC.strict_tool_schemas);
    assert_eq!(
        Quirks::ANTHROPIC.default_max_tokens("claude-haiku-4-5"),
        Some(64_000)
    );
    assert_eq!(
        Quirks::ANTHROPIC.default_max_tokens("some-unknown-model"),
        None
    );
    assert_eq!(
        Quirks::GATEWAY.default_max_tokens("some-unknown-model"),
        Some(4096)
    );
    let mut unset = request("some-unknown-model", vec![Message::user("hi")]);
    unset.generation.max_output_tokens = None;
    let error = encoded(&provider("anthropic"), &unset).expect_err("no max_tokens");
    assert_eq!(error.kind, Kind::Unsupported);
    assert_eq!(sent(&provider("zai"), &unset)["max_tokens"], 4096);
}

/// Every host asks for tool input as it is written by default.
#[test]
fn every_host_defaults_to_eager_tool_input() {
    for (host, quirks) in HOSTS {
        assert_eq!(
            quirks.tool_input_streaming,
            ToolInputStreaming::Eager,
            "{}",
            host.id
        );
        assert_eq!(host.key, KeyStyle::Header("x-api-key"), "{}", host.id);
    }
    assert_eq!(Quirks::GATEWAY.max_tokens, Some(4096));
}

/// Every host posts to exactly one `/v1/messages`, whether its base URL
/// names the version, the endpoint, or neither; and names the API version.
#[test]
fn every_host_posts_to_one_v1_messages() {
    for (host, quirks) in HOSTS {
        let provider = provider_with(host.id, *quirks).with_base_url(host.base_url);
        let http = provider
            .request("a-model", &request("a-model", vec![Message::user("hello")]))
            .expect("the request encodes");
        let path = http.uri().path().to_owned();
        assert!(path.ends_with("/v1/messages"), "{}: {path}", host.id);
        assert_eq!(path.matches("/v1").count(), 1, "{}: {path}", host.id);
        assert_eq!(http.headers()["anthropic-version"], VERSION);
    }
    for base in [
        "https://gateway.example/anthropic",
        "https://gateway.example/anthropic/v1",
        "https://gateway.example/anthropic/v1/messages/",
    ] {
        let provider = provider("anthropic").with_base_url(base);
        let http = provider
            .request("a-model", &request("a-model", vec![Message::user("hello")]))
            .expect("the request encodes");
        assert_eq!(http.uri().path(), "/anthropic/v1/messages", "{base}");
    }
}

/// Anthropic takes call ids of `[A-Za-z0-9_-]`, at most 64 long.
#[test]
fn foreign_call_ids_are_fitted_to_anthropics_grammar() {
    let messages = Messages::default();
    assert_eq!(messages.tool_id("toolu_01"), None);
    assert_eq!(messages.tool_id("call.1|x").as_deref(), Some("call_1_x"));
    assert_eq!(
        messages.tool_id(&"a".repeat(70)).map(|id| id.len()),
        Some(64)
    );
    assert!(messages.tool_id("").is_some_and(|id| !id.is_empty()));
}

/// The error envelope's type refines what the status says.
#[test]
fn the_error_envelope_classifies_a_rejection() {
    let messages = Messages::default();
    let headers = http::HeaderMap::new();
    let error = messages.error(
        400,
        &headers,
        br#"{"type":"error","error":{"type":"billing_error","message":"Your credit balance is too low"}}"#,
    );
    assert_eq!(error.kind, Kind::Quota);
    assert_eq!(error.message, "Your credit balance is too low");
    let error = messages.error(
        529,
        &headers,
        br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
    );
    assert_eq!(error.kind, Kind::Server);
    assert!(error.is_retryable());
    let error = messages.error(
        400,
        &headers,
        br#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#,
    );
    assert_eq!(error.kind, Kind::ContextOverflow);
}
