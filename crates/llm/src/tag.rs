//! Model-facing markers: context a model reads that no person typed (a
//! paste, a steer, another session's message, another model's reasoning),
//! named for what it is with the facts about it as attributes. Every
//! marker is a [`Tag`] and [`Tag::render`] is the one place tag text is
//! made, so producers cannot drift into formats of their own; the typed
//! markers below are the vocabulary, each spelling its name and
//! attributes once.
//!
//! Attribute values are escaped; a body goes in verbatim, on lines of its
//! own, since escaping a paste or a log would change what the model reads.

use std::borrow::Cow;
use std::fmt::{self, Write as _};

use crate::spec::Origin;

/// One marker: `<name attr="value">body</name>`, or `<name attr="value"/>`
/// without a body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag<'a> {
    pub name: &'static str,
    pub attrs: Vec<(&'static str, Cow<'a, str>)>,
    pub body: Option<Cow<'a, str>>,
}

impl<'a> Tag<'a> {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            attrs: Vec::new(),
            body: None,
        }
    }

    #[must_use]
    pub fn attr(mut self, key: &'static str, value: impl Into<Cow<'a, str>>) -> Self {
        self.attrs.push((key, value.into()));
        self
    }

    #[must_use]
    pub fn body(mut self, body: impl Into<Cow<'a, str>>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// Appends the tag's text to `out`.
    pub fn render(&self, out: &mut String) {
        out.push('<');
        out.push_str(self.name);
        for (key, value) in &self.attrs {
            out.push(' ');
            out.push_str(key);
            out.push_str("=\"");
            escape(value, out);
            out.push('"');
        }
        let Some(body) = &self.body else {
            out.push_str("/>");
            return;
        };
        out.push_str(">\n");
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("</");
        out.push_str(self.name);
        out.push('>');
    }
}

impl fmt::Display for Tag<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        self.render(&mut out);
        f.write_str(&out)
    }
}

fn escape(value: &str, out: &mut String) {
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\n' => out.push_str("&#10;"),
            c => out.push(c),
        }
    }
}

/// Text the user pasted, whole: the model can tell it from what they typed.
#[derive(Clone, Copy, Debug)]
pub struct Pasted<'a>(pub &'a str);

impl<'a> From<Pasted<'a>> for Tag<'a> {
    fn from(Pasted(text): Pasted<'a>) -> Self {
        let lines = text.lines().count();
        Tag::new("pasted-content")
            .attr("lines", lines.to_string())
            .attr("bytes", text.len().to_string())
            .body(text)
    }
}

/// A paste the daemon kept apart from the input, by where it is.
#[derive(Clone, Copy, Debug)]
pub struct PastedRef<'a> {
    /// Where the model can read it.
    pub at: &'a str,
    pub bytes: u64,
}

impl<'a> From<PastedRef<'a>> for Tag<'a> {
    fn from(paste: PastedRef<'a>) -> Self {
        Tag::new("pasted-content")
            .attr("path", paste.at)
            .attr("bytes", paste.bytes.to_string())
    }
}

/// A file the user attached, by where it is.
#[derive(Clone, Copy, Debug)]
pub struct Attached<'a> {
    pub at: &'a str,
    pub media_type: &'a str,
    pub bytes: u64,
}

impl<'a> From<Attached<'a>> for Tag<'a> {
    fn from(file: Attached<'a>) -> Self {
        Tag::new("attachment")
            .attr("path", file.at)
            .attr("type", file.media_type)
            .attr("bytes", file.bytes.to_string())
    }
}

/// Input given to the running turn mid-way, and who gave it.
#[derive(Clone, Copy, Debug)]
pub struct Steer<'a> {
    pub from: &'a str,
    pub text: &'a str,
}

impl<'a> From<Steer<'a>> for Tag<'a> {
    fn from(steer: Steer<'a>) -> Self {
        Tag::new("steer").attr("from", steer.from).body(steer.text)
    }
}

/// A message another session sent this one.
#[derive(Clone, Copy, Debug)]
pub struct Inbox<'a> {
    pub from: &'a str,
    pub channel: &'a str,
    pub text: &'a str,
}

