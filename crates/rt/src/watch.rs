use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use notify::{RecursiveMode, Watcher as _};

use crate::BoxFuture;

type Spawn = Arc<dyn Fn(BoxFuture<'static, ()>) + Send + Sync>;

type Changed = Arc<dyn Fn(Vec<PathBuf>) + Send + Sync>;

/// Owned directory trees, including roots created later. Refresh before
/// rescanning after a change to follow newly created or replaced roots.
pub struct Watcher {
    roots: Arc<[PathBuf]>,
    changed: Changed,
    invalidated: Arc<AtomicBool>,
    recursive: Option<notify::RecommendedWatcher>,
    watched: BTreeMap<PathBuf, RecursiveMode>,
    #[cfg(target_os = "macos")]
    spawn: Spawn,
    #[cfg(target_os = "macos")]
    parents: Vec<Parent>,
}

impl Watcher {
    pub fn new(
        spawn: Spawn,
        roots: impl IntoIterator<Item = impl AsRef<Path>>,
        changed: impl Fn(Vec<PathBuf>) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let mut watcher = Self {
            roots: roots.into_iter().map(|p| p.as_ref().to_owned()).collect(),
            changed: Arc::new(changed),
            invalidated: Arc::new(AtomicBool::new(true)),
            recursive: None,
            watched: BTreeMap::new(),
            #[cfg(target_os = "macos")]
            spawn,
            #[cfg(target_os = "macos")]
            parents: Vec::new(),
        };
        #[cfg(not(target_os = "macos"))]
        drop(spawn);
        watcher.refresh()?;
        Ok(watcher)
    }

    pub fn refresh(&mut self) -> io::Result<()> {
        for _ in 0..8 {
            let (wanted, parents) = self.paths();
            if !self.invalidated.swap(false, Ordering::AcqRel) && wanted == self.watched {
                return Ok(());
            }
            // Rearm even unchanged paths after ancestor events: a replacement can
            // have the same name while the old watch still names its removed inode.
            let changed = self.callback();
            let result: io::Result<()> = (|| {
                let watcher = if wanted.is_empty() {
                    None
                } else {
                    let changed = Arc::clone(&changed);
                    let mut watcher =
                        notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                            match event {
                                Ok(event)
                                    if !matches!(event.kind, notify::EventKind::Access(_)) =>
                                {
                                    changed(event.paths);
                                }
                                Err(error) => eprintln!("file watcher: {error}"),
                                _ => {}
                            }
                        })
                        .map_err(io::Error::other)?;
                    let mut paths = watcher.paths_mut();
                    for (path, mode) in &wanted {
                        paths.add(path, *mode).map_err(io::Error::other)?;
                    }
                    paths.commit().map_err(io::Error::other)?;
                    Some(watcher)
                };
                #[cfg(target_os = "macos")]
                let parent_watches = parents
                    .iter()
                    .map(|path| Parent::new(&self.spawn, path.clone(), Arc::clone(&changed)))
                    .collect::<io::Result<Vec<_>>>()?;
                self.recursive = watcher;
                self.watched = wanted;
                #[cfg(target_os = "macos")]
                {
                    self.parents = parent_watches;
                }
                Ok(())
            })();
            if result.is_err() {
                self.invalidated.store(true, Ordering::Release);
            }
            result?;
            // An ancestor can be created between stat and registration. Recheck
            // after arming, before the consumer scans, so its descendants stay observable.
            let (after, parent_paths) = self.paths();
            if after == self.watched && parent_paths == parents {
                return Ok(());
            }
            self.invalidated.store(true, Ordering::Release);
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "directory roots changed during subscription",
        ))
    }

    fn paths(&self) -> (BTreeMap<PathBuf, RecursiveMode>, Vec<PathBuf>) {
        let mut wanted = BTreeMap::new();
        let mut parents = Vec::new();
        for root in &*self.roots {
            if root.is_dir() {
                wanted.insert(root.clone(), RecursiveMode::Recursive);
            }
            // The parent observes removal/replacement of the root itself.
            // Until it exists, watch only its nearest existing ancestor.
            let mut parent = root.parent();
            while let Some(path) = parent {
                if path.is_dir() {
                    parents.push(path.to_owned());
                    break;
                }
                parent = path.parent();
            }
        }
        parents.sort();
        parents.dedup();
        #[cfg(not(target_os = "macos"))]
        for parent in &parents {
            wanted
                .entry(parent.clone())
                .or_insert(RecursiveMode::NonRecursive);
        }
        (wanted, parents)
    }

    fn callback(&self) -> Changed {
        let roots = Arc::clone(&self.roots);
        let invalidated = Arc::clone(&self.invalidated);
        let changed = Arc::clone(&self.changed);
        Arc::new(move |paths| {
            if paths
                .iter()
                .any(|path| roots.iter().any(|root| root.starts_with(path)))
            {
                invalidated.store(true, Ordering::Release);
            }
            changed(paths);
        })
    }
}

