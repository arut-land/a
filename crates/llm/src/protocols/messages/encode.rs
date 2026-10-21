//! A request as a Messages body. Turns this protocol produced replay their
//! provider blocks verbatim: signed thinking, redacted thinking, calls with
//! their `caller`, cited text, server-tool steps. A part edited since, or
//! from another origin, is rebuilt from its shared fields as pi rebuilds it:
//! thinking without its signature goes as text.

use std::num::NonZeroU32;

use std::borrow::Cow;

use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::{Map, Value, json};

use crate::json::obj;

use super::cache::{Caching, Markers};
use super::{
    Dialect, FINE_GRAINED_TOOL_STREAMING_BETA, KEY, PROTOCOL, Quirks, THINKING_BINDING_BETA,
    ToolInputStreaming, options, strict,
};
use crate::conversation::{self, Fold, History, Settled};
use crate::error::Error;
use crate::json::Lenient;
use crate::models::anthropic::{
    binds_context, rejects_forced_tool_choice, takes_mid_conversation_system,
};
use crate::protocol::{Encoded, Target};
use crate::replay::{Replay, replay, replays};
use crate::spec::{
    Assistant, AssistantPart, Format, Json, Media, Message, Output, Request, Source, System, Tool,
    ToolChoice, ToolResult, ToolWire, UserPart,
};

/// What an assistant image goes as: assistant turns take no images.
pub(super) const ASSISTANT_IMAGE_OMITTED: &str = "(image omitted: model does not support images)";

/// The request for `request` to `model`, but its path and headers, and the
/// `anthropic-beta` header it needs: the request's own fields with the
/// cache breakpoints placed inside them, then the mapped generation
/// settings, then the caller's options, then the rewrites the API requires.
pub(super) fn body(
    quirks: Quirks,
    target: &Target<'_>,
    request: &Request,
) -> Result<(Encoded, Option<String>), Error> {
    let model = target.model;
    let mut options = request.options_for(PROTOCOL);
    let max_tokens = request.generation.max_output_tokens.map(NonZeroU32::get);
    let Some(max_tokens) = max_tokens.or_else(|| quirks.default_max_tokens(model)) else {
        let unset = format!("`max_tokens` must be set for the model {model}");
        return Err(Error::unsupported(unset));
    };
    let caching = Caching::of(&options)?;
    let wire = ToolWire::Messages {
        strict: quirks.strict_tool_schemas,
        eager: quirks.tool_input_streaming == ToolInputStreaming::Eager,
    };
    let typed = (!request.tools.is_empty())
        .then(|| request.tools.encoded(wire, |tool| tool_json(tool, quirks)));
    let mut typed = typed.map_or_default(|run| run.to_vec());
    // The options the encoder reads rather than sends.
    let [raw_tools, betas] = ["tools", "betas"].map(|key| options.remove(key));
    for key in ["prompt_caching", "static_prefix_cache_ttl"] {
        options.remove(key);
    }
    let mut tools = match raw_tools {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(raw)) => raw,
        Some(_) => return Err(Error::unsupported("the `tools` option must be an array")),
    };
    // Only relocate a typed tool when it will receive the breakpoint;
    // otherwise its cached declaration must stay in `typed`.
    let deferred = |tool: &Value| tool.get("defer_loading") == Some(&Value::Bool(true));
    if caching.marks_tools() && tools.iter().all(deferred) && typed.pop().is_some() {
        let last = request.tools.last().expect("a tool was encoded");
        tools.insert(0, tool_json(last, quirks));
    }
    let has_tools = !typed.is_empty() || !tools.is_empty();
    let mid = takes_mid_conversation_system(model);
    let (turns, settled) = conversation::fold(&request.messages, target, Turns::new(mid))?;
    let Ended {
        mut system,
        mut messages,
        container,
        markers,
    } = turns.finish()?;
    caching.place(&mut system, &mut messages, &mut tools, markers)?;
    let mut body = obj!({"model": model, "max_tokens": max_tokens});
    if !system.is_empty() {
        body["system"] = system.into();
    }
    if let Some(choice) = tool_choice(quirks.dialect, model, request, has_tools) {
        body["tool_choice"] = choice;
    }
    if let Some(Format::Json { schema, .. }) = &request.format
        && let Some(schema) = schema
    {
        let mut schema = schema.clone();
        crate::protocols::schema::anthropic(&mut schema);
        body["output_config"] = obj!({"format": obj!({"type": "json_schema", "schema": schema})});
    }
    if !options.contains_key("container")
        && let Some(container) = container
    {
        body["container"] = container.into();
    }
    let history =
        (!options.contains_key("messages")).then(|| History::new("messages", settled, messages));
    let tools = has_tools.then(|| History::new("tools", typed, tools));
    let mapped = options::generation(quirks, model, &request.generation, max_tokens);
    crate::json::overlay(&mut body, &mapped);
    crate::json::overlay(&mut body, &options);
    let unbound =
        quirks.dialect == Dialect::Anthropic && binds_context(model) && drop_unbound(&mut body);
    // Anthropic rejects a tool choice without tools.
    if !has_tools {
        if let Some(fields) = body.as_object_mut() {
            fields.remove("tool_choice");
        }
    } else if body.get("tool_choice").is_none() {
        body["tool_choice"] = json!({"type": "auto"});
    }
    body["stream"] = true.into();
    let fine_grained =
        !request.tools.is_empty() && quirks.tool_input_streaming == ToolInputStreaming::BetaHeader;
    let mut betas: Vec<&str> = match &betas {
        Some(Value::Array(betas)) => betas.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let needed = [
        unbound.then_some(THINKING_BINDING_BETA),
        fine_grained.then_some(FINE_GRAINED_TOOL_STREAMING_BETA),
    ];
    let missing: Vec<&str> = needed
        .into_iter()
        .flatten()
        .filter(|beta| !betas.contains(beta))
        .collect();
    betas.extend(missing);
    let betas = (!betas.is_empty()).then(|| betas.join(","));
    let encoded = Encoded {
        path: String::new(),
        body,
        history,
        tools,
        headers: Vec::new(),
    };
    Ok((encoded, betas))
}

