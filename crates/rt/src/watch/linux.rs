//! inotify: one descriptor for the hub, waited on by the runtime's
//! reactor, and one watch per directory, counted across the subscriptions
//! that need it. A tree is walked and watched directory by directory; an
//! entered subscription watches only what it enters. Above each root its
//! nearest existing parent is watched for entries coming and going, and
//! the router keeps only those naming the root.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use rustix::io::Errno;

use super::{Raw, Scope, Shared, Sub, Unavailable, existing_parent};
use crate::{Async, Rt, Wakeup};

type Wd = i32;

/// Why a subscription holds a watch.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Role {
    /// A directory of its own, whose entries it hears about.
    Dir,
    /// The directory above its root, for the root coming and going.
    Parent,
}

const DIR: WatchFlags = WatchFlags::CREATE
    .union(WatchFlags::DELETE)
    .union(WatchFlags::MODIFY)
    .union(WatchFlags::ATTRIB)
    .union(WatchFlags::MOVED_FROM)
    .union(WatchFlags::MOVED_TO)
    .union(WatchFlags::DELETE_SELF)
    .union(WatchFlags::MOVE_SELF);
const PARENT: WatchFlags = WatchFlags::CREATE
    .union(WatchFlags::DELETE)
    .union(WatchFlags::MOVED_FROM)
    .union(WatchFlags::MOVED_TO)
    .union(WatchFlags::DELETE_SELF)
    .union(WatchFlags::MOVE_SELF);
const OPTIONS: WatchFlags = WatchFlags::ONLYDIR
    .union(WatchFlags::DONT_FOLLOW)
    .union(WatchFlags::EXCL_UNLINK);

pub(super) struct Native {
    me: Weak<Shared>,
    rt: Rt,
    /// Open while any directory is watched.
    open: Option<Open>,
    dirs: HashMap<Wd, Watched>,
    held: HashMap<u64, HashSet<(Wd, Role)>>,
    /// How many watches the OS grants, to run out of in a test.
    #[cfg(test)]
    pub(super) budget: Option<usize>,
    #[cfg(test)]
    pub(super) walks: usize,
}

struct Open {
    fd: Arc<Async<OwnedFd>>,
    stop: Arc<Wakeup>,
}

impl Drop for Open {
    fn drop(&mut self) {
        self.stop.notify();
    }
}

struct Watched {
    path: PathBuf,
    dirs: usize,
    parents: usize,
}

/// One inotify record, owned.
struct Note {
    wd: Wd,
    flags: ReadFlags,
    name: Option<OsString>,
}

impl Native {
    pub(super) fn new(me: Weak<Shared>, rt: Rt) -> Self {
        Self {
            me,
            rt,
            open: None,
            dirs: HashMap::new(),
            held: HashMap::new(),
            #[cfg(test)]
            budget: None,
            #[cfg(test)]
            walks: 0,
        }
    }

    pub(super) fn arm(&mut self, id: u64, sub: &Sub) -> Result<(), Unavailable> {
        let mut held = HashSet::new();
        let result = self.arm_into(&mut held, sub);
        // Held anew before the old ones go, so what both need stays open.
        for (wd, role) in self.held.insert(id, held).unwrap_or_default() {
            self.release(wd, role);
        }
        self.close_if_idle();
        result
    }

    fn arm_into(&mut self, held: &mut HashSet<(Wd, Role)>, sub: &Sub) -> Result<(), Unavailable> {
        if let Some(parent) = existing_parent(&sub.real) {
            match self.hold(held, parent, Role::Parent) {
                Ok(()) | Err(Unavailable::Gone) => {}
                Err(error) => return Err(error),
            }
        }
        self.hold(held, &sub.real, Role::Dir)?;
        if sub.scope == Scope::Tree {
            self.walk(held, &sub.real, sub)?;
        }
        Ok(())
    }

    pub(super) fn disarm(&mut self, id: u64) {
        for (wd, role) in self.held.remove(&id).unwrap_or_default() {
            self.release(wd, role);
        }
        self.close_if_idle();
    }

    pub(super) fn enter(&mut self, id: u64, dir: &Path) -> Result<(), Unavailable> {
        let mut held = self.held.remove(&id).unwrap_or_default();
        let result = self.hold(&mut held, dir, Role::Dir);
        self.held.insert(id, held);
        result
    }

