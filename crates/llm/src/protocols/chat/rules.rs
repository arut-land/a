//! How each vendor spells the generation options, as far as the model takes
//! them ([`crate::models`]). An option the model cannot take is left out:
//! the request still goes, without it. `OpenAI`'s effort levels go
//! unchecked; the host decides.

use serde_json::{Map, Value, json};

use super::{Quirks, ReasoningField, Routing, Vendor};
use crate::models::{
    self, LOW_TO_HIGH, Maker, Thinking, cohere, deepseek, groq, minimax, moonshot, openai,
    perplexity, xai, zai,
};
use crate::spec::{Effort, Generation};

/// Whether `model` reads user images on a host with `quirks`. An
/// `OpenRouter` id names its upstream, whose rule decides.
pub(super) fn reads_images(quirks: &Quirks, model: &str) -> bool {
    let (maker, model) = match (quirks.vendor, model.split_once('/')) {
        (Vendor::OpenRouter, Some((upstream, model))) => (Maker::named(upstream), model),
        (Vendor::OpenRouter, None) => (None, model),
        _ => (quirks.maker, model),
    };
    maker.is_none_or(|maker| maker.reads_images(model))
}

/// The field every assistant message carries its reasoning under: the
/// host's own, `reasoning_content` for `DeepSeek` at any base URL, and the
/// models that ask for it behind any gateway.
pub(super) fn reasoning_field(
    quirks: &Quirks,
    base_url: &str,
    model: &str,
) -> Option<ReasoningField> {
    let deepseek = base_url.to_ascii_lowercase().contains("deepseek.com");
    let asks = deepseek || moonshot::replays_reasoning(model);
    quirks
        .reasoning_field
        .or(asks.then_some(ReasoningField::ReasoningContent))
}

/// What the request asks of the model's reasoning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reasoning {
    Off,
    Effort(Effort),
    Budget(u32),
}

impl Reasoning {
    fn of(generation: &Generation) -> Option<Self> {
        match (generation.reasoning, generation.reasoning_budget) {
            (Some(Effort::None), _) => Some(Self::Off),
            (Some(effort), _) => Some(Self::Effort(effort)),
            (None, Some(budget)) => Some(Self::Budget(budget.get())),
            (None, None) => None,
        }
    }

    /// The reasoning asked, if `model` takes it: an off it cannot turn off
    /// or a level it lacks is dropped, and any budget. An unknown model
    /// takes any off or level.
    fn taken(reasoning: Option<Self>, model: models::Reasoning) -> Option<Self> {
        reasoning.filter(|reasoning| match (reasoning, model) {
            (Self::Budget(_), _) => false,
            (_, models::Reasoning::Unknown) => true,
            (Self::Off, model) => model.can_disable(),
            (Self::Effort(effort), model) => model.takes(*effort),
        })
    }

    /// The level's word, `none` for off.
    fn effort(self) -> Option<Effort> {
        match self {
            Self::Off => Some(Effort::None),
            Self::Effort(effort) => Some(effort),
            Self::Budget(_) => None,
        }
    }
}

/// The fields a host's options go under, filled as each is allowed.
#[derive(Default)]
struct Sent(Map<String, Value>);

impl Sent {
    fn send(&mut self, key: &str, value: impl Into<Value>) {
        self.0.insert(key.to_owned(), value.into());
    }

    /// `reasoning_effort`: the level's word, `none` for off.
    fn effort(&mut self, reasoning: Option<Reasoning>) {
        if let Some(effort) = reasoning.and_then(Reasoning::effort) {
            self.send("reasoning_effort", effort.as_str());
        }
    }

    /// `thinking: {type: disabled}` for off; a level as `thinking: {type:
    /// on}`, when given, beside `reasoning_effort`.
    fn thinking(&mut self, reasoning: Option<Reasoning>, on: Option<&str>) {
        match reasoning {
            Some(Reasoning::Off) => self.send("thinking", json!({"type": "disabled"})),
            Some(Reasoning::Effort(_)) => {
                if let Some(on) = on {
                    self.send("thinking", json!({ "type": on }));
                }
                self.effort(reasoning);
            }
            Some(Reasoning::Budget(_)) | None => {}
        }
    }
}

