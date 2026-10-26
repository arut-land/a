//! A store that loses what a power cut would: lines written and not yet
//! synced. For holding the daemon to what it shows before it is durable.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use protocol::{Entry, SessionId};

use super::{Encoded, Header, ImportStep, Loaded, Log, LogStore, Snapshot};
use crate::lock;

/// `inner`, whose logs keep what is only written apart from what is
/// synced, until [`Faulty::cut`] loses the former and stops the latter.
pub struct Faulty {
    inner: Arc<dyn LogStore>,
    state: Arc<State>,
}

/// A batch written, with its lines.
type Written = (Vec<Entry>, Encoded);
type SharedLog = Arc<Mutex<Box<dyn Log>>>;

#[derive(Default)]
struct State {
    /// Each session's batches written and not yet synced.
    unsynced: Mutex<HashMap<SessionId, Vec<Written>>>,
    /// The power is off: nothing more is written or synced.
    cut: AtomicBool,
    generation: AtomicU64,
    fail_sync: Mutex<Option<SyncFailure>>,
    fail_write: AtomicBool,
    fail_import: Mutex<Option<ImportStep>>,
    cut_flush: AtomicBool,
    flushes: AtomicUsize,
    flusher: rt::Flusher,
    logs: Mutex<HashMap<SessionId, SharedLog>>,
    holds: Mutex<VecDeque<Arc<Hold>>>,
}

#[derive(Clone, Copy)]
pub enum SyncFailure {
    Discard,
    Retain,
}

#[derive(Default)]
struct Hold {
    target: Option<HoldTarget>,
    started: ext::Flag,
    released: Mutex<bool>,
    ready: Condvar,
}

struct HoldTarget {
    session: SessionId,
    matches: fn(&Entry) -> bool,
}

pub struct HeldFlush(Arc<Hold>);

impl HeldFlush {
    pub fn started(&self) {
        rt::block_on(self.0.started.wait());
    }
    pub fn release(&self) {
        *lock(&self.0.released) = true;
        self.0.ready.notify_all();
    }
}

impl Drop for HeldFlush {
    fn drop(&mut self) {
        self.release();
    }
}

impl Faulty {
    pub fn new(inner: Arc<dyn LogStore>) -> Self {
        Self {
            inner,
            state: Arc::default(),
        }
    }

    pub fn hold_next_flush(&self) -> HeldFlush {
        let hold = Arc::new(Hold::default());
        lock(&self.state.holds).push_back(Arc::clone(&hold));
        HeldFlush(hold)
    }

    /// Holds the epoch that captures a matching append, leaving earlier epochs free.
    pub fn hold_flush_containing(
        &self,
        session: SessionId,
        matches: fn(&Entry) -> bool,
    ) -> HeldFlush {
        let hold = Arc::new(Hold {
            target: Some(HoldTarget { session, matches }),
            ..Hold::default()
        });
        lock(&self.state.holds).push_back(Arc::clone(&hold));
        HeldFlush(hold)
    }

    pub fn pending(&self, id: SessionId) -> Vec<Entry> {
        lock(&self.state.unsynced)
            .get(&id)
            .into_iter()
            .flatten()
            .flat_map(|(entries, _)| entries.iter().cloned())
            .collect()
    }

    pub fn flushes(&self) -> usize {
        self.state.flushes.load(Ordering::SeqCst)
    }

    /// The power cut: what was only written is gone, and every log fails
    /// from now on. What `inner` holds is what the next boot reads.
    pub fn cut(&self) {
        self.state.cut.store(true, Ordering::SeqCst);
        lock(&self.state.unsynced).clear();
    }

    /// Cuts power after the next barrier write, before its cache flush.
    pub fn cut_before_next_flush(&self) {
        self.state.cut_flush.store(true, Ordering::SeqCst);
    }

    /// Fails the next sync after losing or retaining its readable bytes.
    pub fn fail_next_sync(&self, failure: SyncFailure) {
        *lock(&self.state.fail_sync) = Some(failure);
    }

    /// Rejects the next deferred write before storing any of its bytes.
    pub fn fail_next_write(&self) {
        self.state.fail_write.store(true, Ordering::SeqCst);
    }

    pub fn fail_import_at(&self, step: ImportStep) {
        *lock(&self.state.fail_import) = Some(step);
    }

