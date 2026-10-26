use super::*;

#[test]
fn specs_name_a_host_a_protocol_and_a_model() {
    let spec = |host, protocol, model| Spec {
        host,
        protocol,
        model,
    };
    assert_eq!(parse("gpt-6-luna"), spec("openai", None, "gpt-6-luna"));
    assert_eq!(
        parse("openrouter:anthropic/claude-sonnet-5"),
        spec("openrouter", None, "anthropic/claude-sonnet-5")
    );
    assert_eq!(
        parse("openai@chat:gpt-4.1"),
        spec("openai", Some("chat"), "gpt-4.1")
    );
    assert_eq!(parse("ollama:qwen3:8b"), spec("ollama", None, "qwen3:8b"));
}

#[test]
fn every_protocol_contributes_hosts() {
    for id in ["openai", "groq", "deepseek", "anthropic", "zai", "gemini"] {
        assert!(
            WIRES
                .iter()
                .any(|wire| (wire.find)(id, &|_| None).is_some()),
            "{id}"
        );
    }
}

#[test]
fn upload_compression_stays_with_the_verified_endpoint() {
    use transport::Compression;
    for (spec, coding) in [
        ("openai:model", Some(Compression::Zstd)),
        ("openai@chat:model", None),
        ("anthropic:model", Some(Compression::Gzip)),
        ("gemini:model", Some(Compression::Gzip)),
        ("openrouter:model", None),
        ("xai:model", None),
        ("deepseek:model", None),
        ("groq:model", None),
        ("mistral:model", None),
        ("zai@messages:model", None),
        ("minimax@messages:model", None),
        ("moonshot@messages:model", None),
        ("xiaomimimo@messages:model", None),
        ("ollama:model", None),
    ] {
        let (configured, _) = provider(spec, &|_| Some("key".into())).unwrap();
        // A custom URL never inherits the built-in host's capability.
        assert_eq!(configured.compression, None, "{spec} with a URL override");
        let (configured, _) = provider(spec, &|name| {
            name.ends_with("API_KEY").then(|| "key".into())
        })
        .unwrap();
        assert_eq!(configured.compression, coding, "{spec}");
        let request = configured
            .request(
                "model",
                &crate::Request {
                    messages: vec![crate::Message::user("Hi")].into(),
                    generation: crate::Generation {
                        max_output_tokens: std::num::NonZeroU32::new(1),
                        ..crate::Generation::default()
                    },
                    ..crate::Request::default()
                },
            )
            .unwrap();
        assert_eq!(
            request.extensions().get::<Compression>().copied(),
            coding,
            "{spec} request"
        );
        assert_eq!(
            configured
                .clone()
                .with_base_url(format!("{}/", configured.base_url))
                .compression,
            coding,
            "{spec} with the same endpoint"
        );
        assert_eq!(
            configured
                .with_base_url("http://127.0.0.1:1234")
                .compression,
            None
        );
    }
}
