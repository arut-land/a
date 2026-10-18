//! Perplexity's Sonar model ids.

use super::{LOW_TO_HIGH, Reasoning, Thinking};

/// Only Sonar Deep Research takes an effort, `low` to `high`.
pub fn reasoning(model: &str) -> Reasoning {
    if model.starts_with("sonar-deep-research") {
        Reasoning::Takes(Thinking::levels(LOW_TO_HIGH))
    } else {
        Reasoning::None
    }
}
