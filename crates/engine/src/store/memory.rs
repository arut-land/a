use std::collections::HashMap;
use std::io;
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use protocol::{Decode, Entry, SessionId};

use super::{
    Encoded, Header, LeaseAuthority, Lines, Loaded, Local, Log, LogStore, Snapshot, Writer,
    not_found, span,
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

/// One session's lines, newline excluded, and its newest snapshot.
#[derive(Default)]
struct Shared {
    lines: Mutex<Vec<Box<[u8]>>>,
    snapshot: Mutex<Option<Snapshot>>,
}

impl Memory {
    fn shared(&self, id: SessionId) -> io::Result<Arc<Shared>> {
        let sessions = lock(&self.sessions);
        let stored = sessions.get(&id).ok_or_else(|| not_found(id))?;
        Ok(Arc::clone(&stored.lines))
    }

    fn loaded(&self, id: SessionId, lines: Arc<Shared>, floor: u32, read: u64) -> Loaded {
        let epoch = self.leases.acquire(id, floor);
        let seq = lock(&lines.lines).len() as u64;
        Loaded {
            seq,
            read,
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
        Ok(self.loaded(header.id, lines, 0, 0))
    }

    fn prefix(&self, id: SessionId, seq: u64) -> io::Result<Vec<Entry>> {
        let mut entries = self.shared(id)?.entries()?;
        if (entries.len() as u64) < seq {
            return Err(io::ErrorKind::NotFound.into());
        }
        entries.truncate(usize::try_from(seq).map_err(io::Error::other)?);
        Ok(entries)
    }

    fn import(&self, header: &Header, entries: &[Entry]) -> io::Result<()> {
        if entries
            .iter()
            .enumerate()
            .any(|(index, entry)| entry.seq.get() != index as u64 + 1)
        {
            return Err(io::Error::other("import has a sequence gap"));
        }
        let encoded = Encoded::of(entries)?;
        let lines = Arc::new(Shared {
            lines: Mutex::new(encoded.lines().map(Into::into).collect()),
            snapshot: Mutex::default(),
        });
        let mut sessions = lock(&self.sessions);
        if sessions.contains_key(&header.id) {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        sessions.insert(
            header.id,
            Stored {
                header: header.clone(),
                lines,
            },
        );
        Ok(())
    }

    fn headers(&self) -> io::Result<Vec<Header>> {
        let mut headers: Vec<Header> = lock(&self.sessions)
            .values()
            .map(|s| s.header.clone())
            .collect();
        headers.sort_unstable_by_key(|h| h.id.0);
        Ok(headers)
    }

    fn load(
        &self,
        id: SessionId,
        restore: &mut dyn FnMut(Snapshot) -> bool,
        each: &mut dyn FnMut(Entry),
    ) -> io::Result<Loaded> {
        let lines = self.shared(id)?;
        let entries = lines.entries()?;
        let snapshot = lock(&lines.snapshot).clone();
        let covered = match snapshot {
            Some(snapshot) if snapshot.covers <= entries.len() as u64 => {
                let covers = snapshot.covers;
                restore(snapshot).then_some(covers)
            }
            _ => None,
        };
        let floor = entries.last().map_or(0, |e| e.epoch.0);
        let skip = usize::try_from(covered.unwrap_or(0)).unwrap_or(usize::MAX);
        let read = (lock(&lines.lines).iter().skip(skip))
            .map(|line| line.len() as u64 + 1)
            .sum();
        for entry in entries.into_iter().skip(skip) {
            each(entry);
        }
        Ok(self.loaded(id, lines, floor, read))
    }

    fn entries(&self, id: SessionId) -> io::Result<Vec<Entry>> {
        self.shared(id)?.entries()
    }

    #[cfg(test)]
    fn count(&self, id: SessionId) -> io::Result<u64> {
        Ok(lock(&self.shared(id)?.lines).len() as u64)
    }
}

impl Shared {
    /// The entries, decoded from a copy of the lines taken at once.
    fn entries(&self) -> io::Result<Vec<Entry>> {
        let lines = lock(&self.lines).clone();
        lines
            .iter()
            .map(|line| Entry::from_json(line).map_err(io::Error::from))
            .collect()
    }
}

impl Lines for Shared {
    fn read(&self, seqs: RangeInclusive<u64>, limit: usize, out: &mut Vec<u8>) -> io::Result<u64> {
        let lines = lock(&self.lines);
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
        lock(&self.0.lines).extend(lines.lines().map(Box::from));
        Ok(())
    }

    fn snapshot(&mut self, snapshot: &Snapshot) -> io::Result<()> {
        if snapshot.covers > lock(&self.0.lines).len() as u64 {
            let why = format!("entry {} is not committed", snapshot.covers);
            return Err(io::Error::new(io::ErrorKind::InvalidInput, why));
        }
        *lock(&self.0.snapshot) = Some(snapshot.clone());
        Ok(())
    }
}

/// Every critical section here is a single push, insert or clone, so a
/// panicking holder cannot leave the data half-changed.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
