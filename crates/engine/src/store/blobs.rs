//! The JSONL store's blobs: one read-only file per blob at `<aa>/<id>`,
//! named by the SHA-256 of its bytes, one namespace for every session.
//!
//! A writer hashes into `staging/` and a stream grows in `open/`; sealing
//! orders the file's bytes, makes it read-only and links it under its id,
//! ordering that name too. Neither is ever trusted after a crash: only a
//! name under `<aa>/` is a blob, and a sweep at rest removes the rest.
//!
//! Ordering, not a flush of its own, is what makes a blob durable no later
//! than the entry that references it: the entry is appended after the
//! seal returns, so the shared flush that makes the entry durable covers
//! the blob's barriers too, as it covers many logs'. A blob on a device
//! that flush does not reach is flushed at once.

use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use ext::blob::{
    BlobId, BlobOrigin, BlobRef, BlobStore, BlobWriter, Open, RefIndex, Sealer, Stream,
};
use sha2::{Digest, Sha256};

const STAGING: &str = "staging";
const OPEN: &str = "open";

pub struct Files(Arc<Inner>);

struct Inner {
    dir: PathBuf,
    /// The logs' flush device: a blob elsewhere is flushed as it is sealed.
    domain: sys::fs::FlushDomain,
    /// Whether this daemon has made the store's own directory, which it
    /// leaves until the first blob: a daemon that writes none never pays.
    made: Mutex<bool>,
    /// The fan directories whose names this daemon has made durable.
    fans: Mutex<[bool; 256]>,
    staged: AtomicU64,
}

impl Files {
    /// Blobs in `dir`, whose parent exists, for logs flushed on the device
    /// `logs` is on.
    pub fn open(dir: impl Into<PathBuf>, logs: &Path) -> io::Result<Self> {
        Ok(Self(Arc::new(Inner {
            domain: sys::fs::FlushDomain::of(logs)?,
            dir: dir.into(),
            made: Mutex::new(false),
            fans: Mutex::new([false; 256]),
            staged: AtomicU64::new(0),
        })))
    }
}

impl Inner {
    /// `dir/sub`, made private, below the store's directory made durable.
    fn private(&self, sub: &Path) -> io::Result<PathBuf> {
        let mut made = self.made.lock().unwrap_or_else(PoisonError::into_inner);
        if !*made {
            let dir = &self.dir;
            sys::fs::create_dir_all_durable(dir.parent().unwrap_or(dir), dir)?;
            sys::fs::private_dir(dir)?;
            *made = true;
        }
        drop(made);
        let dir = self.dir.join(sub);
        sys::fs::private_dir(&dir)?;
        Ok(dir)
    }

    fn path(&self, id: &BlobId) -> PathBuf {
        self.dir.join(id.fan()).join(id.to_string())
    }

    /// Orders `file`'s bytes, makes it read-only and publishes it at its
    /// id, unless that blob exists already: the same bytes, published by
    /// an earlier seal.
    fn seal(&self, file: &File, staged: &Path, id: BlobId) -> io::Result<()> {
        sys::sync_data(file)?;
        if self.domain.check(file).is_err() {
            sys::fs::flush_cache(file)?;
        }
        let mut read_only = file.metadata()?.permissions();
        read_only.set_readonly(true);
        file.set_permissions(read_only)?;
        let path = self.path(&id);
        if !path.exists() {
            self.fan(&id)?;
            match sys::fs::publish_ordered(staged, &path) {
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                published => return published,
            }
        }
        fs::remove_file(staged)
    }

    /// Publishes the fan directory `id` goes in, once per daemon: an
    /// earlier daemon may have made it without making its name durable.
    fn fan(&self, id: &BlobId) -> io::Result<()> {
        let mut fans = self.fans.lock().unwrap_or_else(PoisonError::into_inner);
        let made = &mut fans[usize::from(id.0[0])];
        if !*made {
            sys::fs::create_dir_all_durable(&self.dir, &self.dir.join(id.fan()))?;
            *made = true;
        }
        Ok(())
    }

