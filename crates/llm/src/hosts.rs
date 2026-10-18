//! Every host this crate knows, across protocols, and the provider a model
//! spec names: `host:model` (`openrouter:anthropic/claude-sonnet-5`), or a
//! bare model on `OpenAI`. A host that speaks several protocols uses the
//! first it is listed under; `host@protocol:model` picks one by its key's
//! last segment (`openai@chat:gpt-4.1`).

use crate::error::{Error, Kind};
use crate::protocol::Provider;
use crate::protocols::{chat, gemini, messages, responses};

/// The default host for a bare model.
pub const DEFAULT_HOST: &str = "openai";

/// A model spec's parts.
#[derive(Debug, PartialEq, Eq)]
pub struct Spec<'a> {
    pub host: &'a str,
    /// The last segment of a protocol key (`chat`, `responses`).
    pub protocol: Option<&'a str>,
    pub model: &'a str,
}

/// `spec` split at its first `:` (model ids may hold more) and the host's
/// `@protocol`.
pub fn parse(spec: &str) -> Spec<'_> {
    let (host, model) = spec.split_once(':').unwrap_or((DEFAULT_HOST, spec));
    let (host, protocol) = match host.split_once('@') {
        Some((host, protocol)) => (host, Some(protocol)),
        None => (host, None),
    };
    Spec {
        host,
        protocol,
        model,
    }
}

/// The provider and model `spec` names, configured from the environment.
pub fn provider(spec: &str) -> Result<(Provider, String), Error> {
    let Spec {
        host,
        protocol,
        model,
    } = parse(spec);
    if model.is_empty() {
        return Err(Error::new(
            Kind::InvalidRequest,
            format!("`{spec}` names no model"),
        ));
    }
    let wants =
        |key: &str| protocol.is_none_or(|protocol| key.rsplit('.').next() == Some(protocol));
    if wants(responses::KEY)
        && let Some((found, quirks)) = responses::HOSTS.iter().find(|(found, _)| found.id == host)
    {
        return Ok((
            found.provider(responses::Responses::new(*quirks))?,
            model.to_owned(),
        ));
    }
    if wants(chat::KEY)
        && let Some((found, quirks)) = chat::HOSTS.iter().find(|(found, _)| found.id == host)
    {
        return Ok((found.provider(chat::Chat::new(*quirks))?, model.to_owned()));
    }
    if wants(messages::KEY)
        && let Some((found, quirks)) = messages::HOSTS.iter().find(|(found, _)| found.id == host)
    {
        return Ok((
            found.provider(messages::Messages::new(*quirks))?,
            model.to_owned(),
        ));
    }
    if wants(gemini::KEY)
        && let Some((found, quirks)) = gemini::HOSTS.iter().find(|(found, _)| found.id == host)
    {
        return Ok((
            found.provider(gemini::Gemini::new(*quirks))?,
            model.to_owned(),
        ));
    }
    Err(Error::new(
        Kind::InvalidRequest,
        format!(
            "no known host `{host}` speaks {}",
            protocol.unwrap_or("any protocol")
        ),
    ))
}

/// Every host id, in lookup order.
pub fn ids() -> impl Iterator<Item = &'static str> {
    responses::HOSTS
        .iter()
        .map(|(host, _)| host.id)
        .chain(chat::HOSTS.iter().map(|(host, _)| host.id))
        .chain(messages::HOSTS.iter().map(|(host, _)| host.id))
        .chain(gemini::HOSTS.iter().map(|(host, _)| host.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_name_a_host_a_protocol_and_a_model() {
        let spec = |host, protocol, model| Spec {
            host,
            protocol,
            model,
        };
        assert_eq!(parse("gpt-6-luna"), spec("openai", None, "gpt-6-luna"));
        assert_eq!(
            parse("openrouter:anthropic/claude-sonnet-5"),
            spec("openrouter", None, "anthropic/claude-sonnet-5")
        );
        assert_eq!(
            parse("openai@chat:gpt-4.1"),
            spec("openai", Some("chat"), "gpt-4.1")
        );
        assert_eq!(parse("ollama:qwen3:8b"), spec("ollama", None, "qwen3:8b"));
    }

    #[test]
    fn every_protocol_contributes_hosts() {
        for id in ["openai", "groq", "deepseek", "anthropic", "zai", "gemini"] {
            assert!(ids().any(|known| known == id), "{id}");
        }
    }
}
