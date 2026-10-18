//! What Claude model ids say about a model (rig's catalog rows for
//! Anthropic, without the catalog): output limits, the reasoning each takes,
//! and the request shapes each accepts.

use crate::spec::Effort;

/// One Claude model's facts.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Claude {
    pub(crate) id: &'static str,
    pub(crate) max_output: u32,
    /// The effort levels it takes, empty when it takes a budget only.
    pub(crate) efforts: &'static [Effort],
    /// Whether it takes a thinking budget (from 1024 tokens).
    pub(crate) budget: bool,
    pub(crate) can_disable: bool,
    /// The `thinking.type` that turns its reasoning off.
    pub(crate) thinking_off: &'static str,
    /// Whether an effort goes with `thinking: {"type": "adaptive"}`.
    pub(crate) adaptive: bool,
    /// Whether it takes `role: "system"` inside `messages`.
    pub(crate) mid_system: bool,
    /// Whether it answers a forced `tool_choice` with an error.
    pub(crate) rejects_forced_tool_choice: bool,
    /// Whether its thinking binds to the tools and system prompt it was
    /// made under.
    pub(crate) binds_context: bool,
    /// Whether it fixes its sampling: no `temperature`, `top_p` or `top_k`.
    pub(crate) fixed_sampling: bool,
}

const LOW_TO_HIGH: &[Effort] = &[Effort::Low, Effort::Medium, Effort::High];
const LOW_TO_MAX: &[Effort] = &[Effort::Low, Effort::Medium, Effort::High, Effort::Max];
const EVERY_LEVEL: &[Effort] = &[
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::XHigh,
    Effort::Max,
];

/// A Claude 4 model that thinks by budget only.
const fn budgeted(id: &'static str, max_output: u32) -> Claude {
    Claude {
        id,
        max_output,
        efforts: &[],
        budget: true,
        can_disable: true,
        thinking_off: "disabled",
        adaptive: false,
        mid_system: false,
        rejects_forced_tool_choice: false,
        binds_context: false,
        fixed_sampling: false,
    }
}

/// A model with adaptive thinking at every level and fixed sampling.
const fn adaptive(id: &'static str) -> Claude {
    Claude {
        id,
        max_output: 128_000,
        efforts: EVERY_LEVEL,
        budget: false,
        can_disable: true,
        thinking_off: "disabled",
        adaptive: true,
        mid_system: false,
        rejects_forced_tool_choice: false,
        binds_context: false,
        fixed_sampling: true,
    }
}

/// The newest models: they keep system messages in place, and the bound
/// ones take no forced tool choice.
const fn current(id: &'static str, binds: bool) -> Claude {
    Claude {
        mid_system: true,
        rejects_forced_tool_choice: binds,
        binds_context: binds,
        ..adaptive(id)
    }
}

const MODELS: &[Claude] = &[
    Claude {
        can_disable: false,
        ..current("claude-fable-5", false)
    },
    Claude {
        can_disable: false,
        ..current("claude-fable-5-1", true)
    },
    Claude {
        can_disable: false,
        ..current("claude-opus-5-5", true)
    },
    Claude {
        binds_context: true,
        ..current("claude-opus-5", false)
    },
    Claude {
        thinking_off: "between_tools",
        ..current("claude-sonnet-5-5", true)
    },
    adaptive("claude-sonnet-5"),
    current("claude-opus-4-8", false),
    adaptive("claude-opus-4-7"),
    Claude {
        efforts: LOW_TO_MAX,
        budget: true,
        fixed_sampling: false,
        ..adaptive("claude-opus-4-6")
    },
    Claude {
        efforts: LOW_TO_MAX,
        budget: true,
        fixed_sampling: false,
        ..adaptive("claude-sonnet-4-6")
    },
    Claude {
        efforts: LOW_TO_HIGH,
        ..budgeted("claude-opus-4-5", 64_000)
    },
    budgeted("claude-sonnet-4-5", 64_000),
    budgeted("claude-haiku-4-5", 64_000),
    budgeted("claude-opus-4-1", 32_000),
    budgeted("claude-opus-4-0", 32_000),
    budgeted("claude-opus-4-20250514", 32_000),
    budgeted("claude-sonnet-4-0", 64_000),
    budgeted("claude-sonnet-4-20250514", 64_000),
];

fn exact(model: &str) -> Option<&'static Claude> {
    MODELS.iter().find(|claude| claude.id == model)
}

/// The facts of Anthropic's `model`, or of the model a dated snapshot of it
/// (`<id>-YYYYMMDD`, or any suffix from `-20`) names. `None` for a model
/// this table does not list, which gets the newest models' behaviour.
pub(crate) fn claude(model: &str) -> Option<&'static Claude> {
    exact(model).or_else(|| {
        model
            .match_indices("-20")
            .filter_map(|(at, _)| exact(model.get(..at)?))
            .last()
    })
}

/// [`claude`] for `model` however a serving API spells it: Anthropic's
/// `claude-opus-5-5`, `OpenRouter`'s `anthropic/claude-opus-5.5` or
/// Bedrock's `us.anthropic.claude-opus-5-5-v1:0`.
pub(crate) fn claude_spelled(model: &str) -> Option<&'static Claude> {
    let model = model
        .rsplit_once("anthropic.")
        .map_or(model, |(_, rest)| rest);
    let model = model.strip_prefix("anthropic/").unwrap_or(model);
    let model = model.split_once("-v1:").map_or(model, |(id, _)| id);
    claude(&model.replace('.', "-"))
}

/// The `max_tokens` Anthropic's `model` defaults to: its published output
/// limit, or 64000 for an unlisted Claude 4 Opus, Sonnet or Haiku 4.5 id.
/// `None` for any other model, which must set it.
pub fn default_max_tokens(model: &str) -> Option<u32> {
    match claude(model) {
        Some(claude) => Some(claude.max_output),
        None => ["claude-opus-4", "claude-sonnet-4", "claude-haiku-4-5"]
            .iter()
            .any(|prefix| model.starts_with(prefix))
            .then_some(64_000),
    }
}

/// Whether Anthropic's `model` answers a forced tool choice (`Required` or
/// a named tool) with an error. Such a request is sent with the choice left
/// to the model.
pub fn rejects_forced_tool_choice(model: &str) -> bool {
    claude(model).is_some_and(|claude| claude.rejects_forced_tool_choice)
}

/// Whether `model` takes `role: "system"` inside `messages`; one this table
/// does not list takes system text only in `system`.
pub(crate) fn takes_mid_conversation_system(model: &str) -> bool {
    claude(model).is_some_and(|claude| claude.mid_system)
}

/// Whether the Claude `model`, however spelled, binds its thinking blocks
/// to the request's tools and system prompt.
pub(crate) fn binds_context(model: &str) -> bool {
    claude_spelled(model).is_some_and(|claude| claude.binds_context)
}