impl<'a> From<Inbox<'a>> for Tag<'a> {
    fn from(message: Inbox<'a>) -> Self {
        Tag::new("message")
            .attr("from", message.from)
            .attr("channel", message.channel)
            .body(message.text)
    }
}

/// How a call the model stopped waiting for ended.
#[derive(Clone, Copy, Debug)]
pub struct BackgroundResult<'a> {
    pub call: &'a str,
    pub command: &'a str,
    pub exit: Option<i32>,
    /// Where its whole output is.
    pub log: Option<&'a str>,
    pub output: Option<&'a str>,
}

impl<'a> From<BackgroundResult<'a>> for Tag<'a> {
    fn from(done: BackgroundResult<'a>) -> Self {
        let mut tag = Tag::new("background-result")
            .attr("call", done.call)
            .attr("command", done.command);
        if let Some(exit) = done.exit {
            tag = tag.attr("exit", exit.to_string());
        }
        if let Some(log) = done.log {
            tag = tag.attr("log", log);
        }
        if let Some(output) = done.output {
            tag = tag.body(output);
        }
        tag
    }
}

/// Reasoning another model showed, as the model that continues reads it:
/// named for its source, so it is not taken for its own. Which kind a part
/// is, its producer's protocol says (`foreign` in each protocol module);
/// encrypted and redacted reasoning has no kind and is never shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Thought<'a> {
    /// A summary the provider wrote of the model's reasoning.
    Summary(&'a str),
    /// The model's reasoning as it produced it.
    Raw(&'a str),
}

impl<'a> Thought<'a> {
    /// `text` as a summary, when there is any to show.
    pub(crate) fn summary(text: &'a str) -> Option<Self> {
        (!text.trim().is_empty()).then_some(Self::Summary(text))
    }

    /// `text` as raw reasoning, when there is any to show.
    pub(crate) fn raw(text: &'a str) -> Option<Self> {
        (!text.trim().is_empty()).then_some(Self::Raw(text))
    }

    /// The tag a model reads it as, naming `source`.
    pub fn tag(self, source: &Origin) -> Tag<'a> {
        let (name, text) = match self {
            Self::Summary(text) => ("reasoning-summary", text),
            Self::Raw(text) => ("reasoning", text),
        };
        let mut from = String::with_capacity(source.provider.len() + 1 + source.model.len());
        let _ = write!(from, "{}/{}", source.provider, source.model);
        Tag::new(name).attr("source", from).body(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_values_cannot_break_out_of_their_quotes() {
        let tag = Tag::new("steer").attr("from", "a\"><x>&\nb");
        assert_eq!(
            tag.to_string(),
            r#"<steer from="a&quot;&gt;&lt;x&gt;&amp;&#10;b"/>"#
        );
    }

    #[test]
    fn a_body_is_verbatim_on_lines_of_its_own() {
        let tag = Tag::from(Pasted("fn f() -> &str {}\n"));
        assert_eq!(
            tag.to_string(),
            "<pasted-content lines=\"1\" bytes=\"18\">\nfn f() -> &str {}\n</pasted-content>"
        );
    }

    /// The policy: what a model reads in a body is exactly what was given
    /// (code, logs, markup), and an attribute value can never end its
    /// quotes or the tag.
    #[test]
    fn a_body_is_never_escaped_and_an_attribute_always_is() {
        let raw = "<b> & \"q\" > </steer>\n";
        let tag = Tag::new("steer").attr("from", raw).body(raw);
        let text = tag.to_string();
        let (open, body) = text.split_once('>').unwrap();
        assert_eq!(
            open,
            "<steer from=\"&lt;b&gt; &amp; &quot;q&quot; &gt; &lt;/steer&gt;&#10;\""
        );
        assert_eq!(body, format!("\n{raw}</steer>"));
    }

    #[test]
    fn reasoning_names_its_source() {
        let source = Origin {
            protocol: crate::ProtocolKey::AnthropicMessages,
            provider: "anthropic".into(),
            model: "claude-sonnet-4-5".into(),
        };
        let tag = Thought::summary("weighing it").unwrap().tag(&source);
        assert_eq!(
            tag.to_string(),
            "<reasoning-summary source=\"anthropic/claude-sonnet-4-5\">\nweighing it\n</reasoning-summary>"
        );
        assert_eq!(Thought::raw("  \n"), None);
    }
}
