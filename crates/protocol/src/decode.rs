//! The internally tagged enums, decoded in one pass. Serde's derive for
//! one buffers the object to find its tag, then decodes the variant from
//! the buffer, in code of its own for every variant. Here every record on
//! the wire (a client's message, the daemon's, an entry, a transcript item)
//! is read into one flat struct holding the tags and the fields of all of
//! them, each optional; a match on the tag then builds the variant and
//! names a required field that is missing.
//!
//! So a field name means one type across all of them: where two differ
//! (`input`, a tool call's JSON and a link's text), the field is [`Json`]
//! and the variant reads its type from it. A `null` reads as absent, but
//! for a [`Json`] field, where it is the value.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::de::{Deserializer, Error as _, Visitor};
use serde::{Deserialize, forward_to_deserialize_any};
use serde_json::{Error, Result};
use uuid::Uuid;

use crate::view::{Approval, Item, SessionView, Status, Usage};
use crate::{
    Body, Caps, Card, ClientDecl, ClientMsg, CommandInfo, DaemonMsg, Decision, Decode, Entry,
    Environment, HomeSettings, Installed, Json, Origin, Outcome, Overrides, Recap, Rejection, Role,
    SessionId, SessionSummary, SettingsChange, Target,
};

/// A field every variant that has it requires.
fn req<T>(field: Option<T>, name: &'static str) -> Result<T> {
    match field {
        Some(field) => Ok(field),
        None => Err(missing(name)),
    }
}

/// Out of line, so a required field costs a call, not the error it builds.
#[cold]
#[inline(never)]
fn missing(name: &'static str) -> Error {
    Error::missing_field(name)
}

/// A text field that shares its name with a [`Json`] one.
fn text(field: Option<Json>, name: &'static str) -> Result<String> {
    req(field, name)?.parse()
}

/// A [`Json`] field, `null` included: `None` only when absent, with
/// `#[serde(default)]`.
fn present<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<Json>, D::Error> {
    Json::deserialize(d).map(Some)
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    User,
    Queued,
    Steer,
    Assistant,
    ToolCall,
    ToolProcess,
    Progress,
    ToolResult,
    ModelCall,
    ModelResult,
    Hook,
    Ask,
    Link,
    Watch,
    Fired,
    Environment,
    Extensions,
    Ext,
    Context,
    Command,
    CommandResult,
    Interrupt,
    Settings,
    #[serde(other)]
    Unknown,
}

/// A message's `t`, the client's or the daemon's.
#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Tag {
    Hello,
    Attach,
    Declare,
    Environment,
    Input,
    Steer,
    List,
    Detach,
    WatchSessions,
    Status,
    Shutdown,
    Settings,
    Approve,
    Interrupt,
    Welcome,
    Mismatch,
    Draining,
    Attached,
    Entry,
    Delta,
    Synced,
    Sessions,
    Recap,
    Ask,
    Error,
    Snapshot,
    Ack,
    Rejected,
    #[serde(other)]
    Unknown,
}

