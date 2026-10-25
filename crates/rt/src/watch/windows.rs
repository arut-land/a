//! `ReadDirectoryChangesW`: one recursive directory handle per outermost
//! root, and above each root a non-recursive handle on its nearest
//! existing parent, whose entries the router keeps only when they name
//! the root. A handle follows its directory, not its path, so a root
//! replaced at its path is opened again.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use super::{Raw, Shared, Sub, Unavailable, existing_parent, identity};
use crate::{Async, Rt, Wakeup};

pub(super) struct Native {
    me: Weak<Shared>,
    rt: Rt,
    trees: BTreeMap<PathBuf, Handle>,
    parents: BTreeMap<PathBuf, Handle>,
    held: HashMap<u64, Held>,
    serial: u64,
}

/// A handle a subscription holds: its path and which opening of it.
type Key = (PathBuf, u64);

#[derive(Default)]
struct Held {
    tree: Option<Key>,
    parent: Option<Key>,
}

struct Handle {
    serial: u64,
    refs: usize,
    identity: Option<sys::fs::FileIdentity>,
    stop: Arc<Wakeup>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop.notify();
    }
}

#[derive(Clone, Copy)]
enum Depth {
    Tree,
    Entries,
}

impl Native {
    pub(super) fn new(me: Weak<Shared>, rt: Rt) -> Self {
        Self {
            me,
            rt,
            trees: BTreeMap::new(),
            parents: BTreeMap::new(),
            held: HashMap::new(),
            serial: 0,
        }
    }

    pub(super) fn arm(&mut self, id: u64, sub: &Sub) -> Result<(), Unavailable> {
        let mut held = Held::default();
        let result = self.arm_into(&mut held, sub);
        // Held anew before the old ones go, so what both need stays open.
        if let Some(old) = self.held.insert(id, held) {
            self.release(old);
        }
        result
    }

    fn arm_into(&mut self, held: &mut Held, sub: &Sub) -> Result<(), Unavailable> {
        if sub.real.is_dir() {
            let shared = (sub.real.ancestors())
                .find(|dir| (self.trees.get(*dir)).is_some_and(|h| h.identity == identity(dir)));
            let key = if let Some(dir) = shared {
                dir.to_owned()
            } else {
                self.open_tree(&sub.real)?;
                sub.real.clone()
            };
            let handle = self.trees.get_mut(&key).expect("opened");
            handle.refs += 1;
            held.tree = Some((key, handle.serial));
        }
        if let Some(parent) = existing_parent(&sub.real)
            && !self.trees.keys().any(|tree| parent.starts_with(tree))
        {
            if !self.parents.contains_key(parent) {
                let handle = self.open(parent, Depth::Entries)?;
                self.parents.insert(parent.to_owned(), handle);
            }
            let handle = self.parents.get_mut(parent).expect("opened");
            handle.refs += 1;
            held.parent = Some((parent.to_owned(), handle.serial));
        }
        if held.tree.is_some() {
            Ok(())
        } else {
            Err(Unavailable::Gone)
        }
    }

    pub(super) fn disarm(&mut self, id: u64) {
        if let Some(held) = self.held.remove(&id) {
            self.release(held);
        }
    }

    /// A recursive handle hears every directory under its root.
    #[allow(
        clippy::unused_self,
        clippy::unnecessary_wraps,
        reason = "one interface for every backend"
    )]
    pub(super) fn enter(&mut self, _: u64, _: &Path) -> Result<(), Unavailable> {
        Ok(())
    }

    #[allow(
        clippy::unused_self,
        clippy::unnecessary_wraps,
        reason = "one interface for every backend"
    )]
    pub(super) fn leave(&mut self, _: u64, _: &Path) {}

    #[allow(
        clippy::unused_self,
        clippy::unnecessary_wraps,
        reason = "one interface for every backend"
    )]
    pub(super) fn grow(&mut self, _: u64, _: &Sub, _: &Path) -> Result<(), Unavailable> {
        Ok(())
    }

    pub(super) fn watches(&self) -> usize {
        self.trees.len() + self.parents.len()
    }

    /// Opens a recursive handle at `root`, which then serves the
    /// subscriptions of the trees under it: those close once it is open.
    fn open_tree(&mut self, root: &Path) -> Result<(), Unavailable> {
        let mut handle = self.open(root, Depth::Tree)?;
        let nested: Vec<PathBuf> = (self.trees.keys())
            .filter(|key| key.starts_with(root))
            .cloned()
            .collect();
        for key in nested {
            let old = self.trees.remove(&key).expect("listed");
            for held in self.held.values_mut() {
                if let Some(tree) = held
                    .tree
                    .as_mut()
                    .filter(|t| **t == (key.clone(), old.serial))
                {
                    *tree = (root.to_owned(), handle.serial);
                    handle.refs += 1;
                }
            }
        }
        self.trees.insert(root.to_owned(), handle);
        Ok(())
    }

    fn open(&mut self, path: &Path, depth: Depth) -> Result<Handle, Unavailable> {
        let watch =
            sys::DirectoryWatch::new(path, matches!(depth, Depth::Tree)).map_err(|error| {
                match error.raw_os_error() {
                    // Not found, denied, or a filesystem without notifications.
                    Some(2 | 3) => Unavailable::Gone,
                    Some(1 | 5 | 50) => Unavailable::Unsupported,
                    _ => Unavailable::Io(error),
                }
            })?;
        let watch = Async::for_reading(&self.rt.reactor(), watch).map_err(Unavailable::Io)?;
        self.serial += 1;
        let serial = self.serial;
        let stop = Arc::new(Wakeup::new());
        let (me, key, stopped) = (self.me.clone(), path.to_owned(), Arc::clone(&stop));
        self.rt.spawn(Box::pin(crate::or(
            async move {
                loop {
                    let read = watch.read_with(sys::DirectoryWatch::take).await;
                    let Some(shared) = me.upgrade() else { return };
                    match read {
                        Ok(Some(paths)) => {
                            shared.handle(|_| paths.into_iter().map(Raw::Changed).collect());
                        }
                        Ok(None) => shared.handle(|_| vec![Raw::Rescan(Some(key.clone()))]),
                        Err(error) => {
                            shared.handle(|state| {
                                state.native.lost(&key, serial);
                                vec![Raw::Failed(Some(key.clone()), error)]
                            });
                            return;
                        }
                    }
                }
            },
            async move { stopped.wait().await },
        )));
        Ok(Handle {
            serial,
            refs: 0,
            identity: identity(path),
            stop,
        })
    }

    fn release(&mut self, held: Held) {
        for (map, key) in [
            (&mut self.trees, held.tree),
            (&mut self.parents, held.parent),
        ] {
            let Some((path, serial)) = key else { continue };
            let Some(handle) = map.get_mut(&path).filter(|h| h.serial == serial) else {
                continue;
            };
            handle.refs -= 1;
            if handle.refs == 0 {
                map.remove(&path);
            }
        }
    }

    /// The handle opened as `serial` at `path` failed: its holders open
    /// another as they rearm.
    fn lost(&mut self, path: &Path, serial: u64) {
        for map in [&mut self.trees, &mut self.parents] {
            if map.get(path).is_some_and(|h| h.serial == serial) {
                map.remove(path);
            }
        }
    }
}
