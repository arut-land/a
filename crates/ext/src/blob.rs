//! Blobs: bytes too large for a log line, owned by the session store and
//! named by their content. A result holds a [`BlobRef`], never a path; a
//! path is a materialized copy that can always be made again.
//!
//! Three seams, each behind a trait so a backend plugs in without the
//! tools or the store changing:
//!
//! - [`BlobStore`]: where the bytes live, one namespace per daemon. The
//!   store that keeps the logs keeps the blobs, its own way.
//! - [`Materialize`]: how a blob appears to the OS as an ordinary file
//!   under one root, for `cat`, `rg` and file managers. A cache directory
//!   now; a virtual file system (Cloud Files, File Provider, FUSE) later.
//! - [`Namespace`]: what that tree holds, derived from the logs. Its shape
//!   only, for the virtual backends.
//!
//! Blob bytes are as private as the log that references them, and are
//! kept as they were written: environment redaction never applies to them.

use std::collections::{BTreeSet, HashSet};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

pub use protocol::{BlobId, BlobOrigin, BlobRef, Payload};
use protocol::{CallId, OutputStream, SessionId};

use crate::BoxFuture;

/// A new blob, hashed as it is written and refused past its cap.
pub trait BlobWriter: Write + Send {
    /// Seals what was written. Once this returns, the blob is ordered
    /// before anything appended to a log after it, and durable no later
    /// than that log's next flush: an entry can never outlive the bytes it
    /// references.
    fn finish(self: Box<Self>, media_type: &str, origin: BlobOrigin) -> io::Result<BlobRef>;
}

/// How a backend seals or drops an open stream.
pub trait Sealer: Send {
    /// As [`BlobWriter::finish`], for the bytes at `path`, cut at the
    /// stream's cap.
    fn seal(
        self: Box<Self>,
        path: &Path,
        media_type: &str,
        origin: BlobOrigin,
    ) -> io::Result<BlobRef>;

    fn discard(self: Box<Self>, path: &Path) -> io::Result<()>;
}

/// A growing blob: what a running call writes, readable at its path while
/// it grows, and sealed into a content-addressed blob when the call ends.
pub struct Stream {
    /// Where the writer (a process) writes. Readers open the path.
    pub file: File,
    pub open: Open,
}

/// An open stream, apart from its writer. Dropped, it stays as it is: a
/// background job's output goes on growing after its call returns.
pub struct Open {
    path: PathBuf,
    sealer: Box<dyn Sealer>,
    /// Where [`Blobs::expose`] shows it in the tree, if it does.
    shown: Option<PathBuf>,
}

impl Open {
    /// For a backend: the stream at `path`, which `sealer` ends.
    pub fn new(path: PathBuf, sealer: Box<dyn Sealer>) -> Self {
        Self {
            path,
            sealer,
            shown: None,
        }
    }

    /// Where the stream can be read while it grows. Not the blob's path:
    /// once sealed, it is [`Blobs::materialize`]'s.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn seal(self, media_type: &str, origin: BlobOrigin) -> io::Result<BlobRef> {
        self.sealer.seal(&self.path, media_type, origin)
    }

    /// Ends the stream without keeping it, nor showing it.
    pub fn discard(self) -> io::Result<()> {
        if let Some(shown) = &self.shown {
            let _ = std::fs::remove_file(shown);
        }
        self.sealer.discard(&self.path)
    }
}

/// What a blob store's sweep keeps: the blobs some log references, as an
/// index derived from the logs (rebuilt, never counted in place).
pub trait RefIndex {
    fn live(&self, id: &BlobId) -> bool;
}

impl<S: std::hash::BuildHasher> RefIndex for HashSet<BlobId, S> {
    fn live(&self, id: &BlobId) -> bool {
        self.contains(id)
    }
}

impl RefIndex for BTreeSet<BlobId> {
    fn live(&self, id: &BlobId) -> bool {
        self.contains(id)
    }
}

/// Where blobs live: beside the logs that reference them, each backend
/// its own way (hash-named files, a table). Calls block: run them on a
/// storage worker.
pub trait BlobStore: Send + Sync {
    /// A new blob of at most `cap` bytes.
    fn writer(&self, cap: u64) -> io::Result<Box<dyn BlobWriter>>;

