//! How each vendor spells the generation options, as far as the model takes
//! them ([`crate::models`]). An option rig would refuse for the model is
//! left out: the request still goes, without it. Effort levels a model's
//! catalog entry would refuse are sent; the host decides.

use serde_json::{Map, Value, json};

use super::{Quirks, Routing, Vendor};
use crate::models::{self, Maker, deepseek, groq, minimax, moonshot, openai, perplexity, xai, zai};
use crate::spec::{Effort, Generation};

/// Whose model rules a host's models follow; `None` for a host serving
/// models of every kind.
fn maker(vendor: Vendor) -> Option<Maker> {
    Some(match vendor {
        Vendor::OpenAi | Vendor::Azure => Maker::OpenAi,
        Vendor::DeepSeek | Vendor::Mira => Maker::TextOnly,
        Vendor::Groq => Maker::Groq,
        Vendor::Mistral => Maker::Mistral,
        Vendor::Xai => Maker::Xai,
        Vendor::Cohere => Maker::Cohere,
        Vendor::Zai => Maker::Zai,
        Vendor::Moonshot => Maker::Moonshot,
        Vendor::MiniMax => Maker::MiniMax,
        Vendor::Mimo => Maker::Xiaomi,
        Vendor::OpenRouter
        | Vendor::Together
        | Vendor::Venice
        | Vendor::LlamaCpp
        | Vendor::Ollama
        | Vendor::Perplexity
        | Vendor::Other => return None,
    })
}

/// Whether `model` reads user images on a host of `vendor`. An
/// `OpenRouter` id names its upstream, whose rule decides.
pub(super) fn reads_images(vendor: Vendor, model: &str) -> bool {
    let (maker, model) = match vendor {
        Vendor::OpenRouter => match model.split_once('/') {
            Some((upstream, model)) => (Maker::named(upstream), model),
            None => (None, model),
        },
        vendor => (maker(vendor), model),
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
) -> Option<&'static str> {
    let deepseek = base_url.to_ascii_lowercase().contains("deepseek.com");
    quirks
        .reasoning_field
        .or((deepseek || moonshot::replays_reasoning(model)).then_some("reasoning_content"))
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
}

/// The fields a host's options go under, filled as each is allowed.
#[derive(Default)]
struct Sent(Map<String, Value>);

impl Sent {
    fn send(&mut self, key: &str, value: impl Into<Value>) {
        self.0.insert(key.to_owned(), value.into());
    }

    fn effort(&mut self, effort: Effort) {
        self.send("reasoning_effort", effort.as_str());
    }

