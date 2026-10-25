//! `spill`: a tool result too long to show whole is shown as its ends and
//! where the whole is, for any tool. Two steps, so the log keeps facts and
//! what the model sees stays a pure projection of it:
//!
//! - **Recorded** ([`ext::Record`]): as a call ends, a result whose text
//!   (what its tool shows the model) is over `limit` bytes is written whole
//!   to a blob, which the result then holds
//!   ([`BlobOrigin::Spilled`]): durable with the entry, and swept with it.
//!   The outcome is logged as the tool gave it. A result that holds
//!   something already (the shell's, which keeps its own whole output) is
//!   left as it is: its tool decided what to show.
//! - **Presented** ([`PresentResult`]): a result holding its spill is shown
//!   as its first `head` and last `tail` bytes around a
//!   `<truncated bytes=".." path=".."/>` tag naming the blob's file. The
//!   cut is a function of the text alone, at line ends where one is near
//!   and never inside a character, so every projection of the log shows the
//!   same bytes and a provider's prompt cache holds.

use std::io::Write as _;
use std::sync::Arc;

use ext::point::{PresentResult, Presenting};
use ext::{
    BoxFuture, Called, Configured, Cx, DynTool, Extension, Placed, Presented, Provides, Record,
    Refused, Registrar, Rewrite, Table,
};
use llm::{Tag, tag};
use protocol::{BlobOrigin, BlobRef, Held, Outcome};

/// The `spill` extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spill {
    /// The longest text shown whole, in bytes.
    pub limit: usize,
    /// How much of a longer one's start is shown, at most.
    pub head: usize,
    /// How much of its end.
    pub tail: usize,
}

impl Default for Spill {
    fn default() -> Self {
        Self {
            limit: 4096,
            head: 2048,
            tail: 2048,
        }
    }
}

/// `[ext.spill] limit`, `head` and `tail`, in bytes.
impl Configured for Spill {
    fn configure(table: &impl Table) -> Result<Self, Refused> {
        let default = Self::default();
        Ok(Self {
            limit: table.number("limit")?.unwrap_or(default.limit),
            head: table.number("head")?.unwrap_or(default.head),
            tail: table.number("tail")?.unwrap_or(default.tail),
        })
    }
}

impl Extension for Spill {
    const NAME: &'static str = "spill";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        let this = *self;
        r.provide::<dyn Record>(Arc::new(this));
        r.on::<PresentResult>(move |(), shown| this.present(&shown));
    }
}

impl Provides<dyn Record> for Spill {}

impl Record for Spill {
    fn record<'a>(
        &'a self,
        tool: &'a dyn DynTool,
        mut called: Called,
        cx: &'a Cx,
    ) -> BoxFuture<'a, Called> {
        Box::pin(async move {
            let (Outcome::Done { output }, []) = (&called.outcome, &called.held[..]) else {
                return called;
            };
            let Some((text, media_type)) = text(tool.to_model(output)) else {
                return called;
            };
            if text.len() <= self.limit {
                return called;
            }
            let Some(rt) = cx.service::<dyn rt::Runtime>() else {
                return called;
            };
            let (blobs, call) = (cx.blobs.clone(), cx.call);
            let sealed = rt::unblock(rt, move || -> std::io::Result<BlobRef> {
                let mut writer = blobs.writer(text.len() as u64)?;
                writer.write_all(text.as_bytes())?;
                writer.finish(media_type, BlobOrigin::Spilled(call))
            })
            .await;
            // Unsealed, the result is shown whole: nothing is lost.
            if let Ok(blob) = sealed {
                called.held.push(Held::Blob(blob));
            }
            called
        })
    }
}

impl Spill {
    /// `shown` cut to its ends, if it holds its spill and is long enough to
    /// cut: what follows from the text and the spill alone.
    fn present(&self, shown: &Presenting<'_>) -> Rewrite<Presented> {
        let Some(at) = shown.held.iter().find_map(spilled) else {
            return Rewrite::Pass;
        };
        let text = match &shown.presented.output {
            llm::Output::Text(text) => text.as_str(),
            llm::Output::Json(json) => json.raw(),
            llm::Output::Content(_) => return Rewrite::Pass,
        };
        let Some((head, tail)) = cut(text, self.head, self.tail) else {
            return Rewrite::Pass;
        };
        let mut out = String::with_capacity(head.len() + tail.len() + at.len() + 48);
        out.push_str(head);
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        let bytes = (text.len() - head.len() - tail.len()) as u64;
        Tag::from(tag::Truncated { bytes, at }).render(&mut out);
        out.push('\n');
        out.push_str(tail);
        Rewrite::Replace(Presented {
            output: llm::Output::Text(out),
            error: shown.presented.error,
        })
    }
}

/// Where a result's spill is read, if it holds one that can be.
fn spilled(placed: &Placed) -> Option<&str> {
    match &placed.held {
        Held::Blob(BlobRef {
            origin: BlobOrigin::Spilled(_),
            ..
        }) => placed.at.as_deref(),
        _ => None,
    }
}

/// What the model reads of `output` as text, and its media type: none for
/// content (images) a file of text could not hold.
fn text(output: llm::Output) -> Option<(String, &'static str)> {
    match output {
        llm::Output::Text(text) => Some((text, "text/plain")),
        llm::Output::Json(json) => Some((json.raw().to_owned(), "application/json")),
        llm::Output::Content(_) => None,
    }
}

/// The first at most `head` and last at most `tail` bytes of `text`, each
/// ending (or starting) at a line's end when one is in the half of it
/// nearest the cut, and never inside a character; `None` when they would
/// leave nothing out.
fn cut(text: &str, head: usize, tail: usize) -> Option<(&str, &str)> {
    if text.len() <= head.saturating_add(tail) {
        return None;
    }
    let end = text.floor_char_boundary(head);
    let end = match text[..end].rfind('\n') {
        Some(line) if line + 1 >= end / 2 => line + 1,
        _ => end,
    };
    let start = text.ceil_char_boundary(text.len() - tail);
    let start = match text[start..].find('\n') {
        Some(line) if line < (text.len() - start) / 2 => start + line + 1,
        _ => start,
    };
    Some((&text[..end], &text[start..]))
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    #[test]
    fn a_cut_ends_at_a_line_near_it_and_never_inside_a_character() {
        let lines = (0..100).fold(String::new(), |mut text, n| {
            let _ = writeln!(text, "line {n:03}");
            text
        });
        let (head, tail) = cut(&lines, 25, 25).unwrap();
        assert_eq!(
            (head, tail),
            ("line 000\nline 001\n", "line 098\nline 099\n")
        );

        // No line end near: at the nearest character, of two bytes each.
        let wide = "é".repeat(100);
        let (head, tail) = cut(&wide, 25, 25).unwrap();
        assert_eq!((head.len(), tail.len()), (24, 24));

        assert_eq!(cut("short", 3, 3), None);
    }
}
