//! Content-addressed bytes too large for a log line: pastes, attachments
//! and tool output. A log entry holds a [`BlobRef`]; the store that owns
//! the log owns the bytes, and a path to them is only ever a materialized
//! copy, never the truth.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{CallId, Json};

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
    /// The whole output of a call that showed less.
    ToolOutput(CallId),
    /// The output of a call's background job.
    JobLog(CallId),
}

/// A value too large for its log line is held as a blob.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Payload {
    Inline(Json),
    Blob(BlobRef),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blob_id_is_its_hex_both_ways() {
        let id = BlobId(std::array::from_fn(|i| u8::try_from(i * 7).unwrap()));
        let text = id.to_string();
        assert_eq!(text.len(), 64);
        assert_eq!(text.parse(), Ok(id));
        assert_eq!(id.fan(), &text[..2]);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<BlobId>(&json).unwrap(), id);
        for bad in [
            "",
            "AB",
            &text.to_uppercase(),
            &text[1..],
            &format!("{text}0"),
        ] {
            assert!(bad.parse::<BlobId>().is_err(), "{bad}");
        }
    }
}
