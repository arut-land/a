//! Cohere model ids.

pub fn reads_images(model: &str) -> bool {
    model.contains("vision")
}
