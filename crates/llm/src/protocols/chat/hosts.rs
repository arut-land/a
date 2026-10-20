//! The hosts that serve Chat Completions, with their quirks.

use super::{LaterSystem, Media, OutputCap, Quirks, ReasoningField, Routing, Vendor};
use crate::models::Maker;
use crate::protocol::{Host, KeyStyle};
use crate::spec::Reason;

/// The `api-version` an Azure deployment is addressed with by default.
pub const AZURE_API_VERSION: &str = "2024-10-21";

/// The path of a host whose bare base URL leaves the `/v1` to it.
const V1: &str = "/v1/chat/completions";

/// A bearer-key host whose base URL `base_url_env` may override.
const fn at(
    id: &'static str,
    base_url: &'static str,
    key_env: &'static [&'static str],
    base_url_env: &'static [&'static str],
) -> Host {
    Host {
        base_url_env,
        ..Host::bearer(id, base_url, key_env)
    }
}

/// What media `vendor`'s hosts take.
const fn media(vendor: Vendor) -> Media {
    let parts = !matches!(vendor, Vendor::DeepSeek | Vendor::Mira);
    let files = parts && !matches!(vendor, Vendor::Perplexity | Vendor::Cohere | Vendor::Ollama);
    Media {
        parts,
        files,
        video: files && !matches!(vendor, Vendor::OpenAi | Vendor::Azure | Vendor::Mistral),
        // Ollama reads an image as data and never fetches a URL.
        url_images: !matches!(vendor, Vendor::Ollama),
        // OpenRouter and Mistral fetch a PDF a URL names.
        url_pdfs: matches!(vendor, Vendor::OpenRouter | Vendor::Mistral),
    }
}

/// Whose model rules `vendor`'s hosts follow.
const fn maker(vendor: Vendor) -> Option<Maker> {
    Some(match vendor {
        Vendor::OpenAi | Vendor::Azure => Maker::OpenAi,
        Vendor::DeepSeek | Vendor::Mira => Maker::TextOnly,
        Vendor::Groq => Maker::Groq,
        Vendor::Mistral => Maker::Mistral,
        Vendor::Xai => Maker::Xai,
        Vendor::Cohere => Maker::Cohere,
        Vendor::Zai => Maker::Zai,
        Vendor::Moonshot => Maker::Moonshot,
        Vendor::MiniMax => Maker::MiniMax,
        Vendor::Mimo => Maker::Xiaomi,
        Vendor::OpenRouter
        | Vendor::Together
        | Vendor::Venice
        | Vendor::LlamaCpp
        | Vendor::Ollama
        | Vendor::Perplexity
        | Vendor::Other => return None,
    })
}

/// The baseline under `vendor`'s rules: what its hosts take follows from
/// it, here, once.
const fn vendor(vendor: Vendor) -> Quirks {
    Quirks {
        vendor,
        media: media(vendor),
        maker: maker(vendor),
        ..Quirks::OPENAI
    }
}

/// A host taking neither tools nor structured output.
const fn text_only(vendor: Vendor) -> Quirks {
    Quirks {
        supports_tools: false,
        supports_response_format: false,
        ..self::vendor(vendor)
    }
}

const ZAI: Quirks = Quirks {
    finishes: &[
        ("model_context_window_exceeded", Reason::Length),
        ("sensitive", Reason::ContentFilter),
    ],
    ..vendor(Vendor::Zai)
};

/// Its thinking models take their reasoning back on every assistant
/// message.
const REPLAYS_REASONING: Option<ReasoningField> = Some(ReasoningField::ReasoningContent);