    fn wrap(&self, id: SessionId, mut loaded: Loaded) -> Loaded {
        let inner = std::mem::replace(&mut loaded.writer.log, Box::new(Off));
        let inner = Arc::new(Mutex::new(inner));
        lock(&self.state.logs).insert(id, Arc::clone(&inner));
        loaded.writer.log = Box::new(FaultyLog {
            generation: self.state.generation.load(Ordering::SeqCst),
            id,
            inner,
            state: Arc::clone(&self.state),
        });
        loaded
    }
}

impl LogStore for Faulty {
    fn create(&self, header: &Header) -> io::Result<Loaded> {
        let loaded = self.inner.create(header)?;
        Ok(self.wrap(header.id, loaded))
    }

    fn prefix(&self, id: SessionId, seq: u64) -> io::Result<Vec<Entry>> {
        self.inner.prefix(id, seq)
    }

    fn suffix(
        &self,
        id: SessionId,
        cursor: &mut ext::LogCursor,
        through: u64,
    ) -> io::Result<(bool, Vec<Entry>)> {
        self.inner.suffix(id, cursor, through)
    }

    fn import(&self, header: &Header, entries: &[Entry]) -> io::Result<()> {
        self.inner.import_checked(header, entries, &mut |step| {
            let mut failure = lock(&self.state.fail_import);
            if *failure == Some(step) {
                failure.take();
                return Err(io::Error::other(format!("import failed at {step:?}")));
            }
            Ok(())
        })
    }

    fn headers(&self) -> io::Result<Vec<Header>> {
        self.inner.headers()
    }

    fn load(
        &self,
        id: SessionId,
        restore: &mut dyn FnMut(Snapshot) -> bool,
        each: &mut dyn FnMut(Entry),
    ) -> io::Result<Loaded> {
        let loaded = self.inner.load(id, restore, each)?;
        Ok(self.wrap(id, loaded))
    }

    fn entries(&self, id: SessionId) -> io::Result<Vec<Entry>> {
        self.inner.entries(id)
    }

    fn count(&self, id: SessionId) -> io::Result<u64> {
        self.inner.count(id)
    }
}

struct FaultyLog {
    generation: u64,
    id: SessionId,
    inner: SharedLog,
    state: Arc<State>,
}

impl FaultyLog {
    fn on(&self) -> io::Result<()> {
        if self.state.cut.load(Ordering::SeqCst)
            || self.generation != self.state.generation.load(Ordering::SeqCst)
        {
            return Err(io::Error::other("the power is off"));
        }
        Ok(())
    }
}

impl Log for FaultyLog {
    fn append_all(&mut self, entries: &[Entry], lines: &Encoded) -> io::Result<()> {
        self.on()?;
        self.write_all(entries, lines)
    }

    /// Kept aside: `inner` never sees it unless a sync follows.
    fn write_all(&mut self, entries: &[Entry], lines: &Encoded) -> io::Result<()> {
        self.on()?;
        if self.state.fail_write.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("deferred write failed"));
        }
        let copy = Encoded::of(entries)?;
        (lock(&self.state.unsynced).entry(self.id).or_default()).push((entries.to_vec(), copy));
        let _ = lines;
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.on()
    }

    fn durable_on_order(&self) -> bool {
        false
    }

    fn enroll(&mut self) -> Option<rt::FlushRequest> {
        {
            let state = Arc::clone(&self.state);
            let generation = self.generation;
            Some(self.state.flusher.enroll_prepared(move || {
                let pending = std::mem::take(&mut *lock(&state.unsynced));
                let mut holds = lock(&state.holds);
                let hold = holds
                    .iter()
                    .position(|hold| {
                        hold.target.as_ref().is_none_or(|target| {
                            pending.get(&target.session).is_some_and(|batches| {
                                batches
                                    .iter()
                                    .any(|(entries, _)| entries.iter().any(target.matches))
                            })
                        })
                    })
                    .and_then(|index| holds.remove(index));
                drop(holds);
                Box::new(move || {
                    if generation != state.generation.load(Ordering::SeqCst) {
                        return Err(io::Error::other("cache epoch invalidated"));
                    }
                    persist(&state, pending, hold)
                })
            }))
        }
    }

    /// Kept at once: it covers only what is durable.
    fn snapshot(&mut self, snapshot: &Snapshot) -> io::Result<()> {
        self.on()?;
        lock(&self.inner).snapshot(snapshot)
    }
}

