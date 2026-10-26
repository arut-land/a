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
fn upload_compression_belongs_only_to_a_documented_endpoint() {
    use transport::Compression;
    for (spec, coding) in [
        ("openai:model", Some(Compression::Zstd)),
        ("openai@chat:model", None),
        ("anthropic:model", None),
        ("gemini:model", None),
        ("openrouter:model", None),
        ("xai:model", None),
        ("deepseek:model", None),
        ("ollama:model", None),
    ] {
        let (configured, _) = provider(spec, &|_| Some("key".into())).unwrap();
        // A custom URL never inherits the documented host's capability.
        assert_eq!(configured.compression, None, "{spec} with a URL override");
        let (configured, _) = provider(spec, &|name| {
            name.ends_with("API_KEY").then(|| "key".into())
        })
        .unwrap();
        assert_eq!(configured.compression, coding, "{spec}");
        assert_eq!(
            configured
                .with_base_url("http://127.0.0.1:1234")
                .compression,
            None
        );
    }
}
