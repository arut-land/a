//! The session catalog: what the daemon lists, and which sessions it
//! recovers at once, without opening a log. It is derived: the daemon, the
//! one writer, coalesces revisions outside session commits. A durable dirty marker
//! precedes use; only an orderly shutdown syncs the complete catalog and
//! removes the marker. An unclean or unreadable catalog is rebuilt from
//! the logs, which are the truth.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

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
    writer: Arc<CatalogWriter>,
    state: Arc<Mutex<State>>,
    uncertain: AtomicBool,
    /// Each change of a session's busy mark, in order.
    #[cfg(test)]
    pub(crate) marks: Mutex<Vec<bool>>,
}

struct CatalogWriter {
    /// `None` keeps it in memory only.
    path: Option<PathBuf>,
    gate: Mutex<Image>,
    #[cfg(test)]
    compacting: Mutex<Option<Box<dyn FnMut() + Send>>>,
    runtime: OnceLock<Arc<dyn rt::Runtime>>,
    scheduled: AtomicBool,
    closed: AtomicBool,
}

#[derive(Default)]
struct Image {
    records: BTreeMap<u128, Record>,
    lines: usize,
}

#[derive(Default)]
struct State {
    records: BTreeMap<u128, Record>,
    pending: BTreeMap<u128, Record>,
}

impl Catalog {
    #[cfg(test)]
    /// One kept in memory.
    pub fn memory() -> Self {
        Self::with(None, Image::default())
    }

    /// The catalog at `path` of the logs `store` keeps: read, or
    /// rebuilt from the logs unless the last daemon left it clean.
    pub fn open(path: PathBuf, store: &dyn LogStore) -> io::Result<Self> {
        use ext_logging::CatalogReason;
        let dirty = path.with_extension("dirty");
        let (read, reason) = match fs::exists(&dirty) {
            Ok(false) => match read(&path) {
                Ok(state) => (Some(state), None),
                Err(e) => (
                    None,
                    Some(if e.kind() == io::ErrorKind::NotFound {
                        CatalogReason::Missing
                    } else {
                        CatalogReason::Unreadable
                    }),
                ),
            },
            _ => (None, Some(CatalogReason::Unclean)),
        };
        // No log may change while a clean catalog can survive a crash.
        let pending = dirty.with_extension("dirty.new");
        File::create(&pending)?.sync_all()?;
        sys::fs::publish(&pending, &dirty)?;
        let rebuilt = read.is_none();
        let catalog = Self::with(Some(path), read.map_or_else(|| rebuild(store), Ok)?);
        if let Some(reason) = reason {
            ext_logging::emit(ext_logging::Fact::CatalogRebuilt { reason });
        }
        store.locate(
            &catalog
                .records()
                .into_iter()
                .map(|r| r.header)
                .collect::<Vec<_>>(),
        )?;
        if rebuilt {
            catalog
                .writer
                .persist(&lock(&catalog.writer.gate).records, true)?;
        }
        Ok(catalog)
    }

    /// Called only once all session writers have stopped. Rewriting avoids
    /// trusting an earlier best-effort append that may have failed.
    pub fn clean(&self) -> io::Result<()> {
        self.writer.closed.store(true, Ordering::Release);
        let mut image = lock(&self.writer.gate);
        let Some(path) = &self.writer.path else {
            return Ok(());
        };
        if self.uncertain.load(Ordering::Acquire) {
            return Err(io::Error::other(
                "an interrupted import requires catalog rebuild",
            ));
        }
        let pending = std::mem::take(&mut lock(&self.state).pending);
        image.records.extend(pending);
        self.writer.persist(&image.records, true)?;
        sys::fs::remove_durable(&path.with_extension("dirty"))
    }

