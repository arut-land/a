//! Session logs: one append-only JSONL file per session. The first line is a
//! header, every following line one entry.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

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
    pub header: Header,
    pub entries: Vec<Entry>,
    pub log: Log,
}

impl Store {
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn create(&self, header: &Header) -> io::Result<Log> {
        let path = self.path(header.id);
        let mut file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)?;
        write_line(&mut file, header)?;
        file.sync_all()?;
        Ok(Log { file })
    }

    /// Loads every session, oldest first. A torn final line (crash mid-append)
    /// is ignored; it was never acknowledged.
    pub fn load_all(&self) -> io::Result<Vec<Loaded>> {
        let mut sessions = Vec::new();
        for item in fs::read_dir(&self.dir)? {
            let path = item?.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                sessions.push(load(&path)?);
            }
        }
        sessions.sort_by_key(|s| s.header.id.0);
        Ok(sessions)
    }

    fn path(&self, id: SessionId) -> PathBuf {
        self.dir.join(format!("{}.jsonl", id.0.simple()))
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

fn load(path: &Path) -> io::Result<Loaded> {
    let mut lines = BufReader::new(File::open(path)?).lines();
    let header: Header = match lines.next() {
        Some(line) => serde_json::from_str(&line?)?,
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "empty session log",
            ));
        }
    };
    let mut entries = Vec::new();
    for line in lines {
        match serde_json::from_str(&line?) {
            Ok(entry) => entries.push(entry),
            Err(_) => break,
        }
    }
    let file = OpenOptions::new().append(true).open(path)?;
    Ok(Loaded {
        header,
        entries,
        log: Log { file },
    })
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

    #[test]
    fn appends_survive_reload_and_torn_tail_is_dropped() {
        let dir = std::env::temp_dir().join(format!("a-store-{}", std::process::id()));
        let store = Store::open(&dir).unwrap();
        let header = Header {
            id: SessionId::new(),
            workspace: "/w".into(),
        };
        let mut log = store.create(&header).unwrap();
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
        let path = store.path(header.id);
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(br#"{"seq":3,"epo"#).unwrap();

        let loaded = store.load_all().unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].entries.len(), 2);
        assert_eq!(loaded[0].header.id, header.id);
    }
}
