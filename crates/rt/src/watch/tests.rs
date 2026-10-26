use super::*;
use std::fs;
use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

/// A directory of its own, canonical, so that nothing else changes in
/// the parent the hub watches above a root.
struct Dir {
    _dir: tempfile::TempDir,
    base: PathBuf,
}

impl Dir {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap().join("base");
        fs::create_dir(&base).unwrap();
        Self { _dir: dir, base }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.base.join(relative)
    }

    fn write(&self, relative: &str) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, relative).unwrap();
        path
    }
}

fn hub() -> Hub {
    Hub::new(Arc::new(crate::Threads::new(1)))
}

fn subscribe(hub: &Hub, watch: Watch) -> (Subscription, Receiver<Event>) {
    let (send, events) = channel();
    let subscription = hub.subscribe(watch, move |batch| {
        for event in batch {
            let _ = send.send(event);
        }
    });
    (subscription, events)
}

/// Waits for an event `wanted` picks, failing with what came instead.
fn until(events: &Receiver<Event>, what: &str, mut wanted: impl FnMut(&Event) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let event = events
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("no {what}; saw {seen:?}"));
        if wanted(&event) {
            return;
        }
        seen.push(event);
    }
}

fn changed(path: &Path) -> impl FnMut(&Event) -> bool {
    move |event| matches!(event, Event::Changed(paths) if paths.iter().any(|p| p == path))
}

/// Nothing arrives for a while.
fn quiet(events: &Receiver<Event>) {
    if let Ok(event) = events.recv_timeout(Duration::from_millis(500)) {
        panic!("unexpected {event:?}");
    }
}

#[test]
fn a_directory_made_after_the_walk_reports_its_files() {
    let dir = Dir::new();
    let hub = hub();
    let (_sub, events) = subscribe(&hub, Watch::tree(&dir.base));
    // Made at once, before the hub can watch the new directories: what a
    // subscriber looks at as it hears of them shows the files.
    let deep = dir.write("new/inner/deeper/file");
    let mut known = HashSet::new();
    until(&events, "the deep file", |event| {
        let paths = match event {
            Event::Changed(paths) => paths.clone(),
            _ => Vec::new(),
        };
        for path in paths {
            let mut stack = vec![path];
            while let Some(path) = stack.pop() {
                if let Ok(entries) = fs::read_dir(&path) {
                    stack.extend(entries.flatten().map(|e| e.path()));
                }
                known.insert(path);
            }
        }
        known.contains(&deep)
    });
    // And the new directories are watched from then on.
    let later = dir.write("new/inner/deeper/later");
    until(&events, "the later file", changed(&later));
}

#[test]
fn sibling_changes_do_not_wake_a_root_subscription() {
    let dir = Dir::new();
    fs::create_dir_all(dir.path("outer/inner")).unwrap();
    fs::create_dir_all(dir.path("outer/sibling")).unwrap();
    let hub = hub();
    let (_outer, outer) = subscribe(&hub, Watch::tree(dir.path("outer")));
    let (_inner, inner) = subscribe(&hub, Watch::tree(dir.path("outer/inner")));
    let sibling = dir.write("outer/sibling/file");
    until(&outer, "the sibling's change", changed(&sibling));
    quiet(&inner);
    let own = dir.write("outer/inner/file");
    until(&inner, "its own change", changed(&own));
}

#[test]
fn overlapping_roots_share_one_native_watch() {
    let dir = Dir::new();
    fs::create_dir_all(dir.path("outer/inner")).unwrap();
    let hub = hub();
    let (_outer, _) = subscribe(&hub, Watch::tree(dir.path("outer")));
    let alone = hub.watches();
    let (_inner, inner) = subscribe(&hub, Watch::tree(dir.path("outer/inner")));
    assert_eq!(hub.watches(), alone);
    let file = dir.write("outer/inner/file");
    until(&inner, "a change through the shared watch", changed(&file));
}

#[test]
fn last_unsubscribe_closes_the_native_watch() {
    let dir = Dir::new();
    fs::create_dir_all(dir.path("tree/nested")).unwrap();
    let hub = hub();
    assert_eq!(hub.watches(), 0);
    let first = subscribe(&hub, Watch::tree(dir.path("tree")));
    let second = subscribe(&hub, Watch::entered(dir.path("tree/nested")));
    drop(first);
    assert_ne!(hub.watches(), 0);
    drop(second);
    assert_eq!(hub.watches(), 0);
}

