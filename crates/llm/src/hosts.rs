//! Every host this crate knows, across protocols, and the provider a model
//! spec names: `host:model` (`openrouter:anthropic/claude-sonnet-5`), or a
//! bare model on `OpenAI`. A host that speaks several protocols uses the
//! first it is listed under; `host@protocol:model` picks one by its key's
//! last segment (`openai@chat:gpt-4.1`).

use crate::error::{Error, Kind};
use crate::protocol::{Hosted, Provider};
use crate::protocols::{chat, gemini, messages, responses};
use crate::spec::SmolStr;

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
    let (host, protocol) = host
        .split_once('@')
        .map_or((host, None), |(host, protocol)| (host, Some(protocol)));
    Spec {
        host,
        protocol,
        model,
    }
}

/// One protocol's host table, as lookups over it.
struct Wire {
    key: &'static str,
    /// The provider a host id names, if the protocol lists it.
    find: fn(&str) -> Option<Result<Provider, Error>>,
    ids: fn() -> Box<dyn Iterator<Item = &'static str>>,
}

const fn wire<P: Hosted>() -> Wire {
    Wire {
        key: P::KEY,
        find: find::<P>,
        ids: host_ids::<P>,
    }
}

fn find<P: Hosted>(id: &str) -> Option<Result<Provider, Error>> {
    let (host, quirks) = P::HOSTS.iter().find(|(host, _)| host.id == id)?;
    Some(host.provider(P::with(*quirks)))
}

fn host_ids<P: Hosted>() -> Box<dyn Iterator<Item = &'static str>> {
    Box::new(P::HOSTS.iter().map(|(host, _)| host.id))
}

/// Every protocol, in lookup order.
const WIRES: [Wire; 4] = [
    wire::<responses::Responses>(),
    wire::<chat::Chat>(),
    wire::<messages::Messages>(),
    wire::<gemini::Gemini>(),
];

/// The provider and model `spec` names, configured from the environment.
pub fn provider(spec: &str) -> Result<(Provider, SmolStr), Error> {
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
    let provider = WIRES
        .iter()
        .filter(|wire| {
            protocol.is_none_or(|protocol| wire.key.rsplit('.').next() == Some(protocol))
        })
        .find_map(|wire| (wire.find)(host))
        .ok_or_else(|| {
            Error::new(
                Kind::InvalidRequest,
                format!(
                    "no known host `{host}` speaks {}",
                    protocol.unwrap_or("any protocol")
                ),
            )
        })?;
    Ok((provider?, model.into()))
}

/// Every host id, in lookup order.
pub fn ids() -> impl Iterator<Item = &'static str> {
    WIRES.iter().flat_map(|wire| (wire.ids)())
}

#[cfg(test)]
mod tests;
