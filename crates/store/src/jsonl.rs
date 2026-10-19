use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};

use protocol::{Decode, Entry, SessionId};

use crate::{Header, Lease, LeaseAuthority, Lines, Loaded, Local, Log, LogStore, span};

/// One append-only JSONL file per session in a directory: the header on the
/// first line, one entry per line after it, and an empty `.busy` file beside
/// it while work is in flight. Sessions share nothing, so they never contend;
/// a single writer per directory is the daemon lock's job.
pub struct Jsonl {
    dir: PathBuf,
    leases: Local,
}

impl Jsonl {
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            leases: Local::default(),
        })
    }

    fn path(&self, id: SessionId) -> PathBuf {
        self.dir.join(format!("{id}.jsonl"))
    }

    fn busy_path(&self, id: SessionId) -> PathBuf {
        self.dir.join(format!("{id}.busy"))
    }
}

impl LogStore for Jsonl {
    /// The file, header included, becomes durable with its first entry: a
    /// session with none has nothing to lose, and creating one costs no sync.
    fn create(&self, header: &Header) -> io::Result<Loaded> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .append(true)
            .open(self.path(header.id))?;
        let mut line = serde_json::to_vec(header)?;
        line.push(b'\n');
        file.write_all(&line)?;
        let index = Index::new(file, line.len() as u64);
        let lease = self.leases.acquire(header.id, 0);
        Ok(index.loaded(0, lease, Some(self.dir.clone())))
    }

    /// A file whose header did not survive a crash holds no entries either,
    /// so it holds no session.
    fn headers(&self) -> io::Result<Vec<Header>> {
        let mut headers = Vec::new();
        for item in fs::read_dir(&self.dir)? {
            let path = item?.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                let mut line = String::new();
                BufReader::new(File::open(&path)?).read_line(&mut line)?;
                if let Ok(header) = serde_json::from_str::<Header>(&line) {
                    headers.push(header);
                }
            }
        }
        headers.sort_unstable_by_key(|h| h.id.0);
        Ok(headers)
    }

    /// Entries end at the first line that is unterminated or unreadable: a
    /// crash mid-append, never acknowledged. The file is cut back to there
    /// before any line is indexed past it, so the next append starts on a
    /// fresh line and no read lands in what was cut.
    fn load(&self, id: SessionId, each: &mut dyn FnMut(Entry)) -> io::Result<Loaded> {
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(self.path(id))?;
        let mut index = Index::new(file, 0);
        let mut floor = 0;
        index.scan(|entry| {
            floor = entry.epoch;
            each(entry);
        })?;
        let seq = index.read().len() as u64;
        if index.end() < index.file.metadata()?.len() {
            index.file.set_len(index.end())?;
            sys::sync_data(&index.file)?;
        }
        let lease = self.leases.acquire(id, floor);
        Ok(index.loaded(seq, lease, None))
    }

    fn entries(&self, id: SessionId) -> io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        Index::new(File::open(self.path(id))?, 0).scan(|e| entries.push(e))?;
        Ok(entries)
    }

    /// Counts lines rather than parsing them.
    fn count(&self, id: SessionId) -> io::Result<u64> {
        let mut file = File::open(self.path(id))?;
        let (mut lines, mut buf) = (0u64, vec![0u8; 64 * 1024]);
        loop {
            match file.read(&mut buf)? {
                0 => return Ok(lines.saturating_sub(1)),
                n => lines += memchr::memchr_iter(b'\n', &buf[..n]).count() as u64,
            }
        }
    }

    fn mark_busy(&self, id: SessionId, busy: bool) -> io::Result<()> {
        let path = self.busy_path(id);
        if busy {
            File::create(path).map(drop)
        } else {
            match fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        }
    }

    fn is_busy(&self, id: SessionId) -> bool {
        self.busy_path(id).exists()
    }
}

/// Where each committed line of a session file is, so that a line is read
/// back with one positioned read when asked for, from the OS's page cache
/// rather than the daemon's memory: 8 bytes an entry. Only lines read whole
/// and terminated are indexed, and only those appended in full after them.
struct Index {
    file: File,
    /// Where the header line ends.
    start: u64,
    /// Where each entry's line ends, newline included: entry `seq`'s at
    /// `seq - 1`.
    ends: RwLock<Vec<u64>>,
}