/// Ask Anthropic to drop a thinking block bound to another context rather
/// than reject the request, when `body` thinks adaptively (the default when
/// it names no thinking), the one mode that takes the binding. Whether it
/// asked.
fn drop_unbound(body: &mut Value) -> bool {
    let binding = json!({"prefix_mismatch_behavior": "drop_block"});
    match body.get_mut("thinking") {
        None => body["thinking"] = obj!({"type": "adaptive", "block_binding": binding}),
        Some(Value::Object(thinking)) if thinking.get("type").is_some_and(|t| t == "adaptive") => {
            thinking.entry("block_binding").or_insert(binding);
        }
        Some(_) => return false,
    }
    true
}

/// A request tool: strict, its schema in the subset constrained decoding
/// compiles, where it asks and the host implements it; asking for its input
/// as it is written when the host takes the field.
fn tool_json(tool: &Tool, quirks: Quirks) -> Value {
    let strict = tool.strict == Some(true) && quirks.strict_tool_schemas;
    let mut schema = tool.schema.clone();
    if strict {
        strict::sanitize(&mut schema);
    }
    let (name, description) = (tool.name.as_str(), tool.description.as_str());
    let mut definition = obj!({"name": name, "description": description, "input_schema": schema});
    if strict {
        definition["strict"] = true.into();
    }
    if quirks.tool_input_streaming == ToolInputStreaming::Eager {
        definition["eager_input_streaming"] = true.into();
    }
    definition
}

/// The request's `tool_choice` before the options merge over it. A forced
/// choice a model rejects is left to the model; `parallel_tool_calls: false`
/// sets `disable_parallel_tool_use` where the dialect takes it.
fn tool_choice(dialect: Dialect, model: &str, request: &Request, has_tools: bool) -> Option<Value> {
    let forced_rejected = dialect == Dialect::Anthropic && rejects_forced_tool_choice(model);
    let mut choice = match &request.tool_choice {
        ToolChoice::None => return Some(json!({"type": "none"})),
        ToolChoice::Auto => None,
        ToolChoice::Required | ToolChoice::Tool(_) if forced_rejected => None,
        ToolChoice::Required => Some(json!({"type": "any"})),
        ToolChoice::Tool(name) => Some(json!({"type": "tool", "name": name})),
    };
    if request.parallel_tool_calls == Some(false) && has_tools && dialect.takes_parallel_off() {
        choice.get_or_insert_with(|| json!({"type": "auto"}))["disable_parallel_tool_use"] =
            true.into();
    }
    choice
}

