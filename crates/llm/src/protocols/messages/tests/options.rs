//! Generation settings: rig's `options` tests. Where rig refuses a setting
//! a model or dialect cannot take, the body here goes without it.

use std::num::NonZeroU32;

use super::*;
use crate::spec::{Effort, Format, Generation, ToolChoice};

const OPUS_4_6: &str = "claude-opus-4-6";
const HAIKU_4_5: &str = "claude-haiku-4-5";

/// The body `generation` sends to `model` on `host`, with room for 2048
/// tokens.
fn with(host: &str, model: &str, generation: Generation, options: Option<Value>) -> Value {
    let mut request = request(model, vec![Message::user("Answer the question")]);
    request.generation = Generation {
        max_output_tokens: NonZeroU32::new(2048),
        ..generation
    };
    if let Some(options) = options {
        request.options.insert(KEY.to_owned(), options);
    }
    sent(&provider(host), &request)
}

fn reasoning(model: &str, effort: Effort) -> Value {
    with(
        "anthropic",
        model,
        Generation {
            reasoning: Some(effort),
            ..Generation::default()
        },
        None,
    )
}

fn budget(model: &str, tokens: u32) -> Value {
    with(
        "anthropic",
        model,
        Generation {
            reasoning_budget: NonZeroU32::new(tokens),
            ..Generation::default()
        },
        None,
    )
}

fn schema() -> Format {
    Format::Json {
        schema: Some(json!({"type": "object", "properties": {"answer": {"type": "string"}}})),
        name: None,
        description: None,
    }
}

/// The table holds what each model's encoding reads, found by its id or a
/// dated snapshot of it, never by a later model whose id merely starts with
/// one.
#[test]
fn the_table_holds_each_models_thinking_facts() {
    use crate::models::Sampling;
    use crate::models::anthropic::claude;
    let haiku = claude("claude-haiku-4-5-20251001").expect("listed");
    assert!(
        haiku.thinking.efforts.is_empty()
            && haiku.thinking.budget.is_some()
            && haiku.thinking.can_disable
            && !haiku.adaptive
    );
    let opus_4_5 = claude("claude-opus-4-5").expect("listed");
    assert_eq!(opus_4_5.thinking.efforts.len(), 3);
    assert!(!opus_4_5.adaptive);
    let sonnet_4_6 = claude(SONNET_4_6).expect("listed");
    assert!(
        sonnet_4_6.adaptive
            && sonnet_4_6.thinking.budget.is_some()
            && !sonnet_4_6.thinking.takes(Effort::XHigh)
    );
    let opus_4_8 = claude(OPUS_4_8).expect("listed");
    assert!(opus_4_8.thinking.budget.is_none() && opus_4_8.sampling == Sampling::Fixed);
    assert_eq!(
        claude(SONNET_5_5).expect("listed").thinking_off,
        crate::models::anthropic::ThinkingOff::BetweenTools
    );
    assert!(
        !claude("claude-opus-5-5-20260101")
            .expect("a snapshot")
            .thinking
            .can_disable
    );
    assert!(
        !claude("claude-fable-5")
            .expect("listed")
            .thinking
            .can_disable
    );
    assert!(
        claude("claude-opus-5")
            .expect("listed")
            .thinking
            .can_disable
    );
    assert!(claude("claude-opus-5-50").is_none());
    assert!(claude("custom-model").is_none());
}

