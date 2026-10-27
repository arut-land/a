//! One file watcher for the whole process, which every feature subscribes
//! to: a [`Hub`]. A subscription names a root and how much of it it wants
//! ([`Watch`]); the hub keeps one native watch per directory (Linux) or
//! per outermost root (macOS, Windows), shared and counted across
//! subscriptions, and closes it with its last subscriber.
//!
//! Sinks run on the runtime's tasks, never on a native thread, one batch
//! at a time. Pending paths are deduplicated; excess detail becomes a
//! rescan, preserving availability transitions. Nothing is debounced here.
//! The hub follows its roots itself: a root that appears, is replaced or
//! goes away is rearmed by the hub
//! before the subscriber hears [`Event::Rescan`] or [`Unavailable::Gone`],
//! so subscribers never refresh a watch. Only an I/O failure retries on a
//! timer; a watch limit or an unsupported filesystem waits for something
//! to change (a directory removed, a new subscription).
//!
//! Not observed: a directory above a root's parent renamed or replaced,
//! and a linked root retargeted, until something at the root itself
//! changes (macOS's `WatchRoot` does follow its ancestors). A tree is
//! walked and armed under the hub's lock (Linux), which suits the small
//! trees subscribed whole; a large one is entered as it is walked.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use crate::{Rt, lock};

#[cfg_attr(
    any(target_os = "linux", target_os = "android"),
    path = "watch/linux.rs"
)]
#[cfg_attr(target_os = "macos", path = "watch/macos.rs")]
#[cfg_attr(windows, path = "watch/windows.rs")]
#[cfg_attr(
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        windows
    )),
    path = "watch/none.rs"
)]
mod native;

#[cfg(test)]
mod tests;

/// How much of its root a subscription is told about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    /// Everything under the root.
    Tree,
    /// The root's entries, and those of each directory the subscriber
    /// [entered](Subscription::enter): on Linux only those directories
    /// take a native watch.
    Entered,
}

/// What to watch: a root, how much of it, and the paths under it that
/// never wake the subscriber.
#[derive(Clone, Debug)]
pub struct Watch {
    root: PathBuf,
    scope: Scope,
    exclude: Vec<PathBuf>,
}

impl Watch {
    /// Everything under `root`.
    pub fn tree(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            scope: Scope::Tree,
            exclude: Vec::new(),
        }
    }

    /// `root`'s entries, and those of the directories entered.
    pub fn entered(root: impl Into<PathBuf>) -> Self {
        Self {
            scope: Scope::Entered,
            ..Self::tree(root)
        }
    }

    /// Leaves out `relative` (to the root) and everything under it: it is
    /// neither watched nor reported.
    #[must_use]
    pub fn excluding(mut self, relative: impl Into<PathBuf>) -> Self {
        self.exclude.push(relative.into());
        self
    }
}

/// What a subscriber is told. Paths are spelled under the root as it was
/// subscribed, whatever links or letter case the filesystem resolved it by.
#[derive(Debug)]
pub enum Event {
    /// Something at each path changed: it may be a file or a directory,
    /// and may be gone. A subscriber looks at what is there now, and at
    /// all of a directory: what a new one holds, or what changed in one
    /// whose changes were coalesced, may not have been reported.
    Changed(Vec<PathBuf>),
    /// Changes were lost, or the root is back or another directory now:
    /// look at the whole root again.
    Rescan,
    /// The root cannot be watched now. It is followed still, and its
    /// return is an [`Event::Rescan`].
    Unavailable(Unavailable),
}

#[derive(Debug)]
pub enum Unavailable {
    /// The OS ran out of watches. Changes may go unreported until a
    /// directory is removed somewhere under a watch, or the subscriber
    /// subscribes again.
    Limit,
    /// The filesystem the root is on reports no changes.
    Unsupported,
    /// The root is not there, or is not a directory.
    Gone,
    /// Watching failed; the hub retries, with backoff, until it succeeds.
    Io(io::Error),
}

/// The process's watcher, shared as an `Arc`.
pub struct Hub(Arc<Shared>);

