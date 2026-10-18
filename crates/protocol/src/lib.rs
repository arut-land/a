//! Wire types shared by the daemon and every client, plus framing.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

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
    /// The reply; `Done` carries the whole `llm::Response`, or
    /// `{"text": ...}` in logs older than that.
    ModelResult {
        call: u64,
        outcome: Outcome,
    },
    /// A hook changed what happens at a decision point.
    Hook {
        hook: String,
        decision: Decision,
    },
    /// Tool call `call` waits for the user's approval. Until a
    /// [`Decision::Approval`] for it is logged it is in the session's inbox:
    /// delivered to every client, and again to each that attaches.
    Ask {
        call: u64,
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
    /// The environment the session's commands run in from here on, as the
    /// attaching client captured it, secrets redacted.
    Environment(Environment),
    /// The extensions the session runs with from here on, in install order,
    /// logged when they differ from those its header or an earlier entry
    /// records.
    Extensions {
        installed: Vec<Installed>,
    },
}

/// An extension a session ran with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    pub name: String,
    pub version: u32,
}

/// A client's shell environment: what a command the agent runs for it sees.
/// Commands run with exactly `vars` in `cwd`, never with the daemon's own.
///
/// Secret values live only in memory: the logged form names them in
/// `redacted` instead, so after a daemon restart commands run without them
/// until a client attaches or refreshes again.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Environment {
    pub cwd: PathBuf,
    pub vars: BTreeMap<String, String>,
    /// Secrets whose values were left out of `vars`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redacted: Vec<String>,
    /// Whether the client ran at a terminal.
    #[serde(default)]
    pub tty: bool,
}

/// Bookkeeping of the client's shell, not meant for the commands run for
/// it: never captured.
const DENIED: &[&str] = &["_", "OLDPWD", "PWD", "SHLVL"];

/// Never secret, whatever the patterns below say.
const PUBLIC: &[&str] = &["SSH_AUTH_SOCK", "GPG_AGENT_INFO"];

/// A name containing any of these, in any case, holds a secret…
const SECRET_WORDS: &[&str] = &[
    "PASSWORD",
    "PASSWD",
    "SECRET",
    "TOKEN",
    "CREDENTIAL",
    "API_KEY",
    "PRIVATE_KEY",
];

/// …as does one ending in one of these.
const SECRET_SUFFIXES: &[&str] = &["_KEY", "_PASS", "_PAT", "_DSN", "_COOKIE"];

impl Environment {
    /// Captures `vars`, leaving out denied names and anything not UTF-8.
    pub fn new(
        cwd: PathBuf,
        vars: impl IntoIterator<Item = (OsString, OsString)>,
        tty: bool,
    ) -> Self {
        // Inserted one by one: collecting would sort, in code of its own.
        let mut map = BTreeMap::new();
        for (k, v) in vars {
            if let (Ok(k), Ok(v)) = (k.into_string(), v.into_string())
                && !DENIED.contains(&k.as_str())
            {
                map.insert(k, v);
            }
        }
        Self {
            cwd,
            vars: map,
            redacted: Vec::new(),
            tty,
        }
    }

    /// Whether variable `name` holds a secret, whose value is never logged.
    pub fn is_secret(name: &str) -> bool {
        let name = name.to_ascii_uppercase();
        !PUBLIC.contains(&name.as_str())
            && (SECRET_WORDS.iter().any(|w| name.contains(w))
                || SECRET_SUFFIXES.iter().any(|s| name.ends_with(s)))
    }

