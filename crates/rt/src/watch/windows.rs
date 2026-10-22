use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use windows_file_watcher::{Monitor, Notification, WatchId, WatchOptions};

use super::{Changed, RecursiveMode, Spawn};

pub(super) struct Watch(Arc<crate::Wakeup>);

impl Watch {
    pub(super) fn new(
        spawn: &Spawn,
        wanted: &BTreeMap<PathBuf, RecursiveMode>,
        changed: Changed,
    ) -> io::Result<Self> {
        let monitor = Monitor::new()?;
        let (session, receiver) = monitor.session();
        let mut subscriptions = Vec::new();
        let mut paths = HashMap::new();
        for (path, mode) in wanted {
            let watch = session.subscribe(
                path,
                WatchOptions::new().subtree(*mode == RecursiveMode::Recursive),
            )?;
            paths.insert(watch.id(), path.clone());
            subscriptions.push(watch);
        }
        // Registration must finish before the consumer scans the subscribed tree.
        monitor.quiesce();
        for watch in &subscriptions {
            if !monitor.is_watching(watch.id()) {
                return Err(monitor.stop_reason(watch.id()).unwrap_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "watch root changed during registration",
                    )
                }));
            }
        }
        let io = crate::Async::for_reading(&crate::Reactor::AsyncIo, receiver.doorbell_owned()?)?;
        let stop = Arc::new(crate::Wakeup::new());
        let stopped = Arc::clone(&stop);
        spawn(Box::pin(async move {
            // Native reads and their queue must outlive every awaited notification.
            let _owned = (monitor, subscriptions);
            crate::or(
                async {
                    loop {
                        io.readable_result().await?;
                        while let Some(notification) = receiver.try_recv() {
                            dispatch(notification, &paths, &changed);
                        }
                    }
                },
                async {
                    stopped.wait().await;
                    Ok::<(), io::Error>(())
                },
            )
            .await
            .unwrap_or_else(|error| eprintln!("file watcher: {error}"));
        }));
        Ok(Self(stop))
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.0.notify();
    }
}

fn dispatch(notification: Notification, paths: &HashMap<WatchId, PathBuf>, changed: &Changed) {
    let Some(root) = paths.get(&notification.watch()) else {
        return;
    };
    match notification {
        Notification::Batch { changes, .. } => {
            changed(
                changes
                    .into_iter()
                    .map(|change| root.join(change.name.to_path_buf()))
                    .collect(),
            );
        }
        // An overflow or a changed directory identity requires a full rescan.
        Notification::Desync { .. } => changed(vec![root.clone()]),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::task::{Context, Waker};

    #[test]
    fn dropping_a_subscription_cancels_its_wait_without_a_filesystem_event() {
        let root = std::env::temp_dir().join(format!("a-watch-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let tasks = Arc::new(Mutex::new(Vec::<crate::BoxFuture<'static, ()>>::new()));
        let pending = Arc::clone(&tasks);
        let spawn: Spawn = Arc::new(move |task| pending.lock().unwrap().push(task));
        let watch = Watch::new(
            &spawn,
            &BTreeMap::from([(root.clone(), RecursiveMode::Recursive)]),
            Arc::new(|_| {}),
        )
        .unwrap();
        drop(watch);
        let mut task = tasks.lock().unwrap().pop().unwrap();
        assert!(
            task.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
        drop(task);
        std::fs::remove_dir_all(root).unwrap();
    }
}
