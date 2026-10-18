//! What a host's models take, by model id (rig's rules for the ids its
//! catalog does not list), and how each vendor spells the generation
//! options. An option rig would refuse for the model is left out: the
//! request still goes, without it. Effort levels a model's catalog entry
//! would refuse are sent; the host decides.

use serde_json::{Map, Value, json};

use super::{Quirks, Routing, Vendor};
use crate::protocols::openai;
use crate::spec::{Effort, Generation};

/// The model's own name, past a `vendor/` prefix.
fn name(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// The image rule of the vendor an id names (a vendor or an `OpenRouter`
/// model's upstream): `None` for one whose API takes text only.
fn image_rule(vendor: &str) -> Option<fn(&str) -> bool> {
    Some(match vendor {
        "deepseek" | "mira" => return None,
        "groq" => groq_images,
        "mistral" | "mistralai" => mistral_images,
        "openai" | "azure.openai" => openai::reads_images,
        "xai" | "x-ai" => xai_images,
        "cohere" => cohere_images,
        "zai" | "z-ai" => zai_images,
        "moonshot" | "moonshotai" => moonshot_images,
        "minimax" => minimax_images,
        "xiaomimimo" | "xiaomi" => mimo_images,
        _ => |_| true,
    })
}

/// Groq reads images on its Llama 4 models.
fn groq_images(model: &str) -> bool {
    model.contains("llama-4")
}

/// Codestral and Devstral are text models.
fn mistral_images(model: &str) -> bool {
    !(model.starts_with("codestral") || model.starts_with("devstral"))
}

/// Grok 2 reads images only in its vision models; Grok 3 and Grok Code not.
fn xai_images(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    !((model.starts_with("grok-2") && !model.contains("vision"))
        || model.starts_with("grok-3")
        || model.starts_with("grok-code"))
}

fn cohere_images(model: &str) -> bool {
    model.contains("vision")
}

/// GLM vision models carry a `v` after their version: `glm-4.5v`.
fn zai_images(model: &str) -> bool {
    model.strip_prefix("glm-").is_some_and(|rest| {
        rest.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.')
            .starts_with('v')
    })
}

fn moonshot_images(model: &str) -> bool {
    !(model == "kimi-k2"
        || model.starts_with("kimi-k2-")
        || (model.starts_with("moonshot-v1") && !model.contains("vision")))
}

fn minimax_images(model: &str) -> bool {
    !model.to_ascii_lowercase().starts_with("minimax-m2")
}

fn mimo_images(model: &str) -> bool {
    !((model.starts_with("mimo-v2-") && !model.contains("omni"))
        || model.starts_with("mimo-v2.5-pro"))
}

/// Whether `model` reads user images on a host of `vendor`. An
/// `OpenRouter` id names its upstream, whose rule decides.
pub(super) fn reads_images(vendor: Vendor, model: &str) -> bool {
    let rule = match vendor {
        Vendor::OpenRouter => {
            return match model.split_once('/') {
                Some((upstream, model)) => image_rule(upstream).is_some_and(|rule| rule(model)),
                None => true,
            };
        }
        Vendor::DeepSeek | Vendor::Mira => None,
        Vendor::Groq => image_rule("groq"),
        Vendor::Mistral => image_rule("mistral"),
        Vendor::OpenAi | Vendor::Azure => image_rule("openai"),
        Vendor::Xai => image_rule("xai"),
        Vendor::Cohere => image_rule("cohere"),
        Vendor::Zai => image_rule("zai"),
        Vendor::Moonshot => image_rule("moonshot"),
        Vendor::MiniMax => image_rule("minimax"),
        Vendor::Mimo => image_rule("xiaomimimo"),
        Vendor::Together
        | Vendor::Venice
        | Vendor::LlamaCpp
        | Vendor::Ollama
        | Vendor::Perplexity
        | Vendor::Other => return true,
    };
    rule.is_some_and(|rule| rule(model))
}

/// The field every assistant message carries its reasoning under: the
/// host's own, `reasoning_content` for `DeepSeek` at any base URL, and the
/// models that ask for it behind any gateway (Kimi K3, and `OpenRouter`'s
/// `moonshotai/kimi-k2.6`).
pub(super) fn reasoning_field(
    quirks: &Quirks,
    base_url: &str,
    model: &str,
) -> Option<&'static str> {
    let deepseek = base_url.to_ascii_lowercase().contains("deepseek.com");
    let kimi_k3 = name(model) == "kimi-k3" || name(model).starts_with("kimi-k3-");
    let listed = kimi_k3 || model == "moonshotai/kimi-k2.6";
    quirks
        .reasoning_field
        .or((deepseek || listed).then_some("reasoning_content"))
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
            stop: if model.contains("non-reasoning") {
                Stop::Any
            } else {
                Stop::None
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
        || openai::takes_sampling(model, generation.reasoning);
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
        Vendor::DeepSeek | Vendor::Zai => match reasoning {
            Some(Reasoning::Off) => sent.send("thinking", json!({"type": "disabled"})),
            // GLM 4 models switch thinking on and off and take no level.
            Some(Reasoning::Effort(effort @ (Effort::Low | Effort::High | Effort::Max)))
                if !(quirks.vendor == Vendor::Zai && model.starts_with("glm-4")) =>
            {
                sent.send("thinking", json!({"type": "enabled"}));
                sent.effort(effort);
            }
            _ => {}
        },
        Vendor::Mistral | Vendor::Venice | Vendor::LlamaCpp | Vendor::Ollama => {
            sent.plain(reasoning);
        }
        // GPT-OSS cannot turn reasoning off, and takes `low` to `high`.
        Vendor::Groq if model.contains("gpt-oss") => {
            if let Some(Reasoning::Effort(effort @ (Effort::Low | Effort::Medium | Effort::High))) =
                reasoning
            {
                sent.effort(effort);
            }
        }
        Vendor::Groq => sent.plain(reasoning),
        // A reasoning Grok model cannot turn reasoning off.
        Vendor::Xai => {
            if let Some(Reasoning::Effort(effort)) = reasoning
                && !matches!(effort, Effort::Minimal | Effort::Max)
            {
                sent.effort(effort);
            }
        }
        Vendor::Together => match reasoning {
            Some(Reasoning::Off) => sent.send("reasoning", json!({"enabled": false})),
            Some(Reasoning::Effort(effort @ (Effort::Low | Effort::Medium | Effort::High))) => {
                sent.send("reasoning", json!({"enabled": true}));
                sent.effort(effort);
            }
            _ => {}
        },
        Vendor::Moonshot => match reasoning {
            Some(Reasoning::Off) if name(model).starts_with("kimi-k2.6") => {
                sent.send("thinking", json!({"type": "disabled"}));
            }
            Some(Reasoning::Effort(effort @ (Effort::Low | Effort::High | Effort::Max)))
                if name(model).starts_with("kimi-k3") =>
            {
                sent.effort(effort);
            }
            _ => {}
        },
        // The Compatibility API takes only `none` and `high`.
        Vendor::Cohere => match reasoning {
            Some(Reasoning::Off) => sent.send("reasoning_effort", "none"),
            Some(Reasoning::Effort(Effort::High)) => sent.effort(Effort::High),
            _ => {}
        },
        // Only Sonar Deep Research takes an effort, `low` to `high`.
        Vendor::Perplexity => {
            if let Some(Reasoning::Effort(effort @ (Effort::Low | Effort::Medium | Effort::High))) =
                reasoning
                && model.starts_with("sonar-deep-research")
            {
                sent.effort(effort);
            }
        }
        Vendor::MiniMax => {
            let model = model.to_ascii_lowercase();
            let flash = model.starts_with("minimax-m3.1");
            match reasoning {
                Some(Reasoning::Off) if !flash && model.starts_with("minimax-m3") => {
                    sent.send("thinking", json!({"type": "disabled"}));
                }
                Some(Reasoning::Effort(effort)) if flash && effort != Effort::Minimal => {
                    sent.send("thinking", json!({"type": "adaptive"}));
                    sent.effort(effort);
                }
                _ => {}
            }
        }
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
