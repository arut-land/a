//! Groq-served model ids.

use super::{LOW_TO_HIGH, Reasoning, Thinking};

/// Groq reads images on its Llama 4 models.
pub fn reads_images(model: &str) -> bool {
    model.contains("llama-4")
}

/// GPT-OSS takes `low` to `high` and cannot turn reasoning off; the rest
/// are unknown.
pub fn reasoning(model: &str) -> Reasoning {
    if model.contains("gpt-oss") {
        Reasoning::Takes(Thinking::levels(LOW_TO_HIGH))
    } else {
        Reasoning::Unknown
    }
}
