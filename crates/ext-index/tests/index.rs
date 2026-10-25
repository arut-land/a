//! The index over real directories, on the real runtime and watcher.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use ext::Cancel;
use ext_index::{
    GrepOptions, GrepQuery, GrepSummary, Grepped, Indexes, Limits, Pattern, Stopped, WorkspaceIndex,
};

fn runtime() -> rt::Rt {
    Arc::new(rt::Threads::new(2))
}

/// The index of `root`, kept current by a watcher on `rt`.
fn open(indexes: &Indexes, root: &Path, rt: rt::Rt) -> Arc<WorkspaceIndex> {
    let hub = Arc::new(rt::watch::Hub::new(Arc::clone(&rt)));
    indexes.open(root, rt, hub)
}

/// A workspace: a git working tree, its root canonical as the daemon
/// resolves one. It is a directory of its own temporary one: the watcher
/// watches the root's parent too, which other tests' temporary
/// directories would keep changing.
struct Tree {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Tree {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("ws");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        Self { _dir: dir, root }
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

fn search(index: &WorkspaceIndex, query: &str) -> Vec<String> {
    let found = rt::block_on(index.search(query.to_owned(), 50)).unwrap();
    found.files.into_iter().map(|f| f.path).collect()
}

fn grep_with(index: &WorkspaceIndex, query: &str, cancel: &Cancel) -> (Vec<Grepped>, GrepSummary) {
    let mut found = Vec::new();
    let query = GrepQuery {
        pattern: Pattern::Typed(query.to_owned()),
        options: GrepOptions::default(),
        limit: usize::MAX,
    };
    let summary = rt::block_on(index.grep(query, cancel, |g| found.push(g))).unwrap();
    (found, summary)
}

fn grep(index: &WorkspaceIndex, query: &str) -> (Vec<String>, GrepSummary) {
    let (found, summary) = grep_with(index, query, &Cancel::default());
    let mut paths: Vec<_> = found.into_iter().map(|g| g.path).collect();
    paths.sort();
    (paths, summary)
}

/// Retries `check` until it holds, the watcher's events being
/// asynchronous.
fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_at_mention_search_finds_a_new_file_after_it_is_created() {
    let tree = Tree::new();
    tree.write("src/main.rs", "fn main() {}");
    tree.write("README.md", "hello");
    let indexes = Indexes::new(Limits::default());
    let index = open(&indexes, tree.root(), runtime());
    assert_eq!(search(&index, "main")[0], "src/main.rs");
    assert!(!search(&index, "widget").contains(&"src/ui/widget.rs".to_owned()));

    tree.write("src/ui/widget.rs", "pub struct Widget;");
    eventually("the new file is found", || {
        search(&index, "widget").first().map(String::as_str) == Some("src/ui/widget.rs")
    });

    std::fs::remove_file(tree.root().join("src/ui/widget.rs")).unwrap();
    eventually("the removed file is gone", || {
        !search(&index, "widget").contains(&"src/ui/widget.rs".to_owned())
    });
}

#[test]
fn gitignored_files_never_appear() {
    let tree = Tree::new();
    tree.write(".gitignore", "build/\n*.log\n");
    tree.write("kept.rs", "needle");
    tree.write("build/out.rs", "needle");
    tree.write("trace.log", "needle");
    let indexes = Indexes::new(Limits::default());
    let index = open(&indexes, tree.root(), runtime());
    let listed = search(&index, "");
    assert!(listed.contains(&"kept.rs".to_owned()), "{listed:?}");
    assert!(
        !listed.iter().any(
            |p| p.starts_with("build/") || Path::new(p).extension().is_some_and(|e| e == "log")
        )
    );
    assert!(!listed.iter().any(|p| p.starts_with(".git/")));
    assert_eq!(grep(&index, "needle").0, ["kept.rs"]);

    // Nor after the index is open: created later, still ignored.
    tree.write("build/later.rs", "needle");
    tree.write("later.log", "needle");
    tree.write("later.rs", "needle");
    eventually("the kept file is found", || {
        search(&index, "later").contains(&"later.rs".to_owned())
    });
    let listed = search(&index, "");
    assert!(
        !listed.iter().any(
            |p| p.starts_with("build/") || Path::new(p).extension().is_some_and(|e| e == "log")
        ),
        "{listed:?}"
    );
    assert_eq!(grep(&index, "needle").0, ["kept.rs", "later.rs"]);

    // A rule added later hides what it names.
    tree.write(".gitignore", "build/\n*.log\nlater.rs\n");
    eventually("the newly ignored file is gone", || {
        !search(&index, "later").contains(&"later.rs".to_owned())
    });
}

#[test]
fn grep_reads_only_what_the_content_index_lets_through_and_stays_current() {
    let tree = Tree::new();
    for i in 0..300 {
        tree.write(&format!("src/file{i}.rs"), &format!("fn item_{i}() {{}}\n"));
    }
    tree.write("src/target.rs", "let unicorn = 1;\n");
    let indexes = Indexes::new(Limits::default());
    let index = open(&indexes, tree.root(), runtime());
    let (found, summary) = grep(&index, "unicorn");
    assert_eq!(found, ["src/target.rs"]);
    assert!(summary.prefiltered);
    assert!(summary.searched < 10, "{summary:?}");

    // A walked file edited after the content index was built.
    tree.write("src/file7.rs", "fn unicorn_too() {}\n");
    eventually("the edit is searched", || {
        grep(&index, "unicorn").0 == ["src/file7.rs", "src/target.rs"]
    });
    // A file new since the walk is searched too.
    tree.write("src/fresh.rs", "unicorn\n");
    eventually("the new file is searched", || {
        grep(&index, "unicorn")
            .0
            .contains(&"src/fresh.rs".to_owned())
    });
    // And one removed is not.
    std::fs::remove_file(tree.root().join("src/target.rs")).unwrap();
    eventually("the removed file is not searched", || {
        !grep(&index, "unicorn")
            .0
            .contains(&"src/target.rs".to_owned())
    });
}

#[test]
fn above_its_memory_a_workspace_greps_every_file_without_a_content_index() {
    let tree = Tree::new();
    tree.write("a.rs", "alpha\n");
    tree.write("b.rs", "beta\n");
    let indexes = Indexes::new(Limits {
        memory: 1,
        ..Limits::default()
    });
    let index = open(&indexes, tree.root(), runtime());
    let (found, summary) = grep(&index, "beta");
    assert_eq!(found, ["b.rs"]);
    assert!(!summary.prefiltered);
    assert_eq!(summary.searched, 2);
}

#[test]
fn a_grep_is_cancelled_mid_stream() {
    let tree = Tree::new();
    for i in 0..4000 {
        tree.write(&format!("f/{i}.txt"), "needle\n");
    }
    let indexes = Indexes::new(Limits::default());
    let index = open(&indexes, tree.root(), runtime());
    let cancel = Cancel::default();
    let seen = AtomicUsize::new(0);
    let query = GrepQuery {
        pattern: Pattern::Typed("needle".into()),
        options: GrepOptions::default(),
        limit: usize::MAX,
    };
    let summary = rt::block_on(index.grep(query, &cancel, |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        cancel.set();
    }))
    .unwrap();
    assert_eq!(summary.stopped, Some(Stopped::Cancelled));
    assert!(summary.searched < 4000, "{summary:?}");
    assert!(seen.load(Ordering::SeqCst) < 4000);
}

#[test]
fn the_index_is_gone_after_its_last_user() {
    let tree = Tree::new();
    tree.write("a.rs", "");
    let linger = Duration::from_millis(200);
    let indexes = Indexes::new(Limits {
        linger,
        ..Limits::default()
    });
    let rt = runtime();
    let index = open(&indexes, tree.root(), Arc::clone(&rt));
    assert_eq!(search(&index, "a.rs"), ["a.rs"]);
    let weak: Weak<WorkspaceIndex> = Arc::downgrade(&index);
    drop(index);
    // Kept for the linger after its last use, not longer.
    assert!(weak.upgrade().is_some());
    eventually("the index closes", || weak.upgrade().is_none());
    // Opened again, it walks again.
    let index = open(&indexes, tree.root(), rt);
    assert_eq!(search(&index, "a.rs"), ["a.rs"]);
}

#[test]
fn an_open_index_nothing_changes_or_asks_never_wakes() {
    let tree = Tree::new();
    tree.write("a.rs", "alpha\n");
    let indexes = Indexes::new(Limits::default());
    let index = open(&indexes, tree.root(), runtime());
    assert_eq!(grep(&index, "alpha").0, ["a.rs"]);
    // The writes above may still be arriving as events: let them settle.
    std::thread::sleep(Duration::from_millis(1500));
    let mut seen = Vec::new();
    for _ in 0..6 {
        let before = rt::wakes();
        std::thread::sleep(Duration::from_millis(500));
        seen.push(rt::wakes() - before);
    }
    assert!(seen.iter().all(|w| *w == 0), "{seen:?}");
    drop(index);
}

#[test]
fn a_rescan_during_the_first_content_build_never_installs_a_stale_index() {
    let tree = Tree::new();
    // Many files, so the build is still running as the rules change; the
    // first ones sort before the rest and their removal moves every slot.
    for i in 0..3000 {
        tree.write(&format!("aaa/{i}.txt"), "filler\n");
    }
    for i in 0..3000 {
        let text = if i % 10 == 0 { "needle\n" } else { "filler\n" };
        tree.write(&format!("zzz/{i}.txt"), text);
    }
    let indexes = Indexes::new(Limits::default());
    let index = open(&indexes, tree.root(), runtime());
    let expected: Vec<String> = (0..3000)
        .filter(|i| i % 10 == 0)
        .map(|i| format!("zzz/{i}.txt"))
        .collect();
    let mut expected = expected;
    expected.sort();
    std::thread::scope(|s| {
        s.spawn(|| grep(&index, "needle"));
        std::thread::sleep(Duration::from_millis(5));
        tree.write(".gitignore", "aaa/\n");
    });
    eventually("the ignored tree is gone", || {
        !search(&index, "").iter().any(|p| p.starts_with("aaa/"))
    });
    eventually("the content index matches the files", || {
        grep(&index, "needle").0 == expected
    });
}

#[test]
fn a_flood_of_changes_in_an_ignored_tree_keeps_the_index_current() {
    let tree = Tree::new();
    tree.write(".gitignore", "build/\n");
    tree.write("src/a.rs", "");
    let indexes = Indexes::new(Limits::default());
    let index = open(&indexes, tree.root(), runtime());
    assert_eq!(search(&index, "a.rs")[0], "src/a.rs");
    let started = Instant::now();
    for i in 0..3000 {
        tree.write(&format!("build/out/{i}.o"), "x");
    }
    tree.write("src/fresh.rs", "");
    eventually("the kept file is found behind the flood", || {
        search(&index, "fresh").contains(&"src/fresh.rs".to_owned())
    });
    assert!(!search(&index, "").iter().any(|p| p.starts_with("build/")));
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn ignored_directories_take_no_watch() {
    let tree = Tree::new();
    tree.write(".gitignore", "target/\n");
    tree.write("src/a.rs", "");
    for i in 0..50 {
        tree.write(&format!("target/debug/deps/{i}/x.o"), "");
    }
    let rt = runtime();
    let hub = Arc::new(rt::watch::Hub::new(Arc::clone(&rt)));
    let indexes = Indexes::new(Limits::default());
    let index = indexes.open(tree.root(), rt, Arc::clone(&hub));
    assert_eq!(search(&index, "a.rs")[0], "src/a.rs");
    // The root's parent, the root, `src`, and `.git`, above the `info/`
    // that is not there: nothing under `target`.
    assert_eq!(hub.watches(), 4);
    tree.write("src/new/b.rs", "");
    eventually("a directory made later is watched", || {
        search(&index, "b.rs").contains(&"src/new/b.rs".to_owned())
    });
    assert_eq!(hub.watches(), 5);
}

#[test]
fn a_home_or_a_volume_root_is_refused_without_a_walk() {
    let home = std::env::home_dir().unwrap().canonicalize().unwrap();
    let temp = std::env::temp_dir().canonicalize().unwrap();
    let volume = temp.ancestors().last().unwrap().to_owned();
    let indexes = Indexes::new(Limits::default());
    for root in [home, volume] {
        let index = open(&indexes, &root, runtime());
        let began = Instant::now();
        let refused = rt::block_on(index.search("a".to_owned(), 10)).unwrap_err();
        assert!(refused.contains("never indexed"), "{refused}");
        assert!(rt::block_on(index.glob("*".to_owned(), 10)).is_err());
        let query = GrepQuery {
            pattern: Pattern::Typed("needle".to_owned()),
            options: GrepOptions::default(),
            limit: 10,
        };
        assert!(rt::block_on(index.grep(query, &Cancel::default(), |_| {})).is_err());
        // A walk of either would take far longer than asking what it is.
        assert!(began.elapsed() < Duration::from_secs(2), "{root:?}");
    }
}

#[test]
fn a_placeholder_is_listed_but_its_content_never_read() {
    let tree = Tree::new();
    tree.write("local.txt", "needle");
    tree.write("cloud.txt", "needle");
    if !sys::fs::mark_placeholder(&tree.root().join("cloud.txt")).unwrap() {
        // No program may make one here (macOS, Linux): `sys` tests the
        // classification.
        return;
    }
    let indexes = Indexes::new(Limits::default());
    let index = open(&indexes, tree.root(), runtime());
    let listed = rt::block_on(index.glob("*.txt".to_owned(), 10)).unwrap();
    let mut listed: Vec<_> = listed.files.into_iter().map(|f| f.path).collect();
    listed.sort();
    assert_eq!(listed, ["cloud.txt", "local.txt"]);
    // Once as the content index is built, once through it.
    assert_eq!(grep(&index, "needle").0, ["local.txt"]);
    assert_eq!(grep(&index, "needle").0, ["local.txt"]);
}

/// Indexes kept in `store` that come to rest `linger` after their last use.
fn kept(store: &Path, linger: Duration) -> Indexes {
    Indexes::new(Limits {
        linger,
        ..Limits::default()
    })
    .kept_in(store)
}

fn rested(index: &WorkspaceIndex) {
    eventually("the index comes to rest", || index.resident() == 0);
}

#[test]
fn an_index_reopened_after_edits_made_while_it_rested_sees_them() {
    let tree = Tree::new();
    let store = tempfile::tempdir().unwrap();
    tree.write("src/kept.rs", "fn kept() {}\n");
    tree.write("src/edited.rs", "fn before() {}\n");
    tree.write("src/gone.rs", "fn gone() { unicorn }\n");
    let rt = runtime();
    let hub = Arc::new(rt::watch::Hub::new(Arc::clone(&rt)));
    let indexes = kept(store.path(), Duration::from_millis(200));
    let index = indexes.open(tree.root(), Arc::clone(&rt), Arc::clone(&hub));
    assert_eq!(grep(&index, "unicorn").0, ["src/gone.rs"]);
    rested(&index);

    // The same length, so only its time (or the second it was taken in)
    // tells the edit.
    tree.write("src/edited.rs", "fn unicorn() {}\n");
    tree.write("src/fresh/new.rs", "unicorn\n");
    std::fs::remove_file(tree.root().join("src/gone.rs")).unwrap();
    let index = indexes.open(tree.root(), rt, hub);
    eventually("what changed at rest is seen", || {
        grep(&index, "unicorn").0 == ["src/edited.rs", "src/fresh/new.rs"]
    });
    assert!(grep(&index, "unicorn").1.prefiltered);
    assert!(!search(&index, "").contains(&"src/gone.rs".to_owned()));
    assert!(search(&index, "").contains(&"src/kept.rs".to_owned()));
}

#[test]
fn an_index_another_daemon_kept_is_checked_against_the_tree_before_it_is_trusted() {
    let tree = Tree::new();
    let store = tempfile::tempdir().unwrap();
    for i in 0..200 {
        tree.write(&format!("src/f{i}.rs"), &format!("fn f{i}() {{}}\n"));
    }
    tree.write("src/edited.rs", "fn before() {}\n");
    tree.write("src/gone.rs", "unicorn\n");
    {
        let indexes = kept(store.path(), Duration::from_millis(200));
        let index = open(&indexes, tree.root(), runtime());
        assert_eq!(grep(&index, "unicorn").0, ["src/gone.rs"]);
        rested(&index);
    }
    // While no daemon runs: nothing watches.
    tree.write("src/edited.rs", "fn unicorn() {}\n");
    tree.write("src/new.rs", "unicorn\n");
    std::fs::remove_file(tree.root().join("src/gone.rs")).unwrap();
    tree.write(".gitignore", "src/f1*.rs\n");

    let indexes = kept(store.path(), Duration::from_secs(60));
    let index = open(&indexes, tree.root(), runtime());
    eventually("what changed meanwhile is seen", || {
        grep(&index, "unicorn").0 == ["src/edited.rs", "src/new.rs"]
    });
    let (_, summary) = grep(&index, "unicorn");
    assert!(summary.prefiltered);
    // The content index came from disk: only what changed is read.
    assert!(summary.searched <= 3, "{summary:?}");
    let listed = search(&index, "");
    assert!(
        !listed.iter().any(|p| p.starts_with("src/f1")),
        "{listed:?}"
    );
    assert!(listed.contains(&"src/f2.rs".to_owned()));
}

#[test]
fn a_damaged_or_older_index_on_disk_is_rebuilt_never_trusted() {
    let tree = Tree::new();
    let store = tempfile::tempdir().unwrap();
    tree.write("alpha.rs", "needle\n");
    tree.write("beta.rs", "other\n");
    {
        let indexes = kept(store.path(), Duration::from_millis(200));
        let index = open(&indexes, tree.root(), runtime());
        assert_eq!(grep(&index, "needle").0, ["alpha.rs"]);
        rested(&index);
    }
    let kept_files: Vec<PathBuf> = std::fs::read_dir(store.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(kept_files.len(), 2, "{kept_files:?}");
    let damages: [(&str, fn(&mut Vec<u8>)); 4] = [
        // A path that is not there, with its checksum as it was.
        ("a renamed path", |bytes| {
            let at = bytes.windows(5).position(|w| w == b"alpha").unwrap();
            bytes[at..at + 5].copy_from_slice(b"ghost");
        }),
        ("a torn write", |bytes| bytes.truncate(bytes.len() / 2)),
        ("another format", |bytes| {
            bytes[8] = bytes[8].wrapping_add(1)
        }),
        ("noise", |bytes| {
            bytes.iter_mut().for_each(|b| *b = b.wrapping_mul(31))
        }),
    ];
    for (damage, apply) in damages {
        for path in &kept_files {
            let mut bytes = std::fs::read(path).unwrap();
            if path.extension().is_some_and(|e| e == "files") || damage != "a renamed path" {
                apply(&mut bytes);
            }
            std::fs::write(path, bytes).unwrap();
        }
        tree.write("gamma.rs", "needle\n");
        let indexes = kept(store.path(), Duration::from_millis(200));
        let index = open(&indexes, tree.root(), runtime());
        assert_eq!(search(&index, "ghost"), Vec::<String>::new(), "{damage}");
        assert_eq!(
            grep(&index, "needle").0,
            ["alpha.rs", "gamma.rs"],
            "{damage}"
        );
        std::fs::remove_file(tree.root().join("gamma.rs")).unwrap();
        // Written again whole, for the next damage.
        rested(&index);
    }
}

#[test]
fn idle_memory_drops_once_the_index_is_kept_on_disk() {
    let tree = Tree::new();
    let store = tempfile::tempdir().unwrap();
    for i in 0..500 {
        tree.write(&format!("src/f{i}.rs"), &format!("fn f{i}() {{}}\n"));
    }
    let indexes = kept(store.path(), Duration::from_millis(300));
    let index = open(&indexes, tree.root(), runtime());
    assert_eq!(grep(&index, "fn f42()").0, ["src/f42.rs"]);
    assert!(index.resident() > 0);
    rested(&index);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(store.path()), 0o700);
        for entry in std::fs::read_dir(store.path()).unwrap() {
            assert_eq!(mode(&entry.unwrap().path()), 0o600);
        }
    }
    // At rest, it costs no wake-ups either.
    std::thread::sleep(Duration::from_millis(500));
    let before = rt::wakes();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(rt::wakes() - before, 0);
    // Used again, it is back.
    assert_eq!(grep(&index, "fn f7()").0, ["src/f7.rs"]);
    assert!(index.resident() > 0);
}

#[test]
fn a_query_while_the_index_is_got_ready_waits_for_it_and_is_answered() {
    let tree = Tree::new();
    for i in 0..3000 {
        tree.write(&format!("d{}/{i}.txt", i % 30), "filler\n");
    }
    tree.write("d7/target.txt", "unicorn\n");
    let rt = runtime();
    let hub = Arc::new(rt::watch::Hub::new(Arc::clone(&rt)));
    let indexes = Indexes::new(Limits::default());
    indexes.warm(tree.root(), Arc::clone(&rt), Arc::clone(&hub));
    let index = indexes.open(tree.root(), rt, hub);
    assert_eq!(search(&index, "target")[0], "d7/target.txt");
    let (found, summary) = grep(&index, "unicorn");
    assert_eq!(found, ["d7/target.txt"]);
    assert!(summary.prefiltered);
}
