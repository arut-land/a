use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};

use protocol::{Decode, Entry, Json, ReadOnly, SessionId};
use serde::{Deserialize, Serialize};

use super::{
    Encoded, Header, Kept, LeaseAuthority, Lines, Loaded, Local, Log, LogStore, Snapshot, Writer,
    span,
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
///
/// Derived from each log and never synced, two files in `derived/` spare
/// the next load its length: `<id>.idx`, where each line ends, and
/// `<id>.snap`, the newest snapshot. Either is used only as far as it
/// agrees with the log, and a load without them reads the log whole, so
/// the directory may be deleted at any time. Being apart, writing them
/// leaves the logs' directory as it was, which the catalog reads the age
/// of.
pub struct Jsonl {
    dir: PathBuf,
    leases: Local,
}

impl Jsonl {
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(dir.join(DERIVED))?;
        Ok(Self {
            dir,
            leases: Local::default(),
        })
    }

    fn path(&self, id: SessionId) -> PathBuf {
        self.dir.join(format!("{id}.jsonl"))
    }
}

/// Where the files derived from the logs are kept, in their directory.
const DERIVED: &str = "derived";

/// `<id>.jsonl`'s derived file of extension `ext`.
fn beside(log: &Path, ext: &str) -> PathBuf {
    let name = Path::new(log.file_name().unwrap_or_default()).with_extension(ext);
    log.with_file_name(DERIVED).join(name)
}

impl LogStore for Jsonl {
    /// The file, header included, becomes durable with its first entry: a
    /// session with none has nothing to lose, and creating one costs no sync.
    fn create(&self, header: &Header) -> io::Result<Loaded> {
        let path = self.path(header.id);
        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .append(true)
            .open(&path)?;
        let mut line = format!(r#"{{"format":{FORMAT},"#).into_bytes();
        let at = line.len();
        serde_json::to_writer(&mut line, header)?;
        line.remove(at);
        seal(&mut line, 0);
        file.write_all(&line)?;
        let index = Index::new(file, path, line.len() as u64);
        index.persist(0);
        let epoch = self.leases.acquire(header.id, 0);
        Ok(index.loaded(0, 0, epoch, Ok(Some(self.dir.clone()))))
    }

    fn prefix(&self, id: SessionId, seq: u64) -> io::Result<Vec<Entry>> {
        let mut reader = BufReader::new(File::open(self.path(id))?);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line)?;
        if !unseal(&mut line) {
            return Err(io::Error::other("unreadable session header"));
        }
        let mut entries = Vec::new();
        for next in 1..=seq {
            line.clear();
            reader.read_until(b'\n', &mut line)?;
            if !unseal(&mut line) {
                return Err(io::Error::other("incomplete or damaged session prefix"));
            }
            let entry = Entry::from_json(&line)?;
            if entry.seq.get() != next {
                return Err(io::Error::other("session prefix has a sequence gap"));
            }
            entries.push(entry);
        }
        Ok(entries)
    }