/// A subscription; dropping it unsubscribes, and the native watches
/// nothing else needs close.
pub struct Subscription {
    hub: Arc<Shared>,
    id: u64,
}

type Sink = Box<dyn FnMut(Vec<Event>) + Send>;

struct Shared {
    rt: Rt,
    state: Mutex<State>,
    /// What native threads report, routed by a task on the runtime.
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "linux", target_os = "android")),
        expect(dead_code)
    )]
    inbox: Mutex<Inbox>,
}

#[derive(Default)]
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", target_os = "android")),
    expect(dead_code)
)]
struct Inbox {
    raws: Vec<Raw>,
    detail_bytes: usize,
    rescanning: bool,
    draining: bool,
}

// One root and its overlapping parent need no index selection.
const DIRECT_SUBSCRIPTIONS: usize = 2;

struct State {
    rt: Rt,
    me: Weak<Shared>,
    subs: BTreeMap<u64, Sub>,
    roots: BTreeMap<PathBuf, BTreeSet<u64>>,
    next: u64,
    #[cfg(test)]
    routing_checks: usize,
    #[cfg(test)]
    index_selections: usize,
    native: native::Native,
}

struct Sub {
    /// As subscribed: how paths are reported.
    root: PathBuf,
    /// As the filesystem resolves it: how the native watch names paths.
    real: PathBuf,
    scope: Scope,
    exclude: Vec<PathBuf>,
    entered: HashSet<PathBuf>,
    status: Status,
    out: Arc<Delivery>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Live(Option<sys::fs::FileIdentity>),
    Gone,
    Limit(Admission),
    Unsupported,
    /// Failed, retried for the `n`th time next.
    Io(u32),
}

/// Which admission must be retried before the subscription is live.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Admission {
    Root,
    Descendant,
}

/// What a native backend reports, in the filesystem's spelling.
#[derive(Debug)]
enum Raw {
    Changed(PathBuf),
    /// A directory made; Windows tells only of a change.
    #[cfg_attr(windows, expect(dead_code))]
    Created(PathBuf),
    /// Lost or coalesced changes under a path, or (`None`) anywhere: every
    /// watch is rearmed.
    Rescan(Option<PathBuf>),
    /// The native watch of a directory is gone, and no other watches what
    /// is at its path now: entered there, it is entered again only when
    /// asked.
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        expect(dead_code)
    )]
    Unwatched(PathBuf),
    /// A watch was removed: capacity is free again.
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        expect(dead_code)
    )]
    Freed,
    /// The native watch over a path, or (`None`) every one, failed.
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android", windows)),
        expect(dead_code)
    )]
    Failed(Option<PathBuf>, io::Error),
}

impl Hub {
    pub fn new(rt: Rt) -> Self {
        Self(Arc::new_cyclic(|me: &Weak<Shared>| Shared {
            rt: Arc::clone(&rt),
            state: Mutex::new(State {
                native: native::Native::new(me.clone(), Arc::clone(&rt)),
                rt,
                me: me.clone(),
                subs: BTreeMap::new(),
                roots: BTreeMap::new(),
                next: 0,
                #[cfg(test)]
                routing_checks: 0,
                #[cfg(test)]
                index_selections: 0,
            }),
            inbox: Mutex::default(),
        }))
    }

    /// The runtime its sinks run on.
    pub fn runtime(&self) -> &Rt {
        &self.0.rt
    }

    /// Watches `watch`, armed before this returns: what changes after is
    /// told to `sink`. A root that is not there yet is not an error; its
    /// arrival is an [`Event::Rescan`].
    pub fn subscribe(
        &self,
        watch: Watch,
        sink: impl FnMut(Vec<Event>) + Send + 'static,
    ) -> Subscription {
        let mut state = lock(&self.0.state);
        let id = state.next;
        state.next += 1;
        state.subs.insert(
            id,
            Sub {
                real: PathBuf::new(),
                root: watch.root,
                scope: watch.scope,
                exclude: watch.exclude,
                entered: HashSet::new(),
                status: Status::Gone,
                out: Arc::new(Delivery {
                    queue: Mutex::default(),
                    sink: Mutex::new(Box::new(sink)),
                }),
            },
        );
        // A root missing is what the subscriber's first look sees too.
        state.rearm(id);
        drop(state);
        Subscription {
            hub: Arc::clone(&self.0),
            id,
        }
    }