#[test]
fn excluded_paths_never_wake_the_subscriber() {
    let dir = Dir::new();
    fs::create_dir(dir.path(".git")).unwrap();
    let hub = hub();
    let (_sub, events) = subscribe(&hub, Watch::tree(&dir.base).excluding(".git"));
    dir.write(".git/index");
    let file = dir.write("file");
    until(&events, "the file outside", |event| {
        if let Event::Changed(paths) = event {
            assert!(
                paths.iter().all(|p| !p.starts_with(dir.path(".git"))),
                "{paths:?}"
            );
        }
        changed(&file)(event)
    });
}

#[test]
fn only_entered_directories_are_reported() {
    let dir = Dir::new();
    fs::create_dir_all(dir.path("entered")).unwrap();
    fs::create_dir_all(dir.path("ignored")).unwrap();
    let hub = hub();
    let (sub, events) = subscribe(&hub, Watch::entered(&dir.base));
    sub.enter(&dir.path("entered"));
    dir.write("ignored/file");
    let file = dir.write("entered/file");
    until(&events, "the entered directory's file", |event| {
        if let Event::Changed(paths) = event {
            assert!(
                // The directory itself is one of the root's entries.
                (paths.iter()).all(|p| p.parent() != Some(&dir.path("ignored"))),
                "{paths:?}"
            );
        }
        changed(&file)(event)
    });
    let top = dir.write("top");
    until(&events, "the root's own entry", changed(&top));
}

#[test]
fn a_root_made_replaced_and_removed_is_followed() {
    let dir = Dir::new();
    let root = dir.path("missing/tree");
    let hub = hub();
    let (_sub, events) = subscribe(&hub, Watch::tree(&root));
    fs::create_dir_all(root.join("nested")).unwrap();
    until(&events, "its arrival", |e| matches!(e, Event::Rescan));
    let first = dir.write("missing/tree/nested/first");
    until(&events, "a change in it", changed(&first));

    fs::rename(&root, dir.path("retired")).unwrap();
    fs::create_dir_all(root.join("nested")).unwrap();
    until(&events, "its replacement", |e| matches!(e, Event::Rescan));
    let again = dir.write("missing/tree/nested/again");
    until(&events, "a change in the replacement", changed(&again));

    fs::remove_dir_all(&root).unwrap();
    until(&events, "its removal", |e| {
        matches!(e, Event::Unavailable(Unavailable::Gone))
    });
}

#[cfg(unix)]
#[test]
fn a_linked_root_hears_changes_in_its_own_spelling() {
    let dir = Dir::new();
    fs::create_dir(dir.path("real")).unwrap();
    std::os::unix::fs::symlink(dir.path("real"), dir.path("alias")).unwrap();
    let hub = hub();
    let (_sub, events) = subscribe(&hub, Watch::tree(dir.path("alias")));
    dir.write("real/file");
    until(
        &events,
        "the change, through the link",
        changed(&dir.path("alias/file")),
    );
}

#[test]
fn a_differently_cased_root_hears_changes_in_its_own_spelling() {
    let dir = Dir::new();
    fs::create_dir(dir.path("Work")).unwrap();
    if !dir.path("work").exists() {
        return;
    }
    let hub = hub();
    let (_sub, events) = subscribe(&hub, Watch::tree(dir.path("work")));
    dir.write("Work/file");
    until(
        &events,
        "the change, in the root's case",
        changed(&dir.path("work/file")),
    );
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn an_exhausted_watch_limit_does_not_rewalk_on_a_timer() {
    let dir = Dir::new();
    for name in ["a", "b", "c", "d"] {
        fs::create_dir_all(dir.path("tree").join(name)).unwrap();
    }
    let hub = hub();
    // The parent, the root, and two of its four directories.
    lock(&hub.0.state).native.budget = Some(4);
    let (_sub, events) = subscribe(&hub, Watch::tree(dir.path("tree")));
    until(&events, "the limit", |e| {
        matches!(e, Event::Unavailable(Unavailable::Limit))
    });
    let walks = lock(&hub.0.state).native.walks;
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(lock(&hub.0.state).native.walks, walks);
    // Removing directories frees their watches: the rest fits now.
    for name in ["a", "b", "c"] {
        fs::remove_dir(dir.path("tree").join(name)).unwrap();
    }
    until(&events, "the walk once watches were freed", |e| {
        matches!(e, Event::Rescan)
    });
    let file = dir.write("tree/d/file");
    until(&events, "a change in the last directory", changed(&file));
}

#[test]
fn a_busy_sink_hears_what_changed_meanwhile_in_its_next_batch() {
    let dir = Dir::new();
    let hub = hub();
    let (send, events) = channel();
    let (release, released) = channel::<()>();
    let released = Mutex::new(released);
    let _sub = hub.subscribe(Watch::tree(&dir.base), move |batch| {
        let _ = send.send(batch);
        let _ = released.lock().unwrap().recv();
    });
    let first = dir.write("first");
    let batch = events.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(batch.iter().any(|e| changed(&first)(e)), "{batch:?}");
    // The sink still has its first batch.
    let second = dir.write("second");
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        events.try_recv().is_err(),
        "a second batch while the first runs"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        release.send(()).unwrap();
        let batch = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if batch.iter().any(|e| changed(&second)(e)) {
            break;
        }
    }
    drop(release);
}

