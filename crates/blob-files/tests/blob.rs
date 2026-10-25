//! The store's blobs and streams, and the tree they appear in, as their
//! users see them: through the store, the cache and the names they give.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use blob::{
    BlobId, BlobOrigin, BlobRef, BlobStore, Blobs, Materialize, OutputStream, Places, Stream,
    TreePath,
};
use blob_files::{CacheDir, Files};
use protocol::{CallId, SessionId};

struct Home {
    dir: tempfile::TempDir,
    files: Arc<Files>,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let files = Arc::new(Files::open(dir.path().join("blobs"), dir.path()).unwrap());
        Self { dir, files }
    }

    fn blobs(&self) -> PathBuf {
        self.dir.path().join("blobs")
    }

    fn tree(&self) -> CacheDir {
        CacheDir::new(self.dir.path().join("cache"), Arc::clone(&self.files) as _)
    }

    fn session(&self, session: SessionId) -> Blobs {
        Blobs::new(Arc::clone(&self.files) as _, Arc::new(self.tree()), session)
    }

    fn put(&self, bytes: &[u8]) -> BlobRef {
        let mut writer = self.files.writer(1 << 20).unwrap();
        writer.write_all(bytes).unwrap();
        writer.finish("text/plain", BlobOrigin::Paste).unwrap()
    }

    fn read(&self, id: &BlobId) -> Vec<u8> {
        let mut bytes = Vec::new();
        self.files
            .open(id)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
    }
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

fn call(n: u64) -> CallId {
    CallId::new(n).unwrap()
}

#[test]
fn the_same_bytes_from_anywhere_are_one_blob_named_by_their_hash() {
    let home = Home::new();
    let first = home.put(b"hello");
    let again = home.put(b"hello");
    assert_eq!(first.id, again.id);
    assert_eq!(
        first.id.to_string(),
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
    assert_eq!((first.size, home.read(&first.id)), (5, b"hello".to_vec()));
    let path = home.files.materialize(&first.id).unwrap();
    assert_eq!(names(path.parent().unwrap()), [first.id.to_string()]);
    assert!(fs::metadata(&path).unwrap().permissions().readonly());
    assert_eq!(names(&home.blobs().join("staging")), Vec::<String>::new());
}

#[cfg(unix)]
#[test]
fn blobs_are_their_owners_alone() {
    use std::os::unix::fs::PermissionsExt;
    let home = Home::new();
    let blob = home.put(b"secret");
    let path = home.files.materialize(&blob.id).unwrap();
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&home.blobs()), 0o700);
    assert_eq!(mode(&path), 0o400);
}

#[test]
fn a_store_that_holds_no_blob_makes_no_directory() {
    let home = Home::new();
    assert!(!home.blobs().exists());
    assert_eq!(home.files.sweep(&|_| false).unwrap(), 0);
    assert!(home.files.open(&BlobId([0; 32])).is_err());
}

#[test]
fn a_writer_refuses_bytes_past_its_cap_and_leaves_nothing_unsealed() {
    let home = Home::new();
    let mut writer = home.files.writer(4).unwrap();
    writer.write_all(b"1234").unwrap();
    let err = writer.write_all(b"5").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::FileTooLarge);
    drop(writer);
    assert_eq!(names(&home.blobs()), ["staging"]);
    assert_eq!(names(&home.blobs().join("staging")), Vec::<String>::new());
}

#[test]
fn a_stream_is_read_where_it_is_named_while_it_grows_and_seals_into_the_same_blob() {
    let home = Home::new();
    let session = SessionId::new();
    let blobs = home.session(session);
    let Stream { mut file, open } = blobs.stream(call(7), 1 << 20).unwrap();
    assert_eq!(
        Some(open.path().display().to_string()),
        blobs.places().stream(OutputStream {
            session,
            call: call(7)
        })
    );
    file.write_all(b"hel").unwrap();
    assert_eq!(fs::read(open.path()).unwrap(), b"hel");
    file.write_all(b"lo").unwrap();
    drop(file);
    let path = open.path().to_owned();
    let sealed = open
        .seal("text/plain", BlobOrigin::ToolOutput(call(7)))
        .unwrap();
    assert!(!path.exists());
    assert_eq!(sealed.id, home.put(b"hello").id);
    assert_eq!(sealed.size, 5);
}

