//! Cohere model ids.

use super::{Reasoning, Thinking};
use crate::Effort;

pub fn reads_images(model: &str) -> bool {
    model.contains("vision")
}

/// The Compatibility API turns reasoning off, and takes only `high`.
pub const REASONING: Reasoning = Reasoning::Takes(Thinking::levels(&[Effort::High]).disabling());