/// Every host that serves Chat Completions, with its quirks: a table, one
/// host to an entry.
#[rustfmt::skip]
pub const HOSTS: &[(Host, Quirks)] = &[
    (at("openai", "https://api.openai.com/v1", &["OPENAI_API_KEY"], &["OPENAI_BASE_URL"]),
        Quirks { output_cap: OutputCap::OpenAiReasoning, call_id_limit: Some(40),
            ..vendor(Vendor::OpenAi) }),
    // The base URL is the account's own resource endpoint. An Entra token
    // goes as `Auth::Bearer`, and another `api-version` in the quirks'
    // routing, on a provider built from these.
    (Host { key: KeyStyle::Header("api-key"),
            ..at("azure.openai", "", &["AZURE_API_KEY"], &["AZURE_ENDPOINT"]) },
        Quirks { routing: Routing::Deployment { api_version: AZURE_API_VERSION },
            ..vendor(Vendor::Azure) }),
    // DeepSeek takes `json_object`, not `json_schema`.
    (at("deepseek", "https://api.deepseek.com", &["DEEPSEEK_API_KEY"], &[]),
        Quirks { supports_response_format: false, reasoning_field: REPLAYS_REASONING,
            ..vendor(Vendor::DeepSeek) }),
    (at("groq", "https://api.groq.com/openai/v1", &["GROQ_API_KEY"], &[]), vendor(Vendor::Groq)),
    // No tool calling, and structured output is unverified.
    (at("hyperbolic", "https://api.hyperbolic.xyz", &["HYPERBOLIC_API_KEY"], &[]),
        Quirks { path: V1, ..text_only(Vendor::Other) }),
    // The gateway rejects tools, structured output and unknown parameters
    // such as `stream_options`.
    (at("mira", "https://api.mira.network", &["MIRA_API_KEY"], &[]),
        Quirks { path: V1, stream_usage: false, bare_string_reply: true,
            ..text_only(Vendor::Mira) }),
    (at("perplexity", "https://api.perplexity.ai", &["PERPLEXITY_API_KEY"], &[]),
        Quirks { stream_usage: false, ..text_only(Vendor::Perplexity) }),
    // Structured output is per model on Together.
    (at("together", "https://api.together.xyz", &["TOGETHER_API_KEY"], &[]),
        Quirks { path: V1, supports_response_format: false, ..vendor(Vendor::Together) }),
    (at("huggingface", "https://router.huggingface.co", &["HUGGINGFACE_API_KEY"], &[]),
        Quirks { path: V1, supports_response_format: false, ..Quirks::OPENAI }),
    // A server started without `--api-key` rejects a request that carries a
    // credential.
    (Host { keyless: true, ..at("llamacpp", "http://localhost:8080/v1", &["LLAMACPP_API_KEY"],
            &["LLAMACPP_API_BASE_URL"]) },
        Quirks { image_tool_results: true, ..vendor(Vendor::LlamaCpp) }),
    // Mistral reports usage on its last chunk unasked.
    (at("mistral", "https://api.mistral.ai", &["MISTRAL_API_KEY"], &[]),
        Quirks { path: V1, stream_usage: false, later_system: LaterSystem::Leading,
            ..vendor(Vendor::Mistral) }),
    (at("openrouter", "https://openrouter.ai/api/v1", &["OPENROUTER_API_KEY"],
            &["OPENROUTER_BASE_URL"]),
        Quirks { stream_usage: false, native_finish_reason: true, response_format_with_tools: true,
            file_ids: false, ..vendor(Vendor::OpenRouter) }),
    (at("venice", "https://api.venice.ai/api/v1", &["VENICE_API_KEY"], &["VENICE_BASE_URL"]),
        vendor(Vendor::Venice)),
    // Qwen3.5 replies report more reasoning tokens than completion tokens.
    (at("doubleword", "https://api.doubleword.ai/v1", &["DOUBLEWORD_API_KEY"],
            &["DOUBLEWORD_BASE_URL"]),
        Quirks { reliable_reasoning_count: false, ..Quirks::OPENAI }),
    (at("zai", "https://api.z.ai/api/paas/v4", &["ZAI_API_KEY"], &["ZAI_API_BASE"]), ZAI),
    (at("zai-coding", "https://api.z.ai/api/coding/paas/v4", &["ZAI_API_KEY"], &["ZAI_API_BASE"]),
        ZAI),
    (at("minimax", "https://api.minimax.io/v1", &["MINIMAX_API_KEY"], &["MINIMAX_API_BASE"]),
        vendor(Vendor::MiniMax)),
    (at("minimax-cn", "https://api.minimaxi.com/v1", &["MINIMAX_API_KEY"], &["MINIMAX_API_BASE"]),
        vendor(Vendor::MiniMax)),
    // Moonshot rejects `json_schema` response formats.
    (at("moonshot", "https://api.moonshot.ai/v1", &["MOONSHOT_API_KEY"], &["MOONSHOT_API_BASE"]),
        Quirks { supports_response_format: false, ..vendor(Vendor::Moonshot) }),
    (at("moonshot-cn", "https://api.moonshot.cn/v1", &["MOONSHOT_API_KEY"], &["MOONSHOT_API_BASE"]),
        Quirks { supports_response_format: false, ..vendor(Vendor::Moonshot) }),
    // Every MiMo model takes its reasoning back on every assistant message.
    (at("xiaomimimo", "https://api.xiaomimimo.com/v1", &["XIAOMI_MIMO_API_KEY"],
            &["XIAOMI_MIMO_API_BASE"]),
        Quirks { reasoning_field: REPLAYS_REASONING, ..vendor(Vendor::Mimo) }),
    (at("cohere", "https://api.cohere.ai/compatibility/v1", &["COHERE_API_KEY"], &[]),
        vendor(Vendor::Cohere)),
    // A local daemon takes no credential; a proxied one takes a token.
    (Host { keyless: true, ..at("ollama", "http://localhost:11434/v1", &["OLLAMA_API_KEY"], &[]) },
        vendor(Vendor::Ollama)),
    (at("xai", "https://api.x.ai", &["XAI_API_KEY"], &["XAI_BASE_URL"]),
        Quirks { path: V1, ..vendor(Vendor::Xai) }),
];