#[test]
fn a_call_run_again_starts_its_stream_over() {
    let home = Home::new();
    let blobs = home.session(SessionId::new());
    let Stream { mut file, open } = blobs.stream(call(2), 1 << 20).unwrap();
    file.write_all(b"cut off").unwrap();
    let path = open.path().to_owned();
    let again = blobs.stream(call(2), 1 << 20).unwrap();
    assert_eq!(again.open.path(), path);
    assert_eq!(fs::read(&path).unwrap(), b"");
}

#[test]
fn a_stream_seals_cut_at_its_cap() {
    let home = Home::new();
    let blobs = home.session(SessionId::new());
    let Stream { mut file, open } = blobs.stream(call(1), 3).unwrap();
    file.write_all(b"hello").unwrap();
    drop(file);
    let sealed = open.seal("text/plain", BlobOrigin::Paste).unwrap();
    assert_eq!(home.read(&sealed.id), b"hel");
}

#[test]
fn a_sweep_keeps_what_a_log_references_and_every_stream_and_drops_the_rest() {
    let home = Home::new();
    let kept = home.put(b"kept");
    let dropped = home.put(b"dropped");
    let blobs = home.session(SessionId::new());
    let Stream { mut file, open } = blobs.stream(call(1), 1 << 20).unwrap();
    file.write_all(b"still named").unwrap();
    fs::write(home.blobs().join("staging").join("torn"), b"torn").unwrap();
    assert_eq!(home.files.sweep(&|id| *id == kept.id).unwrap(), 7 + 4);
    assert_eq!(home.read(&kept.id), b"kept");
    assert!(home.files.open(&dropped.id).is_err());
    assert_eq!(fs::read(open.path()).unwrap(), b"still named");
    assert_eq!(names(&home.blobs().join("staging")), Vec::<String>::new());
}

#[test]
fn a_named_blob_is_a_read_only_file_where_any_session_names_it() {
    let home = Home::new();
    let blob = home.put(b"whole output");
    let blobs = home.session(SessionId::new());
    let path = PathBuf::from(blobs.places().blob(&blob).unwrap());
    assert_eq!(
        path,
        home.dir
            .path()
            .join("cache")
            .join("tree")
            .join("by-id")
            .join(blob.id.to_string())
    );
    assert_eq!(fs::read(&path).unwrap(), b"whole output");
    assert!(fs::metadata(&path).unwrap().permissions().readonly());
    let fork = home.session(SessionId::new());
    assert_eq!(fork.places().blob(&blob), blobs.places().blob(&blob));
}

#[test]
fn the_cache_is_made_again_after_it_is_deleted_or_evicted() {
    let home = Home::new();
    let tree = home.tree();
    let blob = home.put(b"0123456789");
    let at = TreePath::by_id(&blob.id);
    tree.ensure(&at, &blob).unwrap();
    fs::remove_dir_all(home.dir.path().join("cache")).unwrap();
    tree.ensure(&at, &blob).unwrap();
    assert_eq!(fs::read(tree.path(&at)).unwrap(), b"0123456789");
    assert_eq!(tree.evict(10).unwrap(), 0);
    assert_eq!(tree.evict(0).unwrap(), 10);
    assert!(!tree.path(&at).exists());
    assert!(
        home.files.open(&blob.id).is_ok(),
        "eviction never reaches the store"
    );
    tree.ensure(&at, &blob).unwrap();
    assert_eq!(fs::read(tree.path(&at)).unwrap(), b"0123456789");
}

#[test]
fn a_write_through_a_materialized_path_never_reaches_the_store() {
    let home = Home::new();
    let tree = home.tree();
    let blob = home.put(b"original");
    let at = TreePath::by_id(&blob.id);
    tree.ensure(&at, &blob).unwrap();
    let path = tree.path(&at);
    let mut writable = fs::metadata(&path).unwrap().permissions();
    #[expect(clippy::permissions_set_readonly_false, reason = "what a user may do")]
    writable.set_readonly(false);
    fs::set_permissions(&path, writable).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .write_all(b"OVERWROTE")
        .unwrap();
    assert_eq!(home.read(&blob.id), b"original");
}

#[test]
fn concurrent_ensures_of_one_blob_each_see_a_complete_file() {
    let home = Home::new();
    let bytes: Vec<u8> = (0..=u8::MAX).cycle().take(8 << 20).collect();
    let mut writer = home.files.writer(16 << 20).unwrap();
    writer.write_all(&bytes).unwrap();
    let blob = writer.finish("text/plain", BlobOrigin::Paste).unwrap();
    let at = TreePath::by_id(&blob.id);
    let start = std::sync::Barrier::new(4);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                let tree = home.tree();
                start.wait();
                tree.ensure(&at, &blob).unwrap();
                assert!(fs::read(tree.path(&at)).unwrap() == bytes);
            });
        }
    });
    let dir = home.tree().path(&at);
    assert_eq!(
        names(dir.parent().unwrap()),
        [blob.id.to_string()],
        "no copy left half made"
    );
    assert_eq!(
        names(&home.dir.path().join("cache/staging")),
        [] as [&str; 0]
    );
}