/// Each class's thinking and effort, as Anthropic documents them; what a
/// model cannot take is left out.
#[test]
fn reasoning_takes_each_classes_documented_shape() {
    for (model, effort) in [
        (OPUS_4_6, Effort::Max),
        (OPUS_4_8, Effort::XHigh),
        ("claude-sonnet-5", Effort::Low),
        (OPUS_5_5, Effort::Medium),
        ("custom-model", Effort::High),
    ] {
        let body = reasoning(model, effort);
        assert_eq!(body["thinking"]["type"], "adaptive", "{model}");
        assert_eq!(body["output_config"]["effort"], effort.as_str(), "{model}");
    }
    let body = reasoning("claude-opus-4-5", Effort::High);
    assert_eq!(body["output_config"], json!({"effort": "high"}));
    assert!(body.get("thinking").is_none(), "{body}");

    for (model, off) in [
        (HAIKU_4_5, json!({"type": "disabled"})),
        ("claude-opus-5", json!({"type": "disabled"})),
        (SONNET_5_5, json!({"type": "between_tools"})),
    ] {
        assert_eq!(reasoning(model, Effort::None)["thinking"], off, "{model}");
    }
    assert_eq!(
        budget(OPUS_4_6, 1024)["thinking"],
        json!({"type": "enabled", "budget_tokens": 1024})
    );

    for body in [
        reasoning(HAIKU_4_5, Effort::High),
        reasoning(OPUS_4_6, Effort::XHigh),
        reasoning(OPUS_4_8, Effort::Minimal),
        budget(OPUS_4_8, 2048),
        budget(HAIKU_4_5, 512),
        budget(HAIKU_4_5, 4096),
    ] {
        assert!(body.get("thinking").is_none(), "{body}");
        assert!(body.get("output_config").is_none(), "{body}");
    }
    // A model that cannot turn thinking off keeps its default, which on a
    // binding model is adaptive thinking that drops unbound blocks.
    for model in [OPUS_5_5, "claude-fable-5"] {
        let body = reasoning(model, Effort::None);
        assert_ne!(body["thinking"]["type"], "disabled", "{model}");
    }
}

/// A model without adaptive thinking takes no effort level; newer models,
/// or one the table does not name, take every level they list.
#[test]
fn legacy_models_skip_an_effort_newer_ones_take_it() {
    for model in ["claude-sonnet-4-5", HAIKU_4_5] {
        let body = reasoning(model, Effort::Max);
        assert!(body.get("output_config").is_none(), "{model}: {body}");
    }
    for model in [OPUS_4_6, SONNET_4_6, OPUS_4_8, "custom-model"] {
        assert_eq!(
            reasoning(model, Effort::Max)["output_config"]["effort"],
            "max",
            "{model}"
        );
    }
}

/// Claude Opus 4, Opus 4.1 and Sonnet 4 take no effort level, and still
/// take a budget.
#[test]
fn claude_4_0_and_4_1_skip_an_effort_and_take_a_budget() {
    for model in [
        "claude-opus-4-0",
        "claude-opus-4-1",
        "claude-sonnet-4-0",
        "claude-opus-4-1-20250805",
        "claude-sonnet-4-20250514",
    ] {
        for effort in [
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::XHigh,
            Effort::Max,
        ] {
            let body = reasoning(model, effort);
            assert!(body.get("thinking").is_none(), "{model}: {body}");
            assert!(body.get("output_config").is_none(), "{model}: {body}");
        }
        assert_eq!(
            budget(model, 1024)["thinking"]["budget_tokens"],
            1024,
            "{model}"
        );
    }
}

#[test]
fn the_remaining_settings_take_their_documented_cells() {
    let one = |generation: Generation| with("anthropic", OPUS_4_6, generation, None);
    let body = one(Generation {
        top_p: Some(0.5),
        ..Generation::default()
    });
    assert_eq!(body["top_p"], 0.5);
    // `top_p` goes only without `temperature`.
    let body = one(Generation {
        top_p: Some(0.5),
        temperature: Some(0.2),
        ..Generation::default()
    });
    assert!(body.get("top_p").is_none(), "{body}");
    assert_eq!(body["temperature"], 0.2);
    let body = one(Generation {
        stop: vec!["END".to_owned()],
        seed: Some(1),
        top_k: NonZeroU32::new(5),
        ..Generation::default()
    });
    assert_eq!(body["stop_sequences"], json!(["END"]));
    assert_eq!(body["top_k"], 5);
    assert!(body.get("seed").is_none());

    // `parallel_tool_calls: false` joins the request's tool choice.
    let provider = provider("anthropic");
    let mut request = request(OPUS_4_6, vec![Message::user("Hi")]);
    request.tools = vec![tool("lookup")].into();
    request.parallel_tool_calls = Some(false);
    assert_eq!(
        sent(&provider, &request)["tool_choice"],
        json!({"type": "auto", "disable_parallel_tool_use": true})
    );
    request.tool_choice = ToolChoice::Required;
    assert_eq!(
        sent(&provider, &request)["tool_choice"],
        json!({"type": "any", "disable_parallel_tool_use": true})
    );
}