    fn with(path: Option<PathBuf>, image: Image) -> Self {
        let state = State {
            records: image.records.clone(),
            ..State::default()
        };
        Self {
            writer: Arc::new(CatalogWriter {
                path,
                gate: Mutex::new(image),
                #[cfg(test)]
                compacting: Mutex::default(),
                runtime: OnceLock::new(),
                scheduled: AtomicBool::new(false),
                closed: AtomicBool::new(false),
            }),
            state: Arc::new(Mutex::new(state)),
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

    pub fn start(&self, runtime: Arc<dyn rt::Runtime>) {
        let _ = self.writer.runtime.set(runtime);
    }

    /// Committers publish only memory; the writer coalesces revisions.
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
        state.pending.insert(id, record.clone());
        state.records.insert(id, record);
        drop(state);
        let Some(runtime) = self.writer.runtime.get() else {
            return;
        };
        if self.writer.scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let writer = Arc::clone(&self.writer);
        let state = Arc::clone(&self.state);
        let rt = Arc::clone(runtime);
        runtime.spawn(Box::pin(async move {
            rt::unblock(&*rt, move || {
                let mut image = lock(&writer.gate);
                loop {
                    if writer.closed.load(Ordering::Acquire) {
                        break;
                    }
                    let pending = {
                        let mut state = lock(&state);
                        if state.pending.is_empty() {
                            writer.scheduled.store(false, Ordering::Release);
                            break;
                        }
                        std::mem::take(&mut state.pending)
                    };
                    image.lines += pending.len();
                    image
                        .records
                        .extend(pending.iter().map(|(&id, record)| (id, record.clone())));
                    let compact = image.lines > 2 * image.records.len() + 64;
                    let records = if compact {
                        image.lines = image.records.len();
                        &image.records
                    } else {
                        &pending
                    };
                    if let Err(e) = writer.persist(records, compact) {
                        catalog_fault(&e);
                    }
                }
            })
            .await;
        }));
    }
}

fn line(record: &Record) -> io::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    Ok(line)
}

/// The catalog's lines folded, the last of each session's winning.
fn read(path: &Path) -> io::Result<Image> {
    let mut image = Image::default();
    for line in BufReader::new(File::open(path)?).split(b'\n') {
        let record: Record = serde_json::from_slice(&line?)?;
        image.records.insert(record.header.id.0.as_u128(), record);
        image.lines += 1;
    }
    Ok(image)
}

/// Every log read through: a session is busy if it was left with
/// anything open or waiting.
fn rebuild(store: &dyn LogStore) -> io::Result<Image> {
    let headers = store.headers()?;
    let workers = headers.len().min(4);
    let headers = Mutex::new(headers.into_iter());
    let records = Mutex::new(BTreeMap::new());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let Some(header) = lock(&headers).next() else {
                        break;
                    };
                    let mut ledger = Ledger::default();
                    let loaded =
                        match store.load(header.id, &mut |_| false, &mut |e| ledger.apply(&e)) {
                            Ok(loaded) => loaded,
                            Err(e) => {
                                ext_logging::emit(ext_logging::Fact::Fault {
                                    session: Some(header.id),
                                    operation: ext_logging::Operation::Reload,
                                    kind: ext_logging::Fault(e.kind()),
                                });
                                continue;
                            }
                        };
                    let activity = ledger.activity();
                    let read_only = loaded.writer.read_only();
                    lock(&records).insert(
                        header.id.0.as_u128(),
                        Record {
                            busy: activity != Activity::Idle && read_only.is_none(),
                            entries: loaded.seq,
                            activity,
                            read_only,
                            header,
                        },
                    );
                }
            });
        }
    });
    let records = records
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok(Image {
        lines: records.len(),
        records,
    })
}

impl CatalogWriter {
    fn persist(&self, records: &BTreeMap<u128, Record>, compact: bool) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let new = path.with_extension("new");
        let mut file = if compact {
            File::create(&new)?
        } else {
            OpenOptions::new().append(true).open(path)?
        };
        for record in records.values() {
            #[cfg(test)]
            if compact && let Some(compacting) = &mut *lock(&self.compacting) {
                compacting();
            }
            file.write_all(&line(record)?)?;
        }
        if compact {
            file.sync_all()?;
            sys::fs::publish(&new, path)?;
        }
        Ok(())
    }
}

