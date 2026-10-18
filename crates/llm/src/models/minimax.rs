//! `MiniMax` model ids.

use super::{LOW_TO_MAX, Reasoning, Thinking};

pub fn reads_images(model: &str) -> bool {
    !model.to_ascii_lowercase().starts_with("minimax-m2")
}

/// M3 turns thinking off; M3.1 takes an effort from `low` and cannot.
pub fn reasoning(model: &str) -> Reasoning {
    let model = model.to_ascii_lowercase();
    if model.starts_with("minimax-m3.1") {
        Reasoning::Takes(Thinking::levels(LOW_TO_MAX))
    } else if model.starts_with("minimax-m3") {
        Reasoning::Takes(Thinking::levels(&[]).disabling())
    } else {
        Reasoning::None
    }
}
