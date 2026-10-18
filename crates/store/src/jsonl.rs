use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;

use protocol::{Decode, Entry, SessionId};
use serde::Serialize;

use crate::{Header, Loaded, Log, LogStore};

/// One append-only JSONL file per session in a directory: the header on the
/// first line, one entry per line after it, and an empty `.busy` file beside
/// it while work is in flight. Sessions share nothing, so they never contend;
/// a single writer per directory is the daemon lock's job.
pub struct Jsonl {
    dir: PathBuf,
}

impl Jsonl {
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
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
            .append(true)
            .open(self.path(header.id))?;
        write_line(&mut file, header)?;
        Ok(Loaded {
            entries: Vec::new(),
            log: Box::new(JsonlLog {
                file,
                new_in: Some(self.dir.clone()),
            }),
        })
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
        headers.sort_by_key(|h| h.id.0);
        Ok(headers)
    }

    /// Entries end at the first line that is unterminated or unreadable: a
    /// crash mid-append, never acknowledged. The file is cut back to there
    /// so the next append starts on a fresh line.
    fn load(&self, id: SessionId) -> io::Result<Loaded> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(self.path(id))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let mut end = memchr::memchr(b'\n', &bytes).map_or(bytes.len(), |i| i + 1);
        let mut entries = Vec::new();
        for line in bytes[end..].split_inclusive(|&b| b == b'\n') {
            match Entry::from_json(line) {
                Ok(entry) if line.ends_with(b"\n") => entries.push(entry),
                _ => break,
            }
            end += line.len();
        }
        if end < bytes.len() {
            file.set_len(end as u64)?;
            sys::sync_data(&file)?;
        }
        Ok(Loaded {
            entries,
            log: Box::new(JsonlLog { file, new_in: None }),
        })
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

/// Durable once [`sys::sync_data`] returns.
struct JsonlLog {
    file: File,
    /// The directory of a file not yet synced since it was created: its
    /// name is only durable once the directory is.
    new_in: Option<PathBuf>,
}

impl Log for JsonlLog {
    fn append(&mut self, entry: &Entry) -> io::Result<()> {
        self.append_all(std::slice::from_ref(entry))
    }

    /// One write and one sync for the lot.
    fn append_all(&mut self, entries: &[Entry]) -> io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut lines = Vec::new();
        for entry in entries {
            entry.encode(&mut lines)?;
            lines.push(b'\n');
        }
        self.file.write_all(&lines)?;
        sys::sync_data(&self.file)?;
        #[cfg(unix)]
        if let Some(dir) = &self.new_in {
            File::open(dir)?.sync_all()?;
        }
        self.new_in = None;
        Ok(())
    }
}

fn write_line(file: &mut File, value: &impl Serialize) -> io::Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    file.write_all(&line)
}