/// What a vendor takes beside reasoning.
#[derive(Clone, Copy)]
struct Takes {
    top_p: bool,
    /// The key `seed` goes under, when it is taken.
    seed: Option<&'static str>,
    parallel: bool,
    /// How many stop sequences it takes.
    stop: usize,
}

const NOTHING: Takes = Takes {
    top_p: false,
    seed: None,
    parallel: false,
    stop: 0,
};

/// Everything, with at most `stop` stop sequences.
const fn all(stop: usize) -> Takes {
    Takes {
        top_p: true,
        seed: Some("seed"),
        parallel: true,
        stop,
    }
}

/// Only `top_p` and stop sequences: no `seed` or `parallel_tool_calls`.
const fn sampling(stop: usize) -> Takes {
    Takes {
        top_p: true,
        stop,
        ..NOTHING
    }
}

/// What `vendor` takes for `model` beside reasoning; `off` says the request
/// turns reasoning off.
fn takes(vendor: Vendor, model: &str, generation: &Generation, off: bool) -> Takes {
    const ANY: usize = usize::MAX;
    match vendor {
        // A reasoning model takes no `stop`, at any effort.
        Vendor::OpenAi | Vendor::Azure => all(if openai::reasons(model) == Some(true) {
            0
        } else {
            4
        }),
        Vendor::OpenRouter | Vendor::Groq | Vendor::Venice => all(4),
        Vendor::DeepSeek => sampling(16),
        Vendor::Mistral => Takes {
            seed: Some("random_seed"),
            ..all(ANY)
        },
        // A Grok model reasons unless its id says otherwise, and then takes
        // no stop sequences.
        Vendor::Xai => Takes {
            seed: None,
            ..all(if xai::reasons(model) { 0 } else { ANY })
        },
        Vendor::Together | Vendor::Ollama | Vendor::Cohere => Takes {
            parallel: false,
            ..all(ANY)
        },
        Vendor::LlamaCpp => all(ANY),
        Vendor::Perplexity | Vendor::MiniMax => sampling(ANY),
        // Stop sequences of at most 32 bytes, five at most.
        Vendor::Moonshot => Takes {
            stop: if generation.stop.iter().all(|sequence| sequence.len() <= 32) {
                5
            } else {
                0
            },
            ..NOTHING
        },
        Vendor::Zai => Takes {
            top_p: generation
                .top_p
                .is_some_and(|top_p| (0.01..=1.0).contains(&top_p)),
            ..sampling(4)
        },
        // MiMo fixes `top_p` while it thinks.
        Vendor::Mimo => Takes {
            top_p: off,
            ..sampling(4)
        },
        // Mira rejects every pass-through parameter, and the rest are
        // unverified.
        Vendor::Mira | Vendor::Other => NOTHING,
    }
}

/// The generation fields `vendor` takes for `model`: sampling, the output
/// cap, reasoning, seed, stop sequences and `parallel_tool_calls` (sent only
/// beside tools).
pub(super) fn generation(
    quirks: &Quirks,
    model: &str,
    generation: &Generation,
    parallel: Option<bool>,
    tools: bool,
) -> Map<String, Value> {
    let mut sent = Sent::default();
    let reasoning = Reasoning::of(generation);
    // OpenAI's reasoning models refuse sampling while they reason.
    let samples = !matches!(quirks.vendor, Vendor::OpenAi | Vendor::Azure)
        || openai::sampling(model).takes(generation.reasoning);
    if let Some(temperature) = generation.temperature.filter(|_| samples) {
        sent.send("temperature", temperature);
    }
    if let Some(max) = generation.max_output_tokens {
        sent.send("max_tokens", max.get());
    }
    reasoning_fields(&mut sent, quirks, model, reasoning);
    let off = reasoning == Some(Reasoning::Off);
    let takes = takes(quirks.vendor, model, generation, off);
    if let Some(top_p) = generation.top_p.filter(|_| takes.top_p && samples) {
        sent.send("top_p", top_p);
    }
    if let (Some(seed), Some(key)) = (generation.seed, takes.seed) {
        sent.send(key, seed);
    }
    if let Some(parallel) = parallel.filter(|_| tools && takes.parallel) {
        sent.send("parallel_tool_calls", parallel);
    }
    let stop = &generation.stop;
    if !stop.is_empty() && stop.len() <= takes.stop {
        sent.send("stop", stop.clone());
    }
    sent.0
}