/// One turn of the conversation: tool results join the user turn they lead.
#[derive(Clone, Copy)]
enum Turn<'a> {
    System(&'a System),
    User(&'a [Message]),
    Assistant(&'a Assistant),
}

/// A content block as the request sends it: one the encoder builds, typed
/// and borrowed from the conversation, or any other as a tree (a provider
/// block replayed, a block a part's state adds fields to). Each writes its
/// fields in key order, as a tree of it would.
enum Block<'a> {
    Text(Cow<'a, str>),
    Result {
        id: &'a str,
        content: Vec<Block<'a>>,
        error: bool,
    },
    Use {
        id: &'a str,
        name: &'a str,
        input: &'a Json,
    },
    Tree(Value),
}

impl Block<'_> {
    /// A block as a tree, for state that adds fields to it.
    fn tree(self) -> Value {
        match self {
            Self::Tree(tree) => tree,
            block => serde_json::to_value(&block).unwrap_or_default(),
        }
    }

    /// The tree a block is, when it is one: only such a block carries a
    /// cache marker before the request places its own.
    fn as_tree(&self) -> Option<&Value> {
        match self {
            Self::Tree(tree) => Some(tree),
            _ => None,
        }
    }
}

impl Serialize for Block<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Text(text) => {
                let mut block = s.serialize_map(Some(2))?;
                block.serialize_entry("text", text)?;
                block.serialize_entry("type", "text")?;
                block.end()
            }
            Self::Result { id, content, error } => {
                let mut block = s.serialize_map(None)?;
                block.serialize_entry("content", content)?;
                if *error {
                    block.serialize_entry("is_error", &true)?;
                }
                block.serialize_entry("tool_use_id", id)?;
                block.serialize_entry("type", "tool_result")?;
                block.end()
            }
            Self::Use { id, name, input } => {
                let mut block = s.serialize_map(Some(4))?;
                block.serialize_entry("id", id)?;
                block.serialize_entry("input", input)?;
                block.serialize_entry("name", name)?;
                block.serialize_entry("type", "tool_use")?;
                block.end()
            }
            Self::Tree(tree) => tree.serialize(s),
        }
    }
}

/// A message of blocks, its fields in key order.
#[derive(serde::Serialize)]
struct Sent<'a> {
    content: Vec<Block<'a>>,
    role: &'static str,
}

/// What turns add to the request: top-level `system` blocks, messages, and
/// whether a system message may follow them.
struct Placed<'a> {
    system: Vec<Value>,
    sent: Vec<Sent<'a>>,
    follows: Option<bool>,
}

/// The turns of `messages`, placed after turns that left `follows`.
///
/// On a model that takes mid-conversation system messages (`mid`), one in
/// a position Anthropic rejects moves to the next valid slot: right after
/// the next user turn that ends the conversation or precedes an assistant
/// turn. Hoisting it into `system` instead would change the prompt prefix,
/// missing the cache and, on models that bind thinking to their context,
/// turning every earlier thinking block into an error. It is hoisted only
/// when no slot exists.
fn place(messages: &[Message], mid: bool, mut follows: Option<bool>) -> Result<Placed<'_>, Error> {
    fn message<'a>(role: &'static str, content: Vec<Block<'a>>) -> Option<Sent<'a>> {
        (!content.is_empty()).then_some(Sent { content, role })
    }
    let user = |message: &Message| matches!(message, Message::User(_) | Message::Tool(_));
    let turns: Vec<Turn<'_>> = messages
        .chunk_by(|earlier, later| user(earlier) && user(later))
        .map(|run| match &run[0] {
            Message::System(system) => Turn::System(system),
            Message::Assistant(assistant) => Turn::Assistant(assistant),
            Message::User(_) | Message::Tool(_) => Turn::User(run),
        })
        .collect();
    // For each turn: whether the next turn that is not a system message is
    // an assistant turn or none, and whether a user turn so placed (a slot)
    // follows it.
    let (mut before_assistant, mut slot_after) =
        (vec![true; turns.len()], vec![false; turns.len()]);
    let (mut next, mut slot) = (true, false);
    for (at, turn) in turns.iter().enumerate().rev() {
        (before_assistant[at], slot_after[at]) = (next, slot);
        match turn {
            Turn::System(_) => {}
            Turn::Assistant(_) => next = true,
            Turn::User(_) => (slot, next) = (slot || next, false),
        }
    }
    let (mut system, mut sent, mut deferred) = (Vec::new(), Vec::new(), Vec::new());
    // Whether a system message may follow the last turn that is not one: a
    // user turn, or an assistant turn ending in a server tool's result.
    // `None` while system messages lead the conversation: the prompt.
    for (at, turn) in turns.iter().enumerate() {
        match *turn {
            Turn::System(prompt) => {
                let text = prompt.text();
                if text.is_empty() {
                } else if mid && follows == Some(true) && before_assistant[at] {
                    sent.extend(message("system", vec![Block::Text(text.into())]));
                } else if mid && follows.is_some() && slot_after[at] {
                    deferred.push(text);
                } else {
                    system.push(text_block(&text));
                }
            }
            Turn::Assistant(assistant) => {
                // The `container` block goes to the request's top level.
                let kinds = assistant.content.iter().rev();
                let mut kinds = kinds.map(|part| opaque_item(part)?.str("type").map(str::to_owned));
                let last = kinds
                    .find(|kind| kind.as_deref() != Some("container"))
                    .flatten();
                follows = Some(last.is_some_and(|kind| kind.ends_with("_tool_result")));
                sent.extend(message("assistant", assistant_blocks(assistant)));
            }
            Turn::User(run) => {
                follows = Some(true);
                sent.extend(message("user", user_blocks(run)?));
                if before_assistant[at] && !deferred.is_empty() {
                    let text = Block::Text(deferred.join("\n\n").into());
                    sent.extend(message("system", vec![text]));
                    deferred.clear();
                }
            }
        }
    }
    Ok(Placed {
        system,
        sent,
        follows,
    })
}

