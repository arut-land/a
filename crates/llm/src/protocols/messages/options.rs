//! How each dialect answers the request's generation settings: the fields
//! it merges into the body. A setting a model or a dialect cannot take is
//! left out rather than failing the request (where rig refuses it):
//!
//! - Anthropic reads its effort levels, thinking budget and sampling from
//!   the model ([`crate::models::anthropic`]); an unlisted model gets the newest
//!   models' shapes. `top_p` goes only without `temperature`, and a model
//!   that fixes its sampling takes neither, nor `top_k`.
//! - `MiniMax` turns thinking off on M3 and takes an effort on M3.1; `MiMo`
//!   turns thinking off; Z.AI, Moonshot and unknown gateways take no
//!   reasoning setting.
//! - Stop sequences reach Anthropic and `MiMo`; `MiniMax` documents them as
//!   ignored and the rest are unverified. No dialect takes a seed.

use serde_json::{Map, Value, json};

use super::{Dialect, Quirks};
use crate::models::anthropic::{self, Claude, claude_spelled};
use crate::models::{Sampling, minimax};
use crate::spec::{Effort, Generation};

/// The body fields `generation` maps to on `quirks`' dialect for `model`,
/// with `max_tokens` the request's output cap.
pub(super) fn generation(
    quirks: Quirks,
    model: &str,
    generation: &Generation,
    max_tokens: u32,
) -> Map<String, Value> {
    let claude = claude_spelled(model);
    let reasoning = match quirks.dialect {
        Dialect::Anthropic => claude_reasoning(claude, generation, max_tokens),
        Dialect::MiniMax => minimax_reasoning(minimax::reasoning(model), generation.reasoning),
        Dialect::MiMo => (generation.reasoning == Some(Effort::None))
            .then(|| json!({"thinking": {"type": "disabled"}})),
        Dialect::ZAi | Dialect::Moonshot | Dialect::Other => None,
    };
    let mut fields = match reasoning {
        Some(Value::Object(fields)) => fields,
        _ => Map::new(),
    };
    let mut set = |key: &str, value: Value| {
        fields.insert(key.to_owned(), value);
    };
    let fixed = quirks.dialect == Dialect::Anthropic
        && claude.is_some_and(|claude| claude.sampling == Sampling::Fixed);
    if let Some(temperature) = generation.temperature.filter(|_| !fixed) {
        set("temperature", json!(temperature));
    }
    let top_p = generation.top_p.filter(|top_p| match quirks.dialect {
        Dialect::Anthropic => !fixed && generation.temperature.is_none(),
        Dialect::MiniMax => true,
        Dialect::MiMo => (0.01..=1.0).contains(top_p),
        Dialect::ZAi | Dialect::Moonshot | Dialect::Other => false,
    });
    if let Some(top_p) = top_p {
        set("top_p", json!(top_p));
    }
    if let Some(top_k) = generation
        .top_k
        .filter(|_| quirks.dialect == Dialect::Anthropic && !fixed)
    {
        set("top_k", json!(top_k.get()));
    }
    if !generation.stop.is_empty() && matches!(quirks.dialect, Dialect::Anthropic | Dialect::MiMo) {
        set("stop_sequences", json!(generation.stop));
    }
    fields
}

/// Whether the dialect takes `disable_parallel_tool_use` on its tool choice.
pub(super) const fn takes_parallel_off(dialect: Dialect) -> bool {
    matches!(dialect, Dialect::Anthropic | Dialect::MiMo)
}

/// The `thinking` and `output_config` the Claude model `claude` takes for
/// the request's reasoning (`None`: a model the table does not list, which
/// gets the newest models' shapes). Effort `none` turns thinking off; a
/// budget goes where the model takes one and an effort elsewhere.
fn claude_reasoning(
    claude: Option<&Claude>,
    generation: &Generation,
    max_tokens: u32,
) -> Option<Value> {
    let effort = generation.reasoning;
    if effort == Some(Effort::None) {
        return match claude {
            Some(claude) if !claude.thinking.can_disable => None,
            _ => {
                let off = claude.map_or("disabled", |claude| claude.thinking_off);
                Some(json!({"thinking": {"type": off}}))
            }
        };
    }
    let budget = generation
        .reasoning_budget
        .map(std::num::NonZeroU32::get)
        .filter(|tokens| {
            claude
                .map_or(Some(anthropic::BUDGET), |claude| claude.thinking.budget)
                .is_some_and(|budget| budget.contains(*tokens))
                && *tokens < max_tokens
        });
    if let Some(tokens) = budget {
        return Some(json!({"thinking": {"type": "enabled", "budget_tokens": tokens}}));
    }
    let effort = effort.filter(|effort| *effort != Effort::Minimal)?;
    match claude {
        Some(claude) if !claude.thinking.takes(effort) => None,
        Some(claude) if !claude.adaptive => {
            Some(json!({"output_config": {"effort": effort.as_str()}}))
        }
        _ => Some(json!({
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": effort.as_str()},
        })),
    }
}

/// `MiniMax`'s reasoning: off where the model turns it off, else an effort
/// it takes.
fn minimax_reasoning(model: crate::models::Reasoning, effort: Option<Effort>) -> Option<Value> {
    match effort? {
        Effort::None if model.can_disable() => Some(json!({"thinking": {"type": "disabled"}})),
        effort if model.takes(effort) => {
            Some(json!({"output_config": {"effort": effort.as_str()}}))
        }
        _ => None,
    }
}
