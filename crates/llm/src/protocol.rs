//! A wire protocol is two pure halves: `encode` turns a [`Request`] into an
//! HTTP request body for one provider, and a [`Decoder`] turns the reply's
//! frames into [`Event`]s. A [`Provider`] is plain data: where to send, how
//! to authenticate, and the protocol instance (with its host quirks) that
//! speaks to it.

use std::fmt;
use std::sync::Arc;

use serde_json::Value;

use crate::error::Error;
use crate::spec::{Event, Request, SmolStr};

pub trait Protocol: Send + Sync + 'static {
    /// The key this protocol's options and part state go under, and the
    /// protocol an [`Origin`](crate::Origin) names.
    fn key(&self) -> &'static str;

    /// The request for `request`, already prepared for `target`.
    fn encode(&self, target: &Target<'_>, request: &Request) -> Result<Encoded, Error>;

    /// A fresh decoder for one reply to `request`.
    fn decoder(&self, target: &Target<'_>, request: &Request) -> Box<dyn Decoder>;

    /// The id a tool call from another origin must take here, when its own
    /// does not fit this protocol's grammar.
    fn tool_id(&self, id: &str) -> Option<String> {
        let _ = id;
        None
    }

    /// The error a non-success reply means. Protocols with an error
    /// envelope of their own refine the default.
    fn error(&self, status: u16, headers: &http::HeaderMap, body: &[u8]) -> Error {
        Error::from_status(status, headers, body)
    }
}

/// Who a request goes to.
pub struct Target<'a> {
    pub provider: &'a Provider,
    pub model: &'a str,
}

/// What `encode` produced.
#[derive(Debug)]
pub struct Encoded {
    /// Appended to the provider's base URL; may carry a query.
    pub path: String,
    pub body: Value,
    pub headers: Vec<(&'static str, String)>,
    pub framing: Framing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    Sse,
    Ndjson,
}

/// One frame of a reply: an SSE event, or one NDJSON line (`event` empty).
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    pub event: &'a str,
    pub data: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    More,
    /// The provider ended the reply; nothing after this is read.
    Done,
}

pub trait Decoder: Send {
    fn frame(&mut self, frame: Frame<'_>, out: &mut Vec<Event>) -> Result<Flow, Error>;

    /// The body ended without [`Flow::Done`]. A decoder that can still
    /// finish the reply (a gateway that never sends the closing event)
    /// emits its `Finish`; otherwise the reply is incomplete.
    fn end(&mut self, out: &mut Vec<Event>) -> Result<(), Error> {
        let _ = out;
        Err(Error::incomplete())
    }
}

/// A credential: shown nowhere.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Auth {
    None,
    Bearer(Secret),
    Header { name: &'static str, key: Secret },
    Query { name: &'static str, key: Secret },
}

#[derive(Clone)]
pub struct Provider {
    pub id: SmolStr,
    pub base_url: String,
    pub auth: Auth,
    pub headers: Vec<(String, String)>,
    pub protocol: Arc<dyn Protocol>,
}

impl fmt::Debug for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Provider")
            .field("id", &self.id)
            .field("base_url", &self.base_url)
            .field("protocol", &self.protocol.key())
            .finish_non_exhaustive()
    }
}

