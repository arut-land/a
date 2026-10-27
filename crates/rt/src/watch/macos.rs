//! `FSEvents`: one stream per outermost root, on sys's shared dispatch
//! queue. A stream follows its path rather than a directory, and with
//! `WatchRoot` reports the root itself going and coming back, so nothing
//! above a root is watched separately.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Weak;

use sys::{FsEvent, FsEventKind, FsEventStream};

use super::{Raw, Shared, Sub, Unavailable};
use crate::Rt;

pub(super) struct Native {
    me: Weak<Shared>,
    streams: BTreeMap<PathBuf, FsEventStream>,
    /// Requested roots stay independent of the streams covering them.
    requested: HashMap<u64, PathBuf>,
}

impl Native {
    pub(super) fn new(me: Weak<Shared>, _: Rt) -> Self {
        Self {
            me,
            streams: BTreeMap::new(),
            requested: HashMap::new(),
        }
    }

    pub(super) fn arm(&mut self, id: u64, sub: &Sub) -> Result<(), Unavailable> {
        let old = self.requested.insert(id, sub.real.clone());
        if let Err(error) = self.reconcile() {
            match old {
                Some(root) => {
                    self.requested.insert(id, root);
                }
                None => {
                    self.requested.remove(&id);
                }
            }
            return Err(error);
        }
        if sub.real.is_dir() {
            Ok(())
        } else {
            Err(Unavailable::Gone)
        }
    }

    pub(super) fn disarm(&mut self, id: u64) {
        self.requested.remove(&id);
        // If replacement admission fails, existing coverage still serves
        // the survivors. The next subscription change tries again.
        let _ = self.reconcile();
    }

    #[allow(
        clippy::unused_self,
        clippy::unnecessary_wraps,
        reason = "one interface for every backend"
    )]
    pub(super) fn enter(&mut self, _: u64, _: &Path) -> Result<(), Unavailable> {
        Ok(())
    }

    /// A stream covers what appears under its root by itself.
    #[allow(
        clippy::unused_self,
        clippy::unnecessary_wraps,
        reason = "one interface for every backend"
    )]
    pub(super) fn grow(&mut self, _: u64, _: &Sub, _: &Path) -> Result<(), Unavailable> {
        Ok(())
    }

    pub(super) fn watches(&self) -> usize {
        self.streams.len()
    }

    fn reconcile(&mut self) -> Result<(), Unavailable> {
        let requested: BTreeSet<&Path> = self.requested.values().map(PathBuf::as_path).collect();
        let mut roots: BTreeSet<PathBuf> = requested
            .iter()
            .filter(|root| {
                !root
                    .ancestors()
                    .skip(1)
                    .any(|parent| requested.contains(parent))
            })
            .map(|root| root.to_path_buf())
            .collect();
        let mut added = BTreeMap::new();
        let mut covered_failures = Vec::new();
        for root in roots
            .iter()
            .filter(|root| !self.streams.contains_key(*root))
        {
            let (me, key) = (self.me.clone(), root.clone());
            let stream = FsEventStream::new(root, move |events| {
                if let Some(shared) = me.upgrade() {
                    shared.post(events.into_iter().map(|e| raw(e, &key)).collect());
                }
            });
            match stream {
                Ok(stream) => {
                    added.insert(root.clone(), stream);
                }
                Err(_)
                    if root
                        .ancestors()
                        .any(|parent| self.streams.contains_key(parent)) =>
                {
                    covered_failures.push(root.clone());
                }
                Err(error) => {
                    return Err(match error.kind() {
                        std::io::ErrorKind::Unsupported => Unavailable::Unsupported,
                        _ => Unavailable::Io(error),
                    });
                }
            }
        }
        // Failed narrowing retains its covering stream without preventing
        // independent roots from being admitted.
        roots.extend(
            self.streams
                .keys()
                .filter(|root| {
                    covered_failures
                        .iter()
                        .any(|failed| failed.starts_with(root))
                })
                .cloned(),
        );
        let coverage: BTreeSet<PathBuf> = roots
            .iter()
            .filter(|root| {
                !root
                    .ancestors()
                    .skip(1)
                    .any(|parent| roots.contains(parent))
            })
            .cloned()
            .collect();
        self.streams.append(&mut added);
        self.streams.retain(|root, stream| {
            if coverage.contains(root) {
                return true;
            }
            // Every replacement is running before pending events are
            // drained and the old coverage is released.
            stream.flush();
            false
        });
        Ok(())
    }
}

fn raw(event: FsEvent, root: &Path) -> Raw {
    match event.kind {
        FsEventKind::Dropped => Raw::Rescan(Some(root.to_owned())),
        FsEventKind::ScanTree | FsEventKind::Mount => Raw::Rescan(Some(event.path)),
        FsEventKind::DirCreated => Raw::Created(event.path),
        FsEventKind::Root | FsEventKind::Item => Raw::Changed(event.path),
    }
}