fn persist(
    state: &State,
    pending: HashMap<SessionId, Vec<Written>>,
    hold: Option<Arc<Hold>>,
) -> io::Result<()> {
    state.flushes.fetch_add(1, Ordering::SeqCst);
    // The snapshot belongs to the epoch that starts now, not to its
    // completion: later barrier writes must survive a different flush.
    if let Some(hold) = hold {
        hold.started.set();
        let mut released = lock(&hold.released);
        while !*released {
            released = hold
                .ready
                .wait(released)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
    if state.cut_flush.swap(false, Ordering::SeqCst) {
        state.cut.store(true, Ordering::SeqCst);
        lock(&state.unsynced).clear();
    }
    if state.cut.load(Ordering::SeqCst) {
        return Err(io::Error::other("the power is off"));
    }
    let failure = lock(&state.fail_sync).take();
    if matches!(failure, Some(SyncFailure::Discard)) {
        state.generation.fetch_add(1, Ordering::SeqCst);
        lock(&state.unsynced).clear();
        return Err(io::Error::other("deferred sync failed"));
    }
    for (id, batches) in pending {
        let log = Arc::clone(lock(&state.logs).get(&id).expect("loaded log"));
        let mut log = lock(&log);
        for (entries, lines) in batches {
            if failure.is_some() {
                log.write_all(&entries, &lines)?;
            } else {
                log.append_all(&entries, &lines)?;
            }
        }
        if failure.is_none() {
            log.sync()?;
            if let Some(request) = log.enroll() {
                if let Some(leader) = request.leader {
                    leader.run();
                }
                rt::block_on(request.epoch)?;
            }
        }
    }
    if failure.is_some() {
        return Err(io::Error::other("deferred sync failed"));
    }
    Ok(())
}

/// Stands in for a log taken out of its writer.
struct Off;

impl Log for Off {
    fn append_all(&mut self, _: &[Entry], _: &Encoded) -> io::Result<()> {
        Err(io::Error::other("no log"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{AppendMode, Jsonl};
    use protocol::{Body, Epoch, Mode, Seq};

    #[test]
    fn a_targeted_hold_covers_its_append_without_blocking_earlier_epochs() {
        let dir = tempfile::tempdir().unwrap();
        let backing = Arc::new(Jsonl::open(dir.path().join("logs")).unwrap());
        let faulty = Faulty::new(Arc::clone(&backing) as _);
        let runtime = rt::Threads::new(1);
        let header = Header {
            id: SessionId::new(),
            workspace: dir.path().to_path_buf(),
            parent: None,
        };
        let mut target = faulty.create(&header).unwrap().writer;
        let other = Header {
            id: SessionId::new(),
            ..header.clone()
        };
        let mut unrelated = faulty.create(&other).unwrap().writer;
        let hold = faulty.hold_flush_containing(header.id, |entry| entry.seq.get() == 2);
        for seq in 1..=2 {
            unrelated
                .append(&mut [entry(seq)], AppendMode::Order)
                .unwrap();
        }
        unrelated.flush_inline().unwrap();
        target.append(&mut [entry(1)], AppendMode::Order).unwrap();
        target.flush_inline().unwrap();
        assert!(!hold.0.started.is_set());
        assert_eq!(backing.entries(other.id).unwrap().len(), 2);
        assert_eq!(backing.entries(header.id).unwrap().len(), 1);

        target.append(&mut [entry(2)], AppendMode::Order).unwrap();
        let epoch = target.enroll().unwrap().dispatch(&runtime);
        hold.started();
        assert_eq!(backing.entries(header.id).unwrap().len(), 1);
        assert_eq!(faulty.pending(header.id), []);
        target.append(&mut [entry(3)], AppendMode::Order).unwrap();
        hold.release();
        rt::block_on(epoch).unwrap();
        assert_eq!(backing.entries(header.id).unwrap().len(), 2);
        assert_eq!(faulty.pending(header.id)[0].seq.get(), 3);
        target.flush_inline().unwrap();
        assert_eq!(backing.entries(header.id).unwrap().len(), 3);
    }

    fn entry(seq: u64) -> Entry {
        Entry::new(
            Seq::new(seq).unwrap(),
            Epoch(0),
            Body::Input {
                segments: format!("m{seq}").into(),
                mode: Mode::Queue,
                taken: true,
                origin: None,
                force: false,
            },
        )
    }
}