/// A model that fixes its sampling takes no `temperature`, `top_p` or
/// `top_k`.
#[test]
fn a_model_with_fixed_sampling_takes_none() {
    let body = with(
        "anthropic",
        OPUS_4_8,
        Generation {
            temperature: Some(0.2),
            top_p: Some(0.5),
            top_k: NonZeroU32::new(5),
            ..Generation::default()
        },
        None,
    );
    for key in ["temperature", "top_p", "top_k"] {
        assert!(body.get(key).is_none(), "{key}: {body}");
    }
}

/// The mapped effort, the schema's format and the caller's `output_config`
/// share one object, the caller's keys over the mapped ones.
#[test]
fn output_config_merges_effort_options_and_the_schema_format() {
    let high = || Generation {
        reasoning: Some(Effort::High),
        ..Generation::default()
    };
    let mut request = request(OPUS_5_5, vec![Message::user("Answer the question")]);
    request.generation = high();
    request.format = Some(schema());
    let provider = provider("anthropic");
    let body = sent(&provider, &request);
    assert_eq!(body["output_config"]["effort"], "high");
    assert_eq!(body["output_config"]["format"]["type"], "json_schema");

    request.options.insert(
        KEY.to_owned(),
        json!({"output_config": {"effort": "low", "task_budget": {"tokens": 1000}}}),
    );
    let body = sent(&provider, &request);
    assert_eq!(body["output_config"]["effort"], "low");
    assert_eq!(body["output_config"]["task_budget"]["tokens"], 1000);
    assert_eq!(body["output_config"]["format"]["type"], "json_schema");

    let body = with(
        "anthropic",
        OPUS_5_5,
        Generation::default(),
        Some(json!({"output_config": {"effort": "max"}})),
    );
    assert_eq!(body["output_config"], json!({"effort": "max"}));
    let body = with("anthropic", OPUS_5_5, Generation::default(), None);
    assert!(body.get("output_config").is_none(), "{body}");
}

/// A raw format merges over the schema's key by key, beside the mapped
/// effort.
#[test]
fn a_raw_output_format_is_above_the_schemas() {
    let mut request = request(OPUS_5_5, vec![Message::user("Answer the question")]);
    request.generation.reasoning = Some(Effort::High);
    request.format = Some(schema());
    request.options.insert(
        KEY.to_owned(),
        json!({"output_config": {"format": {"type": "json_schema",
            "schema": {"additionalProperties": true}}}}),
    );
    let body = sent(&provider("anthropic"), &request);
    let format = &body["output_config"]["format"];
    assert_eq!(format["schema"]["additionalProperties"], true);
    assert_eq!(format["schema"]["properties"]["answer"]["type"], "string");
    assert_eq!(body["output_config"]["effort"], "high");
}

/// The caller's thinking keys merge into the mapped thinking, and its
/// `type` replaces the mapped one.
#[test]
fn thinking_merges_the_mapped_thinking_with_the_options() {
    let high = Generation {
        reasoning: Some(Effort::High),
        ..Generation::default()
    };
    let body = with(
        "anthropic",
        OPUS_5_5,
        high.clone(),
        Some(json!({"thinking": {"display": "summarized",
            "block_binding": {"prefix_mismatch_behavior": "error"}}})),
    );
    assert_eq!(
        body["thinking"],
        json!({"type": "adaptive", "display": "summarized",
            "block_binding": {"prefix_mismatch_behavior": "error"}})
    );
    let body = with(
        "anthropic",
        OPUS_5_5,
        high,
        Some(json!({"thinking": {"type": "disabled"}})),
    );
    assert_eq!(body["thinking"], json!({"type": "disabled"}));
}

