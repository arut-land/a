//! A store that loses what a power cut would: lines written and not yet
//! synced. For holding the daemon to what it shows before it is durable.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use protocol::{Entry, SessionId};

use super::{Encoded, Header, Loaded, Log, LogStore, Snapshot};
use crate::lock;

/// `inner`, whose logs keep what is only written apart from what is
/// synced, until [`Faulty::cut`] loses the former and stops the latter.
pub struct Faulty {
    inner: Arc<dyn LogStore>,
    state: Arc<State>,
}

/// A batch written, with its lines.
type Written = (Vec<Entry>, Encoded);

#[derive(Default)]
struct State {
    /// Each session's batches written and not yet synced.
    unsynced: Mutex<HashMap<SessionId, Vec<Written>>>,
    /// The power is off: nothing more is written or synced.
    cut: AtomicBool,
}

impl Faulty {
    pub fn new(inner: Arc<dyn LogStore>) -> Self {
        Self {
            inner,
            state: Arc::default(),
        }
    }

    /// The power cut: what was only written is gone, and every log fails
    /// from now on. What `inner` holds is what the next boot reads.
    pub fn cut(&self) {
        self.state.cut.store(true, Ordering::SeqCst);
        lock(&self.state.unsynced).clear();
    }

    fn wrap(&self, id: SessionId, mut loaded: Loaded) -> Loaded {
        let inner = std::mem::replace(&mut loaded.writer.log, Box::new(Off));
        loaded.writer.log = Box::new(FaultyLog {
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
    id: SessionId,
    inner: Box<dyn Log>,
    state: Arc<State>,
}

impl FaultyLog {
    fn on(&self) -> io::Result<()> {
        if self.state.cut.load(Ordering::SeqCst) {
            return Err(io::Error::other("the power is off"));
        }
        Ok(())
    }

    /// Hands `inner` what was only written, in order.
    fn flush(&mut self) -> io::Result<()> {
        let pending = lock(&self.state.unsynced)
            .remove(&self.id)
            .unwrap_or_default();
        for (entries, lines) in &pending {
            self.inner.append_all(entries, lines)?;
        }
        Ok(())
    }
}

impl Log for FaultyLog {
    fn append_all(&mut self, entries: &[Entry], lines: &Encoded) -> io::Result<()> {
        self.on()?;
        self.flush()?;
        self.inner.append_all(entries, lines)
    }

    /// Kept aside: `inner` never sees it unless a sync follows.
    fn write_all(&mut self, entries: &[Entry], lines: &Encoded) -> io::Result<()> {
        self.on()?;
        let copy = Encoded::of(entries)?;
        (lock(&self.state.unsynced).entry(self.id).or_default()).push((entries.to_vec(), copy));
        let _ = lines;
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.on()?;
        self.flush()
    }

    /// Kept at once: it covers only what is durable.
    fn snapshot(&mut self, snapshot: &Snapshot) -> io::Result<()> {
        self.on()?;
        self.inner.snapshot(snapshot)
    }
}

/// Stands in for a log taken out of its writer.
struct Off;

impl Log for Off {
    fn append_all(&mut self, _: &[Entry], _: &Encoded) -> io::Result<()> {
        Err(io::Error::other("no log"))
    }
}
