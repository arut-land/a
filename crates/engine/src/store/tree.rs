//! Blobs as ordinary files in a cache directory, the materialize backend
//! every OS has. A file shares the store's storage where the file system
//! can (an APFS clone, a reflink, a hard link) and is copied where it
//! cannot; either way it is read-only, and everything here can be deleted
//! and made again from the store.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use ext::blob::{BlobRef, BlobStore, Materialize, TreePath};

pub struct CacheDir {
    root: PathBuf,
    store: Arc<dyn BlobStore>,
}

impl CacheDir {
    pub fn new(root: impl Into<PathBuf>, store: Arc<dyn BlobStore>) -> Self {
        Self {
            root: root.into(),
            store,
        }
    }
}

impl Materialize for CacheDir {
    fn root(&self) -> &Path {
        &self.root
    }

    fn ensure(&self, at: &TreePath, blob: &BlobRef) -> io::Result<()> {
        let path = self.path(at);
        match fs::metadata(&path) {
            Ok(file) if file.is_file() && file.len() == blob.size => return Ok(()),
            // Torn or not this blob: made again.
            Ok(_) => fs::remove_file(&path)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let dir = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
        sys::fs::private_dir(dir)?;
        let from = self.store.materialize(&blob.id)?;
        match sys::fs::clone_file(&from, &path) {
            // Another call made it first, from the same blob.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            made => made,
        }
    }

    fn evict(&self, budget: u64) -> io::Result<u64> {
        let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
        let mut pending = vec![self.root.clone()];
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

#[cfg(test)]
mod tests {
    use std::io::Write;

    use ext::blob::{BlobOrigin, Blobs};
    use protocol::SessionId;

    use super::*;
    use crate::store::Files;

    fn cache() -> (tempfile::TempDir, Arc<Files>, CacheDir) {
        let home = tempfile::tempdir().unwrap();
        let files = Arc::new(Files::open(home.path().join("blobs"), home.path()).unwrap());
        let tree = CacheDir::new(home.path().join("cache"), Arc::clone(&files) as _);
        (home, files, tree)
    }

    fn put(files: &Files, bytes: &[u8]) -> BlobRef {
        let mut writer = files.writer(1 << 20).unwrap();
        writer.write_all(bytes).unwrap();
        writer.finish("text/plain", BlobOrigin::Paste).unwrap()
    }

    #[test]
    fn a_materialized_blob_is_a_read_only_file_at_a_path_fixed_by_its_id() {
        let (home, files, tree) = cache();
        let blob = put(&files, b"whole output");
        let blobs = Blobs::new(Arc::clone(&files) as _, Arc::new(tree), SessionId::new());
        let path = blobs.materialize(&blob).unwrap();
        assert_eq!(
            path,
            home.path()
                .join("cache")
                .join("by-id")
                .join(blob.id.to_string())
        );
        assert_eq!(fs::read(&path).unwrap(), b"whole output");
        assert!(fs::metadata(&path).unwrap().permissions().readonly());
        // Another session, a fork, the same path.
        let other = Blobs::new(
            Arc::clone(&files) as _,
            Arc::new(CacheDir::new(home.path().join("cache"), files)),
            SessionId::new(),
        );
        assert_eq!(other.materialize(&blob).unwrap(), path);
    }

    #[test]
    fn the_cache_is_made_again_after_it_is_deleted_or_evicted() {
        let (home, files, tree) = cache();
        let blob = put(&files, b"0123456789");
        let at = TreePath::by_id(&blob.id);
        tree.ensure(&at, &blob).unwrap();
        fs::remove_dir_all(home.path().join("cache")).unwrap();
        tree.ensure(&at, &blob).unwrap();
        assert_eq!(fs::read(tree.path(&at)).unwrap(), b"0123456789");
        assert_eq!(tree.evict(10).unwrap(), 0);
        assert_eq!(tree.evict(0).unwrap(), 10);
        assert!(!tree.path(&at).exists());
        assert!(
            files.open(&blob.id).is_ok(),
            "eviction never reaches the store"
        );
        tree.ensure(&at, &blob).unwrap();
        assert_eq!(fs::read(tree.path(&at)).unwrap(), b"0123456789");
    }
}