    /// This environment as it may be logged: secret values dropped, their
    /// names kept in `redacted`.
    #[must_use]
    pub fn redacted(&self) -> Self {
        let mut redacted = self.redacted.clone();
        let mut vars = BTreeMap::new();
        for (k, v) in &self.vars {
            if Self::is_secret(k) {
                redacted.push(k.clone());
            } else {
                vars.insert(k.clone(), v.clone());
            }
        }
        Self {
            cwd: self.cwd.clone(),
            vars,
            redacted,
            tty: self.tty,
        }
    }
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
    /// The answer to a permission `ask`; `call` is set when the user gave
    /// it, to the call's [`Body::Ask`].
    Approval {
        tool: String,
        approved: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<u64>,
    },
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

/// What happened in a session, for a client that attaches: derived from the
/// log alone, so every client gets the same one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recap {
    /// The user's last message, shortened.
    pub prompt: Option<String>,
    /// The tools called since, in order.
    pub tools: Vec<String>,
    /// Calls since that failed or were interrupted.
    pub failed: u32,
    /// Calls since with no outcome yet.
    pub running: u32,
    /// The agent's last reply since, shortened.
    pub reply: Option<String>,
    /// Calls waiting for the user's approval.
    pub asks: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    /// `build` is the client's staged build id (`<version>+<hash>`).
    Hello {
        proto: u32,
        build: String,
    },
    /// `env` is the client's, for the commands the session runs; without
    /// one the session keeps the environment it had.
    Attach {
        target: Target,
        workspace: PathBuf,
        since: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<Environment>,
    },
    /// Replaces the attached session's environment (`/env refresh`).
    Environment(Environment),
    Input {
        text: String,
    },
    List,
    Detach,
    /// Answers the [`DaemonMsg::Ask`] for tool call `call`.
    Approve {
        call: u64,
        approved: bool,
    },
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
    /// Sent after `Synced` when the replay held entries.
    Recap(Recap),
    /// Tool call `call` waits for approval: sent when it is asked, and
    /// after `Synced` for every call still waiting.
    Ask {
        call: u64,
        tool: String,
        input: Value,
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

/// A frame encoded once, length prefix included, for any number of peers:
/// cloning it shares the bytes.
#[derive(Clone, Debug)]
pub struct Frame(Arc<Vec<u8>>);

impl Frame {
    pub fn new(msg: &DaemonMsg) -> io::Result<Self> {
        let mut bytes = Self::start();
        serde_json::to_writer(&mut bytes, msg)?;
        Self::finish(bytes)
    }

    /// The frame of `DaemonMsg::Entry(entry.clone())`, without the clone:
    /// the entry's own JSON with the tag spliced in first, as serde puts it.
    pub fn entry(entry: &Entry) -> io::Result<Self> {
        let mut bytes = Self::start();
        bytes.extend_from_slice(br#"{"t":"entry""#);
        let at = bytes.len();
        entry.encode(&mut bytes)?;
        bytes[at] = b',';
        Self::finish(bytes)
    }

    /// Room for the length, and as much again as `serde_json::to_vec`
    /// starts with.
    fn start() -> Vec<u8> {
        let mut bytes = Vec::with_capacity(132);
        bytes.extend_from_slice(&[0; 4]);
        bytes
    }

    fn finish(mut bytes: Vec<u8>) -> io::Result<Self> {
        let len = u32::try_from(bytes.len() - 4)
            .ok()
            .filter(|&n| n <= MAX_FRAME)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
        bytes[..4].copy_from_slice(&len.to_le_bytes());
        Ok(Self(Arc::new(bytes)))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Entry {
    /// Appends the entry's JSON object to `out`. Encoded here, once, for
    /// the reason [`Decode`] gives.
    pub fn encode(&self, out: &mut Vec<u8>) -> serde_json::Result<()> {
        serde_json::to_writer(out, self)
    }
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
    fn a_shared_frame_is_the_frame_written() {
        let entry = Entry {
            seq: 7,
            epoch: 0,
            body: Body::Assistant { text: "hi".into() },
        };
        let mut written = Vec::new();
        write_frame(&mut written, &DaemonMsg::Entry(entry.clone())).unwrap();
        assert_eq!(Frame::entry(&entry).unwrap().as_bytes(), written);
        let msg = DaemonMsg::Synced { seq: 7 };
        written.clear();
        write_frame(&mut written, &msg).unwrap();
        assert_eq!(Frame::new(&msg).unwrap().as_bytes(), written);
    }

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
    fn secrets_are_redacted_by_name() {
        let vars = [
            ("PATH", "/bin"),
            ("OPENAI_API_KEY", "sk-1"),
            ("GITHUB_TOKEN", "ghp-1"),
            ("DB_PASSWORD", "hunter2"),
            ("SSH_AUTH_SOCK", "/tmp/agent"),
            ("PWD", "/x"),
        ]
        .map(|(k, v)| (k.into(), v.into()));
        let env = Environment::new("/w".into(), vars, true);
        assert!(!env.vars.contains_key("PWD"));
        let logged = env.redacted();
        assert_eq!(
            logged.vars.keys().collect::<Vec<_>>(),
            ["PATH", "SSH_AUTH_SOCK"]
        );
        assert_eq!(
            logged.redacted.iter().collect::<Vec<_>>(),
            ["DB_PASSWORD", "GITHUB_TOKEN", "OPENAI_API_KEY"]
        );
        assert_eq!(logged.redacted(), logged);
        let json = serde_json::to_string(&Body::Environment(logged)).unwrap();
        assert!(
            json.starts_with(r#"{"kind":"environment","cwd":"/w","#),
            "{json}"
        );
        for secret in ["sk-1", "ghp-1", "hunter2"] {
            assert!(!json.contains(secret), "{json}");
        }
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
    fn approvals_by_a_hook_keep_their_shape_and_the_users_name_the_call() {
        let by_hook = r#"{"seq":2,"epoch":0,"kind":"hook","hook":"h","decision":{"approval":{"tool":"t","approved":true}}}"#;
        let e: Entry = serde_json::from_str(by_hook).unwrap();
        assert_eq!(serde_json::to_string(&e).unwrap(), by_hook);
        let by_user = Body::Hook {
            hook: "user".into(),
            decision: Decision::Approval {
                tool: "t".into(),
                approved: false,
                call: Some(3),
            },
        };
        assert_eq!(
            serde_json::to_value(&by_user).unwrap()["decision"]["approval"]["call"],
            3
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
