use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;

use protocol::{Entry, SessionId};
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
    fn create(&self, header: &Header) -> io::Result<Loaded> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(self.path(header.id))?;
        write_line(&mut file, header)?;
        file.sync_all()?;
        // The new file's name is only durable once its directory is.
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok(Loaded {
            entries: Vec::new(),
            log: Box::new(JsonlLog(file)),
        })
    }

    fn headers(&self) -> io::Result<Vec<Header>> {
        let mut headers = Vec::new();
        for item in fs::read_dir(&self.dir)? {
            let path = item?.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                let mut line = String::new();
                BufReader::new(File::open(&path)?).read_line(&mut line)?;
                headers.push(serde_json::from_str::<Header>(&line)?);
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
            match serde_json::from_slice(line) {
                Ok(entry) if line.ends_with(b"\n") => entries.push(entry),
                _ => break,
            }
            end += line.len();
        }
        if end < bytes.len() {
            file.set_len(end as u64)?;
            file.sync_data()?;
        }
        Ok(Loaded {
            entries,
            log: Box::new(JsonlLog(file)),
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

/// Durable once `sync_data` returns.
struct JsonlLog(File);

impl Log for JsonlLog {
    fn append(&mut self, entry: &Entry) -> io::Result<()> {
        write_line(&mut self.0, entry)?;
        self.0.sync_data()
    }
}

fn write_line(file: &mut File, value: &impl Serialize) -> io::Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    file.write_all(&line)
}
