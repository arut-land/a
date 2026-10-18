//! Prompt-cache breakpoints: the top-level marker the caller sets, and the
//! block markers manual caching places, within Anthropic's budget of four.

use serde_json::{Map, Value, json};

use crate::error::Error;
use crate::json::Lenient;

/// The most `cache_control` markers Anthropic takes in one request.
const MAX_MARKERS: usize = 4;

/// A cache breakpoint's lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ttl {
    FiveMinutes,
    OneHour,
}

impl Ttl {
    fn parse(value: &Value) -> Option<Self> {
        match value.as_str()? {
            "5m" => Some(Self::FiveMinutes),
            "1h" => Some(Self::OneHour),
            _ => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
        }
    }
}

/// The caching the request's options ask for.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Caching {
    /// The top-level marker, checked, sent as written.
    pub(super) top: Option<Value>,
    /// Markers on the last tool, the system prompt and the conversation's
    /// last block.
    pub(super) manual: bool,
    /// The static prefix's own lifetime: markers on the last tool and the
    /// system prompt.
    pub(super) static_ttl: Option<Ttl>,
}

impl Caching {
    pub(super) fn of(options: &Map<String, Value>) -> Result<Self, Error> {
        let top = match options.get("cache_control") {
            None | Some(Value::Null) => None,
            Some(marker) => {
                let ttl = marker.get("ttl").filter(|ttl| !ttl.is_null());
                if marker.str("type") != Some("ephemeral")
                    || ttl.is_some_and(|ttl| Ttl::parse(ttl).is_none())
                {
                    return Err(Error::unsupported(format!(
                        "an Anthropic `cache_control` option must be an ephemeral marker with \
                         no ttl, \"5m\" or \"1h\": {marker}"
                    )));
                }
                Some(marker.clone())
            }
        };
        let static_ttl = match options.get("static_prefix_cache_ttl") {
            None | Some(Value::Null) => None,
            Some(ttl) => Some(Ttl::parse(ttl).ok_or_else(|| {
                Error::unsupported(format!(
                    "`static_prefix_cache_ttl` must be \"5m\" or \"1h\": {ttl}"
                ))
            })?),
        };
        Ok(Self {
            top,
            manual: options.get("prompt_caching").and_then(Value::as_bool) == Some(true),
            static_ttl,
        })
    }

    /// Place the breakpoints, one fewer with a top-level marker. Markers a
    /// tool already carries are kept and count toward the budget, and every
    /// one-hour marker must precede the five-minute ones.
    pub(super) fn place(
        &self,
        system: &mut [Value],
        messages: &mut [Value],
        tools: &mut [Value],
    ) -> Result<(), Error> {
        for tool in tools.iter_mut().filter_map(Value::as_object_mut) {
            if tool.get("cache_control").is_some_and(Value::is_null) {
                tool.remove("cache_control");
            }
        }
        let top = self.top.as_ref();
        let budget = MAX_MARKERS - usize::from(top.is_some());
        let held = tools
            .iter()
            .filter(|tool| tool.get("cache_control").is_some())
            .count();
        let Some(mut remaining) = budget.checked_sub(held) else {
            return Err(Error::unsupported(format!(
                "Too many Anthropic tool `cache_control` markers: {held} exceeds the available \
                 prompt caching budget of {budget}"
            )));
        };
        let top_ttl = top.and_then(|top| Ttl::parse(top.get("ttl")?));
        if self.static_ttl == Some(Ttl::FiveMinutes) && top_ttl == Some(Ttl::OneHour) {
            return Err(Error::unsupported(
                "a five-minute `static_prefix_cache_ttl` conflicts with the one-hour top-level \
                 `cache_control`: Anthropic requires 1h markers to precede 5-minute ones, and \
                 the static prefix precedes the conversation tail",
            ));
        }
        if self.manual || self.static_ttl.is_some() {
            let marker = ephemeral(self.static_ttl.or(top_ttl));
            let last_tool = tools
                .iter_mut()
                .filter_map(Value::as_object_mut)
                .rfind(|tool| tool.get("defer_loading") != Some(&Value::Bool(true)));
            if let Some(tool) = last_tool
                && !tool.contains_key("cache_control")
            {
                if remaining == 0 {
                    return Err(Error::unsupported(
                        "Anthropic manual prompt caching requires a cache_control marker on the \
                         final non-deferred tool, but explicit tool markers exhaust the available \
                         cache point budget",
                    ));
                }
                tool.insert("cache_control".into(), marker.clone());
                remaining -= 1;
            }
            if remaining > 0
                && let Some(Value::Object(block)) = system.last_mut()
                && !block.contains_key("cache_control")
            {
                block.insert("cache_control".into(), marker);
                remaining -= 1;
            }
        }
        if self.manual && top.is_none() && remaining > 0 {
            let block = messages
                .last_mut()
                .filter(|message| message.str("role") != Some("assistant"))
                .and_then(|message| message.get_mut("content")?.as_array_mut()?.last_mut())
                .and_then(Value::as_object_mut);
            if let Some(block) = block {
                block.insert("cache_control".into(), ephemeral(None));
            }
        }
        let mut short_seen = false;
        let markers = tools
            .iter()
            .chain(system.iter())
            .chain(messages.iter().flat_map(|message| message.arr("content")))
            .filter_map(|block| block.get("cache_control"))
            .chain(top);
        for marker in markers {
            if marker.str("ttl") != Some("1h") {
                short_seen = true;
            } else if short_seen {
                return Err(Error::unsupported(
                    "Anthropic cache_control markers with ttl `1h` must appear before markers \
                     with the default 5-minute TTL",
                ));
            }
        }
        Ok(())
    }
}

/// A `cache_control` marker.
fn ephemeral(ttl: Option<Ttl>) -> Value {
    match ttl {
        Some(ttl) => json!({"type": "ephemeral", "ttl": ttl.as_str()}),
        None => json!({"type": "ephemeral"}),
    }
}
