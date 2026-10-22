use std::collections::HashMap;
use std::io;
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use protocol::{Decode, Entry, SessionId};

use super::{
    Encoded, Header, History, LeaseAuthority, Lines, Loaded, Local, Log, LogStore, Snapshot,
    Writer, not_found, span,
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
        let shared = self.shared(id)?;
        let lines = lock(&shared.lines);
        let mut history = History::default();
        let entries = lines
            .iter()
            .take(usize::try_from(seq).map_err(io::Error::other)?)
            .map(|line| {
                history
                    .decode(line)
                    .map_err(|why| io::Error::other(why.to_string()))
            })
            .collect::<io::Result<Vec<_>>>()?;
        if (entries.len() as u64) < seq {
            return Err(io::ErrorKind::NotFound.into());
        }
        Ok(entries)
    }

    fn suffix(
        &self,
        id: SessionId,
        cursor: &mut ext::LogCursor,
        through: u64,
    ) -> io::Result<(bool, Vec<Entry>)> {
        let shared = self.shared(id)?;
        let lines = lock(&shared.lines);
        let reset = cursor.seq != 0
            && (cursor.seq > through
                || lines
                    .get(cursor.seq as usize - 1)
                    .and_then(|line| Entry::from_json(line).ok())
                    .is_none_or(|e| (e.seq.get(), e.epoch) != (cursor.seq, cursor.epoch)));
        if reset {
            *cursor = ext::LogCursor::default();
        }
        let mut history = History {
            seq: cursor.seq,
            epoch: cursor.epoch.0,
        };
        let mut entries = Vec::new();
        for seq in cursor.seq..through {
            let line = lines
                .get(usize::try_from(seq).map_err(io::Error::other)?)
                .ok_or(io::ErrorKind::NotFound)?;
            let entry = history
                .decode(line)
                .map_err(|e| io::Error::other(e.to_string()))?;
            *cursor = ext::LogCursor {
                seq: entry.seq.get(),
                epoch: entry.epoch,
                offset: 0,
            };
            entries.push(entry);
        }
        Ok((reset, entries))
    }

    fn import(&self, header: &Header, entries: &[Entry]) -> io::Result<()> {
        let mut history = History::default();
        for entry in entries {
            history
                .accept(entry)
                .map_err(|why| io::Error::other(why.to_string()))?;
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
        let mut history = History::default();
        let mut entries = Vec::new();
        let mut damage = None;
        for line in lock(&lines.lines).iter() {
            match history.decode(line) {
                Ok(entry) => entries.push(entry),
                Err(why) => {
                    damage = Some(why);
                    break;
                }
            }
        }
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
        let read = (lock(&lines.lines).iter().take(entries.len()).skip(skip))
            .map(|line| line.len() as u64 + 1)
            .sum();
        let valid = entries.len();
        for entry in entries.into_iter().skip(skip) {
            each(entry);
        }
        let mut loaded = self.loaded(id, lines, floor, read);
        loaded.seq = history.seq;
        if let Some(why) = damage {
            loaded.writer.keep(why);
            let prefix = lock(&self.shared(id)?.lines)
                .iter()
                .take(valid)
                .cloned()
                .collect();
            loaded.lines = Arc::new(Shared {
                lines: Mutex::new(prefix),
                snapshot: Mutex::default(),
            });
        }
        Ok(loaded)
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
        let mut history = History::default();
        lines
            .iter()
            .map(|line| {
                history
                    .decode(line)
                    .map_err(|why| io::Error::other(why.to_string()))
            })
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
