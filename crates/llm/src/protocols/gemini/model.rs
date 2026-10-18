//! What Gemini model ids say about a model: rig's id rules, and its catalog
//! rows for thinking written as family rules.

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
pub(crate) fn major(model: &str) -> Option<u32> {
    let model = id(model);
    let rest = model.strip_prefix("gemini-")?;
    let rest = rest.strip_prefix("live-").unwrap_or(rest);
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    Some(rest[..end].parse().unwrap_or(u32::MAX))
}

/// Gemini 3 and later validate the thought signatures of function calls.
pub(crate) fn validates_signatures(model: &str) -> bool {
    major(model).is_some_and(|major| major >= 3)
}

/// Whether calls and their responses carry ids: Claude, gpt-oss, and
/// Gemini 3 or later.
pub(crate) fn takes_call_ids(model: &str) -> bool {
    let name = id(model);
    name.starts_with("claude-") || name.starts_with("gpt-oss-") || validates_signatures(model)
}

/// Whether a function response may carry images: from Gemini 3 on, and on
/// any model that is not Gemini.
pub(crate) fn reads_result_images(model: &str) -> bool {
    major(model).is_none_or(|major| major >= 3)
}

/// The thinking a model takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Thinking {
    /// These levels (Gemini 3), and whether level `minimal` turns thinking
    /// off (Gemma 4). A model that thinks but takes no level has none.
    Levels {
        levels: &'static [Effort],
        can_disable: bool,
    },
    /// A token budget in `min..=max` (Gemini 2.5), and whether `0` turns it
    /// off.
    Budget {
        min: u32,
        max: u32,
        can_disable: bool,
    },
    None,
    /// An id no rule knows.
    Unknown,
}

const LOW_HIGH: Thinking = levels(&[Effort::Low, Effort::High]);
const MINIMAL_HIGH: Thinking = levels(&[Effort::Minimal, Effort::High]);
const LOW_TO_HIGH: Thinking = levels(&[Effort::Low, Effort::Medium, Effort::High]);
const MINIMAL_TO_HIGH: Thinking =
    levels(&[Effort::Minimal, Effort::Low, Effort::Medium, Effort::High]);
/// A model that thinks and takes no level.
const NO_LEVEL: Thinking = levels(&[]);

const fn levels(levels: &'static [Effort]) -> Thinking {
    Thinking::Levels {
        levels,
        can_disable: false,
    }
}

/// Families by id prefix, the more specific first.
const FAMILIES: &[(&str, Thinking)] = &[
    ("gemini-2.5-flash-image", Thinking::None),
    ("gemini-2.5-flash-preview-tts", Thinking::None),
    ("gemini-2.5-pro-preview-tts", Thinking::None),
    ("gemini-2.5-computer-use", NO_LEVEL),
    (
        "gemini-2.5-flash-lite",
        Thinking::Budget {
            min: 512,
            max: 24_576,
            can_disable: true,
        },
    ),
    (
        "gemini-2.5-flash",
        Thinking::Budget {
            min: 0,
            max: 24_576,
            can_disable: true,
        },
    ),
    (
        "gemini-2.5-pro",
        Thinking::Budget {
            min: 128,
            max: 32_768,
            can_disable: false,
        },
    ),
    ("gemini-3-pro", LOW_HIGH),
    ("gemini-3-flash-preview", MINIMAL_TO_HIGH),
    ("gemini-3.1-pro", LOW_TO_HIGH),
    ("gemini-3.1-flash-lite-image", MINIMAL_HIGH),
    ("gemini-3.1-flash-lite", MINIMAL_TO_HIGH),
    ("gemini-3.1-flash-image", MINIMAL_HIGH),
    ("gemini-3.1-flash-live", MINIMAL_TO_HIGH),
    ("gemini-3.1-flash-tts", NO_LEVEL),
    ("gemini-3.5-live-translate", Thinking::None),
    ("gemini-3.5-flash", MINIMAL_TO_HIGH),
    ("gemini-3.6-flash", MINIMAL_TO_HIGH),
    ("gemini-3.7-flash", LOW_TO_HIGH),
    ("gemini-3.8-flash", LOW_TO_HIGH),
    ("gemini-flash-lite-latest", MINIMAL_TO_HIGH),
    ("gemini-flash-latest", LOW_TO_HIGH),
    ("gemini-omni-flash", NO_LEVEL),
    (
        "gemma-4-",
        Thinking::Levels {
            levels: &[Effort::High],
            can_disable: true,
        },
    ),
];

/// The thinking `model` takes: its family's, else none before Gemini 2.5,
/// else unknown.
pub(crate) fn thinking(model: &str) -> Thinking {
    let name = id(model);
    if let Some((_, thinking)) = FAMILIES.iter().find(|(prefix, _)| name.starts_with(prefix)) {
        return *thinking;
    }
    match major(&name) {
        Some(major) if major < 2 || name.starts_with("gemini-2.0") => Thinking::None,
        _ => Thinking::Unknown,
    }
}
