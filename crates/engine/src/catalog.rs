//! The session catalog: what the daemon lists, and which sessions it
//! recovers at once, without opening a log. It is derived: the daemon, the
//! one writer, appends changes without syncing. A durable dirty marker
//! precedes use; only an orderly shutdown syncs the complete catalog and
//! removes the marker. An unclean or unreadable catalog is rebuilt from
//! the logs, which are the truth.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use protocol::ledger::Ledger;
use protocol::{Activity, ReadOnly, SessionId, SessionSummary};
use serde::{Deserialize, Serialize};

use crate::lock;
use crate::store::{Header, LogStore};

/// A session as it lists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub header: Header,
    /// Work is in flight, or a call waits on someone: the next daemon
    /// loads the session at once, to recover it or to show what it asks.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub busy: bool,
    pub entries: u64,
    #[serde(default)]
    pub activity: Activity,
    /// Why its log is kept as it is, if it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<ReadOnly>,
}

impl Record {
    /// A session just made: nothing logged, nothing to do.
    pub const fn new(header: Header) -> Self {
        Self {
            header,
            busy: false,
            entries: 0,
            activity: Activity::Idle,
            read_only: None,
        }
    }

    /// How it lists, unloaded.
    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.header.id,
            workspace: self.header.workspace.clone(),
            entries: self.entries,
            clients: 0,
            activity: self.activity,
            residency: protocol::Residency::Hibernated,
            read_only: self.read_only,
        }
    }
}

pub struct Catalog {
    /// `None` keeps it in memory only.
    path: Option<PathBuf>,
    state: Mutex<State>,
    uncertain: AtomicBool,
    /// Each change of a session's busy mark, in order.
    #[cfg(test)]
    pub(crate) marks: Mutex<Vec<bool>>,
}

#[derive(Default)]
struct State {
    records: BTreeMap<u128, Record>,
    /// Lines in the file, old ones included.
    lines: usize,
    file: Option<File>,
}

impl Catalog {
    #[cfg(test)]
    /// One kept in memory.
    pub fn memory() -> Self {
        Self::with(None, State::default())
    }

    /// The catalog at `path` of the logs `store` keeps: read, or
    /// rebuilt from the logs unless the last daemon left it clean.
    pub fn open(path: PathBuf, store: &dyn LogStore) -> io::Result<Self> {
        let dirty = path.with_extension("dirty");
        let read = match fs::exists(&dirty) {
            Ok(false) => read(&path).ok(),
            _ => None,
        };
        // No log may change while a clean catalog can survive a crash.
        File::create(&dirty)?.sync_all()?;
        sys::fs::sync_dir(path.parent().unwrap())?;
        let rebuilt = read.is_none();
        let catalog = Self::with(Some(path), read.map_or_else(|| rebuild(store), Ok)?);
        let mut state = lock(&catalog.state);
        if rebuilt || state.bloated() {
            catalog.compact(&mut state);
        } else if let Some(path) = &catalog.path {
            state.file = Some(OpenOptions::new().append(true).open(path)?);
        }
        drop(state);
        Ok(catalog)
    }

    /// Called only once all session writers have stopped. Rewriting avoids
    /// trusting an earlier best-effort append that may have failed.
    pub fn clean(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if self.uncertain.load(Ordering::Acquire) {
            return Err(io::Error::other(
                "an interrupted import requires catalog rebuild",
            ));
        }
        let mut state = lock(&self.state);
        state.file = Some(rewrite(path, &state)?);
        state.lines = state.records.len();
        fs::remove_file(path.with_extension("dirty"))?;
        sys::fs::sync_dir(path.parent().unwrap())
    }

    fn with(path: Option<PathBuf>, state: State) -> Self {
        Self {
            path,
            state: Mutex::new(state),
            uncertain: AtomicBool::new(false),
            #[cfg(test)]
            marks: Mutex::default(),
        }
    }

    /// Every session, oldest first.
    pub fn records(&self) -> Vec<Record> {
        lock(&self.state).records.values().cloned().collect()
    }