#[test]
fn eviction_during_a_materialize_never_breaks_it() {
    let home = Home::new();
    let bytes: Vec<u8> = (0..=u8::MAX).cycle().take(8 << 20).collect();
    let mut writer = home.files.writer(16 << 20).unwrap();
    writer.write_all(&bytes).unwrap();
    let blob = writer.finish("text/plain", BlobOrigin::Paste).unwrap();
    let small = home.put(b"small");
    let tree = home.tree();
    tree.ensure(&TreePath::by_id(&small.id), &small).unwrap();
    let start = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            start.wait();
            for _ in 0..64 {
                tree.evict(0).unwrap();
            }
        });
        start.wait();
        for n in 0..16 {
            let at = TreePath::new(&format!("sessions/x/{n}")).unwrap();
            tree.ensure(&at, &blob).unwrap();
        }
    });
    for n in 0..16 {
        let at = TreePath::new(&format!("sessions/x/{n}")).unwrap();
        tree.ensure(&at, &blob).unwrap();
        assert!(fs::read(tree.path(&at)).unwrap() == bytes);
    }
}

#[test]
fn the_tree_never_holds_what_a_crash_left_mid_copy() {
    let home = Home::new();
    let staging = home.dir.path().join("cache/staging");
    fs::create_dir_all(&staging).unwrap();
    // As a copy cut off by a crash, under a name a tree path could take.
    fs::write(staging.join("by-id"), b"half a cop").unwrap();
    let tree = home.tree();
    let blob = home.put(b"whole");
    let at = TreePath::by_id(&blob.id);
    tree.ensure(&at, &blob).unwrap();
    assert_eq!(fs::read(tree.path(&at)).unwrap(), b"whole");
    assert_eq!(names(&staging), [] as [&str; 0], "leftovers are swept");
    assert_eq!(names(tree.root()), ["by-id"]);
    fs::write(staging.join("left"), b"another crash").unwrap();
    assert_eq!(tree.evict(0).unwrap(), 5, "only the tree is counted");
}

#[test]
fn a_place_holding_another_blob_is_made_again() {
    let home = Home::new();
    let tree = home.tree();
    let (first, second) = (home.put(b"first"), home.put(b"the second"));
    let at = TreePath::new("sessions/x/attachments/a.txt").unwrap();
    tree.ensure(&at, &first).unwrap();
    tree.ensure(&at, &second).unwrap();
    assert_eq!(fs::read(tree.path(&at)).unwrap(), b"the second");
    assert!(
        fs::metadata(tree.path(&at))
            .unwrap()
            .permissions()
            .readonly()
    );
}

#[test]
fn a_reference_is_named_from_what_the_log_records_whoever_projects_it() {
    let blob = BlobRef {
        id: BlobId([1; 32]),
        size: 1,
        media_type: "text/plain".into(),
        origin: BlobOrigin::Paste,
    };
    let session = SessionId::new();
    let stream = OutputStream {
        session,
        call: call(4),
    };
    let bare = Places::default();
    assert_eq!(bare.blob(&blob), Some(format!("by-id/{}", blob.id)));
    assert_eq!(bare.stream(stream).as_deref(), Some("stream:4"));
    // A store names only what it can show.
    let home = Home::new();
    let placed = home.session(session).places();
    assert_eq!(placed.blob(&blob), None, "a blob the store does not have");
    assert_eq!(placed.stream(stream), None, "a stream not begun");
    let begun = home.session(session).stream(call(4), 1).unwrap();
    assert_eq!(
        placed.stream(stream),
        Some(begun.open.path().display().to_string())
    );
}

#[test]
fn a_tree_path_never_leaves_its_root() {
    for bad in [
        "..", "a/../b", "/a", "a//b", "a/", "./a", "a\\b", "c:x", "a\0",
    ] {
        assert_eq!(TreePath::new(bad), None, "{bad:?}");
    }
    let at = TreePath::new("sessions/x/attachments/a.png").unwrap();
    let root = Path::new("root");
    assert_eq!(
        at.under(root),
        root.join("sessions")
            .join("x")
            .join("attachments")
            .join("a.png")
    );
    assert_eq!(TreePath::ROOT.under(root), root);
}
