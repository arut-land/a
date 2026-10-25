//! Blobs as ordinary files in a cache directory, the materialize backend
//! every OS has. A file shares the store's storage where the file system
//! can (an APFS clone, a reflink) and is copied where it cannot; either
//! way it is a read-only file of its own, appears whole, and everything
//! here can be deleted and made again from the store. Copies are made in a
//! staging directory beside the tree, so the tree holds only whole files.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::SystemTime;

use blob::{BlobRef, BlobStore, Materialize, TreePath};

pub struct CacheDir {
    tree: PathBuf,
    staging: PathBuf,
    swept: Once,
    store: Arc<dyn BlobStore>,
}

impl CacheDir {
    /// The cache in `dir`: the tree in `dir/tree`, copies being made in
    /// `dir/staging`, both on one file system. One `CacheDir` owns `dir`.
    pub fn new(dir: impl AsRef<Path>, store: Arc<dyn BlobStore>) -> Self {
        let dir = dir.as_ref();
        Self {
            tree: dir.join("tree"),
            staging: dir.join("staging"),
            swept: Once::new(),
            store,
        }
    }

    /// Emptied before this owner's first copy, when none of its own can be
    /// in flight: what staging holds then a crash left. A leftover that
    /// cannot be removed is outside the tree, and the next owner tries again.
    fn staging(&self) -> io::Result<&Path> {
        self.swept.call_once(|| {
            let _ = fs::remove_dir_all(&self.staging);
        });
        sys::fs::private_dir(&self.staging)?;
        Ok(&self.staging)
    }
}

impl Materialize for CacheDir {
    fn root(&self) -> &Path {
        &self.tree
    }

    fn ensure(&self, at: &TreePath, blob: &BlobRef) -> io::Result<()> {
        let path = self.path(at);
        match fs::metadata(&path) {
            Ok(file) if file.is_file() && file.len() == blob.size => return Ok(()),
            // Another blob at this place: made again.
            Ok(_) => fs::remove_file(&path)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let dir = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
        sys::fs::private_dir(dir)?;
        let from = self.store.materialize(&blob.id)?;
        match sys::fs::clone_file(&from, self.staging()?, &path) {
            // Another call made it first, from the same blob.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            made => made,
        }
    }

    fn evict(&self, budget: u64) -> io::Result<u64> {
        let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
        let mut pending = vec![self.tree.clone()];
        while let Some(dir) = pending.pop() {
            let items = match fs::read_dir(&dir) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                items => items?,
            };
            for item in items {
                let item = item?;
                let meta = item.metadata()?;
                if meta.is_dir() {
                    pending.push(item.path());
                } else {
                    let used = meta.accessed().or_else(|_| meta.modified())?;
                    files.push((used, meta.len(), item.path()));
                }
            }
        }
        let mut held: u64 = files.iter().map(|(_, size, _)| size).sum();
        files.sort_unstable();
        let mut freed = 0;
        for (_, size, path) in files {
            if held <= budget {
                break;
            }
            fs::remove_file(&path)?;
            held -= size;
            freed += size;
        }
        Ok(freed)
    }
}
