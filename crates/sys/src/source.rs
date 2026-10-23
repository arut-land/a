//! Bounded source snapshots. Every pathname check uses a nonblocking regular
//! opener, including revalidation after a source was replaced by a FIFO.
use std::fs::Metadata;
use std::io::{self, Read};
use std::path::Path;
use std::time::SystemTime;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Version {
    identity: (u64, u64),
    len: u64,
    modified: Option<SystemTime>,
    changed: Option<(i64, i64)>,
}

impl Version {
    #[cfg_attr(
        unix,
        expect(clippy::unnecessary_wraps, reason = "Windows identity lookup can fail")
    )]
    fn of(meta: &Metadata) -> io::Result<Self> {
        #[cfg(unix)]
        let (identity, changed) = {
            use std::os::unix::fs::MetadataExt;
            (
                (meta.dev(), meta.ino()),
                Some((meta.ctime(), meta.ctime_nsec())),
            )
        };
        #[cfg(windows)]
        let (identity, changed) = {
            use std::os::windows::fs::MetadataExt;
            let identity = meta
                .volume_serial_number()
                .zip(meta.file_index())
                .ok_or_else(|| io::Error::other("source has no file identity"))?;
            ((u64::from(identity.0), identity.1), None)
        };
        Ok(Self {
            identity,
            len: meta.len(),
            modified: meta.modified().ok(),
            changed,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    version: Version,
    bytes: Vec<u8>,
}

impl Source {
    /// Links may resolve within `root`; native regular-file checks apply at
    /// every open, not just when first discovering the directory entry.
    pub fn read(root: &Path, path: &Path, limit: usize) -> io::Result<Option<Self>> {
        let file = match crate::fs::open_regular_within(root, path)? {
            crate::fs::RegularFile::Open(file) => file,
            crate::fs::RegularFile::Skipped(_) => return Ok(None),
        };
        let before = Version::of(&file.metadata()?)?;
        let mut bytes = Vec::new();
        // Retain failed bytes and version observations before decoding UTF-8.
        (&file).take(limit as u64 + 1).read_to_end(&mut bytes)?;
        let after = Version::of(&file.metadata()?)?;
        if before != after {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "source changed during capture",
            ));
        }
        if bytes.len() > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("source exceeds {limit} bytes"),
            ));
        }
        Ok(Some(Self {
            version: after,
            bytes,
        }))
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn text(&self) -> io::Result<&str> {
        std::str::from_utf8(&self.bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// Inventory may inspect a bounded prefix to resolve names before capturing
/// winning files. Its bytes carry no version or writer-completion assertion.
pub fn prefix(root: &Path, path: &Path, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let file = match crate::fs::open_regular_within(root, path)? {
        crate::fs::RegularFile::Open(file) => file,
        crate::fs::RegularFile::Skipped(_) => return Ok(None),
    };
    let mut bytes = Vec::new();
    file.take(limit as u64).read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

/// Native sharing/locking conflicts may disappear without a filesystem event.
pub fn retryable(error: &io::Error) -> bool {
    let retryable = matches!(
        error.kind(),
        io::ErrorKind::WouldBlock
            | io::ErrorKind::Interrupted
            | io::ErrorKind::TimedOut
            | io::ErrorKind::ResourceBusy
            | io::ErrorKind::ExecutableFileBusy
    );
    #[cfg(windows)]
    let retryable = retryable || matches!(error.raw_os_error(), Some(32 | 33 | 170));
    retryable
}

#[cfg(all(test, unix, not(miri)))]
mod tests {
    use super::*;

    #[test]
    fn contained_symlink_targets_participate_in_snapshot_identity() {
        let root = std::env::temp_dir().join(format!("a-source-link-{}", std::process::id()));
        std::fs::create_dir_all(root.join("review")).unwrap();
        std::fs::create_dir_all(root.join("shared")).unwrap();
        let target = root.join("shared/body.md");
        let source = root.join("review/SKILL.md");
        std::fs::write(&target, "old").unwrap();
        std::os::unix::fs::symlink("../shared/body.md", &source).unwrap();
        let before = Source::read(&root, &source, 64).unwrap().unwrap();
        std::fs::write(&target, "new").unwrap();
        let after = Source::read(&root, &source, 64).unwrap().unwrap();
        assert_ne!(before, after);
        assert_eq!(after.text().unwrap(), "new");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn revalidation_never_waits_for_a_fifo_writer() {
        let root = std::env::temp_dir().join(format!("a-source-fifo-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("source");
        std::fs::write(&path, "settled").unwrap();
        let before = Source::read(&root, &path, 64).unwrap().unwrap();
        assert_eq!(before.text().unwrap(), "settled");
        std::fs::remove_file(&path).unwrap();
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: name is a terminated path, confined to this test's directory.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(Source::read(&root, &path, 64).unwrap().is_none());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "settled").unwrap();
        assert_ne!(Source::read(&root, &path, 64).unwrap().unwrap(), before);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(all(test, windows))]
mod sharing_tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;

    #[test]
    fn a_sharing_conflict_is_retryable_until_the_handle_is_released() {
        let root = std::env::temp_dir().join(format!("a-source-sharing-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("source");
        std::fs::write(&path, "settled").unwrap();
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        for _ in 0..3 {
            assert!(retryable(&Source::read(&root, &path, 64).unwrap_err()));
        }
        drop(writer);
        assert_eq!(
            Source::read(&root, &path, 64)
                .unwrap()
                .unwrap()
                .text()
                .unwrap(),
            "settled"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