    /// The native watches open: inotify watches on Linux, streams on
    /// macOS, directory handles on Windows.
    pub fn watches(&self) -> usize {
        lock(&self.0.state).native.watches()
    }
}

impl Subscription {
    /// Watches `dir` (under the root, as subscribed) too, for a
    /// [`Scope::Entered`] subscription: its entries are reported from now.
    pub fn enter(&self, dir: &Path) {
        let mut state = lock(&self.hub.state);
        let State {
            subs, native, rt, ..
        } = &mut *state;
        let Some(sub) = subs.get_mut(&self.id) else {
            return;
        };
        let Some(real) = sub.entering(dir) else {
            return;
        };
        // Asked again even when entered: a watch the OS has not yet said
        // it dropped may name a directory gone and made again.
        sub.entered.insert(real.clone());
        if let Err(error) = native.enter(self.id, &real) {
            sub.entered.remove(&real);
            // Gone already, or not watchable: nothing is missed.
            if matches!(error, Unavailable::Limit) {
                sub.limited(rt);
            }
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut state = lock(&self.hub.state);
        state.unindex(self.id);
        let sub = state.subs.remove(&self.id);
        state.native.disarm(self.id);
        drop(state);
        // The sink goes after the lock: what it owns may unsubscribe too.
        if let Some(sub) = sub {
            lock(&sub.out.queue).closed = true;
        }
    }
}

impl Shared {
    /// Routes what `report` returns, under the lock it is given.
    fn handle(&self, report: impl FnOnce(&mut State) -> Vec<Raw>) {
        let mut state = lock(&self.state);
        let raws = report(&mut state);
        state.route(raws);
    }

    /// Routes `raws` from a native thread, on a task of the runtime.
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "linux", target_os = "android")),
        expect(dead_code)
    )]
    fn post(self: &Arc<Self>, raws: Vec<Raw>) {
        let mut inbox = lock(&self.inbox);
        for raw in raws {
            inbox.push(raw);
        }
        if std::mem::replace(&mut inbox.draining, true) {
            return;
        }
        let me = Arc::downgrade(self);
        self.rt.spawn(Box::pin(async move {
            while let Some(shared) = me.upgrade() {
                let raws = {
                    let mut inbox = lock(&shared.inbox);
                    if inbox.raws.is_empty() {
                        inbox.draining = false;
                        return;
                    }
                    inbox.detail_bytes = 0;
                    inbox.rescanning = false;
                    std::mem::take(&mut inbox.raws)
                };
                shared.handle(|_| raws);
                crate::yield_now().await;
            }
        }));
    }
}

impl State {
    fn route(&mut self, raws: Vec<Raw>) {
        let mut roots = Vec::new();
        let mut freed = false;
        for raw in raws {
            match raw {
                Raw::Changed(path) => self.changed(&path, false, &mut roots),
                Raw::Created(path) => self.changed(&path, true, &mut roots),
                Raw::Rescan(None) => {
                    for id in self.ids() {
                        let was = self.subs[&id].status;
                        match self.rearm(id) {
                            Armed::Live => self.tell(id, Event::Rescan),
                            Armed::Gone if was != Status::Gone => {
                                self.tell(id, Event::Unavailable(Unavailable::Gone));
                            }
                            Armed::Gone | Armed::Failed => {}
                        }
                    }
                }
                Raw::Rescan(Some(path)) => {
                    for id in self.related(&path) {
                        let sub = &self.subs[&id];
                        let event = if sub.real.starts_with(&path) {
                            Event::Rescan
                        } else if let Some(reported) =
                            sub.visible(&path).then(|| sub.reported(&path)).flatten()
                        {
                            Event::Changed(vec![reported])
                        } else {
                            continue;
                        };
                        self.tell(id, event);
                    }
                }
                Raw::Unwatched(path) => {
                    for id in self.related(&path) {
                        self.subs
                            .get_mut(&id)
                            .expect("indexed")
                            .entered
                            .remove(&path);
                    }
                }
                Raw::Freed => freed = true,
                Raw::Failed(path, error) => {
                    for id in path
                        .as_ref()
                        .map_or_else(|| self.ids(), |path| self.related(path))
                    {
                        self.failed(id, &error);
                    }
                }
            }
        }
        if freed {
            for id in self.ids() {
                if matches!(self.subs[&id].status, Status::Limit(_)) {
                    self.recover(id);
                }
            }
        }
        roots.sort_unstable();
        roots.dedup();
        for id in roots {
            self.root_changed(id);
        }
    }