    /// Watches `dir`, which appeared under `sub`'s tree, and what it holds.
    pub(super) fn grow(&mut self, id: u64, sub: &Sub, dir: &Path) -> Result<(), Unavailable> {
        let mut held = self.held.remove(&id).unwrap_or_default();
        let result =
            (self.hold(&mut held, dir, Role::Dir)).and_then(|()| self.walk(&mut held, dir, sub));
        self.held.insert(id, held);
        match result {
            Err(Unavailable::Limit) => Err(Unavailable::Limit),
            // Gone again, or unreadable: nothing in it to hear of.
            _ => Ok(()),
        }
    }

    pub(super) fn watches(&self) -> usize {
        self.dirs.len()
    }

    /// Watches every directory under `dir`, each before it is read, so
    /// that what is made in one meanwhile is heard of.
    fn walk(
        &mut self,
        held: &mut HashSet<(Wd, Role)>,
        dir: &Path,
        sub: &Sub,
    ) -> Result<(), Unavailable> {
        #[cfg(test)]
        {
            self.walks += 1;
        }
        let mut stack = vec![dir.to_owned()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !entry.file_type().is_ok_and(|t| t.is_dir()) || sub.excluded(&path) {
                    continue;
                }
                match self.hold(held, &path, Role::Dir) {
                    Ok(()) => stack.push(path),
                    Err(Unavailable::Limit) => return Err(Unavailable::Limit),
                    Err(_) => {}
                }
            }
        }
        Ok(())
    }

    fn hold(
        &mut self,
        held: &mut HashSet<(Wd, Role)>,
        path: &Path,
        role: Role,
    ) -> Result<(), Unavailable> {
        #[cfg(test)]
        if self.budget.is_some_and(|budget| self.dirs.len() >= budget)
            && !self.dirs.values().any(|w| w.path == path)
        {
            return Err(Unavailable::Limit);
        }
        let fd = self.open()?;
        let mask = match role {
            Role::Dir => DIR,
            Role::Parent => PARENT,
        };
        // Added to what the directory is watched for already.
        let wd = inotify::add_watch(fd.get_ref(), path, mask | OPTIONS | WatchFlags::MASK_ADD)
            .map_err(unavailable)?;
        let watched = self.dirs.entry(wd).or_insert_with(|| Watched {
            path: path.to_owned(),
            dirs: 0,
            parents: 0,
        });
        if held.insert((wd, role)) {
            match role {
                Role::Dir => watched.dirs += 1,
                Role::Parent => watched.parents += 1,
            }
        }
        Ok(())
    }

    fn release(&mut self, wd: Wd, role: Role) {
        let Some(watched) = self.dirs.get_mut(&wd) else {
            return;
        };
        match role {
            Role::Dir => watched.dirs -= 1,
            Role::Parent => watched.parents -= 1,
        }
        let Some(open) = &self.open else { return };
        if watched.dirs == 0 && watched.parents == 0 {
            self.dirs.remove(&wd);
            let _ = inotify::remove_watch(open.fd.get_ref(), wd);
        } else if role == Role::Dir && watched.dirs == 0 {
            // Back to a parent's events only (replacing the mask, not
            // adding to it), unless the path names another directory by now.
            match inotify::add_watch(open.fd.get_ref(), &watched.path, PARENT | OPTIONS) {
                Ok(same) if same == wd => {}
                Ok(other) if !self.dirs.contains_key(&other) => {
                    let _ = inotify::remove_watch(open.fd.get_ref(), other);
                }
                _ => {}
            }
        }
    }

    fn open(&mut self) -> Result<Arc<Async<OwnedFd>>, Unavailable> {
        if let Some(open) = &self.open {
            return Ok(Arc::clone(&open.fd));
        }
        let fd =
            inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK).map_err(|e| match e {
                Errno::MFILE | Errno::NFILE => Unavailable::Limit,
                e => Unavailable::Io(e.into()),
            })?;
        let fd = Arc::new(Async::for_reading(&self.rt.reactor(), fd).map_err(Unavailable::Io)?);
        let stop = Arc::new(Wakeup::new());
        let (me, reading, stopped) = (self.me.clone(), Arc::clone(&fd), Arc::clone(&stop));
        self.rt.spawn(Box::pin(crate::or(
            async move {
                let mut buffer = vec![MaybeUninit::uninit(); 16 << 10];
                loop {
                    let read = reading.read_with(|fd| notes(fd, &mut buffer)).await;
                    let Some(shared) = me.upgrade() else { return };
                    match read {
                        Ok(notes) => shared.handle(|state| state.native.translate(notes)),
                        Err(error) => {
                            shared.handle(|state| {
                                state.native.lost();
                                vec![Raw::Failed(None, error)]
                            });
                            return;
                        }
                    }
                }
            },
            async move { stopped.wait().await },
        )));
        self.open = Some(Open {
            fd: Arc::clone(&fd),
            stop,
        });
        Ok(fd)
    }

    fn close_if_idle(&mut self) {
        if self.dirs.is_empty() {
            self.open = None;
        }
    }

    /// The descriptor failed: every watch is gone with it.
    fn lost(&mut self) {
        self.open = None;
        self.dirs.clear();
        self.held.values_mut().for_each(HashSet::clear);
    }

    fn translate(&mut self, notes: Vec<Note>) -> Vec<Raw> {
        let mut raws = Vec::new();
        let mut freed = false;
        for note in notes {
            if note.flags.contains(ReadFlags::QUEUE_OVERFLOW) {
                raws.push(Raw::Rescan(None));
                continue;
            }
            if note.flags.contains(ReadFlags::IGNORED) {
                if let Some(path) = self.forget(note.wd) {
                    // Dropped late (it was held open), its path may name a
                    // directory made since and watched anew.
                    if !self.dirs.values().any(|w| w.path == path) {
                        raws.push(Raw::Unwatched(path));
                    }
                    freed = true;
                }
                continue;
            }
            let Some(watched) = self.dirs.get(&note.wd) else {
                continue;
            };
            let Some(name) = note.name else {
                // The directory itself: removed, moved, or its attributes.
                raws.push(Raw::Changed(watched.path.clone()));
                continue;
            };
            let path = watched.path.join(name);
            let dir = note.flags.contains(ReadFlags::ISDIR);
            if dir
                && note
                    .flags
                    .intersects(ReadFlags::CREATE | ReadFlags::MOVED_TO)
            {
                raws.push(Raw::Created(path));
                continue;
            }
            if dir && note.flags.contains(ReadFlags::MOVED_FROM) {
                // Its watches follow it out, naming paths it no longer has.
                raws.extend(self.forget_under(&path).into_iter().map(Raw::Unwatched));
            }
            raws.push(Raw::Changed(path));
        }
        // However many went, those out of watches try again once.
        if freed {
            raws.push(Raw::Freed);
        }
        self.close_if_idle();
        raws
    }

    /// The OS dropped watch `wd`: the path it watched, if it was one.
    fn forget(&mut self, wd: Wd) -> Option<PathBuf> {
        let watched = self.dirs.remove(&wd)?;
        for held in self.held.values_mut() {
            held.remove(&(wd, Role::Dir));
            held.remove(&(wd, Role::Parent));
        }
        Some(watched.path)
    }

    /// Drops the watches at and under `path`: the paths they watched.
    fn forget_under(&mut self, path: &Path) -> Vec<PathBuf> {
        let gone: Vec<Wd> = (self.dirs.iter())
            .filter(|(_, w)| w.path.starts_with(path))
            .map(|(&wd, _)| wd)
            .collect();
        (gone.into_iter())
            .filter_map(|wd| {
                if let Some(open) = &self.open {
                    let _ = inotify::remove_watch(open.fd.get_ref(), wd);
                }
                self.forget(wd)
            })
            .collect()
    }
}

/// What is ready on `fd`, or `WouldBlock` with nothing.
fn notes(fd: &OwnedFd, buffer: &mut [MaybeUninit<u8>]) -> io::Result<Vec<Note>> {
    let mut reader = inotify::Reader::new(fd, buffer);
    let mut notes = Vec::new();
    loop {
        match reader.next() {
            Ok(event) => notes.push(Note {
                wd: event.wd(),
                flags: event.events(),
                name: (event.file_name())
                    .map(|name| OsStr::from_bytes(name.to_bytes()).to_owned())
                    .filter(|name| !name.is_empty()),
            }),
            Err(Errno::AGAIN) if !notes.is_empty() => return Ok(notes),
            Err(Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

fn unavailable(error: Errno) -> Unavailable {
    match error {
        Errno::NOSPC => Unavailable::Limit,
        Errno::NOENT | Errno::NOTDIR => Unavailable::Gone,
        // Not ours to watch: retrying would not change that.
        Errno::ACCESS | Errno::PERM => Unavailable::Unsupported,
        error => Unavailable::Io(error.into()),
    }
}
