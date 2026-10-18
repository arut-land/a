//! Mistral model ids.

/// Codestral and Devstral are text models.
pub fn reads_images(model: &str) -> bool {
    !(model.starts_with("codestral") || model.starts_with("devstral"))
}