#[test]
fn adaptive_settings_merge_schema_effort_tools_and_passthrough() {
    let mut request = request(SONNET_4_6, vec![Message::user("Answer the question")]);
    request.generation.reasoning = Some(Effort::Max);
    request.format = Some(schema());
    request.options.insert(
        KEY.to_owned(),
        json!({
            "thinking": {"display": "omitted"},
            "output_config": {"custom_option": true},
            "tools": [{"type": "web_search_20250305", "name": "web_search"}],
            "metadata": {"user_id": "test-only"}
        }),
    );
    let body = sent(&provider("anthropic"), &request);
    assert_eq!(
        body["thinking"],
        json!({"type": "adaptive", "display": "omitted"})
    );
    assert_eq!(body["output_config"]["effort"], "max");
    assert_eq!(body["output_config"]["custom_option"], true);
    assert_eq!(body["output_config"]["format"]["schema"]["type"], "object");
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["metadata"]["user_id"], "test-only");
}

/// A gateway takes no reasoning setting no vendor page or recording
/// confirms, and passes the caller's thinking and effort through unchecked.
#[test]
fn gateways_skip_unverified_reasoning_and_pass_raw_keys_through() {
    let body = with(
        "zai",
        "glm-5",
        Generation {
            reasoning: Some(Effort::High),
            top_p: Some(0.5),
            stop: vec!["END".to_owned()],
            ..Generation::default()
        },
        None,
    );
    for key in ["thinking", "output_config", "top_p", "stop_sequences"] {
        assert!(body.get(key).is_none(), "{key}: {body}");
    }
    let body = with(
        "zai",
        "glm-5",
        Generation::default(),
        Some(json!({"thinking": {"type": "enabled"}, "output_config": {"effort": "turbo"}})),
    );
    assert_eq!(body["thinking"], json!({"type": "enabled"}));
    assert_eq!(body["output_config"]["effort"], "turbo");
}

/// `MiniMax` turns thinking off on M3 and takes an effort on M3.1; `MiMo`
/// turns it off, and takes `top_p` in its range and stop sequences.
#[test]
fn minimax_and_mimo_take_their_documented_cells() {
    let off = Generation {
        reasoning: Some(Effort::None),
        ..Generation::default()
    };
    let high = Generation {
        reasoning: Some(Effort::High),
        ..Generation::default()
    };
    assert_eq!(
        with("minimax", "MiniMax-M3", off.clone(), None)["thinking"],
        json!({"type": "disabled"})
    );
    assert!(
        with("minimax", "MiniMax-M2.7", off.clone(), None)
            .get("thinking")
            .is_none()
    );
    assert_eq!(
        with("minimax", "MiniMax-M3.1-flash", high.clone(), None)["output_config"],
        json!({"effort": "high"})
    );
    assert!(
        with("minimax", "MiniMax-M3", high.clone(), None)
            .get("output_config")
            .is_none()
    );
    assert_eq!(
        with("xiaomimimo", "mimo-v2.5", off, None)["thinking"],
        json!({"type": "disabled"})
    );
    assert!(
        with("xiaomimimo", "mimo-v2.5", high, None)
            .get("thinking")
            .is_none()
    );
    let sampled = |top_p| {
        with(
            "xiaomimimo",
            "mimo-v2.5",
            Generation {
                top_p: Some(top_p),
                stop: vec!["END".to_owned()],
                ..Generation::default()
            },
            None,
        )
    };
    assert_eq!(sampled(0.5)["top_p"], 0.5);
    assert_eq!(sampled(0.5)["stop_sequences"], json!(["END"]));
    assert!(sampled(0.0).get("top_p").is_none());
}

/// Tools that arrive only through the options still count: turning
/// parallel calls off sets the flag rather than being dropped.
#[test]
fn raw_tools_count_for_parallel_tool_calls() {
    let raw =
        json!({"tools": [{"name": "t", "description": "d", "input_schema": {"type": "object"}}]});
    for (host, model) in [("anthropic", OPUS_4_6), ("xiaomimimo", "mimo-v2-flash")] {
        let mut request = request(model, vec![Message::user("Hi")]);
        request.parallel_tool_calls = Some(false);
        request.options.insert(KEY.to_owned(), raw.clone());
        let body = sent(&provider(host), &request);
        assert_eq!(
            body["tool_choice"],
            json!({"type": "auto", "disable_parallel_tool_use": true}),
            "{host}"
        );
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
    }
}