impl Provider {
    pub fn new(
        id: impl Into<SmolStr>,
        base_url: impl Into<String>,
        auth: Auth,
        protocol: impl Protocol,
    ) -> Self {
        Self {
            id: id.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            auth,
            headers: Vec::new(),
            protocol: Arc::new(protocol),
        }
    }

    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        base_url
            .into()
            .trim_end_matches('/')
            .clone_into(&mut self.base_url);
        self
    }

    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// The HTTP request `request` becomes, encoded for `model`.
    pub fn request(
        &self,
        model: &str,
        request: &Request,
    ) -> Result<(http::Request<bytes::Bytes>, Framing), Error> {
        let target = Target {
            provider: self,
            model,
        };
        let mut encoded = self.protocol.encode(&target, request)?;
        if let Some(overlay) = &request.body {
            merge(&mut encoded.body, overlay);
        }
        let mut uri = format!("{}{}", self.base_url, encoded.path);
        let mut builder = http::Request::post(&uri)
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(
                http::header::ACCEPT,
                match encoded.framing {
                    Framing::Sse => "text/event-stream",
                    Framing::Ndjson => "application/x-ndjson",
                },
            );
        match &self.auth {
            Auth::None => {}
            Auth::Bearer(key) => {
                builder = builder.header(
                    http::header::AUTHORIZATION,
                    format!("Bearer {}", key.expose()),
                );
            }
            Auth::Header { name, key } => builder = builder.header(*name, key.expose()),
            Auth::Query { name, key } => {
                uri.push(if uri.contains('?') { '&' } else { '?' });
                uri.push_str(name);
                uri.push('=');
                uri.push_str(key.expose());
                builder = builder.uri(&uri);
            }
        }
        for (name, value) in &encoded.headers {
            builder = builder.header(*name, value);
        }
        for (name, value) in self.headers.iter().chain(&request.headers) {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let body = serde_json::to_vec(&encoded.body)
            .map_err(|error| Error::unsupported(error.to_string()))?;
        let request = builder
            .body(bytes::Bytes::from(body))
            .map_err(|error| Error::unsupported(format!("invalid request: {error}")))?;
        Ok((request, encoded.framing))
    }
}

/// `overlay` deep-merged into `body`: objects merge key by key, anything
/// else replaces, and `null` removes.
pub fn merge(body: &mut Value, overlay: &serde_json::Map<String, Value>) {
    let Value::Object(map) = body else {
        *body = Value::Object(overlay.clone());
        return;
    };
    for (key, value) in overlay {
        match (map.get_mut(key), value) {
            (_, Value::Null) => {
                map.remove(key);
            }
            (Some(existing @ Value::Object(_)), Value::Object(inner)) => merge(existing, inner),
            _ => {
                map.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Reads an environment variable that may be unset or blank.
pub fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// How a host takes its credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyStyle {
    Bearer,
    Header(&'static str),
    Query(&'static str),
}

/// Where a host is and how it authenticates: the data a provider is built
/// from, with the protocol (and its quirks) that speaks to it.
#[derive(Clone, Copy, Debug)]
pub struct Host {
    pub id: &'static str,
    pub base_url: &'static str,
    /// Environment variables naming the credential, first set wins.
    pub key_env: &'static [&'static str],
    /// Environment variables overriding the base URL.
    pub base_url_env: &'static [&'static str],
    pub key: KeyStyle,
    /// Whether the host runs without a credential (a local server).
    pub keyless: bool,
}

impl Host {
    /// The provider for this host, `protocol` speaking to it, configured
    /// from the environment.
    pub fn provider(&self, protocol: impl Protocol) -> Result<Provider, Error> {
        let key = self.key_env.iter().find_map(|name| env(name));
        let auth = match (key, self.key) {
            (Some(key), KeyStyle::Bearer) => Auth::Bearer(Secret::new(key)),
            (Some(key), KeyStyle::Header(name)) => Auth::Header {
                name,
                key: Secret::new(key),
            },
            (Some(key), KeyStyle::Query(name)) => Auth::Query {
                name,
                key: Secret::new(key),
            },
            (None, _) if self.keyless => Auth::None,
            (None, _) => {
                return Err(Error::new(
                    crate::Kind::Auth,
                    format!("{} needs a key: set {}", self.id, self.key_env.join(" or ")),
                ));
            }
        };
        let base_url = self
            .base_url_env
            .iter()
            .find_map(|name| env(name))
            .unwrap_or_else(|| self.base_url.to_owned());
        Ok(Provider::new(self.id, base_url, auth, protocol))
    }
}