    /// `reasoning_effort` as the level's word, `none` for off; a budget is
    /// not taken.
    fn plain(&mut self, reasoning: Option<Reasoning>) {
        match reasoning {
            Some(Reasoning::Off) => self.send("reasoning_effort", "none"),
            Some(Reasoning::Effort(effort)) => self.effort(effort),
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
    stop: Stop,
}

/// How many stop sequences a vendor takes.
#[derive(Clone, Copy)]
enum Stop {
    None,
    Any,
    AtMost(usize),
}

impl Stop {
    fn takes(self, count: usize) -> bool {
        match self {
            Self::None => false,
            Self::Any => true,
            Self::AtMost(limit) => count <= limit,
        }
    }
}

const NOTHING: Takes = Takes {
    top_p: false,
    seed: None,
    parallel: false,
    stop: Stop::None,
};

/// Everything, with at most `stop` stop sequences.
const fn all(stop: Stop) -> Takes {
    Takes {
        top_p: true,
        seed: Some("seed"),
        parallel: true,
        stop,
    }
}

/// Only `top_p` and stop sequences: no `seed` or `parallel_tool_calls`.
const fn sampling(stop: Stop) -> Takes {
    Takes {
        top_p: true,
        stop,
        ..NOTHING
    }
}

/// What `vendor` takes for `model` beside reasoning; `off` says the request
/// turns reasoning off.
fn takes(vendor: Vendor, model: &str, generation: &Generation, off: bool) -> Takes {
    match vendor {
        // A reasoning model takes no `stop`, at any effort.
        Vendor::OpenAi | Vendor::Azure => Takes {
            stop: if openai::reasons(model) == Some(true) {
                Stop::None
            } else {
                Stop::AtMost(4)
            },
            ..all(Stop::Any)
        },
        Vendor::OpenRouter | Vendor::Groq | Vendor::Venice => all(Stop::AtMost(4)),
        Vendor::DeepSeek => sampling(Stop::AtMost(16)),
        Vendor::Mistral => Takes {
            seed: Some("random_seed"),
            ..all(Stop::Any)
        },
        // A Grok model reasons unless its id says otherwise, and then takes
        // no stop sequences.
        Vendor::Xai => Takes {
            seed: None,
            stop: if xai::reasons(model) {
                Stop::None
            } else {
                Stop::Any
            },
            ..all(Stop::Any)
        },
        Vendor::Together | Vendor::Ollama | Vendor::Cohere => Takes {
            parallel: false,
            ..all(Stop::Any)
        },
        Vendor::LlamaCpp => all(Stop::Any),
        Vendor::Perplexity | Vendor::MiniMax => sampling(Stop::Any),
        // Stop sequences of at most 32 bytes, five at most.
        Vendor::Moonshot => Takes {
            stop: if generation.stop.iter().all(|sequence| sequence.len() <= 32) {
                Stop::AtMost(5)
            } else {
                Stop::None
            },
            ..NOTHING
        },
        Vendor::Zai => Takes {
            top_p: generation
                .top_p
                .is_some_and(|top_p| (0.01..=1.0).contains(&top_p)),
            ..sampling(Stop::AtMost(4))
        },
        // MiMo fixes `top_p` while it thinks.
        Vendor::Mimo => Takes {
            top_p: off,
            ..sampling(Stop::AtMost(4))
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
    let takes = takes(
        quirks.vendor,
        model,
        generation,
        reasoning == Some(Reasoning::Off),
    );
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
    if !stop.is_empty() && takes.stop.takes(stop.len()) {
        sent.send("stop", stop.clone());
    }
    sent.0
}

/// The fields `reasoning` goes under for `model` on the host.
fn reasoning_fields(sent: &mut Sent, quirks: &Quirks, model: &str, reasoning: Option<Reasoning>) {
    match quirks.vendor {
        Vendor::OpenAi | Vendor::Azure => openai_reasoning(sent, model, reasoning, quirks.routing),
        Vendor::OpenRouter => match reasoning {
            Some(Reasoning::Off) => sent.send("reasoning", json!({"effort": "none"})),
            Some(Reasoning::Effort(effort)) => {
                sent.send("reasoning", json!({"effort": effort.as_str()}));
            }
            // These upstreams take an effort level, not a budget.
            Some(Reasoning::Budget(_))
                if model.starts_with("openai/") || model.starts_with("x-ai/") => {}
            Some(Reasoning::Budget(tokens)) => {
                sent.send("reasoning", json!({"max_tokens": tokens}));
            }
            None => {}
        },
        Vendor::DeepSeek | Vendor::Zai => {
            let model = match quirks.vendor {
                Vendor::Zai => zai::reasoning(model),
                _ => deepseek::REASONING,
            };
            match Reasoning::taken(reasoning, model) {
                Some(Reasoning::Off) => sent.send("thinking", json!({"type": "disabled"})),
                Some(Reasoning::Effort(effort)) => {
                    sent.send("thinking", json!({"type": "enabled"}));
                    sent.effort(effort);
                }
                _ => {}
            }
        }
        Vendor::Mistral | Vendor::Venice | Vendor::LlamaCpp | Vendor::Ollama => {
            sent.plain(reasoning);
        }
        Vendor::Groq => sent.plain(Reasoning::taken(reasoning, groq::reasoning(model))),
        Vendor::Xai => sent.plain(Reasoning::taken(reasoning, xai::REASONING)),
        Vendor::Perplexity => sent.plain(Reasoning::taken(reasoning, perplexity::reasoning(model))),
        Vendor::Together => match reasoning {
            Some(Reasoning::Off) => sent.send("reasoning", json!({"enabled": false})),
            Some(Reasoning::Effort(effort @ (Effort::Low | Effort::Medium | Effort::High))) => {
                sent.send("reasoning", json!({"enabled": true}));
                sent.effort(effort);
            }
            _ => {}
        },
        Vendor::Moonshot => match Reasoning::taken(reasoning, moonshot::reasoning(model)) {
            Some(Reasoning::Off) => sent.send("thinking", json!({"type": "disabled"})),
            Some(Reasoning::Effort(effort)) => sent.effort(effort),
            _ => {}
        },
        // The Compatibility API takes only `none` and `high`.
        Vendor::Cohere => match reasoning {
            Some(Reasoning::Off) => sent.send("reasoning_effort", "none"),
            Some(Reasoning::Effort(Effort::High)) => sent.effort(Effort::High),
            _ => {}
        },
        Vendor::MiniMax => match Reasoning::taken(reasoning, minimax::reasoning(model)) {
            Some(Reasoning::Off) => sent.send("thinking", json!({"type": "disabled"})),
            Some(Reasoning::Effort(effort)) => {
                sent.send("thinking", json!({"type": "adaptive"}));
                sent.effort(effort);
            }
            _ => {}
        },
        Vendor::Mimo => {
            if reasoning == Some(Reasoning::Off) {
                sent.send("thinking", json!({"type": "disabled"}));
            }
        }
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
    match reasoning {
        Some(Reasoning::Off)
            if openai::reasons(model) != Some(true) || openai::chat_can_disable(model) =>
        {
            sent.send("reasoning_effort", "none");
        }
        // Azure takes `max` only on Responses.
        Some(Reasoning::Effort(Effort::Max)) if azure.is_some() => {}
        Some(Reasoning::Effort(effort)) => sent.effort(effort),
        Some(Reasoning::Off | Reasoning::Budget(_)) | None => {}
    }
}
