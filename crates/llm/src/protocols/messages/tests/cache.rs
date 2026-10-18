//! Prompt-cache breakpoints.

use super::*;

/// A request with the given caching options and raw tools, with a system
/// prompt and one user message.
fn cached(options: Value, tools: Vec<Tool>) -> Result<Value, Error> {
    let mut request = request(
        SONNET_4_6,
        vec![Message::system("System prompt"), Message::user("Hello")],
    );
    request.tools = tools;
    request.options.insert(KEY.to_owned(), options);
    encoded(&provider("anthropic"), &request).map(|(body, _)| body)
}

fn raw_tool(name: &str, cache_control: Option<Value>) -> Value {
    let mut tool = json!({"name": name, "description": name, "input_schema": {"type": "object"}});
    if let Some(marker) = cache_control {
        tool["cache_control"] = marker;
    }
    tool
}

fn marked(name: &str) -> Value {
    raw_tool(name, Some(json!({"type": "ephemeral"})))
}

fn system_marked(body: &Value) -> bool {
    body["system"]
        .as_array()
        .and_then(|blocks| blocks.last())
        .and_then(|block| block.get("cache_control"))
        .is_some()
}

fn tail_marker(body: &Value) -> Option<&Value> {
    body["messages"]
        .as_array()?
        .last()?
        .get("content")?
        .as_array()?
        .last()?
        .get("cache_control")
}

#[test]
fn manual_caching_keeps_three_tool_markers_and_skips_the_tail() {
    let body = cached(
        json!({"prompt_caching": true, "tools": [marked("a"), marked("b"), marked("c")]}),
        Vec::new(),
    )
    .expect("encodes");
    for tool in body["tools"].as_array().expect("tools") {
        assert_eq!(tool["cache_control"]["type"], "ephemeral");
    }
    assert!(system_marked(&body));
    assert!(tail_marker(&body).is_none());
}

#[test]
fn manual_caching_replaces_a_null_final_tool_marker() {
    let body = cached(
        json!({"prompt_caching": true, "tools": [raw_tool("final", Some(Value::Null))]}),
        Vec::new(),
    )
    .expect("encodes");
    assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
}

#[test]
fn a_top_level_marker_takes_one_breakpoint_of_the_budget() {
    let body = cached(
        json!({"prompt_caching": true, "cache_control": {"type": "ephemeral"},
            "tools": [marked("a"), marked("b"), marked("c")]}),
        Vec::new(),
    )
    .expect("encodes");
    assert_eq!(body["cache_control"]["type"], "ephemeral");
    assert!(!system_marked(&body));
    assert!(tail_marker(&body).is_none());

    let error = cached(
        json!({"prompt_caching": true, "cache_control": {"type": "ephemeral"},
            "tools": [marked("a"), marked("b"), marked("c"), raw_tool("d", None)]}),
        Vec::new(),
    )
    .expect_err("no budget for the final tool");
    assert!(error.message.contains("final non-deferred tool"), "{error}");

    let error = cached(
        json!({"cache_control": {"type": "ephemeral"},
            "tools": [marked("a"), marked("b"), marked("c"), marked("d")]}),
        Vec::new(),
    )
    .expect_err("too many markers");
    assert!(error.message.contains("Too many Anthropic tool"), "{error}");
}

#[test]
fn a_one_hour_top_level_marker_after_a_five_minute_tool_marker_is_refused() {
    let error = cached(
        json!({"cache_control": {"type": "ephemeral", "ttl": "1h"}, "tools": [marked("a")]}),
        Vec::new(),
    )
    .expect_err("refused");
    assert!(error.message.contains("ttl `1h`"), "{error}");
}

#[test]
fn a_one_hour_top_level_marker_gives_the_placed_markers_its_ttl() {
    let body = cached(
        json!({"prompt_caching": true, "cache_control": {"type": "ephemeral", "ttl": "1h"},
            "metadata": {"source": "test"}}),
        vec![tool("cached_tool")],
    )
    .expect("encodes");
    assert_eq!(body["tools"][0]["cache_control"]["ttl"], "1h");
    assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
    assert_eq!(body["cache_control"]["ttl"], "1h");
    assert_eq!(body["metadata"]["source"], "test");
    assert!(tail_marker(&body).is_none());
}

#[test]
fn a_static_prefix_ttl_splits_the_prefix_from_the_tail() {
    let body = cached(
        json!({"prompt_caching": true, "static_prefix_cache_ttl": "1h"}),
        vec![tool("cached_tool")],
    )
    .expect("encodes");
    assert_eq!(body["tools"][0]["cache_control"]["ttl"], "1h");
    assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
    // The tail keeps the five-minute default: a marker with no `ttl`.
    assert_eq!(tail_marker(&body), Some(&json!({"type": "ephemeral"})));
    assert!(body.get("cache_control").is_none());

    let error = cached(
        json!({"static_prefix_cache_ttl": "5m", "cache_control": {"type": "ephemeral", "ttl": "1h"}}),
        vec![tool("cached_tool")],
    )
    .expect_err("refused");
    assert!(error.message.contains("static_prefix_cache_ttl"), "{error}");

    // A 5m prefix under a 5m top-level marker is uniform.
    let body = cached(
        json!({"static_prefix_cache_ttl": "5m", "cache_control": {"type": "ephemeral"}}),
        vec![tool("cached_tool")],
    )
    .expect("encodes");
    assert_eq!(body["tools"][0]["cache_control"]["ttl"], "5m");
}

#[test]
fn manual_caching_without_tools_sends_no_tools() {
    let body = cached(json!({"prompt_caching": true}), Vec::new()).expect("encodes");
    assert!(body.get("tools").is_none());
    assert_eq!(tail_marker(&body), Some(&json!({"type": "ephemeral"})));
}

/// A top-level `cache_control` that is not an ephemeral marker with a known
/// TTL is refused, never sent as written.
#[test]
fn an_invalid_top_level_cache_marker_is_refused() {
    for marker in [
        json!({"type": "persistent"}),
        json!({"type": "ephemeral", "ttl": "1d"}),
    ] {
        let error = cached(json!({"cache_control": marker}), vec![tool("cached_tool")])
            .expect_err("refused");
        assert!(error.message.contains("`cache_control` option"), "{error}");
    }
}
