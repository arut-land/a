//! Wire types shared by the daemon and every client, plus framing.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const PROTO: u32 = 1;
pub const BUILD: &str = env!("CARGO_PKG_VERSION");

const MAX_FRAME: u32 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Short form for display: the random tail of the `UUIDv7`.
    pub fn short(&self) -> String {
        self.0.simple().to_string()[24..].to_owned()
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub seq: u64,
    pub epoch: u32,
    #[serde(flatten)]
    pub body: Body,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Body {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    /// The committed intent to run a tool. Its seq is the call's id and
    /// idempotency key.
    ToolCall {
        tool: String,
        input: Value,
    },
    /// The process group running a call, so recovery can kill it. Only
    /// meaningful within the same `boot`: pgids are reused across reboots.
    ToolProcess {
        call: u64,
        pgid: u32,
        boot: String,
    },
    ToolResult {
        call: u64,
        outcome: Outcome,
    },
    /// The committed intent to ask a model for a reply.
    ModelCall {
        model: String,
    },
    /// The reply; `Done` carries `{"text": ...}`.
    ModelResult {
        call: u64,
        outcome: Outcome,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Done {
        output: Value,
    },
    Failed {
        error: String,
    },
    /// The daemon stopped mid-call and the tool is not safe to rerun.
    Interrupted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    New,
    Latest { any_workspace: bool },
    Id(SessionId),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub workspace: PathBuf,
    pub entries: u64,
    pub clients: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        proto: u32,
        build: String,
    },
    Attach {
        target: Target,
        workspace: PathBuf,
        since: u64,
    },
    Input {
        text: String,
    },
    List,
    Detach,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum DaemonMsg {
    Welcome {
        proto: u32,
        build: String,
    },
    Mismatch {
        supported: u32,
    },
    Attached {
        session: SessionId,
        workspace: PathBuf,
    },
    Entry(Entry),
    /// Live text of an unfinished model call. Never logged: the committed
    /// `model_result` is the fact.
    Delta {
        call: u64,
        text: String,
    },
    /// Replay is complete; everything after this is live.
    Synced {
        seq: u64,
    },
    Sessions {
        sessions: Vec<SessionSummary>,
    },
    Error {
        message: String,
    },
}

/// Writes one frame: a little-endian u32 length followed by a JSON body.
pub fn write_frame<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    let len = u32::try_from(body.len())
        .ok()
        .filter(|&n| n <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Reads one frame; `Ok(None)` means the peer closed the stream cleanly.
pub fn read_frame<T: DeserializeOwned>(r: &mut impl Read) -> io::Result<Option<T>> {
    let mut len = [0; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0; len as usize];
    r.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let msg = ClientMsg::Input { text: "hi".into() };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        write_frame(&mut buf, &ClientMsg::Detach).unwrap();
        let mut r = buf.as_slice();
        assert!(
            matches!(read_frame(&mut r).unwrap(), Some(ClientMsg::Input { text }) if text == "hi")
        );
        assert!(matches!(
            read_frame(&mut r).unwrap(),
            Some(ClientMsg::Detach)
        ));
        assert!(read_frame::<ClientMsg>(&mut r).unwrap().is_none());
    }

    #[test]
    fn entry_wire_shape_is_flat() {
        let e = Entry {
            seq: 1,
            epoch: 0,
            body: Body::User { text: "hi".into() },
        };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"seq":1,"epoch":0,"kind":"user","text":"hi"}"#
        );
    }
}