    fn import(&self, header: &Header, entries: &[Entry]) -> io::Result<()> {
        let path = self.path(header.id);
        if path.exists() {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        // Pending imports are outside the catalog's namespace, including
        // after a crash. Only the complete, synced file becomes a session.
        let pending = path.with_extension("pending");
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&pending)?;
            let mut line = format!(r#"{{"format":{FORMAT},"#).into_bytes();
            let at = line.len();
            serde_json::to_writer(&mut line, header)?;
            line.remove(at);
            seal(&mut line, 0);
            file.write_all(&line)?;
            for (index, entry) in entries.iter().enumerate() {
                if entry.seq.get() != index as u64 + 1 {
                    return Err(io::Error::other("import has a sequence gap"));
                }
                line.clear();
                entry.encode(&mut line)?;
                seal(&mut line, 0);
                file.write_all(&line)?;
            }
            file.sync_all()?;
            drop(file);
            fs::rename(&pending, &path)?;
            sys::fs::sync_dir(&self.dir)
        })();
        if result.is_err() {
            let _ = fs::remove_file(pending);
        }
        result
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
    ///
    /// The scan starts after the snapshot if `restore` takes it, else at
    /// the first entry; the lines before where it starts are placed by
    /// `<id>.idx`, checked against the log where they end. What the scan
    /// indexes that the file lacks is added to it.
    fn load(
        &self,
        id: SessionId,
        restore: &mut dyn FnMut(Snapshot) -> bool,
        each: &mut dyn FnMut(Entry),
    ) -> io::Result<Loaded> {
        let path = self.path(id);
        let file = OpenOptions::new().read(true).append(true).open(&path)?;
        let mut index = Index::new(file, path, 0);
        let other = index.header()?;
        let mut floor = index.recall()?;
        let covered = match Snap::read(&index.path) {
            Some(snap) if index.places(&snap) => {
                let covers = snap.covers;
                let taken = restore(Snapshot {
                    covers,
                    image: snap.image,
                });
                taken.then_some(covers)
            }
            _ => None,
        };
        if covered.is_none() {
            floor = 0;
        }
        let placed = index.resume(covered.unwrap_or(0));
        let from = index.end();
        let kept = index.scan(other, |entry| {
            floor = entry.epoch.0;
            each(entry);
        })?;
        let (seq, read) = (index.read().ends.len() as u64, index.end() - from);
        if kept.is_none() && index.end() < index.file.metadata()?.len() {
            index.file.set_len(index.end())?;
            sys::sync_data(&index.file)?;
        }
        let agreed = placed.agreed(&index.read().ends);
        index.persist(agreed);
        let epoch = self.leases.acquire(id, floor);
        Ok(index.loaded(seq, read, epoch, kept.map_or(Ok(None), Err)))
    }

    #[cfg(test)]
    fn entries(&self, id: SessionId) -> io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        let path = self.path(id);
        let mut index = Index::new(File::open(&path)?, path, 0);
        let other = index.header()?;
        index.scan(other, |e| entries.push(e))?;
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
/// The same 8 bytes an entry go to `<id>.idx` as they are indexed, never
/// synced, so the next load places the lines it does not read.
struct Index {
    file: File,
    /// The log's path: its sidecars are named after it.
    path: PathBuf,
    /// Where the header line ends.
    start: u64,
    ends: RwLock<Ends>,
}

struct Ends {
    /// Where each entry's line ends, newline included: entry `seq`'s at
    /// `seq - 1`.
    ends: Vec<u64>,
    /// `<id>.idx`, open for appending the ends of lines appended; `None`
    /// once a write to it failed, and the next load scans the lines it
    /// lacks.
    idx: Option<File>,
}

impl Index {
    fn new(file: File, path: PathBuf, start: u64) -> Self {
        Self {
            file,
            path,
            start,
            ends: RwLock::new(Ends {
                ends: Vec::new(),
                idx: None,
            }),
        }
    }

