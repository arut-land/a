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
        assert!(ids().any(|known| known == id), "{id}");
    }
}
