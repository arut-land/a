//! Xiaomi `MiMo` model ids.

pub fn reads_images(model: &str) -> bool {
    !((model.starts_with("mimo-v2-") && !model.contains("omni"))
        || model.starts_with("mimo-v2.5-pro"))
}
