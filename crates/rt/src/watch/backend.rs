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
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
                Ok(event) if event.need_rescan() => changed(WatchEvent::Rescan),
                Ok(event) if !matches!(event.kind, notify::EventKind::Access(_)) => {
                    changed(WatchEvent::Changed(event.paths));
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
