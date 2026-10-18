//! The hosts that serve Chat Completions, with their quirks (rig's
//! dialects).

use super::{LaterSystem, OutputCap, Quirks, Rewrite, Routing, Vendor};
use crate::protocol::{Host, KeyStyle};
use crate::spec::Reason;

/// The `api-version` an Azure deployment is addressed with by default.
pub const AZURE_API_VERSION: &str = "2024-10-21";

const ZAI: Quirks = Quirks {
    vendor: Vendor::Zai,
    finishes: &[
        ("model_context_window_exceeded", Reason::Length),
        ("sensitive", Reason::ContentFilter),
    ],
    ..Quirks::OPENAI
};

const MINIMAX: Quirks = Quirks {
    vendor: Vendor::MiniMax,
    ..Quirks::OPENAI
};

const MOONSHOT: Quirks = Quirks {
    vendor: Vendor::Moonshot,
    // Moonshot rejects `json_schema` response formats.
    supports_response_format: false,
    rewrite: Rewrite::Moonshot,
    ..Quirks::OPENAI
};

/// Every host that serves Chat Completions, with its quirks.
pub const HOSTS: &[(Host, Quirks)] = &[
    (
        Host {
            base_url_env: &["OPENAI_BASE_URL"],
            ..Host::bearer("openai", "https://api.openai.com/v1", &["OPENAI_API_KEY"])
        },
        Quirks {
            vendor: Vendor::OpenAi,
            output_cap: OutputCap::OpenAiReasoning,
            call_id_limit: Some(40),
            ..Quirks::OPENAI
        },
    ),
    (
        // The base URL is the account's own resource endpoint. An Entra
        // token goes as `Auth::Bearer`, and another `api-version` in the
        // quirks' routing, on a provider built from these.
        Host {
            base_url_env: &["AZURE_ENDPOINT"],
            key: KeyStyle::Header("api-key"),
            ..Host::bearer("azure.openai", "", &["AZURE_API_KEY"])
        },
        Quirks {
            vendor: Vendor::Azure,
            routing: Routing::Deployment {
                api_version: AZURE_API_VERSION,
            },
            ..Quirks::OPENAI
        },
    ),
    (
        Host::bearer(
            "deepseek",
            "https://api.deepseek.com",
            &["DEEPSEEK_API_KEY"],
        ),
        Quirks {
            vendor: Vendor::DeepSeek,
            // DeepSeek takes `json_object`, not `json_schema`.
            supports_response_format: false,
            rewrite: Rewrite::DeepSeek,
            // Its thinking models take their reasoning back on every
            // assistant message.
            reasoning_field: Some("reasoning_content"),
            ..Quirks::OPENAI
        },
    ),
    (
        Host::bearer("groq", "https://api.groq.com/openai/v1", &["GROQ_API_KEY"]),
        Quirks {
            vendor: Vendor::Groq,
            ..Quirks::OPENAI
        },
    ),
    (
        // The bare host: the path carries its own `/v1`.
        Host::bearer(
            "hyperbolic",
            "https://api.hyperbolic.xyz",
            &["HYPERBOLIC_API_KEY"],
        ),
        Quirks {
            path: "/v1/chat/completions",
            // No tool calling, and structured output is unverified.
            supports_tools: false,
            supports_response_format: false,
            ..Quirks::OPENAI
        },
    ),
    (
        Host::bearer("mira", "https://api.mira.network", &["MIRA_API_KEY"]),
        Quirks {
            path: "/v1/chat/completions",
            vendor: Vendor::Mira,
            // The gateway rejects tools, structured output and unknown
            // parameters such as `stream_options`.
            supports_tools: false,
            supports_response_format: false,
            stream_usage: false,
            bare_string_reply: true,
            rewrite: Rewrite::Mira,
            ..Quirks::OPENAI
        },
    ),
    (
        Host::bearer(
            "perplexity",
            "https://api.perplexity.ai",
            &["PERPLEXITY_API_KEY"],
        ),
        Quirks {
            vendor: Vendor::Perplexity,
            supports_tools: false,
            supports_response_format: false,
            stream_usage: false,
            rewrite: Rewrite::Perplexity,
            ..Quirks::OPENAI
        },
    ),
    (
        Host::bearer(
            "together",
            "https://api.together.xyz",
            &["TOGETHER_API_KEY"],
        ),
        Quirks {
            path: "/v1/chat/completions",
            vendor: Vendor::Together,
            // Structured output is per model on Together.
            supports_response_format: false,
            ..Quirks::OPENAI
        },
    ),
    (
        Host::bearer(
            "huggingface",
            "https://router.huggingface.co",
            &["HUGGINGFACE_API_KEY"],
        ),
        Quirks {
            path: "/v1/chat/completions",
            supports_response_format: false,
            ..Quirks::OPENAI
        },
    ),
    (
        // A server started without `--api-key` rejects a request that
        // carries a credential.
        Host {
            base_url_env: &["LLAMACPP_API_BASE_URL"],
            keyless: true,
            ..Host::bearer(
                "llamacpp",
                "http://localhost:8080/v1",
                &["LLAMACPP_API_KEY"],
            )
        },
        Quirks {
            vendor: Vendor::LlamaCpp,
            image_tool_results: true,
            rewrite: Rewrite::LlamaCpp,
            ..Quirks::OPENAI
        },
    ),
    (
        Host::bearer("mistral", "https://api.mistral.ai", &["MISTRAL_API_KEY"]),
        Quirks {
            path: "/v1/chat/completions",
            vendor: Vendor::Mistral,
            // Mistral reports usage on its last chunk unasked.
            stream_usage: false,
            later_system: LaterSystem::Leading,
            rewrite: Rewrite::Mistral,
            ..Quirks::OPENAI
        },
    ),
    (
        Host {
            base_url_env: &["OPENROUTER_BASE_URL"],
            ..Host::bearer(
                "openrouter",
                "https://openrouter.ai/api/v1",
                &["OPENROUTER_API_KEY"],
            )
        },
        Quirks {
            vendor: Vendor::OpenRouter,
            stream_usage: false,
            native_finish_reason: true,
            response_format_with_tools: true,
            file_ids: false,
            rewrite: Rewrite::OpenRouter,
            ..Quirks::OPENAI
        },
    ),
    (
        Host {
            base_url_env: &["VENICE_BASE_URL"],
            ..Host::bearer(
                "venice",
                "https://api.venice.ai/api/v1",
                &["VENICE_API_KEY"],
            )
        },
        Quirks {
            vendor: Vendor::Venice,
            ..Quirks::OPENAI
        },
    ),
    (
        Host {
            base_url_env: &["DOUBLEWORD_BASE_URL"],
            ..Host::bearer(
                "doubleword",
                "https://api.doubleword.ai/v1",
                &["DOUBLEWORD_API_KEY"],
            )
        },
        Quirks {
            // Qwen3.5 replies report more reasoning tokens than completion
            // tokens.
            reliable_reasoning_count: false,
            ..Quirks::OPENAI
        },
    ),
    (
        Host {
            base_url_env: &["ZAI_API_BASE"],
            ..Host::bearer("zai", "https://api.z.ai/api/paas/v4", &["ZAI_API_KEY"])
        },
        ZAI,
    ),
    (
        Host {
            base_url_env: &["ZAI_API_BASE"],
            ..Host::bearer(
                "zai-coding",
                "https://api.z.ai/api/coding/paas/v4",
                &["ZAI_API_KEY"],
            )
        },
        ZAI,
    ),
    (
        Host {
            base_url_env: &["MINIMAX_API_BASE"],
            ..Host::bearer("minimax", "https://api.minimax.io/v1", &["MINIMAX_API_KEY"])
        },
        MINIMAX,
    ),
    (
        Host {
            base_url_env: &["MINIMAX_API_BASE"],
            ..Host::bearer(
                "minimax-cn",
                "https://api.minimaxi.com/v1",
                &["MINIMAX_API_KEY"],
            )
        },
        MINIMAX,
    ),
    (
        Host {
            base_url_env: &["MOONSHOT_API_BASE"],
            ..Host::bearer(
                "moonshot",
                "https://api.moonshot.ai/v1",
                &["MOONSHOT_API_KEY"],
            )
        },
        MOONSHOT,
    ),
    (
        Host {
            base_url_env: &["MOONSHOT_API_BASE"],
            ..Host::bearer(
                "moonshot-cn",
                "https://api.moonshot.cn/v1",
                &["MOONSHOT_API_KEY"],
            )
        },
        MOONSHOT,
    ),
    (
        Host {
            base_url_env: &["XIAOMI_MIMO_API_BASE"],
            ..Host::bearer(
                "xiaomimimo",
                "https://api.xiaomimimo.com/v1",
                &["XIAOMI_MIMO_API_KEY"],
            )
        },
        Quirks {
            vendor: Vendor::Mimo,
            // Every MiMo model takes its reasoning back on every assistant
            // message.
            reasoning_field: Some("reasoning_content"),
            ..Quirks::OPENAI
        },
    ),
    (
        Host::bearer(
            "cohere",
            "https://api.cohere.ai/compatibility/v1",
            &["COHERE_API_KEY"],
        ),
        Quirks {
            vendor: Vendor::Cohere,
            ..Quirks::OPENAI
        },
    ),
    (
        // A local daemon takes no credential; a proxied one takes a token.
        Host {
            keyless: true,
            ..Host::bearer("ollama", "http://localhost:11434/v1", &["OLLAMA_API_KEY"])
        },
        Quirks {
            vendor: Vendor::Ollama,
            rewrite: Rewrite::Ollama,
            ..Quirks::OPENAI
        },
    ),
    (
        Host {
            base_url_env: &["XAI_BASE_URL"],
            ..Host::bearer("xai", "https://api.x.ai", &["XAI_API_KEY"])
        },
        Quirks {
            path: "/v1/chat/completions",
            vendor: Vendor::Xai,
            ..Quirks::OPENAI
        },
    ),
];