/// The conversation, a message at a time. The turns up to an assistant
/// message settle with it; those after it wait for the request, which
/// places its breakpoints on the last. A system message mid-conversation,
/// on a model that takes one there, goes where every turn after it says,
/// so from it on nothing settles.
#[derive(Clone)]
pub(super) struct Turns {
    /// Whether the model takes mid-conversation system messages.
    mid: bool,
    /// The top-level system blocks.
    system: Vec<Value>,
    follows: Option<bool>,
    /// The messages since the turns that settled.
    open: Vec<Message>,
    /// Whether a message other than a system message came.
    begun: bool,
    /// Whether a system message came mid-conversation on `mid`.
    pinned: bool,
    /// The id of the container the last turn holding one ran in.
    container: Option<String>,
    /// The markers of the messages settled.
    markers: Markers,
}

/// The system blocks, the messages the request makes, the container and
/// the markers settled.
pub(super) struct Ended {
    pub system: Vec<Value>,
    /// The messages after those settled: the request places its cache
    /// breakpoints on them.
    pub messages: Vec<Value>,
    pub container: Option<String>,
    pub markers: Markers,
}

impl Turns {
    pub(super) fn new(mid: bool) -> Self {
        Self {
            mid,
            system: Vec::new(),
            follows: None,
            open: Vec::new(),
            begun: false,
            pinned: false,
            container: None,
            markers: Markers::default(),
        }
    }

    /// The turns open, placed for this request.
    pub(super) fn finish(mut self) -> Result<Ended, Error> {
        let placed = place(&self.open, self.mid, self.follows)?;
        self.system.extend(placed.system);
        let messages = placed.sent.into_iter().map(|sent| {
            let content = sent
                .content
                .into_iter()
                .map(Block::tree)
                .collect::<Vec<_>>();
            obj!({"role": sent.role, "content": content})
        });
        let messages = messages.collect();
        Ok(Ended {
            system: self.system,
            messages,
            container: container(&self.open).or(self.container),
            markers: self.markers,
        })
    }
}

impl Fold for Turns {
    type Cx = bool;

    fn cx(&self) -> &bool {
        &self.mid
    }

    fn push(&mut self, _: usize, message: &Message, out: &mut Settled) -> Result<(), Error> {
        match message {
            Message::System(system) => {
                self.pinned |= self.mid && self.begun && !system.text().is_empty();
            }
            _ => self.begun = true,
        }
        self.open.push(message.clone());
        if self.pinned || !matches!(message, Message::Assistant(_)) {
            return Ok(());
        }
        let placed = place(&self.open, self.mid, self.follows)?;
        // A turn that sends nothing leaves the last message the one
        // before, which takes the request's breakpoint.
        let last = placed.sent.last();
        if last.map(|message| message.role) == Some("assistant") {
            let blocks = placed.sent.iter().flat_map(|sent| &sent.content);
            self.markers = self
                .markers
                .then(Markers::of(blocks.filter_map(Block::as_tree)));
            for sent in &placed.sent {
                out.item(sent);
            }
            self.system.extend(placed.system);
            self.follows = placed.follows;
            let id = container(&self.open);
            if id.is_some() {
                self.container = id;
            }
            self.open.clear();
        }
        Ok(())
    }
}