    pub fn get(&self, id: SessionId) -> Option<Record> {
        lock(&self.state).records.get(&id.0.as_u128()).cloned()
    }

    /// Publication and cataloguing share one recovery scope. Even an unwind
    /// after rename leaves the dirty marker for the next startup to rebuild.
    pub fn import(
        &self,
        store: &dyn LogStore,
        header: &Header,
        entries: &[protocol::Entry],
    ) -> io::Result<()> {
        struct Publishing<'a> {
            catalog: &'a Catalog,
            complete: bool,
        }
        impl Drop for Publishing<'_> {
            fn drop(&mut self) {
                if !self.complete {
                    self.catalog.uncertain.store(true, Ordering::Release);
                }
            }
        }
        let mut publishing = Publishing {
            catalog: self,
            complete: false,
        };
        store.import(header, entries)?;
        let mut record = Record::new(header.clone());
        record.entries = entries.last().map_or(0, |entry| entry.seq.get());
        self.record(record);
        publishing.complete = true;
        Ok(())
    }

    /// Notes `record`, unless it is what the catalog says already. It
    /// blocks on a write, so it runs where that may.
    pub fn record(&self, record: Record) {
        let mut state = lock(&self.state);
        let id = record.header.id.0.as_u128();
        #[cfg(test)]
        if state
            .records
            .get(&id)
            .is_some_and(|r| r.busy != record.busy)
        {
            lock(&self.marks).push(record.busy);
        }
        if state.records.get(&id) == Some(&record) {
            return;
        }
        let line = line(&record);
        state.records.insert(id, record);
        state.lines += 1;
        if state.bloated() {
            return self.compact(&mut state);
        }
        if let (Some(file), Ok(line)) = (&mut state.file, line)
            && let Err(e) = file.write_all(&line)
        {
            eprintln!("the session catalog: {e}");
        }
    }

    /// Rewrites the file with one line a session, in one step: a crash
    /// leaves the old one or the new.
    fn compact(&self, state: &mut State) {
        let Some(path) = &self.path else { return };
        let written = rewrite(path, state);
        match written {
            Ok(file) => {
                state.file = Some(file);
                state.lines = state.records.len();
            }
            Err(e) => eprintln!("the session catalog: {e}"),
        }
    }
}

impl State {
    /// Whether most of the file's lines are old ones.
    fn bloated(&self) -> bool {
        self.lines > 2 * self.records.len() + 64
    }
}

fn line(record: &Record) -> io::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    Ok(line)
}

/// The catalog's lines folded, the last of each session's winning.
fn read(path: &Path) -> io::Result<State> {
    let mut state = State::default();
    for line in BufReader::new(File::open(path)?).split(b'\n') {
        let record: Record = serde_json::from_slice(&line?)?;
        state.records.insert(record.header.id.0.as_u128(), record);
        state.lines += 1;
    }
    Ok(state)
}

/// Every log read through: a session is busy if it was left with
/// anything open or waiting.
fn rebuild(store: &dyn LogStore) -> io::Result<State> {
    let mut state = State::default();
    for header in store.headers()? {
        let mut ledger = Ledger::default();
        let loaded = store.load(header.id, &mut |_| false, &mut |e| ledger.apply(&e));
        let (entries, read_only) = match loaded {
            Ok(loaded) => (loaded.seq, loaded.writer.read_only()),
            Err(e) => {
                eprintln!("session {}: cataloguing: {e}", header.id.short());
                continue;
            }
        };
        let activity = ledger.activity();
        let record = Record {
            busy: activity != Activity::Idle && read_only.is_none(),
            entries,
            activity,
            read_only,
            header,
        };
        state.records.insert(record.header.id.0.as_u128(), record);
    }
    Ok(state)
}

fn rewrite(path: &Path, state: &State) -> io::Result<File> {
    let new = path.with_extension("new");
    let mut file = File::create(&new)?;
    for record in state.records.values() {
        file.write_all(&line(record)?)?;
    }
    file.sync_all()?;
    sys::fs::replace(&new, path)?;
    sys::fs::sync_dir(path.parent().unwrap())?;
    OpenOptions::new().append(true).open(path)
}