fn catalog_fault(error: &io::Error) {
    ext_logging::emit(ext_logging::Fact::Fault {
        session: None,
        operation: ext_logging::Operation::Catalog,
        kind: ext_logging::Fault(error.kind()),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Memory;

    #[test]
    fn coalescing_persists_the_last_revision_before_clean() {
        let dir = std::env::temp_dir().join(format!("a-review-coalesce-{}", SessionId::new()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("catalog");
        let catalog = Catalog::open(path.clone(), &Memory::default()).unwrap();
        let runtime = Arc::new(rt::Threads::new(2));
        catalog.start(runtime.clone());
        let header = Header {
            id: SessionId::new(),
            workspace: "/w".into(),
            parent: None,
        };
        lock(&catalog.writer.gate).lines = 100;
        for wave in 0..4 {
            let writing = lock(&catalog.writer.gate);
            for revision in 1..=128 {
                let mut record = Record::new(header.clone());
                record.entries = wave * 128 + revision;
                catalog.record(record);
            }
            drop(writing);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while catalog.writer.scheduled.load(Ordering::Acquire) {
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
            assert_eq!(
                read(&path).unwrap().records[&header.id.0.as_u128()].entries,
                (wave + 1) * 128
            );
            assert!(path.with_extension("dirty").exists());
        }
        catalog.clean().unwrap();
        runtime.shutdown();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn parallel_rebuild_matches_serial_replay() {
        let store = Memory::default();
        for n in 0..17 {
            let header = Header {
                id: SessionId::new(),
                workspace: format!("/w/{n}").into(),
                parent: None,
            };
            let mut loaded = store.create(&header).unwrap();
            for _ in 0..n {
                loaded
                    .append(protocol::Body::Watch {
                        child: SessionId::new(),
                    })
                    .unwrap();
            }
        }
        let serial: BTreeMap<_, _> = store
            .headers()
            .unwrap()
            .into_iter()
            .map(|header| {
                let mut ledger = Ledger::default();
                let loaded = store
                    .load(header.id, &mut |_| false, &mut |e| ledger.apply(&e))
                    .unwrap();
                let activity = ledger.activity();
                let read_only = loaded.writer.read_only();
                (
                    header.id.0.as_u128(),
                    Record {
                        busy: activity != Activity::Idle && read_only.is_none(),
                        entries: loaded.seq,
                        activity,
                        read_only,
                        header,
                    },
                )
            })
            .collect();
        assert_eq!(rebuild(&store).unwrap().records, serial);
    }

    #[test]
    fn catalog_cut_recovers_only_durable_log_revisions() {
        use crate::store::Faulty;
        for cut_at in 0..5 {
            let dir = std::env::temp_dir().join(format!("a-review-cut-{}", SessionId::new()));
            fs::create_dir(&dir).unwrap();
            let inner = Arc::new(Memory::default());
            let store = Faulty::new(inner.clone());
            let path = dir.join("catalog");
            let catalog = Catalog::open(path.clone(), &store).unwrap();
            let header = Header {
                id: SessionId::new(),
                workspace: "/w".into(),
                parent: None,
            };
            let mut loaded = store.create(&header).unwrap();
            let mut record = Record::new(header.clone());
            catalog.record(record.clone());
            if cut_at >= 1 {
                record.busy = true;
                catalog.record(record.clone());
            }
            if cut_at >= 2 {
                let mut entries = [protocol::Entry::new(
                    protocol::Seq::new(1).unwrap(),
                    protocol::Epoch(0),
                    protocol::Body::Assistant {
                        text: "done".into(),
                    },
                )];
                loaded.writer.append(&mut entries, true).unwrap();
            }
            if cut_at >= 3 {
                loaded.writer.flush_inline().unwrap();
                record.entries = 1;
                record.busy = false;
                catalog.record(record);
            }
            if cut_at == 4 {
                catalog.clean().unwrap();
            }
            store.cut();
            drop((loaded, catalog));
            let restarted = Catalog::open(path, &*inner).unwrap();
            let recovered = restarted.get(header.id).unwrap();
            assert_eq!(recovered.entries, u64::from(cut_at >= 3), "cut {cut_at}");
            assert!(!recovered.busy, "cut {cut_at}");
            drop(restarted);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn large_catalog_compaction_allows_a_commit_for_every_record() {
        use std::sync::atomic::AtomicUsize;
        const SESSIONS: usize = 10_000;
        let dir = std::env::temp_dir().join(format!("a-catalog-large-{}", SessionId::new()));
        fs::create_dir(&dir).unwrap();
        let store = Memory::default();
        let catalog = Arc::new(Catalog::open(dir.join("catalog"), &store).unwrap());
        for _ in 0..SESSIONS {
            catalog.record(Record::new(Header {
                id: SessionId::new(),
                workspace: "/w".into(),
                parent: None,
            }));
        }
        let commits = Arc::new(AtomicUsize::new(0));
        let progress = Arc::clone(&commits);
        let committer = Arc::downgrade(&catalog);
        let header = Header {
            id: SessionId::new(),
            workspace: "/committer".into(),
            parent: None,
        };
        let mut loaded = store.create(&header).unwrap();
        *lock(&catalog.writer.compacting) = Some(Box::new(move || {
            let catalog = committer.upgrade().unwrap();
            assert!(catalog.state.try_lock().is_ok());
            let mut record = Record::new(header.clone());
            record.entries = progress.load(Ordering::Relaxed) as u64;
            catalog.record(record);
            loaded
                .append(protocol::Body::Assistant {
                    text: "committed".into(),
                })
                .unwrap();
            progress.fetch_add(1, Ordering::Relaxed);
        }));
        let mut image = lock(&catalog.writer.gate);
        let pending = std::mem::take(&mut lock(&catalog.state).pending);
        image.records.extend(pending);
        catalog.writer.persist(&image.records, true).unwrap();
        assert_eq!(commits.load(Ordering::Relaxed), SESSIONS);
        assert_eq!(lock(&catalog.state).pending.len(), 1);
        drop(image);
        *lock(&catalog.writer.compacting) = None;
        catalog.clean().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn catalog_compaction_does_not_hold_session_commits() {
        let dir = std::env::temp_dir().join(format!("a-catalog-{}", SessionId::new()));
        fs::create_dir(&dir).unwrap();
        let store = Memory::default();
        let catalog = Catalog::open(dir.join("catalog"), &store).unwrap();
        let runtime = Arc::new(rt::Threads::new(2));
        catalog.start(runtime.clone());
        let header = Header {
            id: SessionId::new(),
            workspace: "/w".into(),
            parent: None,
        };
        let mut loaded = store.create(&header).unwrap();
        lock(&catalog.writer.gate).lines = 100;
        let writing = lock(&catalog.writer.gate);
        std::thread::scope(|scope| {
            let (sent, done) = std::sync::mpsc::channel();
            let catalog = &catalog;
            scope.spawn(move || {
                for entries in 0..128 {
                    let mut record = Record::new(header.clone());
                    record.entries = entries;
                    catalog.record(record);
                    loaded
                        .append(protocol::Body::Assistant {
                            text: "committed".into(),
                        })
                        .unwrap();
                }
                sent.send(()).unwrap();
            });
            let committed = done.recv_timeout(std::time::Duration::from_secs(5));
            drop(writing);
            committed.unwrap();
        });
        catalog.clean().unwrap();
        runtime.shutdown();
        assert_eq!(
            read(&dir.join("catalog"))
                .unwrap()
                .records
                .values()
                .next()
                .unwrap()
                .entries,
            127
        );
        assert!(!dir.join("catalog.dirty").exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
