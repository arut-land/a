//! Session logs: a header, then append-only entries, behind [`LogStore`] so
//! the daemon picks the backend. Each backend owns its own concurrency and
//! durability guarantees; the conformance suite in `tests` holds every one
//! to the same contract.

#[cfg(test)]
mod faulty;
mod jsonl;
mod lease;
#[cfg(test)]
mod memory;
#[cfg(test)]
mod tests;

use std::io;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(test)]
use protocol::Seq;
use protocol::{Entry, Epoch, Json, ReadOnly, SessionId};
use serde::{Deserialize, Serialize};

#[cfg(test)]
pub use faulty::{Faulty, SyncFailure};
pub use jsonl::Jsonl;
#[cfg(test)]
pub(crate) use jsonl::{FORMAT, seal, unseal};
pub use lease::{LeaseAuthority, Local};
#[cfg(test)]
pub use memory::Memory;

#[cfg(test)]
static FLUSH_RT: std::sync::LazyLock<rt::Threads> = std::sync::LazyLock::new(rt::Threads::default);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub id: SessionId,
    pub workspace: PathBuf,
    /// The session and call that own this one, if it is a child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Parent>,
}

/// Where a child session hangs off its parent: the parent's `link` entry,
/// whose seq is the owning call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parent {
    pub session: SessionId,
    pub call: u64,
}

/// What a session's log folds to at entry `covers`, as whoever loads it
/// reads it back: derived, so a load that cannot use it folds the log
/// instead, and a store keeps it as it can, beside the log and never
/// ahead of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub covers: u64,
    pub image: Json,
}

/// A session's writer, and the entries it continues as their lines.
pub struct Loaded {
    pub writer: Writer,
    /// The committed entries' lines, read back by seq from where the log
    /// keeps them rather than held in memory; appends add to them.
    pub lines: Arc<dyn Lines>,
    /// The seq of the last entry committed.
    pub seq: u64,
    /// The bytes of log read to load it: those after the snapshot taken,
    /// or all of them.
    pub read: u64,
}

impl Loaded {
    /// Appends `body` as the next entry, durably, and returns it.
    #[cfg(test)]
    pub fn append(&mut self, body: protocol::Body) -> io::Result<Entry> {
        let mut entry = [Entry::new(Seq::new(self.seq + 1).unwrap(), Epoch(0), body)];
        self.writer.append(&mut entry, true)?;
        rt::block_on(self.writer.flushed(&*FLUSH_RT))?;
        let [entry] = entry;
        self.seq = entry.seq.get();
        Ok(entry)
    }
}

/// Entries as the lines a log stores them: each one's JSON, encoded once
/// as it is appended, end to end without newlines.
#[derive(Default)]
pub struct Encoded {
    bytes: Vec<u8>,
    /// Where each line ends.
    ends: Vec<usize>,
}

impl Encoded {
    pub fn of(entries: &[Entry]) -> io::Result<Self> {
        let mut encoded = Self::default();
        for entry in entries {
            entry.encode(&mut encoded.bytes)?;
            debug_assert!(
                !encoded.bytes[encoded.ends.last().copied().unwrap_or(0)..]
                    .iter()
                    .any(|b| matches!(b, b'\n' | b'\r'))
            );
            encoded.ends.push(encoded.bytes.len());
        }
        Ok(encoded)
    }

    pub fn lines(&self) -> impl ExactSizeIterator<Item = &[u8]> {
        let mut start = 0;
        self.ends.iter().map(move |&end| {
            let line = &self.bytes[start..end];
            start = end;
            line
        })
    }
}

/// The one appender of a session's log, with the epoch of its lease: only
/// a store makes one, as it creates or loads the log, and it is moved, never
/// shared, so a session has one writer for as long as it lives.
pub struct Writer {
    pub(crate) log: Box<dyn Log>,
    epoch: u32,
}

impl Writer {
    pub(crate) fn new(log: Box<dyn Log>, epoch: u32) -> Self {
        Self { log, epoch }
    }

    #[cfg(test)]
    pub const fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Why the log is kept as it is, if it is: then every append fails.
    pub fn read_only(&self) -> Option<ReadOnly> {
        self.log.read_only()
    }

    /// Appends `entries`, each stamped with the lease's epoch, ordered if
    /// `durable`. Before publishing or starting effects, [`Writer::flushed`]
    /// completes that promise. Returns the lines encoded for publication.
    pub fn append(&mut self, entries: &mut [Entry], durable: bool) -> io::Result<Encoded> {
        for e in &mut *entries {
            e.epoch = Epoch(self.epoch);
        }
        let lines = Encoded::of(entries)?;
        if durable {
            self.log.append_all(entries, &lines)?;
        } else {
            self.log.write_all(entries, &lines)?;
        }
        Ok(lines)
    }

    /// Orders what was only written, before [`Writer::flushed`].
    pub fn sync(&mut self) -> io::Result<()> {
        self.log.sync()
    }

