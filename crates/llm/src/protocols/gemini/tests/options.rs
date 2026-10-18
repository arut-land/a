//! Ported from rig's `options/tests.rs`.

use super::*;

fn thinking(model: &str, generation: Generation) -> Value {
    let body = sent(
        model,
        Request {
            generation,
            ..asking(vec![Message::user("hi")])
        },
    );
    body.at("/generationConfig/thinkingConfig")
        .cloned()
        .unwrap_or(Value::Null)
}

fn effort(effort: Effort) -> Generation {
    Generation {
        reasoning: Some(effort),
        ..Generation::default()
    }
}

fn budget(tokens: u32) -> Generation {
    Generation {
        reasoning_budget: NonZeroU32::new(tokens),
        ..Generation::default()
    }
}

/// Thinking follows the model: Gemini 2.5 takes a budget, Gemini 3 a level,
/// and what a model cannot take is left out, with thought summaries kept on
/// while it thinks.
#[test]
fn reasoning_follows_the_models_thinking() {
    let summaries = json!({"includeThoughts": true});
    for (model, generation, expected) in [
        (
            "gemini-2.5-flash",
            effort(Effort::None),
            json!({"thinkingBudget": 0}),
        ),
        (
            "gemini-2.5-pro",
            budget(4096),
            json!({"thinkingBudget": 4096, "includeThoughts": true}),
        ),
        (
            "gemini-3.8-flash",
            effort(Effort::Low),
            json!({"thinkingLevel": "low", "includeThoughts": true}),
        ),
        ("gemini-2.5-flash", Generation::default(), summaries.clone()),
        (
            "gemini-3.8-flash",
            effort(Effort::Minimal),
            summaries.clone(),
        ),
        ("gemini-3.8-flash", effort(Effort::None), summaries.clone()),
        ("gemini-3.8-flash", effort(Effort::XHigh), summaries.clone()),
        ("gemini-2.5-pro", effort(Effort::None), summaries.clone()),
        ("gemini-2.5-pro", budget(64), summaries.clone()),
        ("gemini-2.5-flash", effort(Effort::High), summaries.clone()),
    ] {
        assert_eq!(
            thinking(model, generation.clone()),
            expected,
            "{model}: {generation:?}"
        );
    }
}

/// Gemma 4 thinks at level `high` and turns off at `minimal`; the image
/// models think as their model pages say.
#[test]
fn gemma_and_the_image_models_think_as_documented() {
    for model in ["gemma-4-26b-a4b-it", "gemma-4-31b-it"] {
        assert_eq!(
            thinking(model, effort(Effort::None)),
            json!({"thinkingLevel": "minimal"})
        );
        assert_eq!(
            thinking(model, effort(Effort::High)),
            json!({"thinkingLevel": "high", "includeThoughts": true})
        );
    }
    assert_eq!(
        thinking("gemini-3-pro-image-preview", effort(Effort::Low)),
        json!({"thinkingLevel": "low", "includeThoughts": true})
    );
    assert_eq!(
        thinking("gemini-2.5-flash-image", effort(Effort::None)),
        Value::Null
    );
}

/// A versioned id takes its model's thinking; a Gemini before 2.5 thinks
/// not at all, and an id no rule knows is sent what it asks for.
#[test]
fn a_versioned_id_takes_its_models_thinking() {
    assert_eq!(
        thinking("gemini-2.5-flash-preview-09-2025", effort(Effort::None)),
        json!({"thinkingBudget": 0})
    );
    assert_eq!(
        thinking("models/gemini-2.5-flash-lite-preview-06-17", budget(100)),
        json!({"includeThoughts": true})
    );
    for model in ["gemini-2.0-flash-001", "gemini-1.5-pro-002"] {
        assert_eq!(
            thinking(model, effort(Effort::None)),
            Value::Null,
            "{model}"
        );
        assert_eq!(thinking(model, budget(1024)), Value::Null, "{model}");
    }
    assert_eq!(
        thinking("my-tuned-model", effort(Effort::Medium)),
        json!({"thinkingLevel": "medium"})
    );
    assert_eq!(
        thinking("my-tuned-model", budget(1024)),
        json!({"thinkingBudget": 1024})
    );
}

/// This protocol's `generationConfig` option merges over the typed fields
/// key by key: a key set both ways is the option's.
#[test]
fn a_raw_generation_config_beats_the_typed_fields_and_keeps_the_rest() {
    let body = sent(
        "gemini-2.0-flash",
        Request {
            generation: Generation {
                temperature: Some(0.2),
                max_output_tokens: NonZeroU32::new(64),
                top_p: Some(0.5),
                seed: Some(7),
                stop: vec!["END".into()],
                ..Generation::default()
            },
            options: Meta::from([(
                KEY.to_owned(),
                json!({"generationConfig": {"temperature": 0.9, "topK": 3}, "serviceTier": "flex"}),
            )]),
            ..asking(vec![Message::user("hi")])
        },
    );
    assert_eq!(
        body["generationConfig"],
        json!({"temperature": 0.9, "maxOutputTokens": 64, "topP": 0.5, "seed": 7, "stopSequences": ["END"], "topK": 3})
    );
    assert_eq!(body["serviceTier"], "flex");
}

/// A seed beyond 32 bits and more than 5 stop sequences are left out.
#[test]
fn options_gemini_cannot_take_are_left_out() {
    let body = sent(
        "gemini-2.0-flash",
        Request {
            generation: Generation {
                seed: Some(u64::from(u32::MAX)),
                stop: (0..6).map(|n| n.to_string()).collect(),
                top_k: NonZeroU32::new(40),
                ..Generation::default()
            },
            parallel_tool_calls: Some(false),
            ..asking(vec![Message::user("hi")])
        },
    );
    assert_eq!(body["generationConfig"], json!({"topK": 40}));
}

#[test]
fn user_media_goes_inline_or_by_reference() {
    let body = sent(
        "gemini-2.5-flash",
        asking(vec![Message::User(User {
            content: vec![
                UserPart::Text(Text::new(" ")),
                image("application/pdf", "JVBERi0="),
                UserPart::Media(Media {
                    media_type: "video/mp4".into(),
                    source: Source::Url {
                        url: "https://www.youtube.com/watch?v=x".into(),
                    },
                    meta: Meta::new(),
                }),
            ],
        })]),
    );
    assert_eq!(
        body["contents"][0]["parts"],
        json!([
            {"inlineData": {"mimeType": "application/pdf", "data": "JVBERi0="}, "thought": false},
            {"fileData": {"mimeType": "video/mp4", "fileUri": "https://www.youtube.com/watch?v=x"}, "thought": false},
        ])
    );
    let provider = provider();
    let request = asking(vec![Message::User(User {
        content: vec![UserPart::Media(Media {
            media_type: "image/png".into(),
            source: Source::File {
                id: "file-1".into(),
            },
            meta: Meta::new(),
        })],
    })]);
    let target = Target {
        provider: &provider,
        model: "gemini-2.5-flash",
    };
    let error = provider
        .protocol
        .encode(&target, &request)
        .expect_err("a file id");
    assert_eq!(error.kind, Kind::Unsupported);
}