#[test]
fn a_directory_removed_and_made_again_is_watched_again() {
    let dir = Dir::new();
    fs::create_dir(dir.path("entered")).unwrap();
    let hub = hub();
    let (sub, events) = subscribe(&hub, Watch::entered(&dir.base));
    sub.enter(&dir.path("entered"));
    let first = dir.write("entered/first");
    until(&events, "a change in it", changed(&first));
    fs::remove_dir_all(dir.path("entered")).unwrap();
    until(&events, "its removal", changed(&dir.path("entered")));
    // The subscriber hears of it made again and enters it, as the index
    // does, whether or not the OS has said yet that its watch went.
    fs::create_dir(dir.path("entered")).unwrap();
    until(&events, "its return", changed(&dir.path("entered")));
    sub.enter(&dir.path("entered"));
    let again = dir.write("entered/again");
    until(&events, "a change in it made again", changed(&again));
}

#[test]
fn a_missing_root_inside_another_tree_outlives_it() {
    let dir = Dir::new();
    fs::create_dir(dir.path("outer")).unwrap();
    let hub = hub();
    let outer = subscribe(&hub, Watch::tree(dir.path("outer")));
    let (_inner, inner) = subscribe(&hub, Watch::tree(dir.path("outer/missing/tree")));
    drop(outer);
    fs::create_dir_all(dir.path("outer/missing/tree")).unwrap();
    until(&inner, "its arrival", |e| matches!(e, Event::Rescan));
    let file = dir.write("outer/missing/tree/file");
    until(&inner, "a change in it", changed(&file));
}

#[test]
fn a_tree_hears_directories_moved_in_and_not_those_moved_out() {
    let dir = Dir::new();
    fs::create_dir(dir.path("tree")).unwrap();
    fs::create_dir(dir.path("outside")).unwrap();
    dir.write("staged/inner/file");
    let hub = hub();
    let (_sub, events) = subscribe(&hub, Watch::tree(dir.path("tree")));
    fs::rename(dir.path("staged"), dir.path("tree/moved")).unwrap();
    until(
        &events,
        "the directory moved in",
        changed(&dir.path("tree/moved")),
    );
    let inside = dir.write("tree/moved/inner/later");
    until(&events, "a change in what moved in", changed(&inside));
    fs::rename(dir.path("tree/moved"), dir.path("outside/moved")).unwrap();
    until(
        &events,
        "the directory moved out",
        changed(&dir.path("tree/moved")),
    );
    dir.write("outside/moved/inner/after");
    let own = dir.write("tree/own");
    until(&events, "a change still in the tree", |event| {
        if let Event::Changed(paths) = event {
            assert!(
                paths.iter().all(|p| p.starts_with(dir.path("tree"))),
                "{paths:?}"
            );
        }
        changed(&own)(event)
    });
}

#[cfg(unix)]
#[test]
fn a_directory_held_open_while_made_again_is_watched_again() {
    let dir = Dir::new();
    let entered = dir.path("entered");
    fs::create_dir(&entered).unwrap();
    let hub = hub();
    let (sub, events) = subscribe(&hub, Watch::entered(&dir.base));
    sub.enter(&entered);
    // Held open, its removal is not the end of its watch, which the OS
    // drops only once it is closed: after the directory is made again.
    let held = fs::File::open(&entered).unwrap();
    fs::remove_dir(&entered).unwrap();
    until(&events, "its removal", changed(&entered));
    fs::create_dir(&entered).unwrap();
    until(&events, "its return", changed(&entered));
    sub.enter(&entered);
    let first = dir.write("entered/first");
    until(&events, "a change in it made again", changed(&first));
    drop(held);
    let second = dir.write("entered/second");
    until(
        &events,
        "a change after the old watch went",
        changed(&second),
    );
}