/// The fields `reasoning` goes under for `model` on the host.
fn reasoning_fields(sent: &mut Sent, quirks: &Quirks, model: &str, reasoning: Option<Reasoning>) {
    let taken = |rules| Reasoning::taken(reasoning, rules);
    match quirks.vendor {
        Vendor::OpenAi | Vendor::Azure => openai_reasoning(sent, model, reasoning, quirks.routing),
        Vendor::OpenRouter => match reasoning {
            // These upstreams take an effort level, not a budget.
            Some(Reasoning::Budget(_))
                if model.starts_with("openai/") || model.starts_with("x-ai/") => {}
            Some(Reasoning::Budget(tokens)) => {
                sent.send("reasoning", json!({ "max_tokens": tokens }));
            }
            Some(reasoning) => {
                let effort = reasoning.effort().map(Effort::as_str);
                sent.send("reasoning", json!({ "effort": effort }));
            }
            None => {}
        },
        Vendor::DeepSeek => sent.thinking(taken(deepseek::REASONING), Some("enabled")),
        Vendor::Zai => sent.thinking(taken(zai::reasoning(model)), Some("enabled")),
        Vendor::MiniMax => sent.thinking(taken(minimax::reasoning(model)), Some("adaptive")),
        Vendor::Moonshot => sent.thinking(taken(moonshot::reasoning(model)), None),
        // MiMo takes no level, only an off.
        Vendor::Mimo => sent.thinking(reasoning.filter(|r| *r == Reasoning::Off), None),
        Vendor::Together => {
            let rules = models::Reasoning::Takes(Thinking::levels(LOW_TO_HIGH).disabling());
            if let Some(reasoning) = taken(rules) {
                let enabled = reasoning != Reasoning::Off;
                sent.send("reasoning", json!({ "enabled": enabled }));
                if enabled {
                    sent.effort(Some(reasoning));
                }
            }
        }
        Vendor::Mistral | Vendor::Venice | Vendor::LlamaCpp | Vendor::Ollama => {
            sent.effort(taken(models::Reasoning::Unknown));
        }
        Vendor::Groq => sent.effort(taken(groq::reasoning(model))),
        Vendor::Xai => sent.effort(taken(xai::REASONING)),
        Vendor::Perplexity => sent.effort(taken(perplexity::reasoning(model))),
        Vendor::Cohere => sent.effort(taken(cohere::REASONING)),
        Vendor::Mira | Vendor::Other => {}
    }
}

/// `OpenAI`'s `reasoning_effort`, and an Azure deployment's: none for a
/// model that does not reason or cannot turn reasoning off, and none on an
/// Azure `api-version` older than the field (the default one among them).
fn openai_reasoning(sent: &mut Sent, model: &str, reasoning: Option<Reasoning>, routing: Routing) {
    let azure = match routing {
        Routing::Deployment { api_version } => Some(api_version),
        Routing::Path => None,
    };
    let dated = azure.is_some_and(|version| version <= super::hosts::AZURE_API_VERSION);
    if dated || openai::reasons(model) == Some(false) {
        return;
    }
    let reasoning = reasoning.filter(|reasoning| match reasoning {
        Reasoning::Off => openai::reasons(model) != Some(true) || openai::chat_can_disable(model),
        // Azure takes `max` only on Responses.
        Reasoning::Effort(effort) => !(*effort == Effort::Max && azure.is_some()),
        Reasoning::Budget(_) => false,
    });
    sent.effort(reasoning);
}
