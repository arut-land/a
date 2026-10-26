//! A wire protocol is two pure halves: `encode` turns a [`Request`] into an
//! HTTP request body for one provider, and a [`Decoder`] turns the reply's
//! frames into [`Event`]s. A [`Provider`] is plain data: where to send, how
//! to authenticate, and the protocol instance (with its host quirks) that
//! speaks to it.

use std::fmt;
use std::sync::Arc;

use serde_json::Value;

use crate::conversation::{History, Pieces};
use crate::error::{Error, Kind};
use crate::http::Payload;
use crate::spec::{Event, ProtocolKey, Request, SmolStr};

pub trait Protocol: Send + Sync + 'static {
    /// Which protocol this is: the key its options and part state go
    /// under, and the protocol an [`Origin`](crate::Origin) names.
    fn key(&self) -> ProtocolKey;

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

/// A protocol with known hosts, each with the quirks it speaks it with.
pub trait Hosted: Protocol + Sized {
    type Quirks: Copy + 'static;

    const PROTOCOL: ProtocolKey;
    const HOSTS: &'static [(Host, Self::Quirks)];

    fn with(quirks: Self::Quirks) -> Self;
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
    /// The body but the conversation.
    pub body: Value,
    /// The conversation, written into the body at its key; `None` when
    /// options set that field themselves.
    pub history: Option<History>,
    /// The tool definitions, written into the body at their key from the
    /// bytes the tool set keeps; `None` when there are none, or options
    /// set that field themselves.
    pub tools: Option<History>,
    pub headers: Vec<(&'static str, String)>,
}

impl Encoded {
    /// The body's bytes: its fields in key order, the conversation's
    /// settled items and the tool definitions spliced in as they are.
    pub fn payload(&self) -> Payload {
        let mut out = Pieces::default();
        let Value::Object(fields) = &self.body else {
            out.json(&self.body);
            return out.finish().into();
        };
        out.byte(b'{');
        let mut spliced: Vec<&History> =
            [&self.history, &self.tools].into_iter().flatten().collect();
        spliced.sort_by_key(|spliced| spliced.key);
        let mut spliced = spliced.into_iter().peekable();
        let mut first = true;
        let mut field = |out: &mut Pieces, key: &str| {
            if !std::mem::take(&mut first) {
                out.byte(b',');
            }
            out.json(key);
            out.byte(b':');
        };
        for (key, value) in fields {
            while let Some(array) = spliced.next_if(|array| array.key < key.as_str()) {
                field(&mut out, array.key);
                array.write(&mut out);
            }
            field(&mut out, key);
            out.json(value);
        }
        for array in spliced {
            field(&mut out, array.key);
            array.write(&mut out);
        }
        out.byte(b'}');
        out.finish().into()
    }

    /// The body as one value, the conversation in it.
    #[cfg(test)]
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.payload().to_vec()).unwrap_or_default()
    }
}

