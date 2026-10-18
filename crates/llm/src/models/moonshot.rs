//! Moonshot's Kimi model ids.

use super::{Reasoning, Thinking, name};
use crate::Effort;

pub fn reads_images(model: &str) -> bool {
    !(model == "kimi-k2"
        || model.starts_with("kimi-k2-")
        || (model.starts_with("moonshot-v1") && !model.contains("vision")))
}

/// Kimi K2.6 turns thinking off; Kimi K3 takes `low`, `high` and `max`.
pub fn reasoning(model: &str) -> Reasoning {
    let name = name(model);
    if name.starts_with("kimi-k2.6") {
        Reasoning::Takes(Thinking::levels(&[]).disabling())
    } else if name.starts_with("kimi-k3") {
        Reasoning::Takes(Thinking::levels(&[Effort::Low, Effort::High, Effort::Max]))
    } else {
        Reasoning::None
    }
}

/// Whether the model asks for its reasoning back on every assistant
/// message, behind any gateway: Kimi K3, and `OpenRouter`'s
/// `moonshotai/kimi-k2.6`.
pub fn replays_reasoning(model: &str) -> bool {
    let name = name(model);
    name == "kimi-k3" || name.starts_with("kimi-k3-") || model == "moonshotai/kimi-k2.6"
}