    /// A new open stream of at most `cap` bytes, named for where it is
    /// read while it grows: `name` is relative, and a taken name gets a
    /// number added.
    fn stream(&self, name: &str, cap: u64) -> io::Result<Stream>;

    fn open(&self, id: &BlobId) -> io::Result<Box<dyn Read + Send>>;

    /// The blob as a read-only file a [`Materialize`] backend can share
    /// (clone or link) rather than copy.
    fn materialize(&self, id: &BlobId) -> io::Result<PathBuf>;

    /// Removes, at rest, every sealed blob `live` does not hold, and every
    /// stream left open by a daemon that stopped; returns the bytes freed.
    fn sweep(&self, live: &dyn RefIndex) -> io::Result<u64>;
}

/// A place in the tree blobs appear in: relative, `/`-separated, the same
/// on every backend and stable across restarts.
///
/// `by-id/<id>` is a blob whatever references it, so a fork or a child
/// shows the same path its parent did. Later, by origin:
/// `sessions/<id>/attachments/<name>`, `sessions/<id>/tool-output/<call>.log`,
/// `sessions/<id>/pastes/<seq>.txt`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TreePath(String);

impl TreePath {
    /// The root of the tree.
    pub const ROOT: Self = Self(String::new());

    /// `path`, if each of its parts is a plain name.
    pub fn new(path: &str) -> Option<Self> {
        let plain = |part: &str| {
            !part.is_empty() && part != "." && part != ".." && !part.contains(['\\', ':', '\0'])
        };
        (path.is_empty() || path.split('/').all(plain)).then(|| Self(path.to_owned()))
    }

    pub fn by_id(id: &BlobId) -> Self {
        Self(format!("by-id/{id}"))
    }

    /// Where call `call`'s output stream is read, while it grows and once
    /// it is sealed: what [`OutputStream`] names.
    pub fn stream(session: SessionId, stream: OutputStream) -> Self {
        Self(format!(
            "sessions/{session}/streams/{}.log",
            stream.call.get()
        ))
    }

    pub fn parts(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|part| !part.is_empty())
    }

    /// Where it is under `root`, in the OS's own form.
    pub fn under(&self, root: &Path) -> PathBuf {
        self.parts()
            .fold(root.to_path_buf(), |path, part| path.join(part))
    }
}

/// How the tree appears to the OS. Read-only on every backend: a blob
/// never changes, and a write never reaches the store.
pub trait Materialize: Send + Sync {
    fn root(&self) -> &Path;

    /// Where `at` is: deterministic, so a path the model was told stays
    /// right across restarts.
    fn path(&self, at: &TreePath) -> PathBuf {
        at.under(self.root())
    }

    /// Makes `at` readable now, as `blob`: before its path is shown to the
    /// model or a client. A cache writes or clones the file; a virtual
    /// backend makes sure its entry exists. Blocks: run it on a storage
    /// worker.
    fn ensure(&self, at: &TreePath, blob: &BlobRef) -> io::Result<()>;

    /// Shows the open stream at `open` at `at` as it grows, and as the
    /// blob it is sealed into: the same file, never a copy of what it
    /// held so far.
    fn expose(&self, at: &TreePath, open: &Path) -> io::Result<()>;

    /// Frees what it holds beyond `budget` bytes, least recently used
    /// first; returns the bytes freed. A cache deletes, a virtual backend
    /// dehydrates, or leaves it to the OS.
    fn evict(&self, budget: u64) -> io::Result<u64>;
}

/// What the tree holds, derived from the logs and never stored: what a
/// virtual [`Materialize`] backend answers `ls` with, hydrating a file
/// through [`BlobStore::open`] when it is read.
pub trait Namespace: Send + Sync {
    fn list(&self, dir: &TreePath) -> io::Result<Vec<TreeEntry>>;

    fn lookup(&self, path: &TreePath) -> io::Result<Option<TreeEntry>>;

