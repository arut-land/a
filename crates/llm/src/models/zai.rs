//! Z.AI's GLM model ids.

use super::{Reasoning, Thinking};

/// GLM vision models carry a `v` after their version: `glm-4.5v`.
pub fn reads_images(model: &str) -> bool {
    model.strip_prefix("glm-").is_some_and(|rest| {
        rest.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.')
            .starts_with('v')
    })
}

/// GLM models turn thinking off; GLM 4 takes no level, the rest `DeepSeek`'s
/// `low`, `high` and `max`.
pub fn reasoning(model: &str) -> Reasoning {
    if model.starts_with("glm-4") {
        Reasoning::Takes(Thinking::levels(&[]).disabling())
    } else {
        super::deepseek::REASONING
    }
}