    fn ids(&self) -> Vec<u64> {
        self.subs.keys().copied().collect()
    }

    /// Ancestors receive visible detail; descendants may have been
    /// replaced or invalidated by a change above their root.
    fn related(&self, path: &Path) -> Vec<u64> {
        let mut ids = Vec::new();
        let mut at = path.parent();
        while let Some(bound) = at {
            let Some((root, subscribers)) = self
                .roots
                .range::<Path, _>((Bound::Unbounded, Bound::Included(bound)))
                .next_back()
            else {
                break;
            };
            if bound.starts_with(root) {
                ids.extend(subscribers);
                at = root.parent();
            } else {
                // An unrelated predecessor's descendants cannot contain
                // an ancestor of this path. Skip to their common prefix.
                at = bound
                    .ancestors()
                    .skip(1)
                    .find(|parent| root.starts_with(parent));
            }
        }
        ids.extend(
            self.roots
                .range::<Path, _>((Bound::Included(path), Bound::Unbounded))
                .take_while(|(root, _)| root.starts_with(path))
                .flat_map(|(_, ids)| ids),
        );
        ids
    }

    fn unindex(&mut self, id: u64) {
        let root = &self.subs[&id].real;
        if let Some(ids) = self.roots.get_mut(root) {
            ids.remove(&id);
            if ids.is_empty() {
                self.roots.remove(root);
            }
        }
    }

    /// Something at `path` changed (`created`, a directory appeared there).
    fn changed(&mut self, path: &Path, created: bool, roots: &mut Vec<u64>) {
        let ids = if self.subs.len() <= DIRECT_SUBSCRIPTIONS {
            None
        } else {
            #[cfg(test)]
            {
                self.index_selections += 1;
            }
            Some(self.related(path))
        };
        let State {
            subs, native, rt, ..
        } = self;
        let mut changed = |id, sub: &mut Sub| {
            #[cfg(test)]
            {
                self.routing_checks += 1;
            }
            // A change under a root thought gone shows it back, even when
            // what made it again was coalesced away.
            if sub.real.starts_with(path)
                || (sub.status == Status::Gone && path.starts_with(&sub.real))
            {
                roots.push(id);
                return;
            }
            // An entered directory that went is entered again only when
            // asked, as Linux's watch on it is gone.
            if sub.entered.contains(path) && !path.is_dir() {
                sub.entered.retain(|dir| !dir.starts_with(path));
            }
            let Some(reported) = sub.visible(path).then(|| sub.reported(path)).flatten() else {
                return;
            };
            // Its directories are watched before it is told of them.
            if created && sub.scope == Scope::Tree && native.grow(id, sub, path).is_err() {
                sub.limited(rt);
            }
            sub.out.push(Event::Changed(vec![reported]), rt);
        };
        if let Some(ids) = ids {
            for id in ids {
                changed(id, subs.get_mut(&id).expect("indexed"));
            }
        } else {
            for (&id, sub) in subs.iter_mut() {
                changed(id, sub);
            }
        }
    }

