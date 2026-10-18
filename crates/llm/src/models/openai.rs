//! `OpenAI` model ids, shared by the Chat Completions and Responses wires.

use super::{Sampling, has_prefix, name};

/// The GPT generation `model` names: `gpt-5.5-pro` is `(5, 5)`,
/// `gpt-6-sol` is `(6, 0)`. `None` for a model that is no numbered GPT.
pub fn gpt_version(model: &str) -> Option<(u32, u32)> {
    let rest = name(model).strip_prefix("gpt-")?;
    let (major, rest) = leading_number(rest);
    let minor = rest
        .strip_prefix('.')
        .map_or(0, |rest| leading_number(rest).0.unwrap_or(0));
    Some((major?, minor))
}

/// The digits `text` starts with, and the rest.
fn leading_number(text: &str) -> (Option<u32>, &str) {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    (text[..end].parse().ok(), &text[end..])
}

/// `o` and a digit, then its end, a digit or a hyphen: `o3`, `o4-mini`.
fn o_series(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next() == Some('o')
        && chars.next().is_some_and(|digit| digit.is_ascii_digit())
        && chars
            .next()
            .is_none_or(|next| next == '-' || next.is_ascii_digit())
}

fn pro(name: &str) -> bool {
    name.ends_with("-pro") || name.contains("-pro-")
}

/// Whether `model` reasons: `Some(true)` for GPT-5 and later and the
/// o-series, `Some(false)` for an earlier numbered GPT, `None` for an id
/// that names neither (a deployment name, a fine-tune).
pub fn reasons(model: &str) -> Option<bool> {
    let name = name(model);
    let gpt = name
        .strip_prefix("gpt-")
        .and_then(|rest| rest.split(['.', '-']).next())
        .filter(|major| major.len() == 1)
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= 5);
    if gpt || o_series(name) {
        Some(true)
    } else {
        gpt_version(model).map(|_| false)
    }
}

/// Whether a model can turn reasoning off on Responses: not the o-series, a
/// `-pro` model, GPT-5 itself, GPT-6 Astra or GPT-6.1 Sol.
pub fn responses_can_disable(model: &str) -> bool {
    let name = name(model);
    let o_series = reasons(model) == Some(true) && gpt_version(model).is_none();
    !(o_series
        || pro(name)
        || gpt_version(model) == Some((5, 0))
        || has_prefix(&["gpt-6-astra", "gpt-6.1-sol"], name))
}

/// Whether a reasoning model can turn reasoning off on Chat Completions:
/// not the o-series, a `-pro` model or GPT-5 itself.
pub fn chat_can_disable(model: &str) -> bool {
    let name = name(model);
    let gpt_5_0 = name == "gpt-5" || name.starts_with("gpt-5-");
    !(o_series(name) || pro(name) || gpt_5_0)
}

/// Whether the model calls tools through Chat Completions: not a GPT-6 or
/// later that cannot turn reasoning off, which calls them only on Responses.
pub fn chat_calls_tools(model: &str) -> bool {
    gpt_version(model).is_none_or(|(major, _)| major < 6) || responses_can_disable(model)
}

/// When the model takes `temperature` and `top_p`: a model that does not
/// reason always; the o-series, `-pro` models, GPT-5 and GPT-5.5 never; the
/// rest only at effort `none`.
pub fn sampling(model: &str) -> Sampling {
    match reasons(model) {
        Some(false) | None => Sampling::Free,
        Some(true) if !responses_can_disable(model) || gpt_version(model) == Some((5, 5)) => {
            Sampling::Fixed
        }
        Some(true) => Sampling::WithoutReasoning,
    }
}

/// Whether the model reads images: all but GPT-3.5, the text-only GPT-4
/// snapshots, o1-mini, o1-preview, o3-mini and GPT-5.3 Codex Spark.
pub fn reads_images(model: &str) -> bool {
    const TEXT_ONLY: &[&str] = &[
        "gpt-3.5",
        "gpt-4-32k",
        "gpt-4-0125-preview",
        "gpt-4-1106-preview",
        "gpt-4-turbo-preview",
        "o1-mini",
        "o1-preview",
        "o3-mini",
        "gpt-5.3-codex-spark",
    ];
    let model = name(model).to_ascii_lowercase();
    !(matches!(model.as_str(), "gpt-4" | "gpt-4-0613" | "gpt-4-0314")
        || has_prefix(TEXT_ONLY, &model))
}