/// One event of a reply's server-sent event stream.
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

    /// The reader stopped reading (an interrupt): what is open ends as far
    /// as it got, so the reply's text so far reaches the consumer.
    fn cut(&mut self, out: &mut Vec<Event>) {
        let _ = out;
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
    pub compression: Option<transport::Compression>,
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
            .field("protocol", &self.protocol.key().as_str())
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
        Self::build(id.into(), base_url.into(), auth, Arc::new(protocol))
    }

    /// [`Provider::new`] past its generic arguments, compiled once.
    fn build(id: SmolStr, base_url: String, auth: Auth, protocol: Arc<dyn Protocol>) -> Self {
        Self {
            id,
            compression: None,
            base_url: String::new(),
            auth,
            headers: Vec::new(),
            protocol,
        }
        .with_base_url(base_url)
    }

    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        let mut base_url = base_url.into();
        base_url.truncate(base_url.trim_end_matches('/').len());
        if self.base_url != base_url {
            self.compression = None;
        }
        self.base_url = base_url;
        self
    }

    /// The HTTP request `request` becomes, prepared and encoded for
    /// `model`.
    pub fn request(&self, model: &str, request: &Request) -> Result<http::Request<Payload>, Error> {
        let target = Target {
            provider: self,
            model,
        };
        let limits = crate::fit::Limits::of_protocol(self.protocol.key(), model);
        let (encoded, payload) = crate::fit::fit(request, limits, |request| {
            let mut encoded = self.protocol.encode(&target, request)?;
            if let Some(overlay) = &request.body {
                // A conversation, or tools, the escape hatch states are the ones
                // sent.
                encoded.history = encoded.history.filter(|h| !overlay.contains_key(h.key));
                encoded.tools = encoded.tools.filter(|t| !overlay.contains_key(t.key));
                crate::json::overlay(&mut encoded.body, overlay);
            }
            Ok(encoded)
        })?;
        let mut uri = format!("{}{}", self.base_url, encoded.path);
        let mut builder = http::Request::post(&uri)
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(http::header::ACCEPT, "text/event-stream");
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
                let separator = if uri.contains('?') { '&' } else { '?' };
                uri = format!("{uri}{separator}{name}={}", key.expose());
                builder = builder.uri(&uri);
            }
        }
        for (name, value) in &encoded.headers {
            builder = builder.header(*name, value);
        }
        for (name, value) in self.headers.iter().chain(&request.headers) {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if let Some(compression) = self.compression {
            builder = builder.extension(compression);
        }
        builder
            .body(payload)
            .map_err(|error| Error::unsupported(format!("invalid request: {error}")))
    }
}

/// Looks up the variables that configure a host: keys and base URLs.
pub type Vars<'a> = dyn Fn(&str) -> Option<String> + 'a;

/// Reads a variable of this process's environment: the lookup
/// [`Host::provider`] takes when nothing else configures the host.
pub fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Variable `name` from `lookup`, unless unset or blank.
fn var(lookup: &Vars<'_>, name: &str) -> Option<String> {
    lookup(name)
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

impl KeyStyle {
    /// The authentication `key` goes as.
    pub fn auth(self, key: Secret) -> Auth {
        match self {
            Self::Bearer => Auth::Bearer(key),
            Self::Header(name) => Auth::Header { name, key },
            Self::Query(name) => Auth::Query { name, key },
        }
    }
}

/// Where a host is and how it authenticates: the data a provider is built
/// from, with the protocol (and its quirks) that speaks to it.
#[derive(Clone, Copy, Debug)]
pub struct Host {
    /// Request coding verified for this endpoint; compatible gateways opt in separately.
    pub compression: Option<transport::Compression>,
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
    /// A host taking a bearer key, at a base URL no variable overrides.
    pub const fn bearer(
        id: &'static str,
        base_url: &'static str,
        key_env: &'static [&'static str],
    ) -> Self {
        Self {
            id,
            base_url,
            key_env,
            base_url_env: &[],
            key: KeyStyle::Bearer,
            keyless: false,
            compression: None,
        }
    }

    /// The provider for this host, `protocol` speaking to it, configured
    /// from the variables `env` looks up (such as [`env()`], this process's).
    pub fn provider(&self, protocol: impl Protocol, env: &Vars<'_>) -> Result<Provider, Error> {
        self.configure(Arc::new(protocol), env)
    }

    /// [`Host::provider`] past its generic argument, compiled once.
    fn configure(&self, protocol: Arc<dyn Protocol>, env: &Vars<'_>) -> Result<Provider, Error> {
        let auth = match self.key_env.iter().find_map(|name| var(env, name)) {
            Some(key) => self.key.auth(Secret(key)),
            None if self.keyless => Auth::None,
            None => {
                return Err(Error::new(
                    Kind::Auth,
                    format!("{} needs a key: set {}", self.id, self.key_env.join(" or ")),
                ));
            }
        };
        let base_url = self.base_url_env.iter().find_map(|name| var(env, name));
        let base_url = base_url.as_deref().unwrap_or(self.base_url);
        let mut provider = Provider::build(self.id.into(), base_url.into(), auth, protocol);
        if base_url == self.base_url {
            provider.compression = self.compression;
        }
        Ok(provider)
    }
}
