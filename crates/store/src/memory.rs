use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use protocol::{Entry, SessionId};

use crate::{Header, Lease, LeaseAuthority, Loaded, Local, Log, LogStore, floor, not_found};

/// Sessions held in this process, for tests: durable until it exits.
#[derive(Default)]
pub struct Memory {
    sessions: Mutex<HashMap<SessionId, Stored>>,
    busy: Mutex<HashSet<SessionId>>,
    leases: Local,
}

struct Stored {
    header: Header,
    entries: Arc<Mutex<Vec<Entry>>>,
}

impl Memory {
    fn shared(&self, id: SessionId) -> io::Result<Arc<Mutex<Vec<Entry>>>> {
        let sessions = lock(&self.sessions);
        let stored = sessions.get(&id).ok_or_else(|| not_found(id))?;
        Ok(Arc::clone(&stored.entries))
    }
}

impl LogStore for Memory {
    fn create(&self, header: &Header) -> io::Result<Loaded> {
        let mut sessions = lock(&self.sessions);
        if sessions.contains_key(&header.id) {
            let taken = format!("session {} exists", header.id);
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, taken));
        }
        let entries = Arc::default();
        let lease = self.leases.acquire(header.id, 0);
        let epoch = lease.epoch;
        let log = Box::new(MemoryLog(Arc::clone(&entries), lease));
        let header = header.clone();
        sessions.insert(header.id, Stored { header, entries });
        Ok(Loaded {
            entries: Vec::new(),
            log,
            epoch,
        })
    }

    fn headers(&self) -> io::Result<Vec<Header>> {
        let mut headers: Vec<Header> = lock(&self.sessions)
            .values()
            .map(|s| s.header.clone())
            .collect();
        headers.sort_by_key(|h| h.id.0);
        Ok(headers)
    }

    fn load(&self, id: SessionId) -> io::Result<Loaded> {
        let shared = self.shared(id)?;
        let entries = lock(&shared).clone();
        let lease = self.leases.acquire(id, floor(&entries));
        Ok(Loaded {
            entries,
            epoch: lease.epoch,
            log: Box::new(MemoryLog(shared, lease)),
        })
    }

    fn entries(&self, id: SessionId) -> io::Result<Vec<Entry>> {
        Ok(lock(&*self.shared(id)?).clone())
    }

    fn count(&self, id: SessionId) -> io::Result<u64> {
        let entries = self.shared(id)?;
        Ok(lock(&entries).len() as u64)
    }

    fn mark_busy(&self, id: SessionId, busy: bool) -> io::Result<()> {
        let mut marked = lock(&self.busy);
        if busy {
            marked.insert(id);
        } else {
            marked.remove(&id);
        }
        Ok(())
    }

    fn is_busy(&self, id: SessionId) -> bool {
        lock(&self.busy).contains(&id)
    }
}

struct MemoryLog(Arc<Mutex<Vec<Entry>>>, Lease);

impl Log for MemoryLog {
    fn append(&mut self, entry: &Entry) -> io::Result<()> {
        self.append_all(std::slice::from_ref(entry))
    }

    fn append_all(&mut self, entries: &[Entry]) -> io::Result<()> {
        self.1.admit(entries)?;
        lock(&self.0).extend_from_slice(entries);
        Ok(())
    }
}

/// Every critical section here is a single push, insert or clone, so a
/// panicking holder cannot leave the data half-changed.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
