use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod backend;

use crate::BoxFuture;

type Spawn = Arc<dyn Fn(BoxFuture<'static, ()>) + Send + Sync>;

/// Uncertainty invalidates the entire source inventory, even without paths.
#[derive(Debug)]
pub enum WatchEvent {
    Changed(Vec<PathBuf>),
    RescanPaths(Vec<PathBuf>),
    Overflow,
    Rescan,
    Unavailable(io::Error),
}

impl WatchEvent {
    pub fn affects(&self, roots: &[PathBuf]) -> bool {
        match self {
            Self::Changed(paths) | Self::RescanPaths(paths) => paths.iter().any(|path| {
                roots
                    .iter()
                    .any(|root| path.starts_with(root) || root.starts_with(path))
            }),
            _ => true,
        }
    }
}

type Changed = Arc<dyn Fn(WatchEvent) + Send + Sync>;

/// Owned directory trees, including roots created later. Refresh before
/// rescanning after a change to follow newly created or replaced roots.
pub struct Watcher {
    roots: Arc<[PathBuf]>,
    changed: Changed,
    invalidated: Arc<AtomicBool>,
    recursive: Option<backend::Backend>,
    watched: BTreeMap<PathBuf, bool>,
    spawn: Spawn,
    recovery: Arc<Recovery>,
    #[cfg(target_os = "macos")]
    parents: Vec<Parent>,
}

impl Watcher {
    pub fn new(
        spawn: Spawn,
        roots: impl IntoIterator<Item = impl AsRef<Path>>,
        changed: impl Fn(WatchEvent) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let changed: Changed = Arc::new(changed);
        let recovery = Arc::new(Recovery {
            spawn: spawn.clone(),
            changed: changed.clone(),
            timer: Mutex::default(),
            closed: AtomicBool::new(false),
        });
        let mut watcher = Self {
            roots: roots.into_iter().map(|p| p.as_ref().to_owned()).collect(),
            changed,
            invalidated: Arc::new(AtomicBool::new(true)),
            recursive: None,
            watched: BTreeMap::new(),
            spawn,
            recovery,
            #[cfg(target_os = "macos")]
            parents: Vec::new(),
        };
        let _ = watcher.refresh();
        Ok(watcher)
    }

    pub fn refresh(&mut self) -> io::Result<()> {
        self.refresh_using(backend::Backend::new)
    }

    fn refresh_using(
        &mut self,
        mut arm: impl FnMut(&Spawn, &BTreeMap<PathBuf, bool>, &Changed) -> io::Result<backend::Backend>,
    ) -> io::Result<()> {
        let result = self.rearm(&mut arm);
        match &result {
            Ok(()) => self.recovery.healthy(),
            Err(error) if self.recovery.start() => {
                (self.changed)(WatchEvent::Unavailable(io::Error::new(
                    error.kind(),
                    error.to_string(),
                )));
            }
            Err(_) => {}
        }
        result
    }

    fn rearm(
        &mut self,
        arm: &mut impl FnMut(&Spawn, &BTreeMap<PathBuf, bool>, &Changed) -> io::Result<backend::Backend>,
    ) -> io::Result<()> {
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
                    let watcher = arm(&self.spawn, &wanted, &changed)?;
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

    fn paths(&self) -> (BTreeMap<PathBuf, bool>, Vec<PathBuf>) {
        let mut wanted = BTreeMap::new();
        let mut parents = Vec::new();
        for root in &*self.roots {
            if root.is_dir() {
                wanted.insert(root.clone(), true);
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
            wanted.entry(parent.clone()).or_insert(false);
        }
        (wanted, parents)
    }

    fn callback(&self) -> Changed {
        let roots = Arc::clone(&self.roots);
        let invalidated = Arc::clone(&self.invalidated);
        let changed = Arc::clone(&self.changed);
        let recovery = self.recovery.clone();
        Arc::new(move |event| {
            let rearm = match &event {
                WatchEvent::RescanPaths(_) => event.affects(&roots),
                WatchEvent::Changed(paths) => paths
                    .iter()
                    .any(|path| roots.iter().any(|root| root.starts_with(path))),
                _ => true,
            };
            if rearm {
                invalidated.store(true, Ordering::Release);
            }
            if matches!(event, WatchEvent::Unavailable(_)) {
                let _ = recovery.start();
            }
            changed(event);
        })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.recovery.closed.store(true, Ordering::Release);
        self.recovery.healthy();
    }
}

struct Recovery {
    spawn: Spawn,
    changed: Changed,
    timer: Mutex<Option<Arc<crate::Wakeup>>>,
    closed: AtomicBool,
}

impl Recovery {
    fn healthy(&self) {
        if let Some(stop) = self.timer.lock().unwrap().take() {
            stop.notify();
        }
    }

    fn start(self: &Arc<Self>) -> bool {
        let mut timer = self.timer.lock().unwrap();
        if timer.is_some() || self.closed.load(Ordering::Acquire) {
            return false;
        }
        let stop = Arc::new(crate::Wakeup::new());
        *timer = Some(stop.clone());
        let weak = Arc::downgrade(self);
        (self.spawn)(Box::pin(async move {
            let mut delay = Duration::from_millis(100);
            loop {
                let due = crate::or(
                    async {
                        crate::Reactor::AsyncIo.sleep(delay).await;
                        true
                    },
                    async {
                        stop.wait().await;
                        false
                    },
                )
                .await;
                if !due {
                    break;
                }
                let Some(recovery) = weak.upgrade() else {
                    break;
                };
                if recovery.closed.load(Ordering::Acquire) {
                    break;
                }
                (recovery.changed)(WatchEvent::Rescan);
                delay = (delay * 2).min(Duration::from_secs(5));
            }
        }));
        true
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
                        if let Err(error) = io.readable_result().await {
                            changed(WatchEvent::Unavailable(error));
                            break;
                        }
                        io.get_ref().clear();
                        changed(WatchEvent::Changed(vec![path.clone()]));
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

    #[test]
    fn writes_after_a_quiet_capture_deliver_another_event_during_preparation() {
        use std::io::Write;
        let root = std::env::temp_dir().join(format!("a-resumed-watch-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let source = root.join("source");
        let mut writer = fs::File::create(&source).unwrap();
        let runtime: crate::Rt = Arc::new(crate::Threads::new(1));
        let (send, events) = mpsc::channel();
        let (resume, paused) = mpsc::channel();
        let paused = Mutex::new(paused);
        let first = AtomicBool::new(true);
        let roots = [root.clone()];
        let watcher = Watcher::new(
            Arc::new(move |task| runtime.spawn(task)),
            roots.clone(),
            move |event| {
                if event.affects(&roots) {
                    let _ = send.send(());
                    if first.swap(false, Ordering::AcqRel) {
                        let _ = paused.lock().unwrap().recv();
                    }
                }
            },
        )
        .unwrap();
        // Drop this sender before the watcher on failure, releasing its callback.
        let release = resume;
        writer.write_all(b"partial").unwrap();
        writer.sync_data().unwrap();
        events.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"partial");
        crate::block_on(crate::Reactor::AsyncIo.sleep(Duration::from_millis(100)));
        assert_eq!(fs::read(&source).unwrap(), b"partial");
        // The same writer resumes after debounce while the delivered callback
        // still owns its batch. Its final write must remain queued independently.
        writer.write_all(b" final").unwrap();
        writer.sync_data().unwrap();
        release.send(()).unwrap();
        events.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"partial final");
        drop(writer);
        drop(watcher);
        fs::remove_dir_all(root).unwrap();
    }

    fn changed(watcher: &mut Watcher, recv: &mpsc::Receiver<WatchEvent>, path: &Path, exact: bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = std::collections::VecDeque::new();
        loop {
            let event = recv
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| panic!("{error}: expected {path:?}, exact={exact}, exists={}, watches={:?}, observed={seen:?}", path.exists(), watcher.watched));
            if seen.len() == 16 {
                seen.pop_front();
            }
            seen.push_back(format!("{event:?}"));
            // Descendants cannot acknowledge root replacement. Uncertainty
            // instead requires a full scan, including the subscription inventory.
            let scan = match event {
                WatchEvent::Changed(paths) | WatchEvent::RescanPaths(paths) => paths
                    .iter()
                    .any(|p| p == path || (!exact && path.starts_with(p))),
                _ => true,
            };
            if scan {
                watcher.refresh().unwrap();
                return;
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn linked_roots_report_changes_in_the_configured_spelling() {
        let base = std::env::temp_dir().join(format!("a-linked-watch-{}", std::process::id()));
        fs::create_dir_all(base.join("real")).unwrap();
        let base = base.canonicalize().unwrap();
        let alias = base.join("alias");
        std::os::unix::fs::symlink(base.join("real"), &alias).unwrap();
        let runtime: crate::Rt = Arc::new(crate::Threads::new(1));
        let (send, recv) = mpsc::channel();
        let roots = [alias.clone()];
        let mut watcher = Watcher::new(
            Arc::new(move |task| runtime.spawn(task)),
            roots.clone(),
            move |event| {
                if event.affects(&roots) {
                    let _ = send.send(event);
                }
            },
        )
        .unwrap();
        fs::write(base.join("real/source"), "visible through the alias").unwrap();
        changed(&mut watcher, &recv, &alias.join("source"), true);
        drop(watcher);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn uncertainty_without_paths_rescans_and_rearms_all_roots() {
        let root = std::env::temp_dir().join(format!("a-uncertain-watch-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let runtime: crate::Rt = Arc::new(crate::Threads::new(1));
        let (send, recv) = mpsc::channel();
        let mut watcher = Watcher::new(
            Arc::new(move |task| runtime.spawn(task)),
            [&root],
            move |event| {
                let _ = send.send(event);
            },
        )
        .unwrap();
        for event in [
            WatchEvent::Overflow,
            WatchEvent::Rescan,
            WatchEvent::Unavailable(io::Error::other("lost subscription")),
        ] {
            assert!(event.affects(std::slice::from_ref(&root)));
            watcher.callback()(event);
            assert!(watcher.invalidated.load(Ordering::Acquire));
            watcher.refresh().unwrap();
        }
        let path = root.join("after-rearm");
        fs::write(&path, "observable").unwrap();
        changed(&mut watcher, &recv, &path, true);
        drop(watcher);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unavailable_subscriptions_retry_failed_rearms_without_filesystem_callbacks() {
        let root = std::env::temp_dir().join(format!("a-watch-recover-{}", std::process::id()));
        fs::create_dir_all(root.join("nested")).unwrap();
        let root = root.canonicalize().unwrap();
        let runtime: crate::Rt = Arc::new(crate::Threads::new(1));
        let (send, recv) = mpsc::channel();
        let mut watcher = Watcher::new(
            Arc::new(move |task| runtime.spawn(task)),
            [&root],
            move |event| {
                let _ = send.send(event);
            },
        )
        .unwrap();
        watcher.callback()(WatchEvent::Changed(vec![root.join("nested")]));
        watcher.callback()(WatchEvent::Unavailable(io::Error::other("lost watch")));
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if matches!(
                recv.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap(),
                WatchEvent::Unavailable(_)
            ) {
                break;
            }
        }
        assert!(
            watcher
                .refresh_using(|_, _, _| Err(io::Error::new(io::ErrorKind::WouldBlock, "busy")))
                .is_err()
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if matches!(
                recv.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap(),
                WatchEvent::Rescan
            ) {
                break;
            }
        }
        watcher.refresh().unwrap();
        assert!(watcher.recovery.timer.lock().unwrap().is_none());
        let no_rescan = || {
            let deadline = Instant::now() + Duration::from_millis(300);
            while let Ok(event) =
                recv.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                assert!(!matches!(event, WatchEvent::Rescan));
            }
        };
        no_rescan();
        let path = root.join("nested/after-recovery");
        fs::write(&path, "observed").unwrap();
        changed(&mut watcher, &recv, &path, true);
        let recovery = watcher.recovery.clone();
        watcher.callback()(WatchEvent::Unavailable(io::Error::other(
            "cancel pending retry",
        )));
        let _ = recv.recv_timeout(Duration::from_secs(3)).unwrap();
        drop(watcher);
        assert!(recovery.timer.lock().unwrap().is_none());
        no_rescan();
        fs::remove_dir_all(root).unwrap();
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