#[cfg(target_os = "macos")]
struct Parent(Arc<crate::Wakeup>);

#[cfg(target_os = "macos")]
impl Parent {
    fn new(spawn: &Spawn, path: PathBuf, changed: Changed) -> io::Result<Self> {
        let io = crate::Async::for_reading(&crate::Reactor::AsyncIo, sys::write_watch(&path)?)?;
        let stop = Arc::new(crate::Wakeup::new());
        let stopped = Arc::clone(&stop);
        spawn(Box::pin(async move {
            crate::or(
                async {
                    loop {
                        io.readable().await;
                        io.get_ref().clear();
                        changed(vec![path.clone()]);
                    }
                },
                stopped.wait(),
            )
            .await;
        }));
        Ok(Self(stop))
    }
}

#[cfg(target_os = "macos")]
impl Drop for Parent {
    fn drop(&mut self) {
        self.0.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn changed(
        watcher: &mut Watcher,
        recv: &mpsc::Receiver<Vec<PathBuf>>,
        path: &Path,
        exact: bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let paths = recv
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("filesystem event");
            // A stale descendant event cannot acknowledge root replacement;
            // rearming for unrelated events can discard the one we await.
            if paths
                .iter()
                .any(|p| p == path || (!exact && path.starts_with(p)))
            {
                watcher.refresh().unwrap();
                return;
            }
        }
    }

    #[test]
    fn missing_trees_are_lazy_and_creation_and_replacement_remain_observable() {
        let root = std::env::temp_dir().join(format!("a-root-watch-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let tree = root.join("missing/tree");
        let runtime: crate::Rt = Arc::new(crate::Threads::new(1));
        let rt = Arc::clone(&runtime);
        let (send, recv) = mpsc::channel();
        let mut watcher = Watcher::new(
            Arc::new(move |task| rt.spawn(task)),
            [&tree],
            move |paths| {
                let _ = send.send(paths);
            },
        )
        .unwrap();
        #[cfg(target_os = "macos")]
        assert!(
            watcher.recursive.is_none(),
            "no recursive watcher for absent trees"
        );

        fs::create_dir_all(tree.join("nested")).unwrap();
        changed(&mut watcher, &recv, &tree, false);
        assert!(watcher.recursive.is_some());
        let file = tree.join("nested/first");
        fs::write(&file, "created after subscription").unwrap();
        changed(&mut watcher, &recv, &file, true);

        fs::rename(&tree, root.join("retired")).unwrap();
        fs::create_dir_all(tree.join("nested")).unwrap();
        changed(&mut watcher, &recv, &tree, false);
        let replacement = tree.join("nested/replacement");
        fs::write(&replacement, "new inode at the same root").unwrap();
        changed(&mut watcher, &recv, &replacement, true);

        fs::remove_dir_all(&tree).unwrap();
        changed(&mut watcher, &recv, &tree, false);
        #[cfg(target_os = "macos")]
        assert!(
            watcher.recursive.is_none(),
            "absent trees release recursive watches"
        );
        drop(watcher);
        drop(runtime);
        fs::remove_dir_all(root).unwrap();
    }
}