#[test]
fn a_new_outer_subscriber_hears_nothing_from_before_it_subscribed() {
    let dir = Dir::new();
    fs::create_dir_all(dir.path("outer/inner")).unwrap();
    fs::create_dir_all(dir.path("outer/sibling")).unwrap();
    let hub = hub();
    let (_inner, inner) = subscribe(&hub, Watch::tree(dir.path("outer/inner")));
    for i in 0..200 {
        dir.write(&format!("outer/sibling/{i}"));
    }
    let own = dir.write("outer/inner/before");
    until(&inner, "the inner root's change", changed(&own));
    std::thread::sleep(Duration::from_millis(500));
    let (_outer, outer) = subscribe(&hub, Watch::tree(dir.path("outer")));
    // Windows may report the taken-over root itself as its handle closes;
    // nothing written before is told again.
    let before = |path: &PathBuf| path.starts_with(dir.path("outer/sibling")) || *path == own;
    while let Ok(event) = outer.recv_timeout(Duration::from_millis(500)) {
        let Event::Changed(paths) = &event else {
            panic!("unexpected {event:?}");
        };
        assert!(!paths.iter().any(before), "{paths:?}");
    }
    let after = dir.write("outer/sibling/after");
    until(&outer, "a change after subscribing", changed(&after));
    let later = dir.write("outer/inner/later");
    until(
        &inner,
        "the inner root's change through the outer stream",
        changed(&later),
    );
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn freed_capacity_rearms_an_entered_root_before_reporting_recovery() {
    for budget in [0, 1] {
        let dir = Dir::new();
        let hub = hub();
        lock(&hub.0.state).native.budget = Some(budget);
        let (_sub, events) = subscribe(&hub, Watch::entered(&dir.base));
        until(&events, "the root admission limit", |e| {
            matches!(e, Event::Unavailable(Unavailable::Limit))
        });
        hub.0.handle(|state| {
            state.native.budget = Some(2);
            vec![Raw::Freed]
        });
        until(&events, "root recovery", |e| matches!(e, Event::Rescan));
        let file = dir.write("after");
        until(&events, "the recovered root's entries", changed(&file));
        fs::remove_dir_all(&dir.base).unwrap();
        until(&events, "the recovered parent's removal report", |e| {
            matches!(e, Event::Unavailable(Unavailable::Gone))
        });
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn unsubscribing_restores_capacity_for_limited_subscriptions() {
    let dir = Dir::new();
    fs::create_dir(dir.path("first")).unwrap();
    fs::create_dir(dir.path("second")).unwrap();
    let hub = hub();
    lock(&hub.0.state).native.budget = Some(2);
    let first = subscribe(&hub, Watch::entered(dir.path("first")));
    let (_second, events) = subscribe(&hub, Watch::entered(dir.path("second")));
    until(&events, "the limit", |e| {
        matches!(e, Event::Unavailable(Unavailable::Limit))
    });
    drop(first);
    until(&events, "recovery after unsubscribe", |e| {
        matches!(e, Event::Rescan)
    });
    let file = dir.write("second/after");
    until(&events, "the recovered root's entries", changed(&file));
}

#[cfg(windows)]
#[test]
fn a_replaced_parent_is_watched_by_new_subscriptions() {
    let dir = Dir::new();
    fs::create_dir(dir.path("parent")).unwrap();
    let hub = hub();
    let (old, _) = subscribe(&hub, Watch::entered(dir.path("parent/old")));
    fs::rename(dir.path("parent"), dir.path("retired")).unwrap();
    fs::create_dir(dir.path("parent")).unwrap();
    let (_new, events) = subscribe(&hub, Watch::entered(dir.path("parent/new")));
    // Releasing an old generation must not close the new parent's handle.
    drop(old);
    fs::create_dir(dir.path("parent/new")).unwrap();
    until(&events, "arrival under the replacement parent", |e| {
        matches!(e, Event::Rescan)
    });
    let file = dir.write("parent/new/file");
    until(&events, "the new root's entries", changed(&file));
}

#[cfg(windows)]
#[test]
fn a_replaced_parent_outlives_the_subscription_that_replaced_it() {
    let dir = Dir::new();
    fs::create_dir(dir.path("parent")).unwrap();
    let hub = hub();
    let (_old, events) = subscribe(&hub, Watch::entered(dir.path("parent/old")));
    fs::rename(dir.path("parent"), dir.path("retired")).unwrap();
    fs::create_dir(dir.path("parent")).unwrap();
    let (new, _) = subscribe(&hub, Watch::entered(dir.path("parent/new")));
    drop(new);
    fs::create_dir(dir.path("parent/old")).unwrap();
    until(&events, "arrival for the surviving subscription", |e| {
        matches!(e, Event::Rescan)
    });
    let file = dir.write("parent/old/file");
    until(&events, "the surviving root's entries", changed(&file));
}