impl Index {
    fn new(file: File, start: u64) -> Self {
        Self {
            file,
            start,
            ends: RwLock::default(),
        }
    }

    /// The lines indexed and an appender continuing them, whose entries up
    /// to `seq` are committed.
    fn loaded(self, seq: u64, lease: Lease, new_in: Option<PathBuf>) -> Loaded {
        let index = Arc::new(self);
        Loaded {
            lines: Arc::clone(&index) as Arc<dyn Lines>,
            seq,
            epoch: lease.epoch,
            log: Box::new(JsonlLog {
                index,
                new_in,
                lease,
                torn: false,
            }),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<u64>> {
        self.ends.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Where the last line indexed ends: where the next is appended.
    fn end(&self) -> u64 {
        self.read().last().copied().unwrap_or(self.start)
    }

    /// Indexes the lines of `lines`, appended at the end.
    fn extend(&self, lines: &[u8]) {
        let at = self.end();
        let mut ends = self.ends.write().unwrap_or_else(PoisonError::into_inner);
        ends.extend(memchr::memchr_iter(b'\n', lines).map(|i| at + i as u64 + 1));
    }

    /// Reads the file from the start, line by line, indexing and handing
    /// `each` every entry up to the first unterminated or unreadable line.
    fn scan(&mut self, mut each: impl FnMut(Entry)) -> io::Result<()> {
        let mut reader = BufReader::with_capacity(64 * 1024, &self.file);
        let mut line = Vec::new();
        self.start = reader.read_until(b'\n', &mut line)? as u64;
        let ends = self.ends.get_mut().unwrap_or_else(PoisonError::into_inner);
        let mut end = self.start;
        loop {
            line.clear();
            let n = reader.read_until(b'\n', &mut line)?;
            if !line.ends_with(b"\n") {
                return Ok(());
            }
            let Ok(entry) = Entry::from_json(&line) else {
                return Ok(());
            };
            end += n as u64;
            ends.push(end);
            each(entry);
        }
    }
}

impl Lines for Index {
    /// One positioned read for the lot: the lines lie end to end.
    fn read(&self, seqs: RangeInclusive<u64>, limit: usize, out: &mut Vec<u8>) -> io::Result<u64> {
        let (from, to, count) = {
            let ends = self.read();
            let (first, last) = span(&seqs, ends.len())?;
            let from = first.checked_sub(1).map_or(self.start, |i| ends[i]);
            let fit = ends[first..=last].partition_point(|&end| end - from <= limit as u64);
            let count = fit.max(1);
            (from, ends[first + count - 1], count)
        };
        let at = out.len();
        let len = usize::try_from(to - from).map_err(io::Error::other)?;
        out.resize(at + len, 0);
        sys::read_exact_at(&self.file, &mut out[at..], from)?;
        Ok(count as u64)
    }
}

/// Durable once [`sys::sync_data`] returns.
struct JsonlLog {
    index: Arc<Index>,
    /// The directory of a file not yet synced since it was created: its
    /// name is only durable once the directory is.
    new_in: Option<PathBuf>,
    lease: Lease,
    /// A failed append may have left part of its lines past the last one
    /// indexed, and cutting them off failed too: the next append cuts them
    /// first.
    torn: bool,
}

impl Log for JsonlLog {
    /// One write and one sync for the lot.
    fn append_all(&mut self, entries: &[Entry]) -> io::Result<()> {
        self.lease.admit(entries)?;
        let mut lines = Vec::new();
        for entry in entries {
            entry.encode(&mut lines)?;
            lines.push(b'\n');
        }
        self.write(&lines)
    }
}

impl JsonlLog {
    fn write(&mut self, lines: &[u8]) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let end = self.index.end();
        let file = &self.index.file;
        if self.torn {
            file.set_len(end)?;
            self.torn = false;
        }
        let written = (|| {
            (&*file).write_all(lines)?;
            sys::sync_data(file)?;
            #[cfg(unix)]
            if let Some(dir) = &self.new_in {
                File::open(dir)?.sync_all()?;
            }
            io::Result::Ok(())
        })();
        match written {
            Ok(()) => {
                self.new_in = None;
                self.index.extend(lines);
                Ok(())
            }
            Err(e) => {
                self.torn = file.set_len(end).is_err();
                Err(e)
            }
        }
    }
}
