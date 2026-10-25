//! The JSONL store's blobs: one read-only file per blob at `<aa>/<id>`,
//! named by the SHA-256 of its bytes, one namespace for every session.
//!
//! A writer hashes into `staging/`; a stream grows in
//! `open/<session>/<call>.log`. Sealing orders the file's bytes, makes it
//! read-only and links it under its id, ordering that name too. Only a
//! name under `<aa>/` is a blob; what `staging/` holds after a crash is
//! never trusted, and a sweep at rest removes it.
//!
//! Ordering, not a flush of its own, is what makes a blob durable no later
//! than the entry that references it: the entry is appended after the
//! seal returns, so the shared flush that makes the entry durable covers
//! the blob's barriers too, as it covers many logs'. A blob on a device
//! that flush does not reach is flushed at once.

use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use blob::{
    BlobId, BlobOrigin, BlobRef, BlobStore, BlobWriter, Open, OutputStream, Sealer, Stream,
    stream_path,
};

const STAGING: &str = "staging";
const OPEN: &str = "open";

pub struct Files(Arc<Inner>);

struct Inner {
    dir: PathBuf,
    streams: PathBuf,
    /// The logs' flush device: a blob elsewhere is flushed as it is sealed.
    domain: sys::fs::FlushDomain,
    /// Whether this daemon has made the store's own directory, which it
    /// leaves until the first blob: a daemon that writes none never pays.
    made: AtomicBool,
    /// The fan directories whose names this daemon has made durable.
    fans: [AtomicBool; 256],
    staged: AtomicU64,
}

impl Files {
    /// Blobs in `dir`, whose parent exists, for logs flushed on the device
    /// `logs` is on.
    pub fn open(dir: impl Into<PathBuf>, logs: &Path) -> io::Result<Self> {
        let dir = dir.into();
        Ok(Self(Arc::new(Inner {
            domain: sys::fs::FlushDomain::of(logs)?,
            streams: dir.join(OPEN),
            dir,
            made: AtomicBool::new(false),
            fans: [const { AtomicBool::new(false) }; 256],
            staged: AtomicU64::new(0),
        })))
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

    fn stream(&self, of: OutputStream, cap: u64) -> io::Result<Stream> {
        let path = stream_path(&self.0.streams, of);
        self.0
            .private(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
        let file = match sys::fs::private_file().open(&path) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                fs::remove_file(&path)?;
                sys::fs::private_file().open(&path)?
            }
            opened => opened?,
        };
        let sealer = Sealing {
            store: Arc::clone(&self.0),
            cap,
        };
        Ok(Stream {
            file,
            open: Open::new(path, Box::new(sealer)),
        })
    }

    fn reopen(&self, of: OutputStream, cap: u64) -> io::Result<Stream> {
        let path = stream_path(&self.0.streams, of);
        let file = fs::OpenOptions::new().append(true).open(&path)?;
        let sealer = Sealing {
            store: Arc::clone(&self.0),
            cap,
        };
        Ok(Stream {
            file,
            open: Open::new(path, Box::new(sealer)),
        })
    }

    fn streams(&self) -> &Path {
        &self.0.streams
    }

    fn discard(&self, of: OutputStream) -> io::Result<()> {
        match fs::remove_file(stream_path(&self.0.streams, of)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            removed => removed,
        }
    }

    fn open(&self, id: &BlobId) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(File::open(self.0.path(id))?))
    }

    fn materialize(&self, id: &BlobId) -> io::Result<PathBuf> {
        let path = self.0.path(id);
        fs::metadata(&path)?;
        Ok(path)
    }

    fn sweep(&self, live: &dyn Fn(&BlobId) -> bool) -> io::Result<u64> {
        let items = match fs::read_dir(&self.0.dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            items => items?,
        };
        let mut freed = 0;
        for fan in items {
            let fan = fan?;
            if fan.file_name() == OPEN || !fan.file_type()?.is_dir() {
                continue;
            }
            let leftover = fan.file_name() == STAGING;
            for item in fs::read_dir(fan.path())? {
                let item = item?;
                let id = item.file_name().to_string_lossy().parse::<BlobId>().ok();
                if leftover || id.is_none_or(|id| !live(&id)) {
                    freed += item.metadata()?.len();
                    fs::remove_file(item.path())?;
                }
            }
        }
        Ok(freed)
    }
}

impl Inner {
    /// `dir`, a directory below the store's, made private, the store's
    /// own made durable first.
    fn private(&self, dir: &Path) -> io::Result<()> {
        if !self.made.load(Ordering::Acquire) {
            let root = &self.dir;
            sys::fs::create_dir_all_durable(root.parent().unwrap_or(root), root)?;
            sys::fs::private_dir(root)?;
            self.made.store(true, Ordering::Release);
        }
        sys::fs::private_dir(dir)
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
        let made = &self.fans[usize::from(id.0[0])];
        if !made.load(Ordering::Acquire) {
            sys::fs::create_dir_all_durable(&self.dir, &self.dir.join(id.fan()))?;
            made.store(true, Ordering::Release);
        }
        Ok(())
    }

    fn staging(&self) -> io::Result<(File, PathBuf)> {
        let dir = self.dir.join(STAGING);
        self.private(&dir)?;
        loop {
            let n = self.staged.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("{}-{n}", std::process::id()));
            match sys::fs::private_file().readable().open(&path) {
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
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!("a blob holds at most {} bytes", self.cap),
            ));
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
