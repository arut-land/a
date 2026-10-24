//! Blobs: bytes too large for a log line, owned by the session store and
//! named by their content. An entry holds a [`BlobRef`] (and says so in
//! its `held` field), never a path; a path is named from the reference
//! when the model is told of it, and is a copy that can be made again.
//!
//! Three seams, each behind a trait so a backend plugs in without the
//! tools or the store changing:
//!
//! - [`BlobStore`]: where the bytes live, one namespace per daemon. The
//!   store that keeps the logs keeps the blobs, its own way: `blob_files`'s
//!   `Files` beside JSONL logs.
//! - [`Materialize`]: how a blob appears to the OS as an ordinary file
//!   under one root, for `cat`, `rg` and file managers: `blob_files`'s
//!   `CacheDir` now; a virtual file system (Cloud Files, File Provider,
//!   FUSE) later.
//!
//! This crate does no I/O: the SDK and the kernel build on it, and the
//! backends implement it.
//! - [`Namespace`]: what that tree holds, derived from the logs. Its shape
//!   only, for the virtual backends.
//!
//! A call's output stream ([`OutputStream`]) grows in the store, where a
//! reader follows it, and is sealed into a blob once nothing writes it.
//!
//! Blob bytes are as private as the log that references them, and are
//! kept as they were written: environment redaction never applies to them.
//!
//! Deferred: making a named path exist before a model call shows it.
//! Today a call ensures the blob it seals (`Blobs::materialize`); a paste,
//! an attachment or a detached call's stream is named but not ensured
//! until the projection calls [`Materialize::ensure`] before each request.

use std::fs::File;
use std::future::Future;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

pub use protocol::{BlobId, BlobOrigin, BlobRef, Held, OutputStream, Payload};
use protocol::{CallId, SessionId};

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
    /// stream's cap. Only once nothing writes the stream any more.
    fn seal(
        self: Box<Self>,
        path: &Path,
        media_type: &str,
        origin: BlobOrigin,
    ) -> io::Result<BlobRef>;

    fn discard(self: Box<Self>, path: &Path) -> io::Result<()>;
}

/// A growing blob: what a running call writes, readable at its path while
/// it grows, and sealed into a content-addressed blob once it is done.
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
}

impl Open {
    /// For a backend: the stream at `path`, which `sealer` ends.
    pub fn new(path: PathBuf, sealer: Box<dyn Sealer>) -> Self {
        Self { path, sealer }
    }

    /// Where the stream is read while it grows: where [`Places::stream`]
    /// names it.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Seals the stream into a blob. Its writers must all be gone: one
    /// still writing would change a blob named by what it held.
    pub fn seal(self, media_type: &str, origin: BlobOrigin) -> io::Result<BlobRef> {
        self.sealer.seal(&self.path, media_type, origin)
    }

    /// Ends the stream without keeping it.
    pub fn discard(self) -> io::Result<()> {
        self.sealer.discard(&self.path)
    }
}

/// Where blobs live: beside the logs that reference them, each backend
/// its own way (hash-named files, a table). Calls block: run them on a
/// storage worker.
pub trait BlobStore: Send + Sync {
    /// A new blob of at most `cap` bytes.
    fn writer(&self, cap: u64) -> io::Result<Box<dyn BlobWriter>>;

    /// `of`'s stream, new and empty, of at most `cap` bytes: a call run
    /// again starts its stream over.
    fn stream(&self, of: OutputStream, cap: u64) -> io::Result<Stream>;

    /// The directory streams grow in, each where [`stream_path`] puts it.
    fn streams(&self) -> &Path;

    fn open(&self, id: &BlobId) -> io::Result<Box<dyn Read + Send>>;

    /// The blob as a read-only file a [`Materialize`] backend can share
    /// (clone or link) rather than copy.
    fn materialize(&self, id: &BlobId) -> io::Result<PathBuf>;

    /// Removes, at rest, every sealed blob `live` does not hold, and what a
    /// writer left half done; returns the bytes freed. Streams are kept:
    /// a log may name one still.
    fn sweep(&self, live: &dyn Fn(&BlobId) -> bool) -> io::Result<u64>;
}

/// A place in the tree blobs appear in: relative, `/`-separated, the same
/// on every backend and stable across restarts.
///
/// `by-id/<id>` is a blob whatever references it, so a fork or a child
/// shows the same path its parent did. Later, by origin:
/// `sessions/<id>/attachments/<name>`, `sessions/<id>/pastes/<seq>.txt`.
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

    pub fn parts(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|part| !part.is_empty())
    }

    /// Where it is under `root`, in the OS's own form.
    pub fn under(&self, root: &Path) -> PathBuf {
        self.parts()
            .fold(root.to_path_buf(), |path, part| path.join(part))
    }
}

/// How the tree appears to the OS. Read-only to the store on every backend
/// a blob is copied or cloned into; where the cache falls back to a hard
/// link, the file is the store's own, and a write that clears its
/// read-only bit reaches the blob.
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
    fn changes(&self) -> Pin<Box<dyn Future<Output = TreePath> + Send + '_>>;
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

    /// Call `call`'s output stream, which it names, new and empty.
    pub fn stream(&self, call: CallId, cap: u64) -> io::Result<Stream> {
        let of = OutputStream {
            session: self.session,
            call,
        };
        self.store.stream(of, cap)
    }

    pub fn open(&self, id: &BlobId) -> io::Result<Box<dyn Read + Send>> {
        self.store.open(id)
    }

    /// Makes `blob` readable where [`Places::blob`] names it.
    pub fn materialize(&self, blob: &BlobRef) -> io::Result<()> {
        self.tree.ensure(&TreePath::by_id(&blob.id), blob)
    }

    /// How references are named for the model.
    pub fn places(&self) -> Places {
        Places::new(self.tree.root(), self.store.streams())
    }
}

/// How a reference reads where the model is told of it: the path it is
/// at. Pure, named from what the log records, so a projection of the log
/// names it, whoever's session projects it; the default names a blob by
/// its place within the tree and a stream by its call, for a projection
/// that has no store.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Places {
    roots: Option<(PathBuf, PathBuf)>,
}

impl Places {
    /// The tree at `tree`, and streams under `streams`.
    pub fn new(tree: &Path, streams: &Path) -> Self {
        Self {
            roots: Some((tree.to_path_buf(), streams.to_path_buf())),
        }
    }

    pub fn blob(&self, blob: &BlobRef) -> String {
        let at = TreePath::by_id(&blob.id);
        match &self.roots {
            Some((tree, _)) => at.under(tree).display().to_string(),
            None => at.parts().collect::<Vec<_>>().join("/"),
        }
    }

    pub fn stream(&self, stream: OutputStream) -> String {
        match &self.roots {
            Some((_, streams)) => stream_path(streams, stream).display().to_string(),
            None => stream.to_string(),
        }
    }
}

/// Where `of` grows under `streams`: the layout every [`BlobStore`] keeps
/// its streams in, so [`Places`] names them without one.
pub fn stream_path(streams: &Path, of: OutputStream) -> PathBuf {
    streams
        .join(of.session.to_string())
        .join(format!("{}.log", of.call.get()))
}
