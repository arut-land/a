//! Session logs: a header, then append-only entries, behind [`LogStore`] so
//! the daemon picks the backend. Each backend owns its own concurrency and
//! durability guarantees; the conformance suite in `tests` holds every one
//! to the same contract.

mod jsonl;
mod lease;
mod memory;
#[cfg(test)]
mod tests;

use std::io;
use std::path::PathBuf;

use protocol::{Body, Entry, Installed, SessionId};
use serde::{Deserialize, Serialize};

pub use jsonl::Jsonl;
pub use lease::{Lease, LeaseAuthority, Local};
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
    /// The epoch of the appender's lease, which every entry it appends
    /// carries.
    pub epoch: u32,
}

impl Loaded {
    /// Appends `body` as the next entry, durably, and keeps it in `entries`.
    pub fn append(&mut self, body: Body) -> io::Result<()> {
        let entry = Entry::new(self.entries.len() as u64 + 1, self.epoch, body);
        self.log.append(&entry)?;
        self.entries.push(entry);
        Ok(())
    }
}

/// The newest epoch in `entries`: a new lease must be past it.
fn floor(entries: &[Entry]) -> u32 {
    entries.last().map_or(0, |e| e.epoch)
}

/// Where session logs live. Each `create` or `load` leases the appender it
/// returns, superseding any earlier one for the session: that one's appends
/// fail from then on.
pub trait LogStore: Send + Sync {
    /// A new, empty session log; fails with `AlreadyExists` if the id is taken.
    fn create(&self, header: &Header) -> io::Result<Loaded>;

    /// Every session's header, oldest first, without reading the entries.
    fn headers(&self) -> io::Result<Vec<Header>>;

    /// One session's entries and an appender; `NotFound` if there is none.
    fn load(&self, id: SessionId) -> io::Result<Loaded>;

    /// One session's committed entries, read without taking its lease: its
    /// writer goes on appending.
    fn entries(&self, id: SessionId) -> io::Result<Vec<Entry>>;

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
    /// crash afterwards never loses it. Fails with `ResourceBusy` and writes
    /// nothing if the appender's lease was superseded or the entry carries
    /// another epoch.
    fn append(&mut self, entry: &Entry) -> io::Result<()> {
        self.append_all(std::slice::from_ref(entry))
    }

    /// Appends `entries` in order, as durably as [`Log::append`]; a backend
    /// may make them durable together. After a crash the log holds a prefix
    /// of them.
    fn append_all(&mut self, entries: &[Entry]) -> io::Result<()>;
}

fn not_found(id: SessionId) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("no session {id}"))
}
