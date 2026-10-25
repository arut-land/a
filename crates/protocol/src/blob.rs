//! Content-addressed bytes too large for a log line: pastes, attachments
//! and tool output. A log entry holds a [`BlobRef`]; the store that owns
//! the log owns the bytes, and a path to them is only ever a materialized
//! copy, never the truth.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{CallId, Json, OutputStream};

/// The SHA-256 of a blob's bytes. One namespace per daemon: the same
/// bytes from any session, child or fork are one blob.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlobId(pub [u8; 32]);

impl BlobId {
    /// The first byte, as the two hex digits a store fans its files out by.
    pub fn fan(&self) -> String {
        format!("{:02x}", self.0[0])
    }
}

impl fmt::Display for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl fmt::Debug for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlobId({self})")
    }
}

impl FromStr for BlobId {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const INVALID: &str = "a blob id is 64 lowercase hex digits";
        let digits = s.as_bytes();
        if digits.len() != 64 {
            return Err(INVALID);
        }
        let nibble = |c: u8| match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            _ => Err(INVALID),
        };
        let mut id = [0; 32];
        for (byte, [high, low]) in id.iter_mut().zip(digits.as_chunks::<2>().0) {
            *byte = nibble(*high)? << 4 | nibble(*low)?;
        }
        Ok(Self(id))
    }
}

impl Serialize for BlobId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for BlobId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// A reference to a blob, as a log entry holds it: what it is beside its
/// bytes. Two references may name one blob from different origins.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    pub id: BlobId,
    pub size: u64,
    pub media_type: String,
    pub origin: BlobOrigin,
}

/// Where a blob's bytes came from, which names it where it is shown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobOrigin {
    Paste,
    Attachment {
        name: String,
    },
    /// The whole output of a call that showed less: one its tool kept, or
    /// its result's payload, too large for the log line.
    ToolOutput(CallId),
    /// The output of a call's background job.
    JobLog(CallId),
}

/// What an entry references outside the log: a sealed blob, or a stream
/// still growing. A sweep keeps what any entry holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Held {
    Blob(BlobRef),
    Stream(OutputStream),
}

/// A reference an entry holds, wherever in it: what [`Body::blobs`]
/// lists.
///
/// [`Body::blobs`]: crate::Body::blobs
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reference<'a> {
    Blob(&'a BlobRef),
    Stream(&'a OutputStream),
}

impl<'a> From<&'a Held> for Reference<'a> {
    fn from(held: &'a Held) -> Self {
        match held {
            Held::Blob(blob) => Self::Blob(blob),
            Held::Stream(stream) => Self::Stream(stream),
        }
    }
}

/// A value, in its log line or, too large for one, held as a blob in its
/// place: the one copy, never both. Keyed by which, so a result's line
/// reads `"output":…` or `"blob":{…}` beside its status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Payload {
    #[serde(rename = "output")]
    Inline(Json),
    /// What the model is shown of the value, as text, held whole: the
    /// bytes its ends are cut from, and the file it greps. Boxed, so an
    /// outcome stays as small as the common inline one.
    Blob(Box<BlobRef>),
}

impl From<Json> for Payload {
    fn from(json: Json) -> Self {
        Self::Inline(json)
    }
}