    /// The lines indexed and an appender continuing them, whose entries up
    /// to `seq` are committed: one that refuses to, for a file kept as it
    /// is.
    fn loaded(
        self,
        seq: u64,
        read: u64,
        epoch: u32,
        kept: Result<Option<PathBuf>, ReadOnly>,
    ) -> Loaded {
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
            read,
            writer: Writer::new(log, epoch),
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, Ends> {
        self.ends.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn ends_mut(&mut self) -> &mut Ends {
        self.ends.get_mut().unwrap_or_else(PoisonError::into_inner)
    }

    /// Where the last line indexed ends: where the next is appended.
    fn end(&self) -> u64 {
        self.read().ends.last().copied().unwrap_or(self.start)
    }

    /// Indexes the lines of `lines`, appended at the end, in memory and in
    /// `<id>.idx`.
    fn extend(&self, lines: &[u8]) {
        let at = self.end();
        let mut ends = self.ends.write().unwrap_or_else(PoisonError::into_inner);
        let from = ends.ends.len();
        (ends.ends).extend(memchr::memchr_iter(b'\n', lines).map(|i| at + i as u64 + 1));
        let bytes = little_endian(&ends.ends[from..]);
        if let Some(idx) = &mut ends.idx
            && idx.write_all(&bytes).is_err()
        {
            ends.idx = None;
        }
    }

    /// Reads the header line, the first: where the entries start, and why
    /// the file is kept as it is if its format is another.
    fn header(&mut self) -> io::Result<Option<ReadOnly>> {
        #[derive(Deserialize)]
        struct Format {
            format: u32,
        }
        let mut reader = BufReader::new(&self.file);
        let mut line = Vec::new();
        self.start = reader.read_until(b'\n', &mut line)? as u64;
        let format = unseal(&mut line)
            .then(|| serde_json::from_slice::<Format>(&line).ok())
            .flatten()
            .map_or(0, |f| f.format);
        Ok((format != FORMAT).then_some(ReadOnly::Format { version: format }))
    }

    /// Takes up `<id>.idx` as far as the file bears it out: its ends must
    /// rise from after the header to no further than the file goes, and
    /// the last line it places must be a sealed entry numbered as it says;
    /// else none of it is used. Returns the epoch of that last line, the
    /// floor of the lease when nothing after it is read.
    fn recall(&mut self) -> io::Result<u32> {
        let path = beside(&self.path, "idx");
        let mut ends: Vec<u64> = fs::read(&path).map_or_else(
            |_| Vec::new(),
            |bytes| {
                bytes
                    .as_chunks()
                    .0
                    .iter()
                    .map(|b| u64::from_le_bytes(*b))
                    .collect()
            },
        );
        let len = self.file.metadata()?.len();
        let rising = ends.first().is_none_or(|&first| first > self.start)
            && ends.windows(2).all(|w| w[0] < w[1])
            && ends.last().is_none_or(|&last| last <= len);
        let mut floor = 0;
        let placed = rising
            && ends.len().checked_sub(1).is_none_or(|last| {
                self.entry(&ends, last).is_some_and(|e| {
                    floor = e.epoch.0;
                    e.seq.get() == ends.len() as u64
                })
            });
        if !placed {
            ends.clear();
        }
        self.ends_mut().ends = ends;
        Ok(floor)
    }

    /// Entry `at + 1`, read where `ends` places its line, if it is there.
    fn entry(&self, ends: &[u64], at: usize) -> Option<Entry> {
        let from = at.checked_sub(1).map_or(self.start, |i| ends[i]);
        let mut line = vec![0; usize::try_from(ends[at].checked_sub(from)?).ok()?];
        sys::read_exact_at(&self.file, &mut line, from).ok()?;
        unseal(&mut line).then(|| Entry::from_json(&line).ok())?
    }

    /// Whether the index places `snap`: its line ends where the snapshot
    /// says, and holds the entry it covers.
    fn places(&self, snap: &Snap) -> bool {
        let ends = &self.read().ends;
        let at = usize::try_from(snap.covers)
            .ok()
            .and_then(|c| c.checked_sub(1));
        at.is_some_and(|at| {
            ends.get(at) == Some(&snap.offset)
                && self
                    .entry(ends, at)
                    .is_some_and(|e| e.seq.get() == snap.covers)
        })
    }

    /// Keeps the lines up to entry `covers`, the snapshot's, as placed, so
    /// the scan starts after them (at the first entry for 0), and hands
    /// back the rest of what the index placed, for the scan to bear out.
    fn resume(&mut self, covers: u64) -> Placed {
        let ends = &mut self.ends_mut().ends;
        let at = usize::try_from(covers)
            .unwrap_or(usize::MAX)
            .min(ends.len());
        Placed {
            at,
            after: ends.split_off(at),
        }
    }

    /// Writes to `<id>.idx` what it lacks of the lines indexed now, the
    /// first `agreed` of which it holds already, and keeps it open to go
    /// on appending.
    fn persist(&self, agreed: usize) {
        let path = beside(&self.path, "idx");
        let mut ends = self.ends.write().unwrap_or_else(PoisonError::into_inner);
        let rest = little_endian(&ends.ends[agreed..]);
        let written = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .and_then(|mut idx| {
                idx.set_len(agreed as u64 * 8)?;
                idx.seek(SeekFrom::End(0))?;
                idx.write_all(&rest)?;
                Ok(idx)
            });
        ends.idx = written.ok();
    }

    /// Reads the file from where the last line indexed ends, line by line,
    /// indexing and handing `each` every entry up to the first line that
    /// is not a sealed entry this build reads. Stopping at the last line,
    /// unsealed, is cutting a torn tail; stopping anywhere else, or in a
    /// file of another format (`other`), is keeping the file as it is,
    /// for the reason returned.
    fn scan(
        &mut self,
        other: Option<ReadOnly>,
        mut each: impl FnMut(Entry),
    ) -> io::Result<Option<ReadOnly>> {
        let mut end = self.end();
        (&self.file).seek(SeekFrom::Start(end))?;
        let mut reader = BufReader::with_capacity(64 * 1024, &self.file);
        let ends = self.ends.get_mut().unwrap_or_else(PoisonError::into_inner);
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = reader.read_until(b'\n', &mut line)?;
            if n == 0 {
                return Ok(other);
            }
            let sealed = unseal(&mut line);
            if let Some(entry) = sealed.then(|| Entry::from_json(&line).ok()).flatten() {
                end += n as u64;
                ends.ends.push(end);
                each(entry);
                continue;
            }
            let torn = !sealed && reader.fill_buf()?.is_empty();
            let damaged = ReadOnly::Damaged {
                seq: ends.ends.len() as u64 + 1,
            };
            return Ok(other.or((!torn).then_some(damaged)));
        }
    }
}

/// What `<id>.idx` placed past where a load's scan starts.
struct Placed {
    at: usize,
    after: Vec<u64>,
}

impl Placed {
    /// How many of `ends`, as the scan left them, the file holds already:
    /// the lines before the scan, and those after it that it bore out.
    fn agreed(&self, ends: &[u64]) -> usize {
        let scanned = ends.get(self.at..).unwrap_or_default();
        let same = (scanned.iter().zip(&self.after))
            .take_while(|(a, b)| a == b)
            .count();
        self.at + same
    }
}

/// `ends`, 8 bytes each, as `<id>.idx` holds them.
fn little_endian(ends: &[u64]) -> Vec<u8> {
    ends.iter().flat_map(|end| end.to_le_bytes()).collect()
}

impl Lines for Index {
    /// One positioned read for the lot: the lines lie end to end, each
    /// unsealed as it is handed over.
    fn read(&self, seqs: RangeInclusive<u64>, limit: usize, out: &mut Vec<u8>) -> io::Result<u64> {
        let (from, to, count) = {
            let ends = &self.read().ends;
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

/// `<id>.snap`: a [`Snapshot`] and where in the log the line of the entry
/// it covers ends, one sealed line.
#[derive(Serialize, Deserialize)]
struct Snap {
    covers: u64,
    offset: u64,
    image: Json,
}

impl Snap {
    /// The snapshot beside `log`, if there is one and it is whole.
    fn read(log: &Path) -> Option<Self> {
        let mut line = fs::read(beside(log, "snap")).ok()?;
        unseal(&mut line).then(|| serde_json::from_slice(&line).ok())?
    }

    /// Written beside `log` whole or not at all: into a file of its own,
    /// renamed into place.
    fn write(&self, log: &Path) -> io::Result<()> {
        let mut line = serde_json::to_vec(self)?;
        seal(&mut line, 0);
        let fresh = beside(log, "snap.new");
        fs::write(&fresh, line)?;
        fs::rename(fresh, beside(log, "snap"))
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

    fn sync(&mut self) -> io::Result<()> {
        sys::sync_data(&self.index.file)?;
        if let Some(dir) = self.new_in.take() {
            sys::fs::sync_dir(&dir)?;
        }
        Ok(())
    }

    /// Placed where the line of the entry it covers ends, which must be
    /// indexed: one it does not is not committed.
    fn snapshot(&mut self, snapshot: &Snapshot) -> io::Result<()> {
        let offset = {
            let ends = &self.index.read().ends;
            let at = usize::try_from(snapshot.covers)
                .ok()
                .and_then(|c| c.checked_sub(1));
            at.and_then(|at| ends.get(at)).copied().ok_or_else(|| {
                let why = format!("entry {} is not committed", snapshot.covers);
                io::Error::new(io::ErrorKind::InvalidInput, why)
            })?
        };
        let snap = Snap {
            covers: snapshot.covers,
            offset,
            image: snapshot.image.clone(),
        };
        snap.write(&self.index.path)
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