    /// The root, or a directory above it, changed: it may be gone, back,
    /// or another directory now.
    fn root_changed(&mut self, id: u64) {
        let sub = &self.subs[&id];
        let now = identity(&sub.root);
        match (sub.status, now) {
            (Status::Live(Some(was)), Some(now)) if was == now => {}
            // What is missing above it may have moved: follow it closer.
            // A backend watches above the root before it looks at it, so a
            // root made meanwhile is armed now or heard of next.
            (Status::Gone, None) => {
                if self.rearm(id) == Armed::Live {
                    self.tell(id, Event::Rescan);
                }
            }
            // Back already, by the time it is rearmed.
            (_, None) => match self.rearm(id) {
                Armed::Live => self.tell(id, Event::Rescan),
                Armed::Gone | Armed::Failed => {
                    self.tell(id, Event::Unavailable(Unavailable::Gone));
                }
            },
            (_, Some(_)) => match self.rearm(id) {
                Armed::Live => self.tell(id, Event::Rescan),
                Armed::Gone => self.tell(id, Event::Unavailable(Unavailable::Gone)),
                Armed::Failed => {}
            },
        }
    }

    /// Watches what `id`'s root is now in place of what it watched. A
    /// failure other than [`Unavailable::Gone`] is told as it begins, and
    /// an I/O failure retried.
    fn rearm(&mut self, id: u64) -> Armed {
        self.unindex(id);
        let State {
            subs,
            native,
            rt,
            roots,
            ..
        } = self;
        let sub = subs.get_mut(&id).expect("subscribed");
        sub.real = resolve(&sub.root);
        roots.entry(sub.real.clone()).or_default().insert(id);
        sub.entered.clear();
        let identity = identity(&sub.root);
        let was = sub.status;
        let (status, error) = match native.arm(id, sub) {
            Ok(()) => (Status::Live(identity), None),
            Err(Unavailable::Gone) => (Status::Gone, None),
            Err(Unavailable::Limit) => (Status::Limit(Admission::Root), Some(Unavailable::Limit)),
            Err(Unavailable::Unsupported) => (Status::Unsupported, Some(Unavailable::Unsupported)),
            Err(error @ Unavailable::Io(_)) => {
                let attempt = if let Status::Io(n) = was { n + 1 } else { 0 };
                (Status::Io(attempt), Some(error))
            }
        };
        sub.status = status;
        if let Some(error) = error
            && std::mem::discriminant(&was) != std::mem::discriminant(&status)
        {
            sub.out.push(Event::Unavailable(error), rt);
        }
        match status {
            Status::Live(_) => Armed::Live,
            Status::Gone => Armed::Gone,
            Status::Io(attempt) => {
                self.retry(id, attempt);
                Armed::Failed
            }
            Status::Limit(_) | Status::Unsupported => Armed::Failed,
        }
    }

    /// A watch is free again: a subscription that ran out tries again.
    fn recover(&mut self, id: u64) {
        let sub = self.subs.get_mut(&id).expect("listed");
        // An entered subscription enters again as it rescans.
        if sub.scope == Scope::Entered && sub.status == Status::Limit(Admission::Descendant) {
            sub.status = Status::Live(identity(&sub.root));
            self.tell(id, Event::Rescan);
            return;
        }
        if self.rearm(id) == Armed::Live {
            self.tell(id, Event::Rescan);
        }
    }

    /// The native watch of `id` failed: it is told, and rearmed after a
    /// while.
    fn failed(&mut self, id: u64, error: &io::Error) {
        let Some(sub) = self.subs.get_mut(&id) else {
            return;
        };
        if matches!(sub.status, Status::Io(_)) {
            return;
        }
        sub.status = Status::Io(0);
        sub.out
            .push(Event::Unavailable(Unavailable::Io(copy(error))), &self.rt);
        self.retry(id, 0);
    }

