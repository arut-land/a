//! The internally tagged enums, decoded in one pass. Serde's derive for
//! one buffers the object to find its tag, then decodes the variant from
//! the buffer, in code of its own for every variant. Here every record on
//! the wire is read into one flat struct of its family (an entry, a
//! message, a snapshot's item) holding the tags and the fields of all its
//! kinds, each optional; a match on the tag then builds the variant and
//! names a required field that is missing.
//!
//! So a field name means one type across a family: where two differ
//! (`input`, a tool call's JSON and a link's text), the field is [`Json`]
//! and the variant reads its type from it ([`Field`]). A `null` reads as
//! absent, but for a [`Json`] field, where it is the value.
//!
//! Each union is declared once, with [`kinds!`]: its variants, their tags,
//! their [`Class`](crate::Class) and how each decodes. What it declares is
//! what the enum, its serializer, its decoder and its tag table are made
//! of, so a kind cannot be in one and not the others.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::de::{Deserializer, Error as _, Visitor};
use serde::{Deserialize, forward_to_deserialize_any};
use serde_json::{Error, Result};

use crate::view::{Approval, Item, SessionView, Status, Usage};
use crate::{
    Audience, Author, Body, CallId, Caps, Card, ClientDecl, ClientId, ClientMsg, CommandInfo,
    DaemonMsg, Decode, Entry, Environment, HomeSettings, Installed, Json, Mode, Outcome,
    Permission, Rejection, Role, Seq, SessionId, SessionSummary, SettingsChange, Target,
    ToolChoice, Verdict, WaitOn,
};