/// The provider item of an opaque part this protocol kept.
fn opaque_item(part: &AssistantPart) -> Option<Value> {
    match part {
        AssistantPart::Opaque(opaque) => crate::replay::item(&opaque.meta, KEY),
        _ => None,
    }
}

/// The id of the container the last turn holding one ran in. Anthropic
/// requires it on a request that answers a programmatic tool call, and it
/// keeps a code-execution session's state.
fn container(messages: &[Message]) -> Option<String> {
    let parts = messages.iter().rev().flat_map(|message| match message {
        Message::Assistant(assistant) => assistant.content.iter().rev(),
        _ => [].iter().rev(),
    });
    let items = parts.filter_map(opaque_item);
    let mut containers = items.filter(|item| item.str("type") == Some("container"));
    containers.find_map(|item| Some(item.at("/container/id")?.as_str()?.to_owned()))
}

fn text_block(text: &str) -> Value {
    obj!({"type": "text", "text": text})
}

/// A user turn's blocks. Anthropic requires its tool results before its
/// text, and rejects blank text. A part's state under [`KEY`] names fields
/// its block takes, such as a document's `title` or a `cache_control`
/// marker.
fn user_blocks(run: &[Message]) -> Result<Vec<Block<'_>>, Error> {
    let (mut results, mut parts) = (Vec::new(), Vec::new());
    for message in run {
        match message {
            Message::Tool(tool) => {
                for result in &tool.results {
                    results.push(result_json(result)?);
                }
            }
            Message::User(user) => {
                for part in &user.content {
                    let (block, meta) = match part {
                        UserPart::Text(text) if text.text.trim().is_empty() => continue,
                        UserPart::Text(text) => {
                            (Block::Text(text.text.as_str().into()), &text.meta)
                        }
                        UserPart::Media(media) => (Block::Tree(media_block(media)?), &media.meta),
                    };
                    parts.push(match meta.get(KEY) {
                        Some(Value::Object(fields)) => {
                            let mut block = block.tree();
                            crate::json::overlay(&mut block, &fields);
                            Block::Tree(block)
                        }
                        _ => block,
                    });
                }
            }
            Message::System(_) | Message::Assistant(_) => {}
        }
    }
    results.append(&mut parts);
    Ok(results)
}

fn result_json(result: &ToolResult) -> Result<Block<'_>, Error> {
    let content = match &result.output {
        Output::Text(text) => vec![Block::Text(text.as_str().into())],
        Output::Json(value) => vec![Block::Text(value.raw().into())],
        Output::Content(parts) => parts
            .iter()
            .map(|part| match part {
                UserPart::Text(text) => Ok(Block::Text(text.text.as_str().into())),
                UserPart::Media(media) => media_block(media).map(Block::Tree),
            })
            .collect::<Result<_, _>>()?,
    };
    Ok(Block::Result {
        id: result.id.as_str(),
        content,
        error: result.error,
    })
}

/// An image or a document block for `media`. An image goes as base64 data
/// of a type Anthropic reads, a URL, or a Files API id; a document as a
/// Files API id, a PDF as data or by URL, or any other typed document as
/// the text it holds.
fn media_block(media: &Media) -> Result<Value, Error> {
    let kind = media.media_type.as_str();
    let image = kind.starts_with("image/");
    let pdf = kind == "application/pdf";
    let readable = ["image/jpeg", "image/png", "image/gif", "image/webp"].contains(&kind);
    let source = match &media.source {
        _ if kind.starts_with("audio/") || kind.starts_with("video/") => None,
        Source::File { id } => Some(obj!({"type": "file", "file_id": id.as_str()})),
        // Anthropic's document URL source is for PDFs and names no type, so
        // an untyped URL is one.
        Source::Url { url } if image || pdf || kind.is_empty() => {
            Some(obj!({"type": "url", "url": url.as_str()}))
        }
        Source::Data { data } if pdf || readable => {
            Some(obj!({"type": "base64", "media_type": kind, "data": data.as_str()}))
        }
        Source::Url { .. } => None,
        Source::Data { .. } if image || kind.is_empty() => None,
        Source::Data { data } => base64(data)
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .map(|text| obj!({"type": "text", "media_type": "text/plain", "data": text})),
    };
    let source = source.ok_or_else(|| {
        let kind = if kind.is_empty() { "untyped" } else { kind };
        Error::unsupported(format!("the Messages API cannot carry this {kind} media"))
    })?;
    Ok(obj!({"type": if image { "image" } else { "document" }, "source": source}))
}