    /// Rearms `id` after the backoff of its `attempt`th retry, unless it
    /// was unsubscribed or recovered meanwhile.
    fn retry(&self, id: u64, attempt: u32) {
        let delay = Duration::from_millis(100 << attempt.min(6)).min(Duration::from_secs(5));
        let (me, rt) = (self.me.clone(), Arc::clone(&self.rt));
        self.rt.spawn(Box::pin(async move {
            rt.sleep(delay).await;
            let Some(shared) = me.upgrade() else { return };
            let mut state = lock(&shared.state);
            if state
                .subs
                .get(&id)
                .is_none_or(|s| s.status != Status::Io(attempt))
            {
                return;
            }
            match state.rearm(id) {
                Armed::Live => state.tell(id, Event::Rescan),
                Armed::Gone => state.tell(id, Event::Unavailable(Unavailable::Gone)),
                Armed::Failed => {}
            }
        }));
    }

    fn tell(&self, id: u64, event: Event) {
        if let Some(sub) = self.subs.get(&id) {
            sub.out.push(event, &self.rt);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Armed {
    Live,
    Gone,
    Failed,
}

impl Sub {
    /// The OS ran out of watches for it: told once.
    fn limited(&mut self, rt: &Rt) {
        if !matches!(self.status, Status::Limit(_)) {
            self.status = Status::Limit(Admission::Descendant);
            self.out.push(Event::Unavailable(Unavailable::Limit), rt);
        }
    }

    /// `path` (in the filesystem's spelling) as the subscriber names it.
    fn reported(&self, path: &Path) -> Option<PathBuf> {
        let relative = path.strip_prefix(&self.real).ok()?;
        (!relative.as_os_str().is_empty()).then(|| self.root.join(relative))
    }

    /// Whether `path` (in the filesystem's spelling) is left out.
    fn excluded(&self, path: &Path) -> bool {
        (path.strip_prefix(&self.real))
            .is_ok_and(|relative| self.exclude.iter().any(|e| relative.starts_with(e)))
    }

    /// Whether a change at `path` (strictly under the root) is told.
    fn visible(&self, path: &Path) -> bool {
        if !path.starts_with(&self.real) || self.excluded(path) {
            return false;
        }
        match self.scope {
            Scope::Tree => true,
            Scope::Entered => path
                .parent()
                .is_some_and(|parent| parent == self.real || self.entered.contains(parent)),
        }
    }

    /// `dir` (as the subscriber names it) as the filesystem does, if an
    /// [`Scope::Entered`] subscription can enter it.
    fn entering(&self, dir: &Path) -> Option<PathBuf> {
        if self.scope != Scope::Entered {
            return None;
        }
        let relative = dir.strip_prefix(&self.root).ok()?;
        if relative.as_os_str().is_empty() || self.exclude.iter().any(|e| relative.starts_with(e)) {
            return None;
        }
        Some(self.real.join(relative))
    }
}

/// Where a subscriber's events wait for its sink, which takes one batch
/// at a time.
struct Delivery {
    queue: Mutex<Queue>,
    sink: Mutex<Sink>,
}

#[derive(Default)]
struct Queue {
    events: Vec<Pending>,
    running: bool,
    closed: bool,
}

// Only disposable path detail is capped. Availability transitions delimit
// invalidation epochs and must remain observable even for a stalled sink.
const DETAIL_BYTES: usize = 64 << 10;
const DETAIL_PATHS: usize = 1024;

enum Pending {
    Changes {
        paths: BTreeSet<PathBuf>,
        bytes: usize,
    },
    Rescan,
    Unavailable(Unavailable),
}

impl From<Pending> for Event {
    fn from(pending: Pending) -> Self {
        match pending {
            Pending::Changes { paths, .. } => Self::Changed(paths.into_iter().collect()),
            Pending::Rescan => Self::Rescan,
            Pending::Unavailable(error) => Self::Unavailable(error),
        }
    }
}

impl Queue {
    fn push(&mut self, event: Event) {
        match event {
            Event::Changed(more) => {
                if matches!(self.events.last(), Some(Pending::Rescan)) {
                    return;
                }
                if !matches!(self.events.last(), Some(Pending::Changes { .. })) {
                    self.events.push(Pending::Changes {
                        paths: BTreeSet::new(),
                        bytes: 0,
                    });
                }
                let Some(Pending::Changes { paths, bytes }) = self.events.last_mut() else {
                    unreachable!()
                };
                for path in more {
                    if paths.contains(&path) {
                        continue;
                    }
                    *bytes += path.as_os_str().len();
                    if *bytes > DETAIL_BYTES || paths.len() == DETAIL_PATHS {
                        *self.events.last_mut().expect("pending changes") = Pending::Rescan;
                        break;
                    }
                    paths.insert(path);
                }
            }
            Event::Rescan => match self.events.last_mut() {
                Some(last @ Pending::Changes { .. }) => *last = Pending::Rescan,
                Some(Pending::Rescan) => {}
                _ => self.events.push(Pending::Rescan),
            },
            Event::Unavailable(error) => self.events.push(Pending::Unavailable(error)),
        }
    }
}

#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", target_os = "android")),
    expect(dead_code)
)]
impl Inbox {
    fn push(&mut self, raw: Raw) {
        let bytes = match &raw {
            Raw::Changed(path) | Raw::Created(path) | Raw::Rescan(Some(path)) => {
                path.as_os_str().len()
            }
            Raw::Rescan(None) => DETAIL_BYTES + 1,
            Raw::Freed => {
                if !self.raws.iter().any(|raw| matches!(raw, Raw::Freed)) {
                    self.raws.push(raw);
                }
                return;
            }
            Raw::Unwatched(_) | Raw::Failed(_, _) => {
                self.detail_bytes = 0;
                self.rescanning = false;
                self.raws.push(raw);
                return;
            }
        };
        if self.rescanning {
            return;
        }
        self.detail_bytes += bytes;
        if self.detail_bytes > DETAIL_BYTES || self.raws.len() >= DETAIL_PATHS {
            while matches!(
                self.raws.last(),
                Some(Raw::Changed(_) | Raw::Created(_) | Raw::Rescan(_))
            ) {
                self.raws.pop();
            }
            self.raws.push(Raw::Rescan(None));
            self.rescanning = true;
        } else {
            self.raws.push(raw);
        }
    }
}

impl Delivery {
    fn push(self: &Arc<Self>, event: Event, rt: &Rt) {
        let mut queue = lock(&self.queue);
        if queue.closed {
            return;
        }
        queue.push(event);
        if std::mem::replace(&mut queue.running, true) {
            return;
        }
        let delivery = Arc::clone(self);
        rt.spawn(Box::pin(async move {
            loop {
                let batch = {
                    let mut queue = lock(&delivery.queue);
                    if queue.closed || queue.events.is_empty() {
                        queue.running = false;
                        return;
                    }
                    std::mem::take(&mut queue.events)
                        .into_iter()
                        .map(Event::from)
                        .collect()
                };
                (lock(&delivery.sink))(batch);
                crate::yield_now().await;
            }
        }));
    }
}

/// `root` as the filesystem resolves it, links followed and letter case
/// as on disk; a part that is missing is kept as written, under what
/// resolves of the rest.
fn resolve(root: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut at = root;
    loop {
        if let Ok(real) = at.canonicalize() {
            return missing
                .iter()
                .rev()
                .fold(real, |path, part| path.join(part));
        }
        let (Some(parent), Some(name)) = (at.parent(), at.file_name()) else {
            return root.to_owned();
        };
        missing.push(name);
        at = parent;
    }
}

/// What directory `root` is now, if it is one.
fn identity(root: &Path) -> Option<sys::fs::FileIdentity> {
    std::fs::metadata(root)
        .is_ok_and(|meta| meta.is_dir())
        .then(|| sys::fs::identity(root).ok())
        .flatten()
}

fn copy(error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), error.to_string())
}

/// The nearest directory strictly above `path` that exists.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", windows)),
    expect(dead_code)
)]
fn existing_parent(path: &Path) -> Option<&Path> {
    path.ancestors().skip(1).find(|dir| dir.is_dir())
}