    fn staging(&self) -> io::Result<(File, PathBuf)> {
        let dir = self.private(Path::new(STAGING))?;
        loop {
            let n = self.staged.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("{}-{n}", std::process::id()));
            match sys::fs::private_file().read(true).open(&path) {
                Ok(file) => return Ok((file, path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
    }
}

fn blob(id: BlobId, size: u64, media_type: &str, origin: BlobOrigin) -> BlobRef {
    BlobRef {
        id,
        size,
        media_type: media_type.to_owned(),
        origin,
    }
}

fn too_large(cap: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::FileTooLarge,
        format!("a blob holds at most {cap} bytes"),
    )
}

struct Writing {
    store: Arc<Inner>,
    file: File,
    path: PathBuf,
    hash: Sha256,
    size: u64,
    cap: u64,
    sealed: bool,
}

impl Write for Writing {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.size + buf.len() as u64 > self.cap {
            return Err(too_large(self.cap));
        }
        let n = self.file.write(buf)?;
        self.hash.update(&buf[..n]);
        self.size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl BlobWriter for Writing {
    fn finish(mut self: Box<Self>, media_type: &str, origin: BlobOrigin) -> io::Result<BlobRef> {
        let id = BlobId(std::mem::take(&mut self.hash).finalize().into());
        self.store.seal(&self.file, &self.path, id)?;
        self.sealed = true;
        Ok(blob(id, self.size, media_type, origin))
    }
}

impl Drop for Writing {
    fn drop(&mut self) {
        if !self.sealed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct Sealing {
    store: Arc<Inner>,
    cap: u64,
}

impl Sealer for Sealing {
    fn seal(
        self: Box<Self>,
        path: &Path,
        media_type: &str,
        origin: BlobOrigin,
    ) -> io::Result<BlobRef> {
        let mut file = fs::OpenOptions::new().read(true).write(true).open(path)?;
        let size = file.metadata()?.len().min(self.cap);
        sys::fs::truncate(&file, size)?;
        file.rewind()?;
        let mut hash = Sha256::new();
        let mut buf = vec![0; 256 << 10];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => hash.update(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let id = BlobId(hash.finalize().into());
        self.store.seal(&file, path, id)?;
        Ok(blob(id, size, media_type, origin))
    }

    fn discard(self: Box<Self>, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }
}

impl BlobStore for Files {
    fn writer(&self, cap: u64) -> io::Result<Box<dyn BlobWriter>> {
        let (file, path) = self.0.staging()?;
        Ok(Box::new(Writing {
            store: Arc::clone(&self.0),
            file,
            path,
            hash: Sha256::new(),
            size: 0,
            cap,
            sealed: false,
        }))
    }

    fn stream(&self, name: &str, cap: u64) -> io::Result<Stream> {
        let at = ext::blob::TreePath::new(name)
            .filter(|at| at.parts().next().is_some())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a stream name"))?;
        let path = at.under(&self.0.dir.join(OPEN));
        let dir = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
        self.0
            .private(dir.strip_prefix(&self.0.dir).map_err(io::Error::other)?)?;
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        let ext = path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()));
        for n in 0..100 {
            let path = match n {
                0 => path.clone(),
                n => path.with_file_name(format!("{stem}-{n}{}", ext.as_deref().unwrap_or(""))),
            };
            match sys::fs::private_file().open(&path) {
                Ok(file) => {
                    let sealer = Sealing {
                        store: Arc::clone(&self.0),
                        cap,
                    };
                    return Ok(Stream {
                        file,
                        open: Open::new(path, Box::new(sealer)),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} and its alternatives exist", path.display()),
        ))
    }

    fn open(&self, id: &BlobId) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(File::open(self.0.path(id))?))
    }

    fn materialize(&self, id: &BlobId) -> io::Result<PathBuf> {
        let path = self.0.path(id);
        fs::metadata(&path)?;
        Ok(path)
    }

    fn sweep(&self, live: &dyn RefIndex) -> io::Result<u64> {
        let mut freed = 0;
        if !self.0.dir.exists() {
            return Ok(freed);
        }
        let mut drop_file = |path: &Path| -> io::Result<()> {
            let size = fs::symlink_metadata(path)?.len();
            fs::remove_file(path)?;
            freed += size;
            Ok(())
        };
        for fan in fs::read_dir(&self.0.dir)? {
            let fan = fan?;
            let name = fan.file_name();
            let leftover = name == STAGING || name == OPEN;
            if !fan.file_type()?.is_dir() {
                continue;
            }
            let mut pending = vec![fan.path()];
            while let Some(dir) = pending.pop() {
                for item in fs::read_dir(&dir)? {
                    let item = item?;
                    if item.file_type()?.is_dir() {
                        pending.push(item.path());
                        continue;
                    }
                    let id = item.file_name().to_string_lossy().parse::<BlobId>().ok();
                    if leftover || id.is_none_or(|id| !live.live(&id)) {
                        drop_file(&item.path())?;
                    }
                }
            }
        }
        Ok(freed)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn store() -> (tempfile::TempDir, Files) {
        let home = tempfile::tempdir().unwrap();
        let files = Files::open(home.path().join("blobs"), home.path()).unwrap();
        (home, files)
    }

    fn put(files: &Files, bytes: &[u8]) -> BlobRef {
        let mut writer = files.writer(1 << 20).unwrap();
        writer.write_all(bytes).unwrap();
        writer.finish("text/plain", BlobOrigin::Paste).unwrap()
    }

    fn read(files: &Files, id: &BlobId) -> Vec<u8> {
        let mut bytes = Vec::new();
        files.open(id).unwrap().read_to_end(&mut bytes).unwrap();
        bytes
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir).map_or_default(|items| {
            items
                .map(|i| i.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        });
        names.sort();
        names
    }

    #[test]
    fn the_same_bytes_from_anywhere_are_one_blob_named_by_their_hash() {
        let (_home, files) = store();
        let first = put(&files, b"hello");
        let again = put(&files, b"hello");
        assert_eq!(first.id, again.id);
        assert_eq!(
            first.id.to_string(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(
            (first.size, read(&files, &first.id)),
            (5, b"hello".to_vec())
        );
        let path = files.materialize(&first.id).unwrap();
        assert_eq!(names(path.parent().unwrap()), [first.id.to_string()]);
        assert!(fs::metadata(&path).unwrap().permissions().readonly());
        assert_eq!(names(&files.0.dir.join(STAGING)), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn blobs_are_their_owners_alone() {
        use std::os::unix::fs::PermissionsExt;
        let (_home, files) = store();
        let blob = put(&files, b"secret");
        let path = files.materialize(&blob.id).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&files.0.dir), 0o700);
        assert_eq!(mode(&path), 0o400);
    }

    #[test]
    fn a_store_that_holds_no_blob_makes_no_directory() {
        let (home, files) = store();
        assert!(!home.path().join("blobs").exists());
        assert_eq!(files.sweep(&HashSet::new()).unwrap(), 0);
        assert!(files.open(&BlobId([0; 32])).is_err());
    }

    #[test]
    fn a_writer_refuses_bytes_past_its_cap_and_leaves_nothing_unsealed() {
        let (_home, files) = store();
        let mut writer = files.writer(4).unwrap();
        writer.write_all(b"1234").unwrap();
        let err = writer.write_all(b"5").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
        drop(writer);
        assert_eq!(names(&files.0.dir), [STAGING]);
        assert_eq!(names(&files.0.dir.join(STAGING)), Vec::<String>::new());
    }

    #[test]
    fn a_stream_is_read_while_it_grows_and_seals_into_the_same_blob() {
        let (_home, files) = store();
        let Stream { mut file, open } = files.stream("s/7.log", 1 << 20).unwrap();
        let taken = files.stream("s/7.log", 1 << 20).unwrap();
        assert_eq!(taken.open.path().file_name().unwrap(), "7-1.log");
        taken.open.discard().unwrap();
        file.write_all(b"hel").unwrap();
        assert_eq!(fs::read(open.path()).unwrap(), b"hel");
        file.write_all(b"lo").unwrap();
        drop(file);
        let path = open.path().to_owned();
        let sealed = open
            .seal(
                "text/plain",
                BlobOrigin::ToolOutput(protocol::CallId::new(7).unwrap()),
            )
            .unwrap();
        assert!(!path.exists());
        assert_eq!(sealed.id, put(&files, b"hello").id);
        assert_eq!(sealed.size, 5);
    }

    #[test]
    fn a_stream_seals_cut_at_its_cap() {
        let (_home, files) = store();
        let Stream { mut file, open } = files.stream("s/1.log", 3).unwrap();
        file.write_all(b"hello").unwrap();
        let sealed = open.seal("text/plain", BlobOrigin::Paste).unwrap();
        assert_eq!(read(&files, &sealed.id), b"hel");
    }

    #[test]
    fn a_stream_name_never_leaves_the_store() {
        let (_home, files) = store();
        for bad in ["", "../x", "/x", "a/../../x"] {
            assert!(files.stream(bad, 1).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_sweep_keeps_what_a_log_references_and_drops_what_a_crash_left() {
        let (_home, files) = store();
        let kept = put(&files, b"kept");
        let dropped = put(&files, b"dropped");
        let Stream { file, open } = files.stream("s/1.log", 1 << 20).unwrap();
        drop((file, open));
        fs::write(files.0.dir.join(STAGING).join("torn"), b"torn").unwrap();
        let live: HashSet<_> = [kept.id].into();
        assert_eq!(files.sweep(&live).unwrap(), 7 + 4);
        assert_eq!(read(&files, &kept.id), b"kept");
        assert!(files.open(&dropped.id).is_err());
        assert_eq!(names(&files.0.dir.join(STAGING)), Vec::<String>::new());
    }
}