    pub fn durable_on_order(&self) -> bool {
        self.log.durable_on_order()
    }

    pub fn enroll(&mut self) -> Option<rt::FlushRequest> {
        self.log.enroll()
    }

    /// Waits for a cache flush covering the writer's earlier barriers.
    /// Backends whose barriers already flush return without scheduling work.
    pub async fn flushed(&mut self, rt: &dyn rt::Runtime) -> io::Result<()> {
        if let Some(request) = self.enroll() {
            request.dispatch(rt).await?;
        }
        Ok(())
    }

    /// Keeps `snapshot` beside the log, for the next load to start from:
    /// it covers entries committed already.
    pub fn snapshot(&mut self, snapshot: &Snapshot) -> io::Result<()> {
        self.log.snapshot(snapshot)
    }

    /// Appends `entries` durably as they are, epochs included: a log laid
    /// down for a test.
    #[cfg(test)]
    pub fn append_raw(&mut self, entries: &[Entry]) -> io::Result<()> {
        self.log.append_all(entries, &Encoded::of(entries)?)?;
        rt::block_on(self.flushed(&*FLUSH_RT))
    }
}

/// Where session logs live. Each `create` or `load` hands out the log's
/// writer under a lease newer than every one before.
pub trait LogStore: Send + Sync {
    /// A new, empty session log; fails with `AlreadyExists` if the id is taken.
    fn create(&self, header: &Header) -> io::Result<Loaded>;

    /// Every session's header, oldest first, without reading the entries.
    fn headers(&self) -> io::Result<Vec<Header>>;

    /// One session's appender, handing `each` the committed entries in
    /// order as they are read, one at a time: loading keeps none that
    /// `each` does not. The newest snapshot the store holds of the log,
    /// if it has one it can place, is handed to `restore` first: taken,
    /// only the entries after it are read. `NotFound` if there is no log.
    /// A log the store cannot continue without changing what it holds is
    /// handed out kept as it is ([`Writer::read_only`]), with the entries
    /// it reads.
    fn load(
        &self,
        id: SessionId,
        restore: &mut dyn FnMut(Snapshot) -> bool,
        each: &mut dyn FnMut(Entry),
    ) -> io::Result<Loaded>;

    /// One session's committed entries, read without taking its lease: its
    /// writer goes on appending.
    #[cfg(test)]
    fn entries(&self, id: SessionId) -> io::Result<Vec<Entry>>;

    /// The number of committed entries, without loading them.
    #[cfg(test)]
    fn count(&self, id: SessionId) -> io::Result<u64>;
}

/// One session's appender.
pub trait Log: Send {
    /// Appends `entries` in order and passes their write barrier. Once
    /// [`Log::enroll`] completes, a power cut never loses them. `lines` is
    /// the entries encoded, which a backend stores as they are.
    fn append_all(&mut self, entries: &[Entry], lines: &Encoded) -> io::Result<()>;

    /// Appends `entries` in order, to be read back at once and made
    /// durable by the next [`Log::append_all`]: a crash before it may lose
    /// them, and nothing after them.
    fn write_all(&mut self, entries: &[Entry], lines: &Encoded) -> io::Result<()> {
        self.append_all(entries, lines)
    }

    /// Makes what [`Log::write_all`] wrote durable, as the next
    /// [`Log::append_all`] would.
    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn durable_on_order(&self) -> bool {
        true
    }

    /// The cache-flush epoch after the last barrier, where it needs one.
    fn enroll(&mut self) -> Option<rt::FlushRequest> {
        None
    }

    /// Keeps `snapshot` for the next load, as it can: a backend that
    /// keeps none has every load fold the log.
    fn snapshot(&mut self, _: &Snapshot) -> io::Result<()> {
        Ok(())
    }

    fn read_only(&self) -> Option<ReadOnly> {
        None
    }
}

/// A session's committed entries as the lines its log stores: each one's
/// JSON, encoded once as it was appended, and read back as it was written.
pub trait Lines: Send + Sync {
    /// Appends the lines of the entries `seqs` starts with, each with its
    /// newline, to `out`: as many as fit in `limit` bytes, and at least
    /// one. Returns how many; `NotFound` if `seqs` holds one not committed.
    fn read(&self, seqs: RangeInclusive<u64>, limit: usize, out: &mut Vec<u8>) -> io::Result<u64>;
}

#[cfg(test)]
fn not_found(id: SessionId) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("no session {id}"))
}

/// Where entries `seqs` are among `len` committed: the index of the first
/// and of the last; `NotFound` unless all are committed.
fn span(seqs: &RangeInclusive<u64>, len: usize) -> io::Result<(usize, usize)> {
    let index = |seq: u64| {
        let at = usize::try_from(seq.checked_sub(1)?).ok()?;
        (at < len).then_some(at)
    };
    index(*seqs.start())
        .zip(index(*seqs.end()))
        .filter(|(first, last)| first <= last)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no entries {seqs:?}")))
}
