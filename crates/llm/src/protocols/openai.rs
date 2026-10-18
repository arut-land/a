//! What `OpenAI` model ids say about a model, shared by the Chat Completions
//! and Responses wires (rig's id rules, without its catalog).

/// The model's own name, past a `vendor/` prefix.
fn name(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// The GPT generation `model` names: `gpt-5.5-pro` is `(5, 5)`,
/// `gpt-6-sol` is `(6, 0)`. `None` for a model that is no numbered GPT.
pub(crate) fn gpt_version(model: &str) -> Option<(u32, u32)> {
    let rest = name(model).strip_prefix("gpt-")?;
    let major_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let major = rest.get(..major_end)?.parse().ok()?;
    let minor = rest
        .get(major_end..)
        .and_then(|rest| rest.strip_prefix('.'))
        .map_or(0, |rest| {
            let end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            rest.get(..end)
                .and_then(|minor| minor.parse().ok())
                .unwrap_or(0)
        });
    Some((major, minor))
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

/// Whether `model` reasons: `Some(true)` for GPT-5 and later and the
/// o-series, `Some(false)` for an earlier numbered GPT, `None` for an id
/// that names neither (a deployment name, a fine-tune).
pub(crate) fn reasons(model: &str) -> Option<bool> {
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
pub(crate) fn responses_can_disable(model: &str) -> bool {
    let name = name(model);
    let o_series = reasons(model) == Some(true) && gpt_version(model).is_none();
    let pro = name.ends_with("-pro") || name.contains("-pro-");
    !(o_series
        || pro
        || gpt_version(model) == Some((5, 0))
        || name.starts_with("gpt-6-astra")
        || name.starts_with("gpt-6.1-sol"))
}

/// Whether a reasoning model can turn reasoning off on Chat Completions:
/// not the o-series, a `-pro` model or GPT-5 itself.
pub(crate) fn chat_can_disable(model: &str) -> bool {
    let name = name(model);
    let gpt_5_0 = name == "gpt-5" || name.starts_with("gpt-5-");
    let pro = name.ends_with("-pro") || name.contains("-pro-");
    !(o_series(name) || pro || gpt_5_0)
}

/// Whether the model takes `temperature` and `top_p` while reasoning at
/// `effort` (`None` for the provider's default): a model that does not
/// reason does; the o-series, `-pro` models and GPT-5 never do; GPT-5.5
/// never does; the rest only at effort `none`.
pub(crate) fn takes_sampling(model: &str, effort: Option<crate::Effort>) -> bool {
    match reasons(model) {
        Some(false) | None => true,
        Some(true) if !responses_can_disable(model) => false,
        Some(true) if gpt_version(model) == Some((5, 5)) => false,
        Some(true) => effort == Some(crate::Effort::None),
    }
}

/// Whether the model reads images: all but GPT-3.5, the text-only GPT-4
/// snapshots, o1-mini, o1-preview, o3-mini and GPT-5.3 Codex Spark.
pub(crate) fn reads_images(model: &str) -> bool {
    const TEXT_ONLY: [&str; 9] = [
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
        || TEXT_ONLY.iter().any(|prefix| model.starts_with(prefix)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_name_what_models_do() {
        assert_eq!(gpt_version("openai/gpt-5.5-pro"), Some((5, 5)));
        assert_eq!(gpt_version("gpt-6-luna"), Some((6, 0)));
        assert_eq!(reasons("gpt-6-luna"), Some(true));
        assert_eq!(reasons("o4-mini"), Some(true));
        assert_eq!(reasons("gpt-4.1"), Some(false));
        assert_eq!(reasons("my-deployment"), None);
        assert!(responses_can_disable("gpt-6-luna"));
        assert!(!responses_can_disable("gpt-6-astra"));
        assert!(!responses_can_disable("o3"));
        assert!(takes_sampling("gpt-4o", None));
        assert!(!takes_sampling("gpt-6-luna", None));
        assert!(takes_sampling("gpt-6-luna", Some(crate::Effort::None)));
        assert!(!takes_sampling("gpt-5.5", Some(crate::Effort::None)));
        assert!(!reads_images("o3-mini"));
        assert!(reads_images("gpt-6-luna"));
    }
}
