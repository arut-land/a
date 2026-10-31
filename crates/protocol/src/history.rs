//! Immutable log cuts and explicitly requested transcript ranges.

use serde::{Deserialize, Serialize};

use crate::view::{Item, Recap, Usage, ViewState};
use crate::{Epoch, SessionId};

/// A committed log prefix in one writer generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Cut {
    pub session: SessionId,
    pub generation: Epoch,
    pub seq: u64,
}

/// Control state and history identity, without committed transcript payloads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Head {
    pub cut: Cut,
    pub count: usize,
    /// The earliest item changed after the reader's preceding cut.
    pub changed_from: usize,
    pub changed: Vec<usize>,
    pub answers: Vec<Answer>,
    pub state: ViewState,
    pub sides: std::collections::BTreeMap<String, SideRef>,
    pub model: Option<String>,
    pub usage: Usage,
    pub recap: Option<Recap>,
    pub conversation: Conversation,
}

/// Public conversation identity and observations, independent of transcript demand.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub parent: Option<SessionId>,
    pub agent: Option<String>,
    pub title: Option<String>,
    pub admitted_unix_ms: Option<u64>,
    /// Observed monotonic spans, summed across daemon clock epochs without inventing downtime.
    pub elapsed_ns: Option<u64>,
    pub outcome: Option<crate::ledger::OutcomeKind>,
    pub children: Vec<Child>,
}

/// A committed spawn, retained even after its transcript page is released.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Child {
    pub session: SessionId,
    pub agent: String,
    pub title: String,
    pub admitted_unix_ms: u64,
}

impl Head {
    pub fn frames(&self) -> std::io::Result<Vec<crate::Frame>> {
        let json = serde_json::to_string(self).map_err(std::io::Error::other)?;
        if json.len() + 64 < crate::MAX_FRAME as usize {
            return crate::Frame::head_json(self.cut.seq, &json).map(|frame| vec![frame]);
        }
        let mut frames = Vec::new();
        let mut at = 0;
        while at < json.len() {
            let mut end = (at + (64 << 10)).min(json.len());
            while !json.is_char_boundary(end) {
                end -= 1;
            }
            frames.push(crate::Frame::new(&crate::DaemonMsg::HeadPart {
                seq: self.cut.seq,
                offset: at as u64,
                text: json[at..end].into(),
                done: end == json.len(),
            })?);
            at = end;
        }
        Ok(frames)
    }
}

/// A completed side result; its payload is obtained through a demanded range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SideRef {
    pub index: usize,
    pub revision: u64,
}

/// The control image being received; released in full once decoded.
#[derive(Debug, Default)]
pub struct Heads {
    seq: Option<u64>,
    bytes: Vec<u8>,
}

impl Heads {
    pub fn part(
        &mut self,
        seq: u64,
        offset: u64,
        text: &str,
        done: bool,
    ) -> std::io::Result<Option<Head>> {
        if offset == 0 {
            self.seq = Some(seq);
            self.bytes = Vec::new();
        }
        if self.seq != Some(seq) || offset != self.bytes.len() as u64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "control chunks arrived out of order",
            ));
        }
        self.bytes.extend_from_slice(text.as_bytes());
        if !done {
            return Ok(None);
        }
        self.seq = None;
        let head: Head = serde_json::from_slice(&std::mem::take(&mut self.bytes))
            .map_err(std::io::Error::other)?;
        if head.cut.seq != seq {
            return Err(std::io::Error::other(
                "control image belongs to another cut",
            ));
        }
        Ok(Some(head))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub seq: crate::Seq,
    pub call: crate::CallId,
    pub approved: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Before,
    After,
}

/// Search and recall scan the same history without retaining visited pages.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Filter {
    #[default]
    Transcript,
    Inputs,
    Search(String),
}

/// An exclusive upper anchor for Before, an inclusive lower anchor for After.
/// The byte allowance admits one oversized item, rather than truncating it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub cut: Cut,
    pub anchor: usize,
    pub text_offset: Option<usize>,
    pub direction: Direction,
    pub bytes: usize,
    pub filter: Filter,
}

/// One requested immutable range. Empty search pages still advance their cursor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub cut: Cut,
    pub start: usize,
    pub end: usize,
    pub total: usize,
    pub version: u64,
    pub fragment: Option<TextSpan>,
    pub items: Vec<Located>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Located {
    pub index: usize,
    pub item: Item,
}

impl crate::Decode for Page {
    fn from_json(bytes: &[u8]) -> serde_json::Result<Self> {
        serde_json::from_slice(bytes)
    }
}

/// The exact source bytes carried for an oversized text item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextSpan {
    pub index: usize,
    pub key: crate::Seq,
    pub start: usize,
    pub end: usize,
    pub total: usize,
    pub markdown: crate::view::md::Checkpoint,
}

/// Small connection metadata for the requested history window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub cut: Cut,
    pub count: usize,
    pub changed: Vec<usize>,
    pub fragment: Option<TextSpan>,
    pub start: usize,
    pub end: usize,
    pub model: Option<String>,
    pub usage: Usage,
}
