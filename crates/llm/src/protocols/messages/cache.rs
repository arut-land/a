//! Prompt-cache breakpoints: the top-level marker the caller sets, and the
//! block markers manual caching places, within Anthropic's budget of four.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::tag;
use crate::error::Error;
use crate::json::Lenient;

/// The most `cache_control` markers Anthropic takes in one request.
const MAX_MARKERS: usize = 4;

/// A cache breakpoint's lifetime.
#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
enum Ttl {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

/// A `cache_control` marker.
fn ephemeral(ttl: Option<Ttl>) -> Value {
    let mut marker = json!({"type": "ephemeral"});
    if let Some(ttl) = ttl {
        marker["ttl"] = json!(ttl);
    }
    marker
}

/// The caching the request's options ask for: a top-level marker sent as
/// written, `prompt_caching` and `static_prefix_cache_ttl`.
pub(super) struct Caching {
    top: Option<Value>,
    manual: bool,
    static_ttl: Option<Ttl>,
}

impl Caching {
    pub(super) fn of(options: &Map<String, Value>) -> Result<Self, Error> {
        let set = |key| options.get(key).filter(|value| !value.is_null());
        let top = set("cache_control");
        let ttl = top.and_then(|marker| marker.at("/ttl"));
        if let Some(marker) = top
            && (marker.str("type") != Some("ephemeral")
                || ttl.is_some_and(|_| tag::<Ttl>(ttl).is_none()))
        {
            return Err(Error::unsupported(format!(
                "an Anthropic `cache_control` option must be an ephemeral marker with no ttl, \
                 \"5m\" or \"1h\": {marker}"
            )));
        }
        let static_ttl = set("static_prefix_cache_ttl");
        if let Some(ttl) = static_ttl.filter(|_| tag::<Ttl>(static_ttl).is_none()) {
            let unknown = format!("`static_prefix_cache_ttl` must be \"5m\" or \"1h\": {ttl}");
            return Err(Error::unsupported(unknown));
        }
        Ok(Self {
            top: top.cloned(),
            manual: options.get("prompt_caching") == Some(&Value::Bool(true)),
            static_ttl: tag(static_ttl),
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
            tool.retain(|key, value| key != "cache_control" || !value.is_null());
        }
        let top = self.top.as_ref();
        let budget = MAX_MARKERS - usize::from(top.is_some());
        let held = tools.iter().filter_map(|tool| tool.get("cache_control"));
        let held = held.count();
        let Some(mut remaining) = budget.checked_sub(held) else {
            return Err(Error::unsupported(format!(
                "Too many Anthropic tool `cache_control` markers: {held} exceeds the available \
                 prompt caching budget of {budget}"
            )));
        };
        let top_ttl = top.and_then(|top| tag(top.get("ttl")));
        if self.static_ttl == Some(Ttl::FiveMinutes) && top_ttl == Some(Ttl::OneHour) {
            return Err(Error::unsupported(
                "a five-minute `static_prefix_cache_ttl` conflicts with the one-hour top-level \
                 `cache_control`: Anthropic requires 1h markers to precede 5-minute ones, and \
                 the static prefix precedes the conversation tail",
            ));
        }
        let unmarked = |block: &&mut Map<String, Value>| !block.contains_key("cache_control");
        if self.manual || self.static_ttl.is_some() {
            let marker = ephemeral(self.static_ttl.or(top_ttl));
            let last_tool = tools
                .iter_mut()
                .filter_map(Value::as_object_mut)
                .rfind(|tool| tool.get("defer_loading") != Some(&Value::Bool(true)));
            if let Some(tool) = last_tool.filter(unmarked) {
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
            let system = system.last_mut().and_then(Value::as_object_mut);
            if let Some(block) = system.filter(unmarked).filter(|_| remaining > 0) {
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
        let content = messages.iter().flat_map(|message| message.arr("content"));
        let markers = tools.iter().chain(&*system).chain(content);
        let markers = markers.filter_map(|block| block.get("cache_control"));
        let long = markers
            .chain(top)
            .map(|marker| tag(marker.get("ttl")) == Some(Ttl::OneHour));
        if !long.is_sorted_by(|earlier, later| earlier >= later) {
            return Err(Error::unsupported(
                "Anthropic cache_control markers with ttl `1h` must appear before markers with \
                 the default 5-minute TTL",
            ));
        }
        Ok(())
    }
}
