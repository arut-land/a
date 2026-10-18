//! Grok model ids.

use super::{Reasoning, Thinking, has_prefix};
use crate::Effort;

/// Grok 2 reads images only in its vision models; Grok 3 and Grok Code not.
pub fn reads_images(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    !((model.starts_with("grok-2") && !model.contains("vision"))
        || has_prefix(&["grok-3", "grok-code"], &model))
}

/// A Grok model reasons unless its id says otherwise.
pub fn reasons(model: &str) -> bool {
    !model.contains("non-reasoning")
}

/// A reasoning Grok model takes `low` to `xhigh` and cannot turn reasoning
/// off.
pub const REASONING: Reasoning = Reasoning::Takes(Thinking::levels(&[
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::XHigh,
]));