impl Tag {
    /// Whether the daemon sends it: a daemon message tagged otherwise is of
    /// a kind this build does not know.
    const fn of_daemon(self) -> bool {
        match self {
            Self::Welcome
            | Self::Mismatch
            | Self::Draining
            | Self::Attached
            | Self::Entry
            | Self::Delta
            | Self::Synced
            | Self::Sessions
            | Self::Status
            | Self::Settings
            | Self::Recap
            | Self::Ask
            | Self::Error
            | Self::Snapshot
            | Self::Ack
            | Self::Rejected => true,
            Self::Hello
            | Self::Attach
            | Self::Declare
            | Self::Environment
            | Self::Input
            | Self::Steer
            | Self::List
            | Self::Detach
            | Self::WatchSessions
            | Self::Shutdown
            | Self::Approve
            | Self::Interrupt
            | Self::Unknown => false,
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Type {
    User,
    Assistant,
    Reply,
    Tool,
    Command,
    Hook,
    Child,
    Environment,
}

/// An [`Entry`], a [`DaemonMsg`] (an entry among them), a [`ClientMsg`] or
/// an [`Item`].
#[derive(Deserialize)]
struct Fields {
    t: Option<Tag>,
    // The entry's head.
    seq: Option<u64>,
    epoch: Option<u32>,
    uuid: Option<Uuid>,
    parent: Option<Uuid>,
    // Its body.
    kind: Option<Kind>,
    text: Option<String>,
    steer: Option<bool>,
    queued: Option<u64>,
    tool: Option<String>,
    #[serde(default, deserialize_with = "present")]
    input: Option<Json>,
    id: Option<String>,
    card: Option<Card>,
    call: Option<u64>,
    pgid: Option<u32>,
    boot: Option<String>,
    background: Option<bool>,
    outcome: Option<Outcome>,
    model: Option<String>,
    hook: Option<String>,
    decision: Option<Decision>,
    child: Option<SessionId>,
    watch: Option<u64>,
    cwd: Option<PathBuf>,
    vars: Option<BTreeMap<String, String>>,
    redacted: Option<Vec<String>>,
    tty: Option<bool>,
    installed: Option<Vec<Installed>>,
    ext: Option<String>,
    state: Option<String>,
    #[serde(default, deserialize_with = "present")]
    data: Option<Json>,
    name: Option<String>,
    args: Option<String>,
    // The other messages'.
    proto: Option<u32>,
    build: Option<String>,
    caps: Option<Caps>,
    client: Option<u32>,
    commands: Option<Vec<CommandInfo>>,
    supported: Option<u32>,
    min: Option<u32>,
    session: Option<SessionId>,
    workspace: Option<PathBuf>,
    sessions: Option<Vec<SessionSummary>>,
    prompt: Option<String>,
    tools: Option<Vec<String>>,
    failed: Option<u32>,
    running: Option<u32>,
    reply: Option<String>,
    asks: Option<u32>,
    message: Option<String>,
    origin: Option<Origin>,
    reason: Option<Rejection>,
    items: Option<Vec<Item>>,
    environment: Option<Environment>,
    pid: Option<u32>,
    started: Option<u64>,
    clients: Option<u32>,
    known: Option<u32>,
    settings: Option<HomeSettings>,
    error: Option<String>,
    // A client's.
    target: Option<Target>,
    since: Option<u64>,
    env: Option<Environment>,
    decl: Option<ClientDecl>,
    role: Option<Role>,
    sites: Option<Vec<String>>,
    client_seq: Option<u64>,
    approved: Option<bool>,
    change: Option<SettingsChange>,
    // An item's.
    #[serde(rename = "type")]
    ty: Option<Type>,
    status: Option<Status>,
    usage: Option<Usage>,
    approval: Option<Approval>,
    fired: Option<Outcome>,
}

/// An [`Environment`]'s fields, wherever they are.
fn environment(
    cwd: Option<PathBuf>,
    vars: Option<BTreeMap<String, String>>,
    redacted: Option<Vec<String>>,
    tty: Option<bool>,
) -> Result<Environment> {
    Ok(Environment {
        cwd: req(cwd, "cwd")?,
        vars: req(vars, "vars")?,
        redacted: redacted.unwrap_or_default(),
        tty: tty.unwrap_or_default(),
    })
}

impl Fields {
    fn read(bytes: &[u8]) -> Result<Self> {
        let mut json = serde_json::Deserializer::from_slice(bytes);
        let fields = Self::deserialize(MapOnly(&mut json))?;
        json.end()?;
        Ok(fields)
    }

    fn entry(&mut self) -> Result<Entry> {
        Ok(Entry {
            seq: req(self.seq.take(), "seq")?,
            epoch: req(self.epoch.take(), "epoch")?,
            uuid: self.uuid.take(),
            parent: self.parent.take(),
            body: self.body()?,
        })
    }

    fn body(&mut self) -> Result<Body> {
        let f = self;
        Ok(match req(f.kind, "kind")? {
            Kind::User => Body::User {
                text: req(f.text.take(), "text")?,
            },
            Kind::Queued => Body::Queued {
                text: req(f.text.take(), "text")?,
                steer: f.steer.take().unwrap_or_default(),
            },
            Kind::Steer => Body::Steer {
                text: req(f.text.take(), "text")?,
                queued: req(f.queued.take(), "queued")?,
            },
            Kind::Assistant => Body::Assistant {
                text: req(f.text.take(), "text")?,
            },
            Kind::ToolCall => Body::ToolCall {
                tool: req(f.tool.take(), "tool")?,
                input: req(f.input.take(), "input")?,
                id: f.id.take(),
                card: f.card.take(),
            },
            Kind::ToolProcess => Body::ToolProcess {
                call: req(f.call.take(), "call")?,
                pgid: req(f.pgid.take(), "pgid")?,
                boot: req(f.boot.take(), "boot")?,
                background: f.background.take().unwrap_or_default(),
            },
            Kind::Progress => Body::Progress {
                call: req(f.call.take(), "call")?,
                text: req(f.text.take(), "text")?,
            },
            Kind::ToolResult => Body::ToolResult {
                call: req(f.call.take(), "call")?,
                outcome: req(f.outcome.take(), "outcome")?,
                card: f.card.take(),
            },
            Kind::ModelCall => Body::ModelCall {
                model: req(f.model.take(), "model")?,
            },
            Kind::ModelResult => Body::ModelResult {
                call: req(f.call.take(), "call")?,
                outcome: req(f.outcome.take(), "outcome")?,
            },
            Kind::Hook => Body::Hook {
                hook: req(f.hook.take(), "hook")?,
                decision: req(f.decision.take(), "decision")?,
            },
            Kind::Ask => Body::Ask {
                call: req(f.call.take(), "call")?,
            },
            Kind::Link => Body::Link {
                child: req(f.child.take(), "child")?,
                input: text(f.input.take(), "input")?,
            },
            Kind::Watch => Body::Watch {
                child: req(f.child.take(), "child")?,
            },
            Kind::Fired => Body::Fired {
                watch: req(f.watch.take(), "watch")?,
                outcome: req(f.outcome.take(), "outcome")?,
            },
            Kind::Environment => Body::Environment(environment(
                f.cwd.take(),
                f.vars.take(),
                f.redacted.take(),
                f.tty.take(),
            )?),
            Kind::Extensions => Body::Extensions {
                installed: req(f.installed.take(), "installed")?,
            },
            Kind::Ext => Body::Ext {
                ext: req(f.ext.take(), "ext")?,
                state: req(f.state.take(), "state")?,
                data: req(f.data.take(), "data")?,
            },
            Kind::Context => Body::Context {
                text: req(f.text.take(), "text")?,
            },
            Kind::Command => Body::Command {
                name: req(f.name.take(), "name")?,
                args: f.args.take().unwrap_or_default(),
            },
            Kind::CommandResult => Body::CommandResult {
                call: req(f.call.take(), "call")?,
                outcome: req(f.outcome.take(), "outcome")?,
            },
            Kind::Interrupt => Body::Interrupt,
            Kind::Settings => Body::Settings(Overrides {
                model: f.model.take(),
            }),
            Kind::Unknown => return Err(Error::custom("unknown entry kind")),
        })
    }

    fn daemon_msg(&mut self) -> Result<DaemonMsg> {
        let f = self;
        Ok(match req(f.t.take(), "t")? {
            Tag::Welcome => DaemonMsg::Welcome {
                proto: req(f.proto.take(), "proto")?,
                build: req(f.build.take(), "build")?,
                caps: f.caps.take().unwrap_or_default(),
                client: f.client.take().unwrap_or_default(),
                commands: f.commands.take().unwrap_or_default(),
            },
            Tag::Mismatch => DaemonMsg::Mismatch {
                supported: req(f.supported.take(), "supported")?,
                min: f.min.take(),
            },
            Tag::Draining => DaemonMsg::Draining,
            Tag::Attached => DaemonMsg::Attached {
                session: req(f.session.take(), "session")?,
                workspace: req(f.workspace.take(), "workspace")?,
            },
            Tag::Entry if f.kind == Some(Kind::Unknown) => DaemonMsg::Unknown { seq: f.seq.take() },
            Tag::Entry => DaemonMsg::Entry(f.entry()?),
            Tag::Delta => DaemonMsg::Delta {
                call: req(f.call.take(), "call")?,
                text: req(f.text.take(), "text")?,
            },
            Tag::Synced => DaemonMsg::Synced {
                seq: req(f.seq.take(), "seq")?,
            },
            Tag::Sessions => DaemonMsg::Sessions {
                sessions: req(f.sessions.take(), "sessions")?,
            },
            Tag::Status => DaemonMsg::Status {
                pid: req(f.pid.take(), "pid")?,
                build: req(f.build.take(), "build")?,
                started: req(f.started.take(), "started")?,
                clients: req(f.clients.take(), "clients")?,
                known: req(f.known.take(), "known")?,
            },
            Tag::Settings => DaemonMsg::Settings {
                settings: req(f.settings.take(), "settings")?,
                error: f.error.take(),
            },
            Tag::Recap => DaemonMsg::Recap(Recap {
                prompt: f.prompt.take(),
                tools: req(f.tools.take(), "tools")?,
                failed: req(f.failed.take(), "failed")?,
                running: req(f.running.take(), "running")?,
                reply: f.reply.take(),
                asks: req(f.asks.take(), "asks")?,
            }),
            Tag::Ask => DaemonMsg::Ask {
                call: req(f.call.take(), "call")?,
                tool: req(f.tool.take(), "tool")?,
                input: req(f.input.take(), "input")?,
            },
            Tag::Error => DaemonMsg::Error {
                message: req(f.message.take(), "message")?,
            },
            Tag::Snapshot => DaemonMsg::Snapshot(Box::new(SessionView {
                session: f.session.take(),
                workspace: req(f.workspace.take(), "workspace")?,
                seq: req(f.seq.take(), "seq")?,
                items: req(f.items.take(), "items")?,
                environment: f.environment.take(),
                ..SessionView::default()
            })),
            Tag::Ack => DaemonMsg::Ack {
                origin: req(f.origin.take(), "origin")?,
            },
            Tag::Rejected => DaemonMsg::Rejected {
                origin: req(f.origin.take(), "origin")?,
                reason: req(f.reason.take(), "reason")?,
                message: f.message.take().unwrap_or_default(),
            },
            Tag::Hello
            | Tag::Attach
            | Tag::Declare
            | Tag::Environment
            | Tag::Input
            | Tag::Steer
            | Tag::List
            | Tag::Detach
            | Tag::WatchSessions
            | Tag::Shutdown
            | Tag::Approve
            | Tag::Interrupt
            | Tag::Unknown => DaemonMsg::Unknown { seq: None },
        })
    }

    fn client_msg(&mut self) -> Result<ClientMsg> {
        let f = self;
        Ok(match req(f.t.take(), "t")? {
            Tag::Hello => ClientMsg::Hello {
                proto: req(f.proto.take(), "proto")?,
                build: req(f.build.take(), "build")?,
                min: f.min.take(),
                caps: f.caps.take().unwrap_or_default(),
            },
            Tag::Attach => ClientMsg::Attach {
                target: req(f.target.take(), "target")?,
                workspace: req(f.workspace.take(), "workspace")?,
                since: req(f.since.take(), "since")?,
                env: f.env.take(),
                decl: f.decl.take().unwrap_or_default(),
            },
            Tag::Declare => ClientMsg::Declare(ClientDecl {
                role: f.role.take().unwrap_or_default(),
                tools: f.tools.take().unwrap_or_default(),
                sites: f.sites.take().unwrap_or_default(),
            }),
            Tag::Environment => ClientMsg::Environment(environment(
                f.cwd.take(),
                f.vars.take(),
                f.redacted.take(),
                f.tty.take(),
            )?),
            Tag::Input => ClientMsg::Input {
                text: req(f.text.take(), "text")?,
                client_seq: f.client_seq.take(),
            },
            Tag::Steer => ClientMsg::Steer {
                text: req(f.text.take(), "text")?,
                client_seq: f.client_seq.take(),
            },
            Tag::List => ClientMsg::List,
            Tag::Detach => ClientMsg::Detach,
            Tag::WatchSessions => ClientMsg::WatchSessions,
            Tag::Status => ClientMsg::Status,
            Tag::Shutdown => ClientMsg::Shutdown,
            Tag::Settings => ClientMsg::Settings {
                change: f.change.take(),
            },
            Tag::Approve => ClientMsg::Approve {
                call: req(f.call.take(), "call")?,
                approved: req(f.approved.take(), "approved")?,
                client_seq: f.client_seq.take(),
            },
            Tag::Interrupt => ClientMsg::Interrupt {
                client_seq: f.client_seq.take(),
            },
            _ => return Err(Error::custom("unknown client message")),
        })
    }

    fn item(&mut self) -> Result<Item> {
        let f = self;
        Ok(match req(f.ty.take(), "type")? {
            Type::User => Item::User {
                seq: req(f.seq.take(), "seq")?,
                text: req(f.text.take(), "text")?,
            },
            Type::Assistant => Item::Assistant {
                seq: req(f.seq.take(), "seq")?,
                text: req(f.text.take(), "text")?,
            },
            Type::Reply => Item::Reply {
                call: req(f.call.take(), "call")?,
                model: req(f.model.take(), "model")?,
                text: req(f.text.take(), "text")?,
                status: req(f.status.take(), "status")?,
                usage: f.usage.take(),
            },
            Type::Tool => Item::Tool {
                call: req(f.call.take(), "call")?,
                tool: req(f.tool.take(), "tool")?,
                input: req(f.input.take(), "input")?,
                approval: f.approval.take(),
                outcome: f.outcome.take(),
                card: f.card.take(),
                asked: false,
            },
            Type::Command => Item::Command {
                call: req(f.call.take(), "call")?,
                name: req(f.name.take(), "name")?,
                args: req(f.args.take(), "args")?,
                outcome: f.outcome.take(),
            },
            Type::Hook => Item::Hook {
                seq: req(f.seq.take(), "seq")?,
                hook: req(f.hook.take(), "hook")?,
                decision: req(f.decision.take(), "decision")?,
            },
            Type::Child => Item::Child {
                call: req(f.call.take(), "call")?,
                child: req(f.child.take(), "child")?,
                input: text(f.input.take(), "input")?,
                watch: f.watch.take(),
                fired: f.fired.take(),
            },
            Type::Environment => Item::Environment {
                seq: req(f.seq.take(), "seq")?,
                cwd: req(f.cwd.take(), "cwd")?,
            },
        })
    }
}

impl Decode for Entry {
    fn from_json(bytes: &[u8]) -> Result<Self> {
        Fields::read(bytes)?.entry()
    }
}

/// Through [`Decode`]'s decoder, whatever the input: for a type that holds
/// an entry and derives its decoder.
impl<'de> Deserialize<'de> for Entry {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(d)?;
        let bytes = serde_json::to_vec(&value).map_err(D::Error::custom)?;
        Self::from_json(&bytes).map_err(D::Error::custom)
    }
}

/// Unknown kinds never abort a client; a known one that is malformed does.
impl Decode for DaemonMsg {
    fn from_json(bytes: &[u8]) -> Result<Self> {
        /// What says a message is of a kind this build does not know: read
        /// again only when the message did not decode, as a newer kind's
        /// fields may not have the types this build's do.
        #[derive(Deserialize)]
        struct Tags {
            t: Option<Tag>,
            kind: Option<Kind>,
            seq: Option<serde_json::Value>,
        }
        Fields::read(bytes)
            .and_then(|mut f| f.daemon_msg())
            .or_else(|e| match serde_json::from_slice::<Tags>(bytes) {
                Ok(Tags { t: Some(t), .. }) if !t.of_daemon() => {
                    Ok(DaemonMsg::Unknown { seq: None })
                }
                Ok(Tags {
                    t: Some(Tag::Entry),
                    kind: Some(Kind::Unknown),
                    seq,
                }) => Ok(DaemonMsg::Unknown {
                    seq: seq.as_ref().and_then(serde_json::Value::as_u64),
                }),
                _ => Err(e),
            })
    }
}

impl Decode for ClientMsg {
    fn from_json(bytes: &[u8]) -> Result<Self> {
        Fields::read(bytes)?.client_msg()
    }
}

/// Decoded in one pass, as the wire's messages are.
impl<'de> Deserialize<'de> for Item {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Fields::deserialize(MapOnly(d))?
            .item()
            .map_err(D::Error::custom)
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Ending {
    Running,
    Done,
    Failed,
    Interrupted,
}

#[derive(Deserialize)]
struct OutcomeFields {
    status: Ending,
    #[serde(default, deserialize_with = "present")]
    output: Option<Json>,
    error: Option<String>,
}

impl OutcomeFields {
    fn outcome(&mut self) -> Result<Outcome> {
        Ok(match self.status {
            Ending::Done => Outcome::Done {
                output: req(self.output.take(), "output")?,
            },
            Ending::Failed => Outcome::Failed {
                error: req(self.error.take(), "error")?,
            },
            Ending::Interrupted => Outcome::Interrupted {
                output: self.output.take().map(|o| o.parse()).transpose()?.flatten(),
            },
            Ending::Running => return Err(Error::custom("unknown variant `running`")),
        })
    }

    fn status(&mut self) -> Result<Status> {
        Ok(match self.status {
            Ending::Running => Status::Running,
            Ending::Done => Status::Done,
            Ending::Failed => Status::Failed {
                error: req(self.error.take(), "error")?,
            },
            Ending::Interrupted => Status::Interrupted,
        })
    }
}

/// A reply's status shares an outcome's tag and fields.
impl<'de> Deserialize<'de> for Status {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        OutcomeFields::deserialize(MapOnly(d))?
            .status()
            .map_err(D::Error::custom)
    }
}

impl<'de> Deserialize<'de> for Outcome {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        OutcomeFields::deserialize(MapOnly(d))?
            .outcome()
            .map_err(D::Error::custom)
    }
}

/// Whether this build knows entry kind `kind`.
#[cfg(test)]
pub(crate) fn known_kind(kind: &str) -> bool {
    matches!(serde_json::from_value::<Kind>(kind.into()), Ok(k) if k != Kind::Unknown)
}

/// Whether this build knows message tag `t`.
#[cfg(test)]
pub(crate) fn known_msg(t: &str) -> bool {
    serde_json::from_value::<Tag>(t.into()).is_ok_and(Tag::of_daemon)
}

/// Reads a struct as a map only. A JSON decoder also reads one from an
/// array of its fields, in as much code again, which no record is.
struct MapOnly<D>(D);

impl<'de, D: Deserializer<'de>> Deserializer<'de> for MapOnly<D> {
    type Error = D::Error;

    fn deserialize_any<V: Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, D::Error> {
        self.0.deserialize_any(visitor)
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> std::result::Result<V::Value, D::Error> {
        self.0.deserialize_map(visitor)
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map enum identifier ignored_any
    }
}
