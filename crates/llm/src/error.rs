use std::fmt;
use std::time::Duration;

/// Why a call failed, classified so a caller can decide whether to retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Kind {
    /// The request never got a reply: connect, TLS, a reset, a timeout.
    Transport,
    Auth,
    RateLimit,
    /// Out of credit or quota; waiting does not help.
    Quota,
    /// The prompt does not fit the model's context.
    ContextOverflow,
    /// The provider rejected the request as malformed or unsupported.
    InvalidRequest,
    ContentFilter,
    /// The provider failed (5xx, an overloaded error mid-stream).
    Server,
    /// The reply could not be understood.
    Decode,
    /// The reply ended before the provider finished it.
    Incomplete,
    /// The request asks for something this protocol cannot express.
    Unsupported,
    Other,
}

impl Kind {
    /// Whether the same request may succeed if sent again.
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::Transport | Self::RateLimit | Self::Server | Self::Incomplete | Self::Other
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Error {
    pub kind: Kind,
    pub message: String,
    pub status: Option<u16>,
    /// A wait the provider asked for.
    pub retry_after: Option<Duration>,
    /// The provider's error body, bounded.
    pub body: Option<String>,
    pub request_id: Option<String>,
}

/// Error bodies and frames quoted in messages are cut to this many bytes.
const QUOTE: usize = 2000;

impl Error {
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            retry_after: None,
            body: None,
            request_id: None,
        }
    }

    pub fn decode(message: impl fmt::Display, frame: &str) -> Self {
        Self::new(Kind::Decode, format!("{message}: {}", quote(frame)))
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(Kind::Unsupported, message)
    }

    pub fn incomplete() -> Self {
        Self::new(
            Kind::Incomplete,
            "the reply ended before the provider finished it",
        )
    }

    /// Whether the same request may succeed if sent again.
    pub const fn is_retryable(&self) -> bool {
        self.kind.is_retryable()
    }

    /// The error a non-success reply means, from its status, headers and
    /// body. Protocols refine the message from their error envelope.
    pub fn from_status(status: u16, headers: &http::HeaderMap, body: &[u8]) -> Self {
        let text = String::from_utf8_lossy(body);
        let header = |name: &str| headers.get(name)?.to_str().ok();
        let kind = match status {
            401 | 403 => Kind::Auth,
            402 => Kind::Quota,
            408 | 409 => Kind::Transport,
            413 => Kind::ContextOverflow,
            429 if is_quota(&text) => Kind::Quota,
            429 => Kind::RateLimit,
            400..=499 if is_overflow(&text) => Kind::ContextOverflow,
            400..=499 => Kind::InvalidRequest,
            500..=599 => Kind::Server,
            _ => Kind::Other,
        };
        // The provider's own verdict wins.
        let kind = match (header("x-should-retry"), kind) {
            (Some("false"), Kind::RateLimit) => Kind::Quota,
            (Some("false"), kind) if kind.is_retryable() => Kind::InvalidRequest,
            (Some("true"), kind) if !kind.is_retryable() && kind != Kind::Auth => Kind::Server,
            (_, kind) => kind,
        };
        Self {
            status: Some(status),
            retry_after: retry_after(headers),
            body: Some(quote(&text)),
            request_id: ["x-request-id", "request-id", "x-goog-request-id"]
                .into_iter()
                .find_map(header)
                .map(str::to_owned),
            ..Self::new(
                kind,
                message_of(&text).unwrap_or_else(|| format!("HTTP {status}")),
            )
        }
    }

    /// An error the provider reported inside a successful reply.
    pub fn provider(code: Option<&str>, message: impl Into<String>) -> Self {
        let message = message.into();
        let kind = match code.unwrap_or_default() {
            "rate_limit_exceeded" | "rate_limit_error" | "RESOURCE_EXHAUSTED" | "429" => {
                Kind::RateLimit
            }
            "insufficient_quota" | "billing_error" => Kind::Quota,
            "context_length_exceeded" => Kind::ContextOverflow,
            "invalid_request_error" | "invalid_prompt" | "INVALID_ARGUMENT" | "400" => {
                if is_overflow(&message) {
                    Kind::ContextOverflow
                } else {
                    Kind::InvalidRequest
                }
            }
            "authentication_error"
            | "permission_error"
            | "PERMISSION_DENIED"
            | "UNAUTHENTICATED" => Kind::Auth,
            "overloaded_error" | "api_error" | "server_error" | "INTERNAL" | "UNAVAILABLE"
            | "DEADLINE_EXCEEDED" | "500" | "502" | "503" | "504" | "529" => Kind::Server,
            _ if is_overflow(&message) => Kind::ContextOverflow,
            _ => Kind::Other,
        };
        Self::new(kind, message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(status) => write!(f, "{} (HTTP {status})", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::new(Kind::Transport, error.to_string())
    }
}

pub(crate) fn quote(text: &str) -> String {
    if text.len() <= QUOTE {
        return text.to_owned();
    }
    let mut end = QUOTE;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// The human message inside a JSON error envelope: `error.message`,
/// `message`, `error` as a string, or `detail`.
fn message_of(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let value = match &value {
        serde_json::Value::Array(items) => items.first()?,
        value => value,
    };
    [
        value.pointer("/error/message"),
        value.get("message"),
        value.get("error").filter(|error| error.is_string()),
        value.get("detail"),
    ]
    .into_iter()
    .flatten()
    .find_map(|message| message.as_str().filter(|text| !text.is_empty()))
    .map(str::to_owned)
}

fn retry_after(headers: &http::HeaderMap) -> Option<Duration> {
    let get = |name: &str| headers.get(name)?.to_str().ok()?.trim().parse::<f64>().ok();
    let seconds = get("retry-after-ms")
        .map(|ms| ms / 1000.0)
        .or_else(|| get("retry-after"))?;
    (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds))
}

fn is_quota(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    [
        "insufficient_quota",
        "quota exceeded",
        "exceeded your current quota",
        "billing",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

/// Whether an error message says the prompt is too long. Throttling messages
/// that mention limits are not overflows.
pub(crate) fn is_overflow(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    if text.contains("rate limit") || text.contains("rate_limit") {
        return false;
    }
    [
        "context_length_exceeded",
        "context length",
        "context window",
        "maximum context",
        "prompt is too long",
        "too many tokens",
        "input is too long",
        "exceeds the maximum",
        "reduce the length",
        "token limit",
        "tokens exceeds",
        "request too large",
        "input token count",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

#[cfg(test)]
mod tests;
