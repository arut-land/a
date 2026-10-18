//! Gemini model ids: what they say about versions, call ids and thinking,
//! as family rules.

use super::{Reasoning, Thinking, by_prefix};
use crate::Effort;

/// The model's id, lowercase, past a `models/` prefix.
fn id(model: &str) -> String {
    let model = model.to_ascii_lowercase();
    match model.strip_prefix("models/") {
        Some(rest) => rest.to_owned(),
        None => model,
    }
}

/// The major version of a Gemini id: `gemini-<major>…` or
/// `gemini-live-<major>…`; `None` for an id that is not Gemini's. An id that
/// names no version (`gemini-flash-latest`) is the current generation.
pub fn major(model: &str) -> Option<u32> {
    let model = id(model);
    let rest = model.strip_prefix("gemini-")?;
    let rest = rest.strip_prefix("live-").unwrap_or(rest);
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    Some(rest[..end].parse().unwrap_or(u32::MAX))
}

/// Gemini 3 and later validate the thought signatures of function calls.
pub fn validates_signatures(model: &str) -> bool {
    major(model).is_some_and(|major| major >= 3)
}

/// Whether calls and their responses carry ids: Claude, gpt-oss, and
/// Gemini 3 or later.
pub fn takes_call_ids(model: &str) -> bool {
    let name = id(model);
    name.starts_with("claude-") || name.starts_with("gpt-oss-") || validates_signatures(model)
}

/// Whether a function response may carry images: from Gemini 3 on, and on
/// any model that is not Gemini.
pub fn reads_result_images(model: &str) -> bool {
    major(model).is_none_or(|major| major >= 3)
}

const LOW_HIGH: Reasoning = levels(&[Effort::Low, Effort::High]);
const MINIMAL_HIGH: Reasoning = levels(&[Effort::Minimal, Effort::High]);
const LOW_TO_HIGH: Reasoning = levels(super::LOW_TO_HIGH);
const MINIMAL_TO_HIGH: Reasoning =
    levels(&[Effort::Minimal, Effort::Low, Effort::Medium, Effort::High]);
/// A model that thinks and takes no level.
const NO_LEVEL: Reasoning = levels(&[]);

const fn levels(levels: &'static [Effort]) -> Reasoning {
    Reasoning::Takes(Thinking::levels(levels))
}

const fn budget(min: u32, max: u32, can_disable: bool) -> Reasoning {
    Reasoning::Takes(Thinking::budget(min, max, can_disable))
}

/// Families by id prefix, the more specific first. Level `minimal` turns
/// a disabling level model's thinking off (Gemma 4), budget `0` a budget
/// model's (Gemini 2.5).
const FAMILIES: &[(&str, Reasoning)] = &[
    ("gemini-2.5-flash-image", Reasoning::None),
    ("gemini-2.5-flash-preview-tts", Reasoning::None),
    ("gemini-2.5-pro-preview-tts", Reasoning::None),
    ("gemini-2.5-computer-use", NO_LEVEL),
    ("gemini-2.5-flash-lite", budget(512, 24_576, true)),
    ("gemini-2.5-flash", budget(0, 24_576, true)),
    ("gemini-2.5-pro", budget(128, 32_768, false)),
    ("gemini-3-pro", LOW_HIGH),
    ("gemini-3-flash-preview", MINIMAL_TO_HIGH),
    ("gemini-3.1-pro", LOW_TO_HIGH),
    ("gemini-3.1-flash-lite-image", MINIMAL_HIGH),
    ("gemini-3.1-flash-lite", MINIMAL_TO_HIGH),
    ("gemini-3.1-flash-image", MINIMAL_HIGH),
    ("gemini-3.1-flash-live", MINIMAL_TO_HIGH),
    ("gemini-3.1-flash-tts", NO_LEVEL),
    ("gemini-3.5-live-translate", Reasoning::None),
    ("gemini-3.5-flash", MINIMAL_TO_HIGH),
    ("gemini-3.6-flash", MINIMAL_TO_HIGH),
    ("gemini-3.7-flash", LOW_TO_HIGH),
    ("gemini-3.8-flash", LOW_TO_HIGH),
    ("gemini-flash-lite-latest", MINIMAL_TO_HIGH),
    ("gemini-flash-latest", LOW_TO_HIGH),
    ("gemini-omni-flash", NO_LEVEL),
    (
        "gemma-4-",
        Reasoning::Takes(Thinking::levels(&[Effort::High]).disabling()),
    ),
];

/// The thinking `model` takes: its family's, else none before Gemini 2.5,
/// else unknown.
pub fn reasoning(model: &str) -> Reasoning {
    let name = id(model);
    by_prefix(FAMILIES, &name).unwrap_or_else(|| match major(&name) {
        Some(major) if major < 2 || name.starts_with("gemini-2.0") => Reasoning::None,
        _ => Reasoning::Unknown,
    })
}
