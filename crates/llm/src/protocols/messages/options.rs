//! How each dialect answers the request's generation settings: the fields
//! it merges into the body. A setting a model or a dialect cannot take is
//! left out rather than failing the request:
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
use crate::models::anthropic::{self, Claude, ThinkingOff, claude_spelled};
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
    let anthropic = quirks.dialect == Dialect::Anthropic;
    let fixed = anthropic && claude.is_some_and(|claude| claude.sampling == Sampling::Fixed);
    let top_p = generation.top_p.filter(|top_p| match quirks.dialect {
        Dialect::Anthropic => !fixed && generation.temperature.is_none(),
        Dialect::MiniMax => true,
        Dialect::MiMo => (0.01..=1.0).contains(top_p),
        Dialect::ZAi | Dialect::Moonshot | Dialect::Other => false,
    });
    let temperature = generation.temperature.filter(|_| !fixed);
    let top_k = generation.top_k.filter(|_| anthropic && !fixed);
    let stop = !generation.stop.is_empty() && quirks.dialect.takes_parallel_off();
    let mut fields = match reasoning {
        Some(Value::Object(fields)) => fields,
        _ => Map::new(),
    };
    for (key, value) in [
        ("temperature", temperature.map(Value::from)),
        ("top_p", top_p.map(Value::from)),
        ("top_k", top_k.map(|top_k| top_k.get().into())),
        ("stop_sequences", stop.then(|| json!(generation.stop))),
    ] {
        fields.extend(value.map(|value| (key.to_owned(), value)));
    }
    fields
}

impl Dialect {
    /// Whether it takes stop sequences, and `disable_parallel_tool_use` on
    /// its tool choice.
    pub(super) const fn takes_parallel_off(self) -> bool {
        matches!(self, Self::Anthropic | Self::MiMo)
    }
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
        let off = |claude: &Claude| claude.thinking.can_disable.then_some(claude.thinking_off);
        let off = claude.map_or(Some(ThinkingOff::Disabled), off);
        return off.map(|off| json!({"thinking": {"type": off}}));
    }
    let budgets = claude.map_or(Some(anthropic::BUDGET), |claude| claude.thinking.budget);
    let fits = |tokens: &u32| budgets.is_some_and(|b| b.contains(*tokens)) && *tokens < max_tokens;
    let budget = generation.reasoning_budget.map(std::num::NonZeroU32::get);
    if let Some(tokens) = budget.filter(fits) {
        return Some(json!({"thinking": {"type": "enabled", "budget_tokens": tokens}}));
    }
    let effort = effort.filter(|effort| *effort != Effort::Minimal)?;
    let output = json!({"effort": effort.as_str()});
    match claude {
        Some(claude) if !claude.thinking.takes(effort) => None,
        Some(claude) if !claude.adaptive => Some(json!({"output_config": output})),
        _ => Some(json!({"thinking": {"type": "adaptive"}, "output_config": output})),
    }
}

/// `MiniMax`'s reasoning: off where the model turns it off, else an effort
/// it takes.
fn minimax_reasoning(model: crate::models::Reasoning, effort: Option<Effort>) -> Option<Value> {
    match effort? {
        Effort::None if model.can_disable() => Some(json!({"thinking": {"type": "disabled"}})),
        effort if model.takes(effort) => Some(json!({"output_config": {"effort": effort}})),
        _ => None,
    }
}