/// Declares a tagged union of the wire, one line per kind:
///
/// ```text
/// /// docs
/// Name "tag" { field: Type, field: Type = default } = Class, read path;
/// ```
///
/// - a field without `= default` is required; one with it reads as its
///   default when absent and is left out when it is the default;
/// - `( Type )` is a newtype variant, which `read path` decodes;
/// - `read path` decodes the kind with `path(&mut F)` instead, `F` the
///   union's flat fields (`in F`);
/// - `= Class` (in a union declared `class`) classifies it, by name or
///   by a `(function)` of the value;
///
/// A union declared `class` may also name the variant decoding yields for
/// a message of a kind it does not know: `unknown Name { fields } = Class;`.
///
/// It makes the enum (tag and kind names as given, so the encoder writes
/// what the decoder reads), `TAGS` (checked distinct at compile time),
/// `knows`, the one-pass decoder `read`, and `class` when asked.
macro_rules! kinds {
    (
        $(#[$meta:meta])*
        $vis:vis enum $E:ident: $slot:ident = $tag:literal in $F:ident, class {
            $(
                $(#[$vmeta:meta])*
                $V:ident $vtag:literal
                $({ $( $(#[$fmeta:meta])* $f:ident : $fty:ty $(= $m:ident)? ),* $(,)? })?
                $(( $nty:ty ))?
                = $class:tt
                $(, read $dec:path)?
            ;)*
        }
        $(
            $(#[$umeta:meta])*
            unknown $U:ident { $( $uf:ident : $uty:ty ),* } = $uclass:ident;
        )?
        else $fallback:expr
    ) => {
        kinds! { @union [$(#[$meta])*] $vis $E $F $slot $tag [$fallback]
            [$( $(#[$umeta])* #[serde(skip)] $U { $( $uf: $uty ),* }, )?]
            [$(
                [$(#[$vmeta])*] $V $vtag [$({ $( [$(#[$fmeta])*] $f : $fty $(= $m)? ),* })?] [$(( $nty ))?] [$($dec)?]
            )*]
        }

        impl $E {
            /// How it stands to the session's log.
            pub const fn class(&self) -> $crate::Class {
                let this = self;
                match this {
                    $(Self::$V { .. } => kinds!(@class this $class),)*
                    $(Self::$U { .. } => $crate::Class::$uclass,)?
                }
            }
        }
    };
    (
        $(#[$meta:meta])*
        $vis:vis enum $E:ident: $slot:ident = $tag:literal in $F:ident {
            $(
                $(#[$vmeta:meta])*
                $V:ident $vtag:literal
                $({ $( $(#[$fmeta:meta])* $f:ident : $fty:ty $(= $m:ident)? ),* $(,)? })?
                $(( $nty:ty ))?
                $(, read $dec:path)?
            ;)*
        }
        else $fallback:expr
    ) => {
        kinds! { @union [$(#[$meta])*] $vis $E $F $slot $tag [$fallback] [] [$(
            [$(#[$vmeta])*] $V $vtag [$({ $( [$(#[$fmeta])*] $f : $fty $(= $m)? ),* })?] [$(( $nty ))?] [$($dec)?]
        )*] }
    };

    (@union [$(#[$meta:meta])*] $vis:vis $E:ident $F:ident $slot:ident $tag:literal [$fallback:expr] [$($extra:tt)*] [$(
        [$(#[$vmeta:meta])*] $V:ident $vtag:literal
        [$({ $( [$(#[$fmeta:meta])*] $f:ident : $fty:ty $(= $m:ident)? ),* })?]
        [$(( $nty:ty ))?]
        [$($dec:path)?]
    )*]) => {
        $(#[$meta])*
        #[serde(tag = $tag)]
        $vis enum $E {
            $(
                $(#[$vmeta])*
                #[serde(rename = $vtag)]
                $V $({ $(
                    $(#[$fmeta])*
                    $(#[serde($m, skip_serializing_if = "crate::decode::skip")])?
                    $f: $fty,
                )* })? $(($nty))?,
            )*
            $($extra)*
        }

        impl $E {
            /// The tag of every kind this build reads.
            pub const TAGS: &'static [&'static str] = &[$($vtag),*];

            /// Whether this build reads kind `tag`.
            pub fn knows(tag: &str) -> bool {
                Self::TAGS.contains(&tag)
            }

            /// Builds the kind its tag names from the fields read.
            pub(crate) fn read(f: &mut $crate::decode::$F) -> ::serde_json::Result<Self> {
                let tag = $crate::decode::req(f.$slot.take(), $tag)?;
                Ok(match tag.as_str() {
                    $($vtag => kinds!(@build $E $V f [$($dec)?] $({ $( $f $(= $m)? ),* })?),)*
                    _ => return $fallback,
                })
            }
        }

        const _: () = assert!($crate::decode::distinct($E::TAGS), "two kinds share a tag");
    };

    (@build $E:ident $V:ident $f:ident [$dec:path] $($fields:tt)?) => { return $dec($f) };
    (@build $E:ident $V:ident $f:ident []) => { $E::$V };
    (@build $E:ident $V:ident $f:ident [] { $( $fld:ident $(= $m:ident)? ),* }) => {
        $E::$V { $( $fld: kinds!(@read $f $fld $(= $m)?), )* }
    };
    (@read $f:ident $fld:ident) => { $crate::decode::need($f.$fld.take(), stringify!($fld))? };
    (@read $f:ident $fld:ident = default) => { $crate::decode::def($f.$fld.take())? };
    (@class $this:ident $class:ident) => { $crate::Class::$class };
    (@class $this:ident ($by:path)) => { $by($this) };
}

/// A field every variant that has it requires.
pub(crate) fn req<T>(field: Option<T>, name: &'static str) -> Result<T> {
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

/// A required field, as its variant has it.
#[inline]
pub(crate) fn need<S, T: Field<S>>(field: Option<S>, name: &'static str) -> Result<T> {
    T::read(req(field, name)?)
}

/// A field that reads as its default when absent.
#[inline]
pub(crate) fn def<S, T: Field<S> + Default>(field: Option<S>) -> Result<T> {
    field.map_or_else(|| Ok(T::default()), T::read)
}

/// Whether a field that reads as its default when absent is left out.
pub(crate) fn skip<T: Default + PartialEq>(field: &T) -> bool {
    *field == T::default()
}

/// Whether no two of `tags` are the same, at compile time.
pub(crate) const fn distinct(tags: &[&str]) -> bool {
    const fn same(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let mut i = 0;
        while i < a.len() {
            if a[i] != b[i] {
                return false;
            }
            i += 1;
        }
        true
    }
    let mut i = 0;
    while i < tags.len() {
        let mut j = i + 1;
        while j < tags.len() {
            if same(tags[i].as_bytes(), tags[j].as_bytes()) {
                return false;
            }
            j += 1;
        }
        i += 1;
    }
    true
}

/// A variant's field from the flat field of its name: the same value, or
/// for a name two variants give different types, read from the [`Json`].
pub(crate) trait Field<S>: Sized {
    fn read(field: S) -> Result<Self>;
}

impl<S, T: Field<S>> Field<S> for Option<T> {
    #[inline]
    fn read(field: S) -> Result<Self> {
        T::read(field).map(Some)
    }
}

macro_rules! same {
    ($($t:ty),* $(,)?) => {$(
        impl Field<$t> for $t {
            #[inline]
            fn read(field: $t) -> Result<Self> {
                Ok(field)
            }
        }
    )*};
}

same!(
    String,
    bool,
    u16,
    u32,
    u64,
    Json,
    Card,
    Outcome,
    Permission,
    Mode,
    ToolChoice,
    Seq,
    Author,
    SessionId,
    PathBuf,
    Caps,
    Rejection,
    Target,
    Environment,
    ClientDecl,
    HomeSettings,
    SettingsChange,
    Vec<String>,
    Vec<Installed>,
    Vec<CommandInfo>,
    Vec<SessionSummary>,
    Vec<Item>,
);

macro_rules! parsed {
    ($($t:ty),* $(,)?) => {$(
        impl Field<Json> for $t {
            fn read(field: Json) -> Result<Self> {
                field.parse()
            }
        }
    )*};
}

parsed!(
    String,
    u32,
    Seq,
    Outcome,
    ClientId,
    Vec<Item>,
    Vec<SessionSummary>
);

/// A call is an entry's seq, never 0.
impl Field<u64> for CallId {
    fn read(field: u64) -> Result<Self> {
        Self::new(field).ok_or_else(|| Error::custom("call 0 names no entry"))
    }
}

/// A caller a newer build has counts as neither the model nor the user.
impl Field<String> for Audience {
    fn read(field: String) -> Result<Self> {
        Ok(match field.as_str() {
            "model" => Self::Model,
            "user" => Self::User,
            _ => Self::Plugin,
        })
    }
}

/// A record's tag (`kind`, `t`, `d`) as read, kept inline: every record has
/// one, and none is long. One longer than any reads as no tag of any kind.
#[derive(Clone, Copy)]
pub(crate) struct Tag {
    len: u8,
    bytes: [u8; Self::MAX],
}

impl Tag {
    const MAX: usize = 22;

    pub(crate) fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or_default()
    }
}

impl<'de> Deserialize<'de> for Tag {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Read;
        impl Visitor<'_> for Read {
            type Value = Tag;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a tag")
            }

            fn visit_str<E: serde::de::Error>(self, s: &str) -> std::result::Result<Tag, E> {
                let mut tag = Tag {
                    len: 0,
                    bytes: [0; Tag::MAX],
                };
                if let Some(to) = tag.bytes.get_mut(..s.len()) {
                    to.copy_from_slice(s.as_bytes());
                    tag.len = u8::try_from(s.len()).unwrap_or_default();
                }
                Ok(tag)
            }
        }
        d.deserialize_str(Read)
    }
}

/// A [`Json`] field, `null` included: `None` only when absent, with
/// `#[serde(default)]`.
fn present<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<Json>, D::Error> {
    Json::deserialize(d).map(Some)
}

// One set of flat fields per family of records (an entry, a message, a
// snapshot's item), so each visitor holds the decoders of its own fields
// only.

/// An [`Entry`], or a [`Verdict`] within one.
#[derive(Deserialize)]
pub(crate) struct EntryFields {
    pub(crate) seq: Option<u64>,
    pub(crate) epoch: Option<u32>,
    pub(crate) kind: Option<Tag>,
    pub(crate) text: Option<String>,
    pub(crate) mode: Option<Mode>,
    pub(crate) taken: Option<bool>,
    pub(crate) tool: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) input: Option<Json>,
    pub(crate) id: Option<String>,
    pub(crate) card: Option<Card>,
    pub(crate) by: Option<String>,
    pub(crate) call: Option<u64>,
    pub(crate) pgid: Option<u32>,
    pub(crate) boot: Option<String>,
    pub(crate) background: Option<bool>,
    pub(crate) outcome: Option<Outcome>,
    pub(crate) model: Option<String>,
    pub(crate) attempt: Option<u16>,
    pub(crate) choice: Option<ToolChoice>,
    #[serde(rename = "for")]
    pub(crate) r#for: Option<u64>,
    pub(crate) purpose: Option<String>,
    pub(crate) on: Option<Tag>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) ui: Option<Json>,
    pub(crate) link: Option<Seq>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) value: Option<Json>,
    pub(crate) origin: Option<Author>,
    pub(crate) child: Option<SessionId>,
    pub(crate) watch: Option<u64>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) vars: Option<BTreeMap<String, String>>,
    pub(crate) redacted: Option<Vec<String>>,
    pub(crate) tty: Option<bool>,
    pub(crate) installed: Option<Vec<Installed>>,
    pub(crate) ext: Option<String>,
    pub(crate) state: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) data: Option<Json>,
    pub(crate) target: Option<Seq>,
    pub(crate) upto: Option<Seq>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) replace: Option<Json>,
    // A decision's.
    pub(crate) d: Option<Tag>,
    pub(crate) permission: Option<Permission>,
    pub(crate) reason: Option<String>,
    pub(crate) approved: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) original: Option<Json>,
    pub(crate) error: Option<String>,
}

/// A [`ClientMsg`] or a [`DaemonMsg`] other than an entry.
#[derive(Deserialize)]
pub(crate) struct MsgFields {
    pub(crate) t: Option<Tag>,
    pub(crate) seq: Option<u64>,
    pub(crate) call: Option<u64>,
    pub(crate) text: Option<String>,
    pub(crate) attempt: Option<u16>,
    pub(crate) proto: Option<u32>,
    pub(crate) build: Option<String>,
    pub(crate) caps: Option<Caps>,
    pub(crate) client: Option<Json>,
    pub(crate) commands: Option<Vec<CommandInfo>>,
    pub(crate) supported: Option<u32>,
    pub(crate) min: Option<u32>,
    pub(crate) session: Option<SessionId>,
    pub(crate) workspace: Option<PathBuf>,
    pub(crate) sessions: Option<Json>,
    pub(crate) tools: Option<Vec<String>>,
    pub(crate) message: Option<String>,
    pub(crate) mode: Option<Mode>,
    pub(crate) reason: Option<Rejection>,
    pub(crate) items: Option<Json>,
    pub(crate) queued: Option<Vec<(u64, String)>>,
    pub(crate) title: Option<String>,
    pub(crate) environment: Option<Environment>,
    pub(crate) pid: Option<u32>,
    pub(crate) started: Option<u64>,
    pub(crate) clients: Option<u32>,
    pub(crate) known: Option<u32>,
    pub(crate) settings: Option<HomeSettings>,
    pub(crate) error: Option<String>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) vars: Option<BTreeMap<String, String>>,
    pub(crate) redacted: Option<Vec<String>>,
    pub(crate) tty: Option<bool>,
    // A client's.
    pub(crate) target: Option<Target>,
    pub(crate) since: Option<u64>,
    pub(crate) env: Option<Environment>,
    pub(crate) decl: Option<ClientDecl>,
    pub(crate) role: Option<Role>,
    pub(crate) sites: Option<Vec<String>>,
    pub(crate) client_seq: Option<u64>,
    pub(crate) approved: Option<bool>,
    pub(crate) change: Option<SettingsChange>,
}

/// An [`Item`] of a snapshot.
#[derive(Deserialize)]
struct ItemFields {
    #[serde(rename = "type")]
    ty: Option<Type>,
    seq: Option<u64>,
    text: Option<String>,
    call: Option<u64>,
    model: Option<String>,
    status: Option<Status>,
    usage: Option<Usage>,
    purpose: Option<String>,
    tool: Option<String>,
    #[serde(default, deserialize_with = "present")]
    input: Option<Json>,
    approval: Option<Approval>,
    outcome: Option<Outcome>,
    card: Option<Card>,
    by: Option<String>,
    hook: Option<String>,
    #[serde(default, deserialize_with = "present")]
    decision: Option<Json>,
    child: Option<SessionId>,
    watch: Option<u64>,
    fired: Option<Outcome>,
    cwd: Option<PathBuf>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Type {
    User,
    Assistant,
    Reply,
    Tool,

    Hook,
    Child,
    Environment,
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

/// Reads a record's fields from `bytes`, which hold one JSON object.
fn read<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let mut json = serde_json::Deserializer::from_slice(bytes);
    let fields = T::deserialize(MapOnly(&mut json))?;
    json.end()?;
    Ok(fields)
}

// How the kinds that are not a plain set of fields read.

pub(crate) fn environment_entry(f: &mut EntryFields) -> Result<Body> {
    environment(f.cwd.take(), f.vars.take(), f.redacted.take(), f.tty.take()).map(Body::Environment)
}

pub(crate) fn wait_entry(f: &mut EntryFields) -> Result<Body> {
    Ok(Body::Wait {
        call: need(f.call.take(), "call")?,
        on: WaitOn::read(f)?,
    })
}

pub(crate) fn decision_entry(f: &mut EntryFields) -> Result<Body> {
    Ok(Body::Decision {
        by: need(f.by.take(), "by")?,
        call: def(f.call.take())?,
        d: Verdict::read(f)?,
    })
}

/// Entries have fields of their own: [`DaemonMsg`]'s decoder reads them
/// with [`EntryFields`] before it gets here.
pub(crate) fn entry_msg(_: &mut MsgFields) -> Result<DaemonMsg> {
    Err(Error::custom("an entry reads as an entry"))
}

pub(crate) fn snapshot_msg(f: &mut MsgFields) -> Result<DaemonMsg> {
    Ok(DaemonMsg::Snapshot(Box::new(SessionView {
        session: f.session.take(),
        workspace: req(f.workspace.take(), "workspace")?,
        seq: req(f.seq.take(), "seq")?,
        items: need(f.items.take(), "items")?,
        environment: def(f.environment.take())?,
        queued: f.queued.take().unwrap_or_default(),
        title: f.title.take(),
        ..SessionView::default()
    })))
}

#[allow(clippy::unnecessary_wraps, reason = "as `kinds!` calls it")]
pub(crate) fn declare_msg(f: &mut MsgFields) -> Result<ClientMsg> {
    Ok(ClientMsg::Declare(ClientDecl {
        role: f.role.take().unwrap_or_default(),
        tools: f.tools.take().unwrap_or_default(),
        sites: f.sites.take().unwrap_or_default(),
    }))
}

pub(crate) fn environment_msg(f: &mut MsgFields) -> Result<ClientMsg> {
    environment(f.cwd.take(), f.vars.take(), f.redacted.take(), f.tty.take())
        .map(ClientMsg::Environment)
}

impl EntryFields {
    fn entry(&mut self) -> Result<Entry> {
        Ok(Entry {
            seq: req(self.seq.take(), "seq")?,
            epoch: req(self.epoch.take(), "epoch")?,
            body: Body::read(self)?,
        })
    }
}

impl ItemFields {
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
                purpose: f.purpose.take(),
            },
            Type::Tool => Item::Tool {
                call: req(f.call.take(), "call")?,
                tool: req(f.tool.take(), "tool")?,
                input: req(f.input.take(), "input")?,
                approval: f.approval.take(),
                outcome: f.outcome.take(),
                card: f.card.take(),
                by: def(f.by.take())?,
            },
            Type::Hook => Item::Hook {
                seq: req(f.seq.take(), "seq")?,
                hook: req(f.hook.take(), "hook")?,
                call: f.call.take(),
                tool: f.tool.take().unwrap_or_default(),
                decision: verdict(&req(f.decision.take(), "decision")?)?,
            },
            Type::Child => Item::Child {
                call: req(f.call.take(), "call")?,
                child: req(f.child.take(), "child")?,
                input: need(f.input.take(), "input")?,
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

/// A [`Verdict`] as an item nests it, read by the decoder an entry's is.
fn verdict(json: &Json) -> Result<Verdict> {
    Verdict::read(&mut read::<EntryFields>(json.raw().as_bytes())?)
}

impl Decode for Entry {
    /// One of a kind this build does not know reads as [`Body::Unknown`],
    /// whatever its other fields hold; a known kind that is malformed is
    /// an error.
    fn from_json(bytes: &[u8]) -> Result<Self> {
        // Looked for only once the entry does not read, off the hot path.
        read::<EntryFields>(bytes)
            .and_then(|mut f| f.entry())
            .or_else(|e| unknown_entry(bytes).ok_or(e))
    }
}

/// An entry of a kind this build does not know, kept as its text; `None`
/// for one of a kind it does.
#[cold]
fn unknown_entry(bytes: &[u8]) -> Option<Entry> {
    #[derive(Deserialize)]
    struct Tags {
        seq: u64,
        epoch: u32,
        kind: String,
    }
    let tags = read::<Tags>(bytes).ok().filter(|t| !Body::knows(&t.kind))?;
    Some(Entry {
        seq: tags.seq,
        epoch: tags.epoch,
        body: Body::Unknown {
            kind: tags.kind,
            raw: serde_json::from_slice(bytes).ok()?,
        },
    })
}

/// Through [`Decode`]'s decoder, whatever the input: for a type that holds
/// an entry and derives its decoder.
impl<'de> Deserialize<'de> for Entry {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        // As text, so what it holds keeps the text it was written as.
        let raw = Box::<serde_json::value::RawValue>::deserialize(d)?;
        Self::from_json(raw.get().as_bytes()).map_err(D::Error::custom)
    }
}

/// An entry's frame, its kind known or not.
fn entry_frame(bytes: &[u8]) -> Result<DaemonMsg> {
    let mut f = read::<EntryFields>(bytes)?;
    if f.kind.is_some_and(|k| !Body::knows(k.as_str())) {
        return Ok(DaemonMsg::Unknown { seq: f.seq.take() });
    }
    f.entry().map(DaemonMsg::Entry)
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
            kind: Option<Tag>,
            seq: Option<serde_json::Value>,
        }
        // A daemon tags an entry's frame first; another peer's order needs
        // a look at the tag.
        let decoded = if bytes.starts_with(br#"{"t":"entry","#) {
            entry_frame(bytes)
        } else {
            read::<MsgFields>(bytes).and_then(|mut f| match f.t {
                Some(t) if t.as_str() == "entry" => entry_frame(bytes),
                _ => DaemonMsg::read(&mut f),
            })
        };
        decoded.or_else(|e| match serde_json::from_slice::<Tags>(bytes) {
            Ok(Tags { t: Some(t), .. }) if !DaemonMsg::knows(t.as_str()) => {
                Ok(DaemonMsg::Unknown { seq: None })
            }
            Ok(Tags {
                t: Some(t),
                kind: Some(kind),
                seq,
            }) if t.as_str() == "entry" && !Body::knows(kind.as_str()) => Ok(DaemonMsg::Unknown {
                seq: seq.as_ref().and_then(serde_json::Value::as_u64),
            }),
            _ => Err(e),
        })
    }
}

impl Decode for ClientMsg {
    fn from_json(bytes: &[u8]) -> Result<Self> {
        read::<MsgFields>(bytes).and_then(|mut f| ClientMsg::read(&mut f))
    }
}

/// Decoded in one pass, as the wire's messages are.
impl<'de> Deserialize<'de> for Item {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        ItemFields::deserialize(MapOnly(d))?
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