/// Standard base64 decoded, padding optional; `None` for anything else.
fn base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut word, mut bits) = (0_u32, 0);
    for byte in text.trim_end_matches('=').bytes() {
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        word = word << 6 | u32::from(digit);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((word >> bits).to_le_bytes()[0]);
        }
    }
    // A last digit alone carries no whole byte.
    (bits < 6).then_some(out)
}

/// The keys of a stale item a rebuilt part keeps: the `caller` that ties a
/// call to the code execution that made it.
fn identity(item: &Value) -> Map<String, Value> {
    match (item.str("type"), item.get("caller")) {
        (Some("tool_use"), Some(caller)) => {
            crate::json::object([("caller".into(), caller.clone())])
        }
        _ => Map::new(),
    }
}

/// An assistant turn's blocks: each part's provider block while it is
/// current, else the part rebuilt. A server tool's result goes only with
/// its use, and the reply's container goes as the request's `container`.
fn assistant_blocks(assistant: &Assistant) -> Vec<Block<'_>> {
    let mut uses: Vec<String> = Vec::new();
    let mut blocks = Vec::new();
    for part in &assistant.content {
        if let AssistantPart::Opaque(_) = part {
            let Some(item) = opaque_item(part).filter(|_| replays(part, KEY)) else {
                continue;
            };
            let kind = item.str("type").unwrap_or_default();
            let answered = item
                .str("tool_use_id")
                .is_some_and(|id| uses.iter().any(|u| u == id));
            if kind == "container" || kind.ends_with("_tool_result") && !answered {
                continue;
            }
            if kind.ends_with("_tool_use")
                && let Some(id) = item.str("id")
            {
                uses.push(id.to_owned());
            }
            blocks.push(Block::Tree(sendable(&item)));
            continue;
        }
        let identity = match replay(part, KEY, identity) {
            Replay::Item(item) => {
                let mut item = sendable(&item);
                if let AssistantPart::ToolCall(call) = part {
                    // The call takes the id its result answers.
                    item["id"] = call.id.as_str().into();
                }
                blocks.push(Block::Tree(item));
                continue;
            }
            Replay::Identity(identity) => identity,
            Replay::Rebuild => Map::new(),
        };
        blocks.push(match part {
            AssistantPart::Text(text) if !text.text.trim().is_empty() => {
                Block::Text(text.text.as_str().into())
            }
            // Thinking without its signature is text; a redacted block's
            // payload lived only in its item.
            AssistantPart::Reasoning(reasoning)
                if !reasoning.redacted && !reasoning.text.trim().is_empty() =>
            {
                Block::Text(reasoning.text.as_str().into())
            }
            AssistantPart::ToolCall(call) => {
                let (id, name) = (call.id.as_str(), call.name.as_str());
                let input = &call.input;
                let block = Block::Use { id, name, input };
                if identity.is_empty() {
                    block
                } else {
                    let mut block = block.tree();
                    if let Some(fields) = block.as_object_mut() {
                        fields.extend(identity);
                    }
                    Block::Tree(block)
                }
            }
            AssistantPart::Media(_) => Block::Text(ASSISTANT_IMAGE_OMITTED.into()),
            _ => continue,
        });
    }
    blocks
}

/// A streamed block as the input side takes it back. Input blocks are
/// validated strictly, so the core kinds keep only their input fields (a
/// gateway may add output-only ones such as `estimated_tokens`), and every
/// kind loses the fields a stream states as `null`.
fn sendable(item: &Value) -> Value {
    let keep: &[&str] = match item.str("type") {
        Some("thinking") => &["type", "thinking", "signature"],
        Some("redacted_thinking") => &["type", "data"],
        Some("text") => &["type", "text", "citations"],
        Some("tool_use") => &["type", "id", "name", "input", "caller"],
        _ => &[],
    };
    let Some(fields) = item.as_object() else {
        return item.clone();
    };
    let mut sent = fields.clone();
    sent.retain(|key, value| !value.is_null() && (keep.is_empty() || keep.contains(&key.as_str())));
    Value::Object(sent)
}
