//! What model ids say about models: the few facts request shaping needs,
//! as id rules and const tables rather than a catalog. Each vendor's rules
//! live in its module; protocols ask here, and keep only how their wire
//! spells an option.

pub mod anthropic;
pub mod cohere;
pub mod deepseek;
pub mod google;
pub mod groq;
pub mod minimax;
pub mod mistral;
pub mod moonshot;
pub mod openai;
pub mod perplexity;
pub mod xai;
pub mod xiaomi;
pub mod zai;

use crate::spec::Effort;

/// The model's own name, past a `vendor/` prefix.
pub fn name(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// The value of the first row whose prefix starts `id`.
pub fn by_prefix<T: Copy>(table: &[(&str, T)], id: &str) -> Option<T> {
    table
        .iter()
        .find_map(|(prefix, value)| id.starts_with(prefix).then_some(*value))
}

/// Whether `id` starts with one of `prefixes`.
pub fn has_prefix(prefixes: &[&str], id: &str) -> bool {
    prefixes.iter().any(|prefix| id.starts_with(prefix))
}

/// Input context windows, in tokens, by id prefix: the first row whose
/// prefix starts the model's own name wins, so a longer prefix goes first.
/// An id no row knows has no window to measure a request against.
const WINDOWS: &[(&str, u32)] = &[
    ("claude-", 200_000),
    ("gpt-4.1", 1_047_576),
    ("gpt-4o", 128_000),
    ("gpt-5-chat", 128_000),
    ("gpt-5", 400_000),
    ("o1-mini", 128_000),
    ("o1", 200_000),
    ("o3", 200_000),
    ("o4-mini", 200_000),
    ("gemini-1.5-pro", 2_097_152),
    ("gemini-", 1_048_576),
    ("deepseek-", 128_000),
    ("grok-4", 256_000),
];

/// The context window of `model`, past any `vendor/` prefix, if known.
pub fn context_window(model: &str) -> Option<u32> {
    by_prefix(WINDOWS, name(model))
}

/// A thinking budget's range, in tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    pub min: u32,
    pub max: u32,
}

impl Budget {
    pub const fn contains(self, tokens: u32) -> bool {
        self.min <= tokens && tokens <= self.max
    }
}

/// The reasoning settings a model that reasons takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Thinking {
    /// The effort levels it takes; empty for a model that takes none.
    pub efforts: &'static [Effort],
    /// The thinking budget it takes, if any.
    pub budget: Option<Budget>,
    /// Whether its reasoning can be turned off.
    pub can_disable: bool,
}

impl Thinking {
    /// A model taking `efforts` and no budget, which cannot turn reasoning
    /// off.
    pub const fn levels(efforts: &'static [Effort]) -> Self {
        Self {
            efforts,
            budget: None,
            can_disable: false,
        }
    }

    /// A model taking a budget in `min..=max` and no level.
    pub const fn budget(min: u32, max: u32, can_disable: bool) -> Self {
        Self {
            efforts: &[],
            budget: Some(Budget { min, max }),
            can_disable,
        }
    }

    #[must_use]
    pub const fn disabling(self) -> Self {
        Self {
            can_disable: true,
            ..self
        }
    }

    pub fn takes(&self, effort: Effort) -> bool {
        self.efforts.contains(&effort)
    }
}

/// What a model's id says about its reasoning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reasoning {
    Takes(Thinking),
    /// It does not reason.
    None,
    /// An id no rule knows.
    Unknown,
}

impl Reasoning {
    pub const fn can_disable(self) -> bool {
        matches!(
            self,
            Self::Takes(Thinking {
                can_disable: true,
                ..
            })
        )
    }

    pub fn takes(self, effort: Effort) -> bool {
        matches!(self, Self::Takes(thinking) if thinking.takes(effort))
    }
}

/// `low` to `high`.
pub const LOW_TO_HIGH: &[Effort] = &[Effort::Low, Effort::Medium, Effort::High];
/// `low` to `max`, `xhigh` included.
pub const LOW_TO_MAX: &[Effort] = &[
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::XHigh,
    Effort::Max,
];

/// When a model takes `temperature` and `top_p`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sampling {
    Free,
    /// Never: the model fixes its sampling.
    Fixed,
    /// Only with reasoning turned off.
    WithoutReasoning,
}

impl Sampling {
    /// Whether sampling goes with reasoning at `effort` (`None` for the
    /// provider's default).
    pub fn takes(self, effort: Option<Effort>) -> bool {
        match self {
            Self::Free => true,
            Self::Fixed => false,
            Self::WithoutReasoning => effort == Some(Effort::None),
        }
    }
}

/// Whose models an id names, for the facts that differ by vendor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Maker {
    OpenAi,
    Groq,
    Mistral,
    Xai,
    Cohere,
    Zai,
    Moonshot,
    MiniMax,
    Xiaomi,
    /// Text-only APIs: `DeepSeek`'s and Mira's.
    TextOnly,
}

impl Maker {
    /// The maker a vendor segment names (an `OpenRouter` id's upstream);
    /// `None` for one no rule knows.
    pub fn named(vendor: &str) -> Option<Self> {
        Some(match vendor {
            "openai" | "azure.openai" => Self::OpenAi,
            "groq" => Self::Groq,
            "mistral" | "mistralai" => Self::Mistral,
            "xai" | "x-ai" => Self::Xai,
            "cohere" => Self::Cohere,
            "zai" | "z-ai" => Self::Zai,
            "moonshot" | "moonshotai" => Self::Moonshot,
            "minimax" => Self::MiniMax,
            "xiaomimimo" | "xiaomi" => Self::Xiaomi,
            "deepseek" | "mira" => Self::TextOnly,
            _ => return None,
        })
    }

    /// Whether `model` from this maker reads images.
    pub fn reads_images(self, model: &str) -> bool {
        match self {
            Self::OpenAi => openai::reads_images(model),
            Self::Groq => groq::reads_images(model),
            Self::Mistral => mistral::reads_images(model),
            Self::Xai => xai::reads_images(model),
            Self::Cohere => cohere::reads_images(model),
            Self::Zai => zai::reads_images(model),
            Self::Moonshot => moonshot::reads_images(model),
            Self::MiniMax => minimax::reads_images(model),
            Self::Xiaomi => xiaomi::reads_images(model),
            Self::TextOnly => false,
        }
    }
}

#[cfg(test)]
mod tests;
