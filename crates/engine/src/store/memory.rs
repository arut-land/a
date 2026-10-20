use std::collections::HashMap;
use std::io;
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use protocol::{Decode, Entry, SessionId};

use super::{
    Encoded, Header, LeaseAuthority, Lines, Loaded, Local, Log, LogStore, Writer, not_found, span,
};

/// Sessions held in this process, for tests: durable until it exits. Each
/// keeps its entries as the lines a file would, so it is read back as one.
#[derive(Default)]
pub struct Memory {
    sessions: Mutex<HashMap<SessionId, Stored>>,

    leases: Local,
}

struct Stored {
    header: Header,
    lines: Arc<Shared>,
}

/// One session's lines, newline excluded.
#[derive(Default)]
struct Shared(Mutex<Vec<Box<[u8]>>>);

impl Memory {
    fn shared(&self, id: SessionId) -> io::Result<Arc<Shared>> {
        let sessions = lock(&self.sessions);
        let stored = sessions.get(&id).ok_or_else(|| not_found(id))?;
        Ok(Arc::clone(&stored.lines))
    }

    fn loaded(&self, id: SessionId, lines: Arc<Shared>, floor: u32) -> Loaded {
        let epoch = self.leases.acquire(id, floor);
        let seq = lock(&lines.0).len() as u64;
        Loaded {
            seq,
            writer: Writer::new(Box::new(MemoryLog(Arc::clone(&lines))), epoch),
            lines,
        }
    }
}

impl LogStore for Memory {
    fn create(&self, header: &Header) -> io::Result<Loaded> {
        let mut sessions = lock(&self.sessions);
        if sessions.contains_key(&header.id) {
            let taken = format!("session {} exists", header.id);
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, taken));
        }
        let lines = Arc::<Shared>::default();
        let stored = Stored {
            header: header.clone(),
            lines: Arc::clone(&lines),
        };
        sessions.insert(header.id, stored);
        Ok(self.loaded(header.id, lines, 0))
    }

    fn headers(&self) -> io::Result<Vec<Header>> {
        let mut headers: Vec<Header> = lock(&self.sessions)
            .values()
            .map(|s| s.header.clone())
            .collect();
        headers.sort_unstable_by_key(|h| h.id.0);
        Ok(headers)
    }

    fn load(&self, id: SessionId, each: &mut dyn FnMut(Entry)) -> io::Result<Loaded> {
        let lines = self.shared(id)?;
        let mut floor = 0;
        for entry in lines.entries()? {
            floor = entry.epoch;
            each(entry);
        }
        Ok(self.loaded(id, lines, floor))
    }

    fn entries(&self, id: SessionId) -> io::Result<Vec<Entry>> {
        self.shared(id)?.entries()
    }

    #[cfg(test)]
    fn count(&self, id: SessionId) -> io::Result<u64> {
        Ok(lock(&self.shared(id)?.0).len() as u64)
    }
}

impl Shared {
    /// The entries, decoded from a copy of the lines taken at once.
    fn entries(&self) -> io::Result<Vec<Entry>> {
        let lines = lock(&self.0).clone();
        lines
            .iter()
            .map(|line| Entry::from_json(line).map_err(io::Error::from))
            .collect()
    }
}

impl Lines for Shared {
    fn read(&self, seqs: RangeInclusive<u64>, limit: usize, out: &mut Vec<u8>) -> io::Result<u64> {
        let lines = lock(&self.0);
        let (first, last) = span(&seqs, lines.len())?;
        let mut count = 0;
        let mut room = limit;
        for line in &lines[first..=last] {
            if count > 0 && line.len() + 1 > room {
                break;
            }
            room = room.saturating_sub(line.len() + 1);
            out.extend_from_slice(line);
            out.push(b'\n');
            count += 1;
        }
        Ok(count)
    }
}

struct MemoryLog(Arc<Shared>);

impl Log for MemoryLog {
    fn append_all(&mut self, _: &[Entry], lines: &Encoded) -> io::Result<()> {
        lock(&self.0.0).extend(lines.lines().map(Box::from));
        Ok(())
    }
}

/// Every critical section here is a single push, insert or clone, so a
/// panicking holder cannot leave the data half-changed.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
