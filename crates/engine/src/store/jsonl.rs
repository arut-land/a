use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};

use protocol::{Decode, Entry, ReadOnly, SessionId};

use super::{
    Encoded, Header, Kept, LeaseAuthority, Lines, Loaded, Local, Log, LogStore, Writer, span,
};

/// The format of the files this build writes, which the header line names:
/// a file in another is kept as it is.
pub(crate) const FORMAT: u32 = 1;

/// One append-only JSONL file per session in a directory: the header on the
/// first line, one entry per line after it. Sessions share nothing, so they
/// never contend; a single writer per directory is the daemon lock's job.
///
/// Every line is sealed with a checksum of itself ([`seal`]), so what a load
/// cuts depends only on whether a line is whole, never on whether this
/// build understands it: a sealed line is never cut, a last line that is not
/// is the tail a crash tore and is cut, and one before the last means the
/// file was damaged otherwise, so it is kept as it is.
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
        let mut line = format!(r#"{{"format":{FORMAT},"#).into_bytes();
        let at = line.len();
        serde_json::to_writer(&mut line, header)?;
        line.remove(at);
        seal(&mut line, 0);
        file.write_all(&line)?;
        let index = Index::new(file, line.len() as u64);
        let epoch = self.leases.acquire(header.id, 0);
        Ok(index.loaded(0, epoch, Ok(Some(self.dir.clone()))))
    }

    /// A file whose header did not survive a crash holds no entries either,
    /// so it holds no session; nor does one whose header is not sealed.
    fn headers(&self) -> io::Result<Vec<Header>> {
        let mut headers = Vec::new();
        for item in fs::read_dir(&self.dir)? {
            let path = item?.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                let mut line = Vec::new();
                BufReader::new(File::open(&path)?).read_until(b'\n', &mut line)?;
                if unseal(&mut line)
                    && let Ok(header) = serde_json::from_slice::<Header>(&line)
                {
                    headers.push(header);
                }
            }
        }
        headers.sort_unstable_by_key(|h| h.id.0);
        Ok(headers)
    }

    /// Entries end where [`Index::scan`] stops. A torn tail, a crash
    /// mid-append never acknowledged, is cut off before any line is indexed
    /// past it, so the next append starts on a fresh line and no read lands
    /// in what was cut; a file kept as it is is not touched.
    fn load(&self, id: SessionId, each: &mut dyn FnMut(Entry)) -> io::Result<Loaded> {
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(self.path(id))?;
        let mut index = Index::new(file, 0);
        let mut floor = 0;
        let kept = index.scan(|entry| {
            floor = entry.epoch;
            each(entry);
        })?;
        let seq = index.read().len() as u64;
        if kept.is_none() && index.end() < index.file.metadata()?.len() {
            index.file.set_len(index.end())?;
            sys::sync_data(&index.file)?;
        }
        let epoch = self.leases.acquire(id, floor);
        Ok(index.loaded(seq, epoch, kept.map_or(Ok(None), Err)))
    }

    #[cfg(test)]
    fn entries(&self, id: SessionId) -> io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        Index::new(File::open(self.path(id))?, 0).scan(|e| entries.push(e))?;
        Ok(entries)
    }

    /// Counts lines rather than parsing them.
    #[cfg(test)]
    fn count(&self, id: SessionId) -> io::Result<u64> {
        use std::io::Read as _;
        let mut file = File::open(self.path(id))?;
        let (mut lines, mut buf) = (0u64, vec![0u8; 64 * 1024]);
        loop {
            match file.read(&mut buf)? {
                0 => return Ok(lines.saturating_sub(1)),
                n => lines += memchr::memchr_iter(b'\n', &buf[..n]).count() as u64,
            }
        }
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
    /// to `seq` are committed: one that refuses to, for a file kept as it
    /// is.
    fn loaded(self, seq: u64, epoch: u32, kept: Result<Option<PathBuf>, ReadOnly>) -> Loaded {
        let index = Arc::new(self);
        let log: Box<dyn Log> = match kept {
            Ok(new_in) => Box::new(JsonlLog {
                index: Arc::clone(&index),
                new_in,
                torn: false,
            }),
            Err(why) => Box::new(Kept(why)),
        };
        Loaded {
            lines: index,
            seq,
            writer: Writer::new(log, epoch),
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
    /// `each` every entry up to the first line that is not a sealed entry
    /// this build reads. Stopping at the last line, unsealed, is cutting a
    /// torn tail; stopping anywhere else, or in a file of another format,
    /// is keeping the file as it is, for the reason returned.
    fn scan(&mut self, mut each: impl FnMut(Entry)) -> io::Result<Option<ReadOnly>> {
        #[derive(serde::Deserialize)]
        struct Format {
            format: u32,
        }
        let mut reader = BufReader::with_capacity(64 * 1024, &self.file);
        let mut line = Vec::new();
        self.start = reader.read_until(b'\n', &mut line)? as u64;
        let format = unseal(&mut line)
            .then(|| serde_json::from_slice::<Format>(&line).ok())
            .flatten()
            .map_or(0, |f| f.format);
        let other = (format != FORMAT).then_some(ReadOnly::Format { version: format });
        let ends = self.ends.get_mut().unwrap_or_else(PoisonError::into_inner);
        let mut end = self.start;
        loop {
            line.clear();
            let n = reader.read_until(b'\n', &mut line)?;
            if n == 0 {
                return Ok(other);
            }
            let sealed = unseal(&mut line);
            if let Some(entry) = sealed.then(|| Entry::from_json(&line).ok()).flatten() {
                end += n as u64;
                ends.push(end);
                each(entry);
                continue;
            }
            let torn = !sealed && reader.fill_buf()?.is_empty();
            let damaged = ReadOnly::Damaged {
                seq: ends.len() as u64 + 1,
            };
            return Ok(other.or((!torn).then_some(damaged)));
        }
    }
}

impl Lines for Index {
    /// One positioned read for the lot: the lines lie end to end, each
    /// unsealed as it is handed over.
    fn read(&self, seqs: RangeInclusive<u64>, limit: usize, out: &mut Vec<u8>) -> io::Result<u64> {
        let (from, to, count) = {
            let ends = self.read();
            let (first, last) = span(&seqs, ends.len())?;
            let from = first.checked_sub(1).map_or(self.start, |i| ends[i]);
            let unsealed = |(end, lines): (&u64, u64)| end - from - lines * SEAL as u64;
            let fit = (ends[first..=last].iter().zip(1..))
                .take_while(|&line| unsealed(line) <= limit as u64)
                .count();
            let count = fit.max(1);
            (from, ends[first + count - 1], count)
        };
        let at = out.len();
        let len = usize::try_from(to - from).map_err(io::Error::other)?;
        out.resize(at + len, 0);
        sys::read_exact_at(&self.file, &mut out[at..], from)?;
        unseal_run(out, at);
        Ok(count as u64)
    }
}

/// Durable once [`sys::sync_data`] returns.
struct JsonlLog {
    index: Arc<Index>,
    /// The directory of a file not yet synced since it was created: its
    /// name is only durable once the directory is.
    new_in: Option<PathBuf>,
    /// A failed append may have left part of its lines past the last one
    /// indexed, and cutting them off failed too: the next append cuts them
    /// first.
    torn: bool,
}

impl Log for JsonlLog {
    /// One write and one sync for the lot, which makes what was only
    /// written before it durable too.
    fn append_all(&mut self, _: &[Entry], lines: &Encoded) -> io::Result<()> {
        self.write(&sealed(lines), true)
    }

    /// One write, and no sync.
    fn write_all(&mut self, _: &[Entry], lines: &Encoded) -> io::Result<()> {
        self.write(&sealed(lines), false)
    }
}

/// `lines`, each sealed, as the file stores them.
fn sealed(lines: &Encoded) -> Vec<u8> {
    let mut out = Vec::new();
    for line in lines.lines() {
        let at = out.len();
        out.extend_from_slice(line);
        seal(&mut out, at);
    }
    out
}

impl JsonlLog {
    fn write(&mut self, lines: &[u8], sync: bool) -> io::Result<()> {
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
            if !sync {
                return Ok(());
            }
            sys::sync_data(file)?;
            if let Some(dir) = &self.new_in {
                sys::fs::sync_dir(dir)?;
            }
            io::Result::Ok(())
        })();
        match written {
            Ok(()) => {
                if sync {
                    self.new_in = None;
                }
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

/// What ends a sealed line, after its JSON object's last field: the CRC32C
/// of every byte before it, as 8 hex digits, so the line is still one JSON
/// object, `,"crc":"1a2b3c4d"}`.
const CRC: &[u8] = br#","crc":""#;
const SEAL: usize = CRC.len() + 8 + br#""}"#.len();

/// Seals the JSON object `line` holds from `at` on, a line of its own.
pub(crate) fn seal(line: &mut Vec<u8>, at: usize) {
    debug_assert_eq!(line.last(), Some(&b'}'), "a line is a JSON object");
    line.pop();
    let crc = crc32c::crc32c(&line[at..]);
    line.extend_from_slice(CRC);
    line.extend_from_slice(&hex(crc));
    line.extend_from_slice(b"\"}\n");
}

/// Whether `line`, newline included, is sealed; if it is, it becomes the
/// JSON object it seals.
pub(crate) fn unseal(line: &mut Vec<u8>) -> bool {
    let Some(fields) = line.len().checked_sub(SEAL + 1) else {
        return false;
    };
    let (object, seal) = line.split_at(fields);
    let sealed = seal.starts_with(CRC)
        && seal.ends_with(b"\"}\n")
        && seal[CRC.len()..][..8] == hex(crc32c::crc32c(object));
    if sealed {
        line.truncate(fields);
        line.push(b'}');
    }
    sealed
}

/// The lines of `out` from `at` on, each sealed, as the objects they seal:
/// lines read back are the entries' JSON.
fn unseal_run(out: &mut Vec<u8>, at: usize) {
    let (mut from, mut to) = (at, at);
    while let Some(newline) = memchr::memchr(b'\n', &out[from..]) {
        let fields = from + newline - SEAL;
        out.copy_within(from..fields, to);
        to += fields - from;
        out[to..to + 2].copy_from_slice(b"}\n");
        to += 2;
        from += newline + 1;
    }
    out.truncate(to);
}

/// `crc` as 8 lower-case hex digits.
fn hex(crc: u32) -> [u8; 8] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    std::array::from_fn(|i| DIGITS[(crc >> (28 - 4 * i) & 0xf) as usize])
}
