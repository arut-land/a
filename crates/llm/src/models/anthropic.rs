//! Claude model ids (rig's catalog rows for Anthropic, as a table): output
//! limits, the reasoning each takes, and the request shapes each accepts.

use super::{LOW_TO_HIGH, LOW_TO_MAX, Sampling, Thinking, has_prefix};
use crate::spec::Effort;

/// One Claude model's facts.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Claude {
    pub id: &'static str,
    pub max_output: u32,
    pub thinking: Thinking,
    /// The `thinking.type` that turns its reasoning off.
    pub thinking_off: ThinkingOff,
    /// Whether an effort goes with `thinking: {"type": "adaptive"}`.
    pub adaptive: bool,
    /// Whether it takes `role: "system"` inside `messages`.
    pub mid_system: bool,
    /// Whether it answers a forced `tool_choice` with an error.
    pub rejects_forced_tool_choice: bool,
    /// Whether its thinking binds to the tools and system prompt it was
    /// made under.
    pub binds_context: bool,
    pub sampling: Sampling,
}

/// A `thinking.type` that turns reasoning off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingOff {
    Disabled,
    BetweenTools,
}

/// The thinking budgets Claude takes: from 1024 tokens, below the
/// request's `max_tokens`.
pub const BUDGET: super::Budget = super::Budget {
    min: 1024,
    max: u32::MAX,
};

const LOW_TO_MAX_NO_XHIGH: &[Effort] = &[Effort::Low, Effort::Medium, Effort::High, Effort::Max];

/// A Claude 4 model that thinks by budget only.
const fn budgeted(id: &'static str, max_output: u32) -> Claude {
    Claude {
        id,
        max_output,
        thinking: Thinking::budget(BUDGET.min, BUDGET.max, true),
        thinking_off: ThinkingOff::Disabled,
        adaptive: false,
        mid_system: false,
        rejects_forced_tool_choice: false,
        binds_context: false,
        sampling: Sampling::Free,
    }
}

/// A model with adaptive thinking at every level and fixed sampling.
const fn adaptive(id: &'static str) -> Claude {
    Claude {
        id,
        max_output: 128_000,
        thinking: Thinking::levels(LOW_TO_MAX).disabling(),
        thinking_off: ThinkingOff::Disabled,
        adaptive: true,
        mid_system: false,
        rejects_forced_tool_choice: false,
        binds_context: false,
        sampling: Sampling::Fixed,
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

/// `claude` with reasoning it cannot turn off.
const fn always_thinks(claude: Claude) -> Claude {
    Claude {
        thinking: Thinking {
            can_disable: false,
            ..claude.thinking
        },
        ..claude
    }
}

/// An adaptive model that also takes a budget and free sampling.
const fn hybrid(id: &'static str) -> Claude {
    Claude {
        thinking: Thinking {
            budget: Some(BUDGET),
            ..Thinking::levels(LOW_TO_MAX_NO_XHIGH).disabling()
        },
        sampling: Sampling::Free,
        ..adaptive(id)
    }
}

const MODELS: &[Claude] = &[
    always_thinks(current("claude-fable-5", false)),
    always_thinks(current("claude-fable-5-1", true)),
    always_thinks(current("claude-opus-5-5", true)),
    Claude {
        binds_context: true,
        ..current("claude-opus-5", false)
    },
    Claude {
        thinking_off: ThinkingOff::BetweenTools,
        ..current("claude-sonnet-5-5", true)
    },
    adaptive("claude-sonnet-5"),
    current("claude-opus-4-8", false),
    adaptive("claude-opus-4-7"),
    hybrid("claude-opus-4-6"),
    hybrid("claude-sonnet-4-6"),
    Claude {
        thinking: Thinking {
            efforts: LOW_TO_HIGH,
            ..Thinking::budget(BUDGET.min, BUDGET.max, true)
        },
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
pub fn claude(model: &str) -> Option<&'static Claude> {
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
pub fn claude_spelled(model: &str) -> Option<&'static Claude> {
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
        None => has_prefix(
            &["claude-opus-4", "claude-sonnet-4", "claude-haiku-4-5"],
            model,
        )
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
pub fn takes_mid_conversation_system(model: &str) -> bool {
    claude(model).is_some_and(|claude| claude.mid_system)
}

/// Whether the Claude `model`, however spelled, binds its thinking blocks
/// to the request's tools and system prompt.
pub fn binds_context(model: &str) -> bool {
    claude_spelled(model).is_some_and(|claude| claude.binds_context)
}
