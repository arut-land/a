//! Wire types shared by the daemon and every client, plus framing.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const PROTO: u32 = 1;

const MAX_FRAME: u32 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Short form for display: the random tail of the `UUIDv7`.
    pub const fn short(self) -> Short {
        Short(self)
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// All 32 hex digits, no dashes.
impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.simple(), f)
    }
}

/// The last 8 hex digits of a [`SessionId`].
#[derive(Clone, Copy, Debug)]
pub struct Short(SessionId);

impl fmt::Display for Short {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = Uuid::encode_buffer();
        f.write_str(&self.0.0.simple().encode_lower(&mut buf)[24..])
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
    /// idempotency key; `id` is the model's own id for the call, under which
    /// its result goes back.
    ToolCall {
        tool: String,
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
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
    /// The reply; `Done` carries the whole `llm::Response` (earlier versions
    /// logged `{"text": ...}`).
    ModelResult {
        call: u64,
        outcome: Outcome,
    },
    /// A hook changed what happens at a decision point.
    Hook {
        hook: String,
        decision: Decision,
    },
    /// The committed intent to start session `child` with `input`. Its seq
    /// is the call that owns the child; the child's id is chosen before the
    /// commit, so recovery finds the child instead of making another.
    Link {
        child: SessionId,
        input: String,
    },
    /// A durable watch on `child`: when it settles, its result comes back
    /// as a `fired` under this entry's seq, which is the idempotency key.
    Watch {
        child: SessionId,
    },
    /// Watch `watch` fired, with its child's result. Logged at most once
    /// per watch.
    Fired {
        watch: u64,
        outcome: Outcome,
    },
}

/// What a hook decided, by decision point. Only decisions that change
/// something are logged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
// Externally tagged: an internally tagged enum inside the internally tagged
// `Body` costs a buffered deserializer per variant.
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// The request of model call `call` was replaced before it was sent.
    PrepareStep { call: u64 },
    /// A tool call's input was replaced by `input`.
    PreTool { tool: String, input: Value },
    /// A tool call was blocked before it ran.
    Block { tool: String, reason: String },
    /// The most restrictive permission the hooks gave a tool call.
    Permission {
        tool: String,
        permission: Permission,
    },
    /// The answer to a permission `ask`.
    Approval { tool: String, approved: bool },
    /// Call `call`'s outcome was replaced; `original` is what it replaced.
    PostTool { call: u64, original: Outcome },
    /// The agent stopped and was given `text` to continue with.
    OnStop { text: String },
}

/// Whether a tool call may run, from least to most restrictive: merging
/// takes the maximum.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    #[default]
    Allow,
    /// Runs once someone approves it.
    Ask,
    /// Decided outside this run, later.
    Defer,
    Deny,
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
    /// `build` is the client's staged build id (`<version>+<hash>`).
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
    /// A newer build was staged: the daemon finishes in-flight work, exits,
    /// and closes every connection. Clients then reconnect, which starts the
    /// new build, and re-attach to replay what they missed. Sent as the reply
    /// to a hello, or to connections already open when the drain began.
    Draining,
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

/// A record decoded from JSON here, once, rather than in every crate that
/// reads one: release builds do not share generic instantiations across
/// crates, and these deserializers are large.
pub trait Decode: Sized {
    fn from_json(bytes: &[u8]) -> serde_json::Result<Self>;
}

macro_rules! decode {
    ($($t:ty),*) => {$(
        impl Decode for $t {
            fn from_json(bytes: &[u8]) -> serde_json::Result<Self> {
                serde_json::from_slice(bytes)
            }
        }
    )*};
}
decode!(Entry, ClientMsg, DaemonMsg);

/// Reads one frame; `Ok(None)` means the peer closed the stream cleanly.
pub fn read_frame<T: Decode>(r: &mut impl Read) -> io::Result<Option<T>> {
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
    Ok(Some(T::from_json(&body)?))
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
    fn session_ids_display_whole_and_short() {
        let id = SessionId(Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef));
        assert_eq!(id.to_string(), "0123456789abcdef0123456789abcdef");
        assert_eq!(id.short().to_string(), "89abcdef");
    }

    #[test]
    fn draining_is_a_bare_tag() {
        assert_eq!(
            serde_json::to_string(&DaemonMsg::Draining).unwrap(),
            r#"{"t":"draining"}"#
        );
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

    #[test]
    fn tool_calls_without_a_model_id_keep_their_shape() {
        let old = r#"{"seq":2,"epoch":0,"kind":"tool_call","tool":"sleep","input":{"seconds":1}}"#;
        let e: Entry = serde_json::from_str(old).unwrap();
        assert!(matches!(&e.body, Body::ToolCall { id: None, .. }));
        assert_eq!(serde_json::to_string(&e).unwrap(), old);
    }

    #[test]
    fn hook_decisions_nest_under_their_point_name() {
        let body = Body::Hook {
            hook: "policy".into(),
            decision: Decision::Permission {
                tool: "run_command".into(),
                permission: Permission::Deny,
            },
        };
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({
                "kind": "hook",
                "hook": "policy",
                "decision": { "permission": { "tool": "run_command", "permission": "deny" } },
            })
        );
    }

    #[test]
    fn link_entries_are_flat() {
        let child = SessionId(Uuid::from_u128(1));
        let entries = [
            Body::Link {
                child,
                input: "hi".into(),
            },
            Body::Watch { child },
            Body::Fired {
                watch: 2,
                outcome: Outcome::Interrupted,
            },
        ];
        let json: Vec<_> = entries
            .iter()
            .map(|b| serde_json::to_string(b).unwrap())
            .collect();
        let id = "00000000-0000-0000-0000-000000000001";
        assert_eq!(
            json,
            [
                format!(r#"{{"kind":"link","child":"{id}","input":"hi"}}"#),
                format!(r#"{{"kind":"watch","child":"{id}"}}"#),
                r#"{"kind":"fired","watch":2,"outcome":{"status":"interrupted"}}"#.to_owned(),
            ]
        );
    }
}