    /// Resolves with the next path that appears, as sessions append.
    fn changes(&self) -> BoxFuture<'_, TreePath>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: String,
    pub kind: TreeKind,
    pub mtime: SystemTime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeKind {
    Dir,
    File(BlobRef),
}

/// A session's blobs, as its calls write and show them.
#[derive(Clone)]
pub struct Blobs {
    store: Arc<dyn BlobStore>,
    tree: Arc<dyn Materialize>,
    session: SessionId,
}

impl Blobs {
    pub fn new(store: Arc<dyn BlobStore>, tree: Arc<dyn Materialize>, session: SessionId) -> Self {
        Self {
            store,
            tree,
            session,
        }
    }

    pub fn writer(&self, cap: u64) -> io::Result<Box<dyn BlobWriter>> {
        self.store.writer(cap)
    }

    /// An open stream for call `call`'s output, its name ending `.ext`.
    pub fn stream(&self, call: CallId, ext: &str, cap: u64) -> io::Result<Stream> {
        self.store
            .stream(&format!("{}/{}.{ext}", self.session, call.get()), cap)
    }

    pub fn open(&self, id: &BlobId) -> io::Result<Box<dyn Read + Send>> {
        self.store.open(id)
    }

    /// A path `blob` can be read at, which exists when this returns.
    pub fn materialize(&self, blob: &BlobRef) -> io::Result<PathBuf> {
        let at = TreePath::by_id(&blob.id);
        self.tree.ensure(&at, blob)?;
        Ok(self.tree.path(&at))
    }

    /// Shows `open`, call `call`'s output stream, where [`Places::stream`]
    /// names it, which exists when this returns.
    pub fn expose(&self, open: &mut Open, call: CallId) -> io::Result<PathBuf> {
        let at = TreePath::stream(self.session, OutputStream { call });
        self.tree.expose(&at, open.path())?;
        let shown = self.tree.path(&at);
        open.shown = Some(shown.clone());
        Ok(shown)
    }

    /// How the session's references are named for the model.
    pub fn places(&self) -> Places {
        Places::new(self.tree.root().to_path_buf(), self.session)
    }
}

/// How a reference reads where the model is told of it: the path the
/// tree shows it at. Pure, so a projection of the log can name it; the
/// default names the place within the tree, for a projection that has
/// no tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Places {
    root: Option<PathBuf>,
    session: Option<SessionId>,
}

impl Places {
    /// Session `session`'s references, in the tree at `root`.
    pub const fn new(root: PathBuf, session: SessionId) -> Self {
        Self {
            root: Some(root),
            session: Some(session),
        }
    }

    fn at(&self, at: &TreePath) -> String {
        match &self.root {
            Some(root) => at.under(root).display().to_string(),
            None => at.parts().collect::<Vec<_>>().join("/"),
        }
    }

    pub fn blob(&self, blob: &BlobRef) -> String {
        self.at(&TreePath::by_id(&blob.id))
    }

    /// Where `stream` is read; without a session, by its call alone.
    pub fn stream(&self, stream: OutputStream) -> String {
        match self.session {
            Some(session) => self.at(&TreePath::stream(session, stream)),
            None => stream.to_string(),
        }
    }
}

/// Blobs for a test whose calls write none: every use fails.
#[cfg(test)]
pub(crate) fn none() -> Blobs {
    struct None;
    fn refused<T>() -> io::Result<T> {
        Err(io::Error::other("no blob store"))
    }
    impl BlobStore for None {
        fn writer(&self, _: u64) -> io::Result<Box<dyn BlobWriter>> {
            refused()
        }
        fn stream(&self, _: &str, _: u64) -> io::Result<Stream> {
            refused()
        }
        fn open(&self, _: &BlobId) -> io::Result<Box<dyn Read + Send>> {
            refused()
        }
        fn materialize(&self, _: &BlobId) -> io::Result<PathBuf> {
            refused()
        }
        fn sweep(&self, _: &dyn RefIndex) -> io::Result<u64> {
            refused()
        }
    }
    impl Materialize for None {
        fn root(&self) -> &Path {
            Path::new("")
        }
        fn ensure(&self, _: &TreePath, _: &BlobRef) -> io::Result<()> {
            refused()
        }
        fn expose(&self, _: &TreePath, _: &Path) -> io::Result<()> {
            refused()
        }
        fn evict(&self, _: u64) -> io::Result<u64> {
            refused()
        }
    }
    Blobs::new(Arc::new(None), Arc::new(None), SessionId::new())
}
