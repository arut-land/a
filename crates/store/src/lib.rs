//! Session logs: a header, then append-only entries, behind [`LogStore`] so
//! the daemon picks the backend. Each backend owns its own concurrency and
//! durability guarantees; the conformance suite in `tests` holds every one
//! to the same contract.

mod jsonl;
mod memory;
#[cfg(test)]
mod tests;

use std::io;
use std::path::PathBuf;

use protocol::{Entry, Installed, SessionId};
use serde::{Deserialize, Serialize};

pub use jsonl::Jsonl;
pub use memory::Memory;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub id: SessionId,
    pub workspace: PathBuf,
    /// The session and call that own this one, if it is a child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Parent>,
    /// The extensions the session was created with; a later
    /// [`protocol::Body::Extensions`] entry supersedes them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<Installed>,
}

/// Where a child session hangs off its parent: the parent's `link` entry,
/// whose seq is the owning call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parent {
    pub session: SessionId,
    pub call: u64,
}

/// A session's committed entries and the appender that continues them.
pub struct Loaded {
    pub entries: Vec<Entry>,
    pub log: Box<dyn Log>,
}

/// Where session logs live. Callers keep at most one appender per session
/// open at a time.
pub trait LogStore: Send + Sync {
    /// A new, empty session log; fails with `AlreadyExists` if the id is taken.
    fn create(&self, header: &Header) -> io::Result<Loaded>;

    /// Every session's header, oldest first, without reading the entries.
    fn headers(&self) -> io::Result<Vec<Header>>;

    /// One session's entries and an appender; `NotFound` if there is none.
    fn load(&self, id: SessionId) -> io::Result<Loaded>;

    /// The number of committed entries, without loading them.
    fn count(&self, id: SessionId) -> io::Result<u64>;

    /// Marks a session as having work in flight. A marker that survives a
    /// crash tells the next daemon to load that session at once and recover.
    /// Setting or clearing it twice is harmless.
    fn mark_busy(&self, id: SessionId, busy: bool) -> io::Result<()>;

    fn is_busy(&self, id: SessionId) -> bool;
}

/// One session's appender.
pub trait Log: Send {
    /// Returns once the entry is as durable as the backend makes anything: a
    /// crash afterwards never loses it.
    fn append(&mut self, entry: &Entry) -> io::Result<()>;
}

fn not_found(id: SessionId) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("no session {id}"))
}
