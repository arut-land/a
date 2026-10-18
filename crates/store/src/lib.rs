//! Session logs: one append-only JSONL file per session. The first line is a
//! header, every following line one entry.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;

use protocol::{Entry, SessionId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Header {
    pub id: SessionId,
    pub workspace: PathBuf,
}

pub struct Store {
    dir: PathBuf,
}

pub struct Loaded {
    pub entries: Vec<Entry>,
    pub log: Log,
}

impl Store {
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// A new, empty session log.
    pub fn create(&self, header: &Header) -> io::Result<Loaded> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(self.path(header.id))?;
        write_line(&mut file, header)?;
        file.sync_all()?;
        Ok(Loaded {
            entries: Vec::new(),
            log: Log { file },
        })
    }

    /// Every session's header, oldest first, without reading the entries.
    pub fn headers(&self) -> io::Result<Vec<Header>> {
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

    /// One session's entries and an appender. A torn final line (crash
    /// mid-append) is ignored; it was never acknowledged.
    pub fn load(&self, id: SessionId) -> io::Result<Loaded> {
        let path = self.path(id);
        let mut lines = BufReader::new(File::open(&path)?).lines().skip(1);
        let mut entries = Vec::new();
        while let Some(line) = lines.next().transpose()? {
            match serde_json::from_str(&line) {
                Ok(entry) => entries.push(entry),
                Err(_) => break,
            }
        }
        let file = OpenOptions::new().append(true).open(path)?;
        Ok(Loaded {
            entries,
            log: Log { file },
        })
    }

    /// The number of complete entries, counted without parsing them.
    pub fn count(&self, id: SessionId) -> io::Result<u64> {
        let mut file = File::open(self.path(id))?;
        let (mut lines, mut buf) = (0u64, vec![0u8; 64 * 1024]);
        loop {
            match file.read(&mut buf)? {
                0 => return Ok(lines.saturating_sub(1)),
                n => lines += memchr::memchr_iter(b'\n', &buf[..n]).count() as u64,
            }
        }
    }

    /// Marks a session as having work in flight. A marker that survives a
    /// crash tells the next daemon to load that session at once and recover.
    pub fn mark_busy(&self, id: SessionId, busy: bool) -> io::Result<()> {
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

    pub fn is_busy(&self, id: SessionId) -> bool {
        self.busy_path(id).exists()
    }

    fn path(&self, id: SessionId) -> PathBuf {
        self.dir.join(format!("{id}.jsonl"))
    }

    fn busy_path(&self, id: SessionId) -> PathBuf {
        self.dir.join(format!("{id}.busy"))
    }
}

pub struct Log {
    file: File,
}

impl Log {
    pub fn append(&mut self, entry: &Entry) -> io::Result<()> {
        write_line(&mut self.file, entry)?;
        self.file.sync_data()
    }
}

fn write_line(file: &mut File, value: &impl Serialize) -> io::Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    file.write_all(&line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::Body;

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("a-store-{name}-{}", std::process::id()))
    }

    #[test]
    fn appends_survive_reload_and_torn_tail_is_dropped() {
        let dir = temp("reload");
        let store = Store::open(&dir).unwrap();
        let header = Header {
            id: SessionId::new(),
            workspace: "/w".into(),
        };
        let mut log = store.create(&header).unwrap().log;
        for seq in 1..=2 {
            let body = Body::User {
                text: format!("m{seq}"),
            };
            log.append(&Entry {
                seq,
                epoch: 0,
                body,
            })
            .unwrap();
        }
        let mut f = OpenOptions::new()
            .append(true)
            .open(store.path(header.id))
            .unwrap();
        f.write_all(br#"{"seq":3,"epo"#).unwrap();

        let headers = store.headers().unwrap();
        let loaded = store.load(header.id).unwrap();
        let count = store.count(header.id).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].id, header.id);
        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(count, 2);
    }

    #[test]
    fn busy_markers_round_trip() {
        let dir = temp("busy");
        let store = Store::open(&dir).unwrap();
        let id = SessionId::new();
        store.mark_busy(id, true).unwrap();
        let marked = store.is_busy(id);
        store.mark_busy(id, false).unwrap();
        store.mark_busy(id, false).unwrap();
        let cleared = !store.is_busy(id);
        fs::remove_dir_all(&dir).unwrap();
        assert!(marked && cleared);
    }
}
