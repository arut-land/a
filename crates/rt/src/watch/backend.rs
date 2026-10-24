use super::{Changed, Spawn, WatchEvent};
#[cfg(windows)]
use std::sync::Arc;
use std::{collections::BTreeMap, io, path::PathBuf};

#[cfg(not(windows))]
pub struct Backend {
    _watcher: notify::RecommendedWatcher,
}

#[cfg(not(windows))]
impl Backend {
    pub fn new(_: &Spawn, wanted: &BTreeMap<PathBuf, bool>, changed: &Changed) -> io::Result<Self> {
        use notify::Watcher as _;
        let changed = changed.clone();
        let aliases: Vec<_> = wanted
            .keys()
            .filter_map(|path| {
                let resolved = path.canonicalize().ok()?;
                (resolved != *path).then(|| (resolved, path.clone()))
            })
            .collect();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
                Ok(event) if event.need_rescan() => changed(WatchEvent::Rescan),
                Ok(event) if structural(&event) => {
                    changed(WatchEvent::RescanPaths(event_paths(event.paths, &aliases)));
                }
                Ok(event) if !matches!(event.kind, notify::EventKind::Access(_)) => {
                    let paths = event_paths(event.paths, &aliases);
                    changed(WatchEvent::Changed(paths));
                }
                Err(error) => changed(WatchEvent::Unavailable(io::Error::other(error))),
                _ => {}
            })
            .map_err(io::Error::other)?;
        let mut paths = watcher.paths_mut();
        for (path, mode) in wanted {
            paths
                .add(
                    path,
                    if *mode {
                        notify::RecursiveMode::Recursive
                    } else {
                        notify::RecursiveMode::NonRecursive
                    },
                )
                .map_err(io::Error::other)?;
        }
        paths.commit().map_err(io::Error::other)?;
        Ok(Self { _watcher: watcher })
    }
}

#[cfg(not(windows))]
fn structural(event: &notify::Event) -> bool {
    use notify::{
        EventKind,
        event::{CreateKind, ModifyKind},
    };
    // inotify watches each directory on its own and reports a new or
    // moved-in directory before notify installs its descendant watches, so a
    // synchronous rearm must precede the capture that consumes it. Rearming
    // costs a new instance (a per-user limit) and a tree walk, so plain file
    // renames such as atomic saves must not trigger it. Removed directories
    // need nothing: notify drops their watches itself.
    const PER_DIRECTORY: bool = cfg!(any(target_os = "linux", target_os = "android"));
    PER_DIRECTORY
        && match event.kind {
            EventKind::Create(CreateKind::Folder) => true,
            EventKind::Modify(ModifyKind::Name(_)) => event.paths.iter().any(|path| path.is_dir()),
            _ => false,
        }
}

#[cfg(not(windows))]
fn event_paths(mut paths: Vec<PathBuf>, aliases: &[(PathBuf, PathBuf)]) -> Vec<PathBuf> {
    // A parent alias can cover a different canonical root. Preserve both
    // spellings so neither root loses events or ancestor invalidation.
    for index in 0..paths.len() {
        for (resolved, original) in aliases {
            if let Ok(relative) = paths[index].strip_prefix(resolved) {
                let rewritten = original.join(relative);
                paths.push(rewritten);
            }
        }
    }
    paths
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn only_new_or_moved_in_directories_require_a_rearm_before_capture() {
        use notify::event::{CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode};
        let root = std::env::temp_dir().join(format!("a-structural-{}", std::process::id()));
        let directory = root.join("moved");
        std::fs::create_dir_all(&directory).unwrap();
        let file = root.join("SKILL.md");
        std::fs::write(&file, "").unwrap();
        let event = |kind, paths: &[&PathBuf]| {
            paths.iter().fold(notify::Event::new(kind), |event, &path| {
                event.add_path(path.clone())
            })
        };
        let renamed = notify::EventKind::Modify(ModifyKind::Name(RenameMode::To));
        assert!(structural(&event(
            notify::EventKind::Create(CreateKind::Folder),
            &[&directory]
        )));
        assert!(structural(&event(renamed, &[&directory])));
        assert!(!structural(&event(renamed, &[&file])));
        assert!(!structural(&event(
            notify::EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &[&root.join("SKILL.md.tmp"), &file]
        )));
        assert!(!structural(&event(
            notify::EventKind::Remove(RemoveKind::Folder),
            &[&root.join("gone")]
        )));
        assert!(!structural(&event(
            notify::EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            &[&file]
        )));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rescans_outside_every_root_are_ignored() {
        let event = WatchEvent::RescanPaths(vec![PathBuf::from("/home/other")]);
        assert!(!event.affects(&[PathBuf::from("/home/work/skills")]));
    }

    #[test]
    fn canonical_roots_under_aliased_parents_remain_observable() {
        let aliases = vec![(PathBuf::from("/real"), PathBuf::from("/alias"))];
        let paths = event_paths(
            vec![PathBuf::from("/real/project/skills/review/SKILL.md")],
            &aliases,
        );
        let event = WatchEvent::Changed(paths);
        assert!(event.affects(&[
            PathBuf::from("/alias/missing"),
            PathBuf::from("/real/project/skills"),
        ]));
        assert!(event.affects(&[PathBuf::from("/alias/project/skills")]));
    }
}

#[cfg(windows)]
pub struct Backend(Vec<Arc<crate::Wakeup>>);

#[cfg(windows)]
impl Backend {
    pub fn new(
        spawn: &Spawn,
        wanted: &BTreeMap<PathBuf, bool>,
        changed: &Changed,
    ) -> io::Result<Self> {
        let mut backend = Self(Vec::new());
        for (path, mode) in wanted {
            let watch = sys::DirectoryWatch::new(path, *mode)?;
            let watch = crate::Async::for_reading(&crate::Reactor::AsyncIo, watch)?;
            let stop = Arc::new(crate::Wakeup::new());
            let stopped = stop.clone();
            let changed = changed.clone();
            spawn(Box::pin(async move {
                crate::or(
                    async {
                        loop {
                            let result = watch.read_with(sys::DirectoryWatch::take).await;
                            match result {
                                Ok(Some(paths)) => changed(WatchEvent::Changed(paths)),
                                Ok(None) => changed(WatchEvent::Overflow),
                                Err(error) => {
                                    changed(WatchEvent::Unavailable(error));
                                    break;
                                }
                            }
                        }
                    },
                    stopped.wait(),
                )
                .await;
            }));
            backend.0.push(stop);
        }
        Ok(backend)
    }
}

#[cfg(windows)]
impl Drop for Backend {
    fn drop(&mut self) {
        for stop in &self.0 {
            stop.notify();
        }
    }
}
