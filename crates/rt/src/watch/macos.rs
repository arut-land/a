//! `FSEvents`: one stream per outermost root, on sys's shared dispatch
//! queue. A stream follows its path rather than a directory, and with
//! `WatchRoot` reports the root itself going and coming back, so nothing
//! above a root is watched separately.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Weak;

use sys::{FsEvent, FsEventKind, FsEventStream};

use super::{Raw, Shared, Sub, Unavailable};
use crate::Rt;

pub(super) struct Native {
    me: Weak<Shared>,
    streams: BTreeMap<PathBuf, Stream>,
    /// The stream each subscription rides on.
    held: HashMap<u64, PathBuf>,
}

struct Stream {
    refs: usize,
    stream: FsEventStream,
}

impl Native {
    pub(super) fn new(me: Weak<Shared>, _: Rt) -> Self {
        Self {
            me,
            streams: BTreeMap::new(),
            held: HashMap::new(),
        }
    }

    pub(super) fn arm(&mut self, id: u64, sub: &Sub) -> Result<(), Unavailable> {
        let key = if let Some(key) = sub.real.ancestors().find(|a| self.streams.contains_key(*a)) {
            key.to_owned()
        } else {
            self.open(&sub.real)?;
            sub.real.clone()
        };
        self.streams.get_mut(&key).expect("opened").refs += 1;
        if let Some(old) = self.held.insert(id, key) {
            self.release(&old);
        }
        if sub.real.is_dir() {
            Ok(())
        } else {
            Err(Unavailable::Gone)
        }
    }

    pub(super) fn disarm(&mut self, id: u64) {
        if let Some(key) = self.held.remove(&id) {
            self.release(&key);
        }
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

    /// Starts a stream at `root`, which then serves the subscriptions of
    /// the streams under it: those close once it runs.
    fn open(&mut self, root: &Path) -> Result<(), Unavailable> {
        let nested: Vec<PathBuf> = (self.streams.keys())
            .filter(|key| key.starts_with(root))
            .cloned()
            .collect();
        let (me, key) = (self.me.clone(), root.to_owned());
        let stream = FsEventStream::new(root, move |events| {
            if let Some(shared) = me.upgrade() {
                shared.post(events.into_iter().map(|e| raw(e, &key)).collect());
            }
        })
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::Unsupported => Unavailable::Unsupported,
            _ => Unavailable::Io(error),
        })?;
        let mut refs = 0;
        for key in nested {
            let nested = self.streams.remove(&key).expect("listed");
            // What it holds is delivered before it goes, the new stream
            // already running: nothing under it falls between the two.
            nested.stream.flush();
            refs += nested.refs;
            for held in self.held.values_mut().filter(|held| **held == key) {
                root.clone_into(held);
            }
        }
        self.streams
            .insert(root.to_owned(), Stream { refs, stream });
        Ok(())
    }

    fn release(&mut self, key: &Path) {
        let stream = self.streams.get_mut(key).expect("held");
        stream.refs -= 1;
        if stream.refs == 0 {
            self.streams.remove(key);
        }
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
