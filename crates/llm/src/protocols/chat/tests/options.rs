//! The generation options per host, ported from rig's option tests.

use std::num::NonZeroU32;

use serde_json::json;

use super::request::{effort, hello, tool, with};
use super::{gateway, provider, sent, sent_to};
use crate::Kind;
use crate::protocols::chat::Quirks;
use crate::spec::{Effort, Generation, Request, Tool};

/// `OpenAI`'s reasoning options by model: effort `none` where the model can
/// turn reasoning off, nothing where it cannot or does not reason; sampling
/// and stop sequences only where the model takes them.
#[test]
fn openai_options_follow_the_model() {
    let openai = provider("openai");
    let body = |model: &str, generation: Generation| sent(&openai, &with(model, generation));
    assert_eq!(
        body("gpt-5.2", effort(Effort::None))["reasoning_effort"],
        "none"
    );
    assert_eq!(
        body("gpt-5.1-codex", effort(Effort::None))["reasoning_effort"],
        "none"
    );
    for model in [
        "gpt-5",
        "gpt-5-codex",
        "o3-deep-research-2025-06-26",
        "gpt-4o",
    ] {
        assert!(
            body(model, effort(Effort::None))
                .get("reasoning_effort")
                .is_none(),
            "{model}"
        );
    }
    assert!(
        body("gpt-4o", effort(Effort::High))
            .get("reasoning_effort")
            .is_none()
    );
    assert_eq!(body("o3", effort(Effort::High))["reasoning_effort"], "high");

    let sampled = |model: &str, reasoning: Option<Effort>| {
        body(
            model,
            Generation {
                top_p: Some(0.9),
                temperature: Some(0.5),
                reasoning,
                ..Generation::default()
            },
        )
    };
    for model in ["gpt-5-mini", "o3"] {
        let body = sampled(model, None);
        assert!(
            body.get("top_p").is_none() && body.get("temperature").is_none(),
            "{model}"
        );
    }
    assert_eq!(sampled("gpt-5.4-nano", Some(Effort::None))["top_p"], 0.9);
    assert!(
        sampled("gpt-5.4-nano", Some(Effort::Low))
            .get("top_p")
            .is_none()
    );
    assert_eq!(sampled("gpt-4.1-mini", None)["top_p"], 0.9);

    let stop = |model: &str, stop: &[&str]| {
        body(
            model,
            Generation {
                stop: stop.iter().map(|stop| (*stop).to_owned()).collect(),
                ..Generation::default()
            },
        )
        .get("stop")
        .cloned()
    };
    for model in ["gpt-5-mini", "gpt-5.4-nano", "o4-mini", "gpt-5.1-codex"] {
        assert_eq!(stop(model, &["END"]), None, "{model}");
    }
    assert_eq!(stop("gpt-4.1-mini", &["END"]), Some(json!(["END"])));
    assert_eq!(stop("prod-deployment", &["END"]), Some(json!(["END"])));
    assert_eq!(
        stop("gpt-4.1", &["a", "b", "c", "d", "e"]),
        None,
        "at most four"
    );

    let parallel = |tools: Vec<Tool>| {
        sent(
            &openai,
            &Request {
                tools: tools.into(),
                parallel_tool_calls: Some(true),
                ..hello("gpt-4o")
            },
        )
        .get("parallel_tool_calls")
        .cloned()
    };
    assert_eq!(parallel(vec![tool("f")]), Some(json!(true)));
    assert_eq!(parallel(Vec::new()), None, "only beside tools");
}

/// GPT-6 models that cannot turn reasoning off cannot call tools through
/// Chat Completions.
#[test]
fn a_gpt_6_model_that_always_reasons_takes_no_tools() {
    let tools = |model: &str| Request {
        tools: vec![tool("f")].into(),
        ..hello(model)
    };
    let refused = sent_to(&provider("openai"), &tools("gpt-6-astra")).expect_err("refused");
    assert_eq!(refused.kind, Kind::Unsupported);
    assert!(sent_to(&provider("openai"), &tools("gpt-6-luna")).is_ok());
}

/// Each host spells reasoning and the other options its own way, and
/// leaves out what the model cannot take.
#[test]
fn hosts_spell_the_generation_options() {
    let body = |id: &str, model: &str, generation: Generation| {
        sent(&provider(id), &with(model, generation))
    };
    let budget = Generation {
        reasoning_budget: NonZeroU32::new(2048),
        ..Generation::default()
    };
    assert_eq!(
        body("openrouter", "openai/gpt-5.2", effort(Effort::None))["reasoning"],
        json!({"effort": "none"})
    );
    assert_eq!(
        body("openrouter", "anthropic/claude-sonnet-4.5", budget.clone())["reasoning"],
        json!({"max_tokens": 2048})
    );
    assert!(
        body("openrouter", "openai/gpt-5.2", budget)
            .get("reasoning")
            .is_none()
    );

    assert_eq!(
        body("deepseek", "deepseek-v4-flash", effort(Effort::None))["thinking"],
        json!({"type": "disabled"})
    );
    let high = body("deepseek", "deepseek-v4-flash", effort(Effort::High));
    assert_eq!(high["thinking"], json!({"type": "enabled"}));
    assert_eq!(high["reasoning_effort"], "high");
    let medium = body("deepseek", "deepseek-v4-flash", effort(Effort::Medium));
    assert!(medium.get("reasoning_effort").is_none() && medium.get("thinking").is_none());

    let seeded = Generation {
        seed: Some(3),
        ..Generation::default()
    };
    let mistral = body("mistral", "mistral-large", seeded.clone());
    assert_eq!(mistral["random_seed"], 3);
    assert!(mistral.get("seed").is_none());
    assert!(
        body("deepseek", "deepseek-chat", seeded.clone())
            .get("seed")
            .is_none()
    );
    assert_eq!(body("groq", "qwen/qwen3.8-27b", seeded)["seed"], 3);

    assert!(
        body("groq", "openai/gpt-oss-120b", effort(Effort::None))
            .get("reasoning_effort")
            .is_none()
    );
    assert_eq!(
        body("groq", "openai/gpt-oss-120b", effort(Effort::High))["reasoning_effort"],
        "high"
    );

    let kimi = body("moonshot", "kimi-k2.6", effort(Effort::None));
    assert_eq!(kimi["thinking"], json!({"type": "disabled"}));
    assert_eq!(
        body("moonshot", "kimi-k3", effort(Effort::Max))["reasoning_effort"],
        "max"
    );

    let together = body("together", "m", effort(Effort::Low));
    assert_eq!(together["reasoning"], json!({"enabled": true}));
    assert_eq!(together["reasoning_effort"], "low");

    assert_eq!(
        body("zai", "glm-4.6", effort(Effort::High)).get("reasoning_effort"),
        None,
        "GLM 4 takes no level"
    );
    assert_eq!(
        body("zai", "glm-5", effort(Effort::High))["reasoning_effort"],
        "high"
    );

    let mimo = |reasoning| {
        body(
            "xiaomimimo",
            "mimo-v2.5",
            Generation {
                top_p: Some(0.5),
                reasoning,
                ..Generation::default()
            },
        )
    };
    assert!(
        mimo(None).get("top_p").is_none(),
        "MiMo fixes top_p while it thinks"
    );
    assert_eq!(mimo(Some(Effort::None))["top_p"], 0.5);

    assert_eq!(
        body("cohere", "command-a", effort(Effort::High))["reasoning_effort"],
        "high"
    );
    assert!(
        body("cohere", "command-a", effort(Effort::Low))
            .get("reasoning_effort")
            .is_none()
    );
}

/// A gateway with no known option mapping sends sampling and the cap, and
/// nothing a caller would have to name in its own options.
#[test]
fn an_unknown_gateway_takes_only_sampling() {
    let body = sent(
        &gateway(Quirks::OPENAI),
        &with(
            "model",
            Generation {
                temperature: Some(0.2),
                top_p: Some(0.5),
                seed: Some(1),
                stop: vec!["x".into()],
                reasoning: Some(Effort::High),
                max_output_tokens: NonZeroU32::new(8),
                ..Generation::default()
            },
        ),
    );
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["max_tokens"], 8);
    for key in ["top_p", "seed", "stop", "reasoning_effort"] {
        assert!(body.get(key).is_none(), "{key}: {body}");
    }
}
