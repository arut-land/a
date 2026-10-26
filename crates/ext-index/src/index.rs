//! One [`WorkspaceIndex`] per workspace, warmed by session lifecycle hooks
//! or opened by the first query. File walks, content builds and persistence
//! run on the runtime's blocking pool. At linger expiry, a configured store
//! persists changed state and releases the large in-memory structures.
//!
//! The file list is a snapshot ([`Files`] behind an `Arc`): a search reads
//! the one it took, and a change copies it only while a search still holds
//! the old one. Content is built in chunks within the workspace's memory
//! budget; above that, grep reads every file instead.
//!
//! What the root is decides how it is kept ([`sys::fs::Tree`], asked as
//! the first walk begins): a volume, a home or a system tree is never
//! walked, and on a root whose filesystem's notifications miss what other
//! machines do the watcher sees only what is done here: a search of files
//! [`REVALIDATE`] old is answered from them as another walk starts, and
//! grep reads every file.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ext::{Cancel, WorkspaceShared};
use rt::watch::{Event, Hub, Subscription, Unavailable, Watch};
use rt::{BlockingClass, Rt};
use sys::fs::Tree;

use crate::disk;
use crate::fff::grep::{self, Grep, GrepMatch, GrepOptions, Known, Prefilter};
use crate::fff::index::{
    BigramFilter, BigramIndexBuilder, BigramOverlay, SKIP_INDEX_MIN_DENSITY_PCT, build_bytes,
    extract_bigrams, fuzzy_candidates, literal_candidates, regex_candidates,
};
use crate::fff::query::{Constraint, FFFQuery, FuzzyQuery, QueryParser};
use crate::fff::types::{Files, Found, recent_first};
use crate::fff::walk::{Rules, is_git_path, relative};
use crate::fff::{MAX_FILE_SIZE, MAX_INDEXABLE_FILE_SIZE, score};

/// What a workspace's index may use, and how long it outlives its last
/// use.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The most the file list and content index of one workspace may
    /// take, in bytes. A content index that would take more is not
    /// built: grep reads every file instead.
    pub memory: usize,
    /// How long an index stays open after it was last used.
    pub linger: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory: 128 << 20,
            linger: Duration::from_secs(30),
        }
    }
}

/// The open indexes, one per workspace.
pub struct Indexes {
    limits: Limits,
    dir: Option<PathBuf>,
    opened: OnceLock<Box<Opened>>,
}

struct Opened {
    open: WorkspaceShared<WorkspaceIndex>,
    store: Option<Arc<Store>>,
}

/// Where indexes are kept between uses, and those at rest there: each
/// keeps its subscription, so what changes meanwhile is known when it is
/// loaded again.
struct Store {
    dir: PathBuf,
    resting: Mutex<BTreeMap<PathBuf, Arc<WorkspaceIndex>>>,
}

impl Store {
    fn release_watches(&self) -> bool {
        let subscriptions: Vec<_> = lock(&self.resting)
            .values()
            .filter_map(|index| {
                let mut state = index.state();
                if state.files.is_some() {
                    return None;
                }
                state.stored = Stored::Unseen;
                state.pending.rescan = true;
                state.watching.take()
            })
            .collect();
        let released = !subscriptions.is_empty();
        drop(subscriptions);
        released
    }

    fn retire(&self, now: Instant) {
        let retired: Vec<_> = {
            let mut resting = lock(&self.resting);
            let stale: Vec<_> = resting
                .iter()
                .filter_map(|(root, index)| {
                    let state = index.state();
                    (state.files.is_none()
                        && state
                            .last_used
                            .is_some_and(|at| now.duration_since(at) > disk::MAX_AGE))
                    .then(|| root.clone())
                })
                .collect();
            stale
                .into_iter()
                .filter_map(|root| resting.remove(&root))
                .collect()
        };
        // Unsubscribing may deliver events to other workspaces.
        drop(retired);
    }
}

impl Indexes {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            dir: None,
            opened: OnceLock::new(),
        }
    }

    /// Indexes kept in `dir` (the daemon's cache) between uses: one that
    /// nothing has used for [`Limits::linger`] is written there and its
    /// memory let go, and loaded again by the next use, with only what
    /// changed since looked at again. Without one, it is dropped, and
    /// walked and built again.
    #[must_use]
    pub fn kept_in(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dir = Some(dir.into());
        self
    }

    /// The index of the workspace at `root`, opened now if no one has it
    /// open, and kept open for [`Limits::linger`] from now.
    pub fn open(&self, root: &Path, rt: Rt, hub: Arc<Hub>) -> Arc<WorkspaceIndex> {
        let opened = self.opened.get_or_init(|| {
            Box::new(Opened {
                open: WorkspaceShared::default(),
                store: self.dir.as_ref().map(|dir| {
                    Arc::new(Store {
                        dir: dir.clone(),
                        resting: Mutex::default(),
                    })
                }),
            })
        });
        let index = opened.open.get_or_make(root, |own| WorkspaceIndex {
            root: Arc::from(root),
            rt,
            hub,
            limits: self.limits,
            store: opened
                .store
                .as_ref()
                .map(|store| (Arc::downgrade(store), disk::Place::new(&store.dir, root))),
            state: Mutex::new(State {
                stored: if opened.store.is_some() {
                    Stored::Unseen
                } else {
                    Stored::None
                },
                ..State::default()
            }),
            changed: rt::Notify::new(),
            closed: Arc::default(),
            own: own.clone(),
        });
        index.used();
        index
    }

    /// Opens the index of the workspace at `root` and gets it ready, file
    /// list and content index, without waiting for either: a workspace is
    /// in use, and its first search or grep should find them there. All of
    /// it runs on the blocking pool; a search meanwhile waits for the file
    /// list, a grep for the content index.
    pub fn warm(&self, root: &Path, rt: Rt, hub: Arc<Hub>) {
        let index = self.open(root, rt, hub);
        let rt = Arc::clone(&index.rt);
        rt.spawn(Box::pin(async move {
            // A refused root has nothing to get ready.
            let _ = index.content().await;
        }));
    }
}

/// A workspace's files and, once a grep asked for it, its content index.
pub struct WorkspaceIndex {
    root: Arc<Path>,
    rt: Rt,
    hub: Arc<Hub>,
    limits: Limits,
    store: Option<(Weak<Store>, disk::Place)>,
    state: Mutex<State>,
    /// Notified as a walk or a content index is installed.
    changed: rt::Notify,
    /// Set as the index goes: the work it handed off stops.
    closed: Arc<AtomicBool>,
    own: Weak<WorkspaceIndex>,
}

impl Drop for WorkspaceIndex {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
    }
}

#[derive(Default)]
struct State {
    files: Option<Arc<Files>>,
    /// How the last walk walked: what changes are looked at by.
    rules: Option<Arc<Rules>>,
    /// Counts the walks installed: what a content index was built from.
    epoch: u64,
    busy: Busy,
    content: Content,
    pending: Pending,
    /// What the last walk installed watches.
    watching: Option<Arc<Watching>>,
    watch_limited: bool,
    last_used: Option<Instant>,
    keeper: bool,
    /// What the root is, from the first walk on.
    tree: Option<Tree>,
    /// When the last walk was installed: how old the files of an unwatched
    /// root are. From its end, so a walk longer than [`REVALIDATE`] is not
    /// stale as it lands.
    walked_at: Option<Instant>,
    /// What of it is on disk while the file list is not in memory.
    stored: Stored,
    /// The files or content index changed since they were last written.
    dirty: bool,
    /// The build of the content index the bigrams file holds.
    bigrams_on_disk: Option<u64>,
    snapshot: Option<uuid::Uuid>,
    /// Earliest observation represented by this snapshot, never its write time.
    observed: u64,
}

/// What a closed index has on disk, and what changed since it was written.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
enum Stored {
    /// Nothing: the first use walks.
    #[default]
    None,
    /// Whatever the last daemon wrote, if anything: loaded, then checked
    /// against a walk, only the files that differ looked at again.
    Unseen,
    /// Written as the index came to rest, its subscription kept: what
    /// changed since is in [`State::pending`].
    Watched(uuid::Uuid),
}

/// The index's subscriptions: the tree, entered as it is walked, and the
/// repository's `info/`, whose `exclude` decides what the walk finds.
struct Watching {
    tree: Subscription,
    _info: Subscription,
}

/// What changes the file list now: at most one thing at a time.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Busy {
    #[default]
    Idle,
    /// Walking, or loading what is on disk.
    Walking,
    /// Applying a batch of changes.
    Applying,
    /// Writing the index to disk: changes wait, so what is written is all
    /// the index knows.
    Persisting,
}

#[derive(Default)]
enum Content {
    #[default]
    Absent,
    /// Being built from the files of walk `epoch`; `touched`, the paths
    /// that changed meanwhile, to be read again into it after.
    Building {
        epoch: u64,
        touched: Vec<String>,
    },
    Ready(Arc<ContentIndex>),
    /// Over [`Limits::memory`]: grep reads every file.
    Refused,
}

/// The content prefilter of a walk, and what changed since.
#[derive(Clone)]
pub(crate) struct ContentIndex {
    /// Which build it is: the bigrams file names the one it holds.
    id: u64,
    filter: Arc<BigramFilter>,
    overlay: BigramOverlay,
    /// One bit per walked file: binary, never searched.
    binary: Arc<Vec<u64>>,
    /// Partially read or unreadable content cannot exclude a candidate.
    unknown: Arc<Vec<u64>>,
}

#[derive(Default)]
struct Pending {
    paths: BTreeSet<PathBuf>,
    rescan: bool,
}

/// The files of a search, best first.
#[derive(Debug)]
pub struct Searched {
    pub files: Vec<SearchedFile>,
    /// How many files matched at all.
    pub matched: usize,
    /// How many files there are.
    pub total: usize,
}

#[derive(Debug)]
pub struct SearchedFile {
    pub path: String,
    pub score: i32,
    /// Byte ranges of `path` that matched.
    pub ranges: Vec<(u32, u32)>,
}

/// One file's matches in a grep.
#[derive(Debug)]
pub struct Grepped {
    pub path: String,
    pub matches: Vec<GrepMatch>,
}

/// How a grep ended.
#[derive(Debug, Default)]
pub struct GrepSummary {
    pub searched: usize,
    pub matched_files: usize,
    pub matches: usize,
    /// Why it stopped with files left to read.
    pub stopped: Option<Stopped>,
    /// The content index narrowed which files were read.
    pub prefiltered: bool,
    pub regex_error: Option<String>,
}

/// Why a grep stopped before it read every file it would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// At its limit of matches.
    AtLimit,
    /// Cancelled: what it found is all it read, not all there is.
    Cancelled,
}

/// Files a grep job reads at once: few enough that a cancel or a full
/// page stops it soon, enough that a job outweighs its scheduling.
const GREP_CHUNK: usize = 256;
/// What a grep job holds at most: the largest file it reads, twice.
const GREP_JOB_BYTES: usize = 20 << 20;
/// Paths a search scores in one part, at least.
const SEARCH_PART: usize = 16 << 10;
/// Grep and build jobs in flight at once, and parts of a search.
const WINDOW: usize = 4;
/// Files one content-index job reads: a multiple of 64, so chunks merge
/// by whole bitset words.
const BUILD_CHUNK: usize = 4096;
/// How old the files of an unwatched root may be before a search starts
/// another walk of it: an @-mention completing starts one at most.
const REVALIDATE: Duration = Duration::from_secs(2);
/// Changed paths an index at rest keeps, at most, before it lets them go
/// for a walk that checks every file once it is loaded.
const RESTING_PATHS: usize = 4096;

/// What a walk is for.
#[derive(Clone, Copy)]
enum Walk {
    /// The files found replace those the index has.
    Replace,
    /// Load what the store has instead, if it is there.
    Load,
    /// Check the files loaded from the store, written at `taken`
    /// (seconds since the epoch), against those found.
    Verify { taken: u64 },
}

impl WorkspaceIndex {
    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn me(&self) -> Option<Arc<Self>> {
        self.own.upgrade()
    }

    /// Keeps the index open for [`Limits::linger`] from now: one task
    /// holds it, sleeping until the last use is that old, then lets it
    /// close, or, kept in a store, come to rest there.
    fn used(&self) {
        let mut state = self.state();
        state.last_used = Some(self.rt.now());
        if state.keeper {
            return;
        }
        let Some(me) = self.me() else { return };
        state.keeper = true;
        drop(state);
        self.rt.spawn(Box::pin(async move {
            loop {
                let now = me.rt.now();
                let due = {
                    let mut state = me.state();
                    let due = state.last_used.map_or(now, |used| used + me.limits.linger);
                    if due <= now && (me.store.is_none() || state.files.is_none()) {
                        state.keeper = false;
                        break;
                    }
                    due
                };
                if due > now {
                    me.rt.sleep(due - now).await;
                } else if me.rest().await {
                    break;
                }
            }
            drop(me);
        }));
    }

    /// Writes the index to its store, unless nothing changed since it
    /// last was, and lets go of the file list and content index, keeping
    /// the subscription that tells what changes until it is loaded again.
    /// It stays open instead (`false`), for another linger, if it is busy
    /// or used meanwhile; it closes, as with no store, if it cannot be
    /// written.
    #[expect(
        clippy::too_many_lines,
        reason = "the algorithm keeps its ordered stages together"
    )]
    async fn rest(&self) -> bool {
        let Some((store, place)) = self
            .store
            .as_ref()
            .and_then(|(store, place)| store.upgrade().map(|store| (store, place)))
        else {
            let mut state = self.state();
            if state
                .last_used
                .is_some_and(|used| self.rt.now() < used + self.limits.linger)
            {
                return false;
            }
            state.keeper = false;
            return true;
        };
        let (files, content, used, mut write_bigrams, observed, snapshot) = {
            let mut state = self.state();
            if state.busy != Busy::Idle || matches!(state.content, Content::Building { .. }) {
                state.last_used = Some(self.rt.now());
                return false;
            }
            let Some(files) = state.files.clone() else {
                state.keeper = false;
                return true;
            };
            let content = match &state.content {
                Content::Ready(content) => Some(Arc::clone(content)),
                Content::Absent | Content::Refused | Content::Building { .. } => None,
            };
            let write_bigrams = content.as_ref().and_then(|content| {
                (state.bigrams_on_disk != Some(content.id)).then_some(WriteBigrams)
            });
            state.busy = Busy::Persisting;
            (
                state.dirty.then_some(files),
                content,
                state.last_used,
                write_bigrams,
                state.observed,
                state.snapshot,
            )
        };
        let (files, content, compacted) = self.compact(files, content).await;
        if compacted && content.is_some() {
            write_bigrams = Some(WriteBigrams);
        }
        let snapshot = if files.is_some() {
            Some(uuid::Uuid::now_v7())
        } else {
            snapshot
        };
        let written = match &files {
            None => Ok(()),
            Some(files) => {
                self.persist(
                    store.dir.clone(),
                    place.clone(),
                    Arc::clone(files),
                    content.clone(),
                    write_bigrams,
                    observed,
                    snapshot.expect("a write has an identity"),
                )
                .await
            }
        };
        let mut state = self.state();
        if state.busy == Busy::Persisting {
            state.busy = Busy::Idle;
        }
        if written.is_err() {
            state.stored = Stored::None;
            state.files = None;
            state.content = Content::Absent;
            state.keeper = false;
            drop(state);
            self.changed.notify();
            return true;
        }
        state.dirty = false;
        state.snapshot = snapshot;
        if compacted {
            state.files = files;
            state.content = content
                .as_ref()
                .map_or(Content::Absent, |c| Content::Ready(Arc::clone(c)));
        }
        if let Some(content) = &content {
            state.bigrams_on_disk = Some(content.id);
        }
        if state.last_used != used || matches!(state.content, Content::Building { .. }) {
            state.last_used = Some(self.rt.now());
            drop(state);
            self.apply();
            return false;
        }
        state.files = None;
        state.content = Content::Absent;
        state.walked_at = None;
        let release = if state.watch_limited {
            state.stored = Stored::Unseen;
            state.watching.take()
        } else {
            state.stored = Stored::Watched(snapshot.expect("stored snapshot has an identity"));
            None
        };
        state.keeper = false;
        drop(state);
        self.changed.notify();
        drop(release);
        store.retire(self.rt.now());
        if let Some(me) = self.me() {
            lock(&store.resting).insert(self.root.to_path_buf(), me);
        }
        true
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the persistence boundary owns one complete snapshot"
    )]
    async fn persist(
        &self,
        dir: PathBuf,
        place: disk::Place,
        files: Arc<Files>,
        content: Option<Arc<ContentIndex>>,
        write_bigrams: Option<WriteBigrams>,
        observed: u64,
        snapshot: uuid::Uuid,
    ) -> io::Result<()> {
        let root = Arc::clone(&self.root);
        rt::unblock_with(&*self.rt, BlockingClass::Storage, 0, move || {
            sys::fs::private_dir(&dir)?;
            disk::prune(&dir, &place);
            let content = content.as_ref().map(|content| disk::Content {
                id: content.id,
                filter: write_bigrams.map(|WriteBigrams| &*content.filter),
                overlay: &content.overlay,
                binary: &content.binary,
                unknown: &content.unknown,
            });
            disk::write(&place, &root, observed, snapshot, &files, content.as_ref())
        })
        .await
    }

    /// At a quarter of slots changed, compact at rest rather than retaining
    /// tombstones and an ever-growing overlay. Only new files need reading.
    async fn compact(
        &self,
        files: Option<Arc<Files>>,
        content: Option<Arc<ContentIndex>>,
    ) -> (Option<Arc<Files>>, Option<Arc<ContentIndex>>, bool) {
        if let Some(files) = files {
            let changed = files.items().len() - files.live_count() + files.items().len()
                - files.walked_count()
                + content.as_ref().map_or(0, |c| c.overlay.modified_count());
            if changed > files.items().len() / 4 {
                let root = Arc::clone(&self.root);
                let budget = self.limits.memory;
                let (files, content) =
                    rt::unblock_with(&*self.rt, BlockingClass::Storage, 0, move || {
                        let slots = files.compact_slots();
                        let compacted_files = Arc::new(files.compact(&slots));
                        let content = content.and_then(|content| {
                            let mut overlay = content.overlay.clone();
                            let mut binary = vec![0u64; slots.len().div_ceil(64)];
                            let mut unknown = vec![0u64; slots.len().div_ceil(64)];
                            let mut buf = Vec::new();
                            for (new, &old) in slots.iter().enumerate() {
                                let is_binary = if old < files.walked_count() {
                                    if content.unknown[old / 64] & (1 << (old % 64)) != 0 {
                                        unknown[new / 64] |= 1 << (new % 64);
                                    }
                                    content.binary[old / 64] & (1 << (old % 64)) != 0
                                } else {
                                    match content_of(
                                        &root,
                                        files.path(&files.items()[old]),
                                        &mut buf,
                                    ) {
                                        Read::Text(bigrams) => {
                                            overlay.modify_bigrams(old, bigrams);
                                            false
                                        }
                                        Read::Binary => true,
                                        Read::Unknown => {
                                            unknown[new / 64] |= 1 << (new % 64);
                                            false
                                        }
                                    }
                                };
                                if is_binary {
                                    binary[new / 64] |= 1 << (new % 64);
                                }
                            }
                            let overlay_empty = BigramOverlay::new(slots.len());
                            let filter_budget = budget.checked_sub(
                                compacted_files.heap_bytes()
                                    + (binary.len() + unknown.len()) * 8
                                    + overlay_empty.heap_bytes(),
                            )?;
                            let filter = content.filter.compact(&slots, &overlay, filter_budget)?;
                            Some(Arc::new(ContentIndex {
                                id: build_id(),
                                filter: Arc::new(filter),
                                overlay: overlay_empty,
                                binary: Arc::new(binary),
                                unknown: Arc::new(unknown),
                            }))
                        });
                        (compacted_files, content)
                    })
                    .await;
                (Some(files), content, true)
            } else {
                (Some(files), content, false)
            }
        } else {
            (None, content, false)
        }
    }

    /// What the file list and content index take in memory now.
    pub fn resident(&self) -> usize {
        let state = self.state();
        let files = state.files.as_ref().map_or(0, |files| files.heap_bytes());
        let content = match &state.content {
            Content::Ready(content) => {
                content.filter.heap_bytes()
                    + content.overlay.heap_bytes()
                    + (content.binary.len() + content.unknown.len()) * size_of::<u64>()
            }
            Content::Absent | Content::Refused | Content::Building { .. } => 0,
        };
        files + content
    }

    /// The files as the last walk and the changes since left them, after
    /// the first walk if it has not finished. Those of an unwatched root
    /// [`REVALIDATE`] old are answered from while another walk starts. An
    /// error says why the root is never walked.
    async fn files(&self) -> Result<Arc<Files>, String> {
        self.used();
        let now = self.rt.now();
        let (epoch, walk) = {
            let mut state = self.state();
            if let Some(Tree::Refused(refusal)) = state.tree {
                return Err(refused(&self.root, refusal));
            }
            let old = matches!(state.tree, Some(Tree::Unwatched(_)))
                && state.walked_at.is_some_and(|at| now >= at + REVALIDATE);
            if let Some(files) = &state.files
                && !old
            {
                return Ok(Arc::clone(files));
            }
            let walked = state.files.clone();
            let walk = match state.busy {
                Busy::Idle => {
                    state.busy = Busy::Walking;
                    Some(if walked.is_none() && state.stored != Stored::None {
                        Walk::Load
                    } else {
                        Walk::Replace
                    })
                }
                Busy::Walking => None,
                Busy::Applying | Busy::Persisting => {
                    state.pending.rescan = true;
                    None
                }
            };
            if let Some(files) = walked {
                drop(state);
                if let Some(walk) = walk {
                    self.walk(walk);
                }
                return Ok(files);
            }
            (state.epoch, walk)
        };
        if let Some(walk) = walk {
            self.walk(walk);
        }
        let mut walked = None;
        self.changed
            .until(|| {
                let state = self.state();
                walked = match (state.tree, &state.files) {
                    (Some(Tree::Refused(refusal)), _) => Some(Err(refused(&self.root, refusal))),
                    (_, Some(files)) if state.epoch > epoch => Some(Ok(Arc::clone(files))),
                    _ => None,
                };
                walked.is_some()
            })
            .await;
        walked.expect("checked")
    }

    /// Walks the root again on the blocking pool, once the first walk has
    /// found what the root is: a refused one is never walked nor watched,
    /// a watched one is walked into a new subscription, each directory
    /// entered as the walk goes; the old one is kept until the walk is
    /// installed, so nothing that changes meanwhile is missed. Or loads
    /// what the store has of it, which a walk may then check. The caller
    /// set [`Busy::Walking`].
    fn walk(&self, walk: Walk) {
        let weak = Weak::clone(&self.own);
        let (rt, root, closed) = (
            Arc::clone(&self.rt),
            Arc::clone(&self.root),
            Arc::clone(&self.closed),
        );
        let tree = {
            let mut state = self.state();
            if matches!(walk, Walk::Replace) {
                state.observed = seconds(SystemTime::now());
            }
            state.tree
        };
        self.rt.spawn(Box::pin(async move {
            let tree = if let Some(tree) = tree {
                tree
            } else {
                let root = Arc::clone(&root);
                rt::unblock_with(&*rt, BlockingClass::Storage, 0, move || Tree::of(&root)).await
            };
            let watching = {
                let Some(index) = weak.upgrade() else { return };
                index.state().tree = Some(tree);
                if let Tree::Refused(_) = tree {
                    index.state().busy = Busy::Idle;
                    index.changed.notify();
                    return;
                }
                if let Walk::Load = walk {
                    index.load().await;
                    return;
                }
                // On a network or virtual root the watcher sees what is
                // done here (the agent's own edits); walks see the rest.
                Arc::new(index.watch())
            };
            let (rules, files) = walk_tree(&rt, root, closed, Arc::clone(&watching)).await;
            let Some(index) = weak.upgrade() else { return };
            if let Walk::Verify { taken } = walk {
                index.verify(rules, watching, files, taken).await;
                return;
            }
            {
                let mut state = index.state();
                state.files = Some(Arc::new(files));
                state.rules = Some(rules);
                state.watching = Some(watching);
                state.walked_at = Some(index.rt.now());
                state.epoch += 1;
                state.busy = Busy::Idle;
                state.dirty = true;
                // Its bit positions were the old walk's.
                state.content = Content::Absent;
            }
            index.changed.notify();
            index.apply();
        }));
    }

    /// Loads what the store has of the index and installs it, searches
    /// answered from it at once; then applies what the subscription it
    /// kept saw since, or, with none kept (another daemon wrote it) or
    /// changes lost, has a walk check it, looking again only at the files
    /// that differ. With nothing whole there, it walks.
    async fn load(&self) {
        let root = Arc::clone(&self.root);
        let place = self.store.as_ref().map(|(_, place)| place.clone());
        let budget = self.limits.memory;
        let loaded = rt::unblock_with(&*self.rt, BlockingClass::Storage, 0, move || {
            disk::read(&place?, &root, budget)
        })
        .await;
        let Some(loaded) = loaded else {
            self.state().stored = Stored::None;
            self.walk(Walk::Replace);
            return;
        };
        let verify = {
            let mut state = self.state();
            state.bigrams_on_disk = loaded.content.as_ref().map(|content| content.id);
            state.files = Some(Arc::new(loaded.files));
            state.content = loaded.content.map_or(Content::Absent, |content| {
                Content::Ready(Arc::new(ContentIndex {
                    id: content.id,
                    filter: Arc::new(content.filter),
                    overlay: content.overlay,
                    binary: Arc::new(content.binary),
                    unknown: Arc::new(content.unknown),
                }))
            });
            state.epoch += 1;
            state.walked_at = Some(self.rt.now());
            state.dirty = false;
            state.snapshot = Some(loaded.snapshot);
            state.observed = loaded.taken;
            // Unwatched, what the subscription saw is not all that changed.
            let verify = state.stored != Stored::Watched(loaded.snapshot)
                || state.pending.rescan
                || state.rules.is_none()
                || matches!(state.tree, Some(Tree::Unwatched(_)));
            if verify {
                state.pending = Pending::default();
            } else {
                state.busy = Busy::Idle;
            }
            verify
        };
        self.changed.notify();
        if verify {
            self.walk(Walk::Verify {
                taken: loaded.taken,
            });
        } else {
            self.apply();
        }
    }

    /// Installs what a walk found that the files loaded from the store
    /// do not say: those new, changed or gone since. The caller is
    /// walking, so nothing else changes the files meanwhile.
    async fn verify(&self, rules: Arc<Rules>, watching: Arc<Watching>, walked: Files, taken: u64) {
        let (files, read) = {
            let state = self.state();
            let read = matches!(state.content, Content::Ready(_) | Content::Building { .. });
            (state.files.clone().expect("loaded"), read)
        };
        let checking = Arc::clone(&rules);
        let changes = rt::unblock_with(&*self.rt, BlockingClass::Storage, 0, move || {
            differences(&checking, &files, &walked, taken, read)
        })
        .await;
        {
            let mut state = self.state();
            state.rules = Some(rules);
            state.watching = Some(watching);
            state.walked_at = Some(self.rt.now());
        }
        self.install(changes);
        self.state().busy = Busy::Idle;
        self.changed.notify();
        self.apply();
    }

    fn watch(&self) -> Watching {
        self.state().watch_limited = false;
        let sink = || {
            let weak = Weak::clone(&self.own);
            move |events| {
                if let Some(index) = weak.upgrade() {
                    index.changed_on_disk(events);
                }
            }
        };
        Watching {
            tree: (self.hub).subscribe(Watch::entered(&*self.root).excluding(".git"), sink()),
            _info: (self.hub).subscribe(Watch::entered(self.root.join(".git/info")), sink()),
        }
    }

    fn changed_on_disk(&self, events: Vec<Event>) {
        let mut release = None;
        let mut limited = false;
        {
            let mut state = self.state();
            for event in events {
                let paths = match event {
                    Event::Changed(paths) => paths,
                    Event::Rescan | Event::Unavailable(Unavailable::Gone) => {
                        state.pending.rescan = true;
                        continue;
                    }
                    Event::Unavailable(Unavailable::Limit) => {
                        limited = true;
                        state.watch_limited = true;
                        if state.files.is_none() {
                            state.pending.rescan = true;
                            state.stored = Stored::Unseen;
                            release = state.watching.take();
                        }
                        continue;
                    }
                    // Watching is short of watches or retrying: what the
                    // walk found stands, and a recovery is a rescan.
                    Event::Unavailable(_) => continue,
                };
                for path in paths {
                    let Some(relative) = relative(&self.root, &path) else {
                        continue;
                    };
                    if changes_ignore_rules(&relative) {
                        state.pending.rescan = true;
                    } else if !is_git_path(&relative) {
                        state.pending.paths.insert(path);
                    }
                }
            }
            // At rest, a flood (a build, a checkout) is not kept path by
            // path: a walk checks the files once they are loaded.
            if state.files.is_none() && state.pending.paths.len() > RESTING_PATHS {
                state.pending.paths.clear();
                state.pending.rescan = true;
            }
        }
        drop(release);
        if limited
            && let Some(store) = self.store.as_ref().and_then(|(store, _)| store.upgrade())
            && store.release_watches()
        {
            // Retry only after freeing capacity; a permanently exhausted hub
            // must not turn a failed watch into a loop of verification walks.
            self.state().pending.rescan = true;
        }
        self.apply();
    }

    /// Applies the changes seen since, one batch at a time, unless a walk
    /// is still to install (it applies them after) or a batch runs.
    fn apply(&self) {
        let mut state = self.state();
        if state.busy != Busy::Idle || state.files.is_none() {
            return;
        }
        if state.pending.rescan {
            state.pending = Pending::default();
            state.busy = Busy::Walking;
            drop(state);
            self.walk(Walk::Replace);
            return;
        }
        if state.pending.paths.is_empty() {
            drop(state);
            self.changed.notify();
            return;
        }
        let Some(rules) = state.rules.clone() else {
            return;
        };
        let paths = std::mem::take(&mut state.pending.paths);
        let read = matches!(state.content, Content::Ready(_) | Content::Building { .. });
        let watching = state.watching.clone();
        state.busy = Busy::Applying;
        drop(state);
        let seen = rt::unblock_with(&*self.rt, BlockingClass::Storage, 0, move || {
            // A directory that appeared is entered as it is looked at.
            let enter = |dir: &Path| {
                if let Some(watching) = &watching {
                    watching.tree.enter(dir);
                }
            };
            look(&rules, &paths, read, &enter)
        });
        let weak = Weak::clone(&self.own);
        self.rt.spawn(Box::pin(async move {
            let changes = seen.await;
            let Some(index) = weak.upgrade() else { return };
            index.install(changes);
            index.state().busy = Busy::Idle;
            index.apply();
        }));
    }

    fn install(&self, changes: Vec<Change>) {
        let mut state = self.state();
        let State {
            files,
            content,
            pending,
            dirty,
            ..
        } = &mut *state;
        let Some(files) = files else { return };
        if changes.is_empty() {
            return;
        }
        *dirty = true;
        let files = Arc::make_mut(files);
        let walked = files.walked_count();
        let mut ready = match content {
            Content::Ready(ready) => Some(Arc::make_mut(ready)),
            Content::Building { touched, .. } => {
                touched.extend(changes.iter().flat_map(Change::paths));
                None
            }
            Content::Absent | Content::Refused => None,
        };
        let mut unread = Vec::new();
        let mut gone = Vec::new();
        for change in changes {
            let found = match change {
                Change::Gone(path) => {
                    gone.extend(files.remove(&path));
                    gone.extend(files.under(&path));
                    continue;
                }
                Change::Dir(dir, found) => {
                    let now: BTreeSet<&str> = found.iter().map(|(f, _)| f.path.as_str()).collect();
                    let under = files.under(&dir);
                    gone.extend(
                        under
                            .into_iter()
                            .filter(|&slot| !now.contains(files.path(&files.items()[slot]))),
                    );
                    found
                }
                Change::File(found, read) => vec![(found, read)],
            };
            for (found, read) in found {
                let path = found.path.clone();
                let Some((slot, _)) = files.upsert(&found) else {
                    continue;
                };
                let Some(ready) = ready.as_deref_mut().filter(|_| slot < walked) else {
                    continue;
                };
                let bit = 1u64 << (slot % 64);
                match read {
                    Some(Read::Unknown) => {
                        Arc::make_mut(&mut ready.binary)[slot / 64] &= !bit;
                        Arc::make_mut(&mut ready.unknown)[slot / 64] |= bit;
                    }
                    Some(Read::Text(bigrams)) => {
                        Arc::make_mut(&mut ready.unknown)[slot / 64] &= !bit;
                        Arc::make_mut(&mut ready.binary)[slot / 64] &= !bit;
                        ready.overlay.modify_bigrams(slot, bigrams);
                    }
                    Some(Read::Binary) => {
                        Arc::make_mut(&mut ready.binary)[slot / 64] |= bit;
                        Arc::make_mut(&mut ready.unknown)[slot / 64] &= !bit;
                    }
                    // Looked at before the content index was there: looked at again.
                    None => unread.push(path),
                }
            }
        }
        for slot in gone {
            files.remove_slot(slot);
            if let Some(ready) = ready.as_deref_mut() {
                ready.overlay.delete_file(slot);
            }
        }
        let root = &self.root;
        pending
            .paths
            .extend(unread.into_iter().map(|path| root.join(path)));
    }

    /// The files and their content index, built now if no grep has asked
    /// for it since the walk; no index when it would take more than the
    /// workspace may use. Both are taken under one lock: the index's bits
    /// are positions in these files.
    async fn content(&self) -> Result<(Arc<Files>, Option<Arc<ContentIndex>>), String> {
        loop {
            self.files().await?;
            self.changed
                .until(|| {
                    let state = self.state();
                    state.files.is_none()
                        || (!matches!(state.busy, Busy::Walking | Busy::Applying)
                            && state.pending.paths.is_empty()
                            && !state.pending.rescan)
                })
                .await;
            let (files, build) = {
                let mut state = self.state();
                let Some(files) = state.files.clone() else {
                    continue;
                };
                if matches!(state.busy, Busy::Walking | Busy::Applying)
                    || !state.pending.paths.is_empty()
                    || state.pending.rescan
                {
                    continue;
                }
                match &state.content {
                    Content::Ready(content) => return Ok((files, Some(Arc::clone(content)))),
                    Content::Refused => return Ok((files, None)),
                    Content::Building { .. } => (files, None),
                    Content::Absent => {
                        // The whole of both builders, and the chunk builders
                        // in flight beside them.
                        let estimate = build_bytes(files.walked_count())
                            + WINDOW * build_bytes(BUILD_CHUNK)
                            + files.heap_bytes();
                        // Unwatched, an edited file's bigrams would go stale
                        // unseen, and grep would miss what it now holds.
                        let unwatched = matches!(state.tree, Some(Tree::Unwatched(_)));
                        if unwatched || estimate > self.limits.memory {
                            state.content = Content::Refused;
                            return Ok((files, None));
                        }
                        let epoch = state.epoch;
                        state.content = Content::Building {
                            epoch,
                            touched: Vec::new(),
                        };
                        (files, Some(epoch))
                    }
                }
            };
            if let Some(epoch) = build {
                self.build(Arc::clone(&files), epoch);
            }
            self.changed
                .until(|| !matches!(self.state().content, Content::Building { .. }))
                .await;
        }
    }

    fn build(&self, files: Arc<Files>, epoch: u64) {
        let rt = Arc::clone(&self.rt);
        let root = Arc::clone(&self.root);
        let closed = Arc::clone(&self.closed);
        let weak = Weak::clone(&self.own);
        self.rt.spawn(Box::pin(async move {
            let built = build(&rt, &root, files, closed).await;
            let Some(index) = weak.upgrade() else { return };
            {
                let mut state = index.state();
                // A walk since installed other files, and set the content
                // index aside for one built from them.
                if !matches!(state.content, Content::Building { epoch: e, .. } if e == epoch) {
                    return;
                }
                let Content::Building { touched, .. } = std::mem::take(&mut state.content) else {
                    unreachable!("matched above");
                };
                state.content = built.map_or(Content::Refused, |c| Content::Ready(Arc::new(c)));
                state.dirty = true;
                // Publish touched paths atomically with the completed build.
                state
                    .pending
                    .paths
                    .extend(touched.into_iter().map(|path| root.join(path)));
            }
            index.apply();
        }));
    }

    /// The best `limit` files for `query`, as an @-mention completes.
    pub async fn search(&self, query: String, limit: usize) -> Result<Searched, String> {
        let files = self.files().await?;
        let query = Arc::<str>::from(query);
        let (on, asked) = (Arc::clone(&files), Arc::clone(&query));
        let working = rt::unblock_with(&*self.rt, BlockingClass::Cpu, 0, move || {
            Arc::new(score::working(&on, &QueryParser::default().parse(&asked)))
        })
        .await;
        // Scored in parts on the CPU pool: one thread takes several
        // milliseconds over a hundred thousand paths.
        let parts = working.len().div_ceil(SEARCH_PART).clamp(1, WINDOW);
        let size = working.len().div_ceil(parts).max(1);
        let jobs: Vec<_> = (0..parts)
            .map(|part| {
                let (files, query, working) =
                    (Arc::clone(&files), Arc::clone(&query), Arc::clone(&working));
                let job = rt::unblock_with(&*self.rt, BlockingClass::Cpu, 0, move || {
                    let slots = &working
                        [(part * size).min(working.len())..((part + 1) * size).min(working.len())];
                    score::score(&files, slots, &QueryParser::default().parse(&query))
                });
                rt::spawn(&*self.rt, job)
            })
            .collect();
        let mut scored = Some(Vec::new());
        for job in jobs {
            match (job.await, &mut scored) {
                (Some(part), Some(all)) => all.extend(part),
                _ => scored = None,
            }
        }
        rt::unblock_with(&*self.rt, BlockingClass::Cpu, 0, move || {
            let parsed = QueryParser::default().parse(&query);
            let found = match scored {
                Some(scored) => score::sort_and_take(&files, scored, limit),
                None => score::recent(&files, Arc::unwrap_or_clone(working), limit),
            };
            let paths: Vec<&str> = (found.items.iter())
                .map(|(slot, _)| files.path(&files.items()[*slot]))
                .collect();
            let ranges = score::match_ranges(&parsed, &paths);
            Ok(Searched {
                files: (paths.iter().zip(&found.items).zip(ranges))
                    .map(|((path, (_, score)), ranges)| SearchedFile {
                        path: (*path).to_owned(),
                        score: score.total,
                        ranges: ranges.into_vec(),
                    })
                    .collect(),
                matched: found.total_matched,
                total: files.live_count(),
            })
        })
        .await
    }

    /// The files the glob `pattern` matches, most recently modified first:
    /// a pattern with a `/` matches whole relative paths, one without
    /// matches file names.
    pub async fn glob(&self, pattern: String, limit: usize) -> Result<Searched, String> {
        let files = self.files().await?;
        rt::unblock_with(&*self.rt, BlockingClass::Cpu, 0, move || {
            let whole_path = pattern.contains('/');
            let glob = globset::GlobBuilder::new(&pattern)
                .literal_separator(whole_path)
                .build()
                .map_err(|e| e.to_string())?
                .compile_matcher();
            let items = files.items();
            let mut slots: Vec<usize> = (0..items.len())
                .filter(|&slot| {
                    let file = &items[slot];
                    !file.is_deleted()
                        && glob.is_match(if whole_path {
                            files.path(file)
                        } else {
                            files.name(file)
                        })
                })
                .collect();
            let matched = slots.len();
            slots.sort_unstable_by(|a, b| recent_first(&files, &items[*a], &items[*b]));
            slots.truncate(limit);
            Ok(Searched {
                files: (slots.into_iter())
                    .map(|slot| SearchedFile {
                        path: files.path(&items[slot]).to_owned(),
                        score: 0,
                        ranges: Vec::new(),
                    })
                    .collect(),
                matched,
                total: files.live_count(),
            })
        })
        .await
    }

    /// Greps the workspace for `query`, each file's matches given to
    /// `found` as its chunk is read, up to its limit or until `cancel`,
    /// which is checked between chunks. Dropping the future stops the jobs
    /// it started at their next file.
    pub async fn grep(
        &self,
        query: GrepQuery,
        cancel: &Cancel,
        mut found: impl FnMut(Grepped) + Send,
    ) -> Result<GrepSummary, String> {
        if query.limit == 0 {
            return Ok(GrepSummary {
                stopped: Some(Stopped::AtLimit),
                ..GrepSummary::default()
            });
        }
        let parsed = query.parse();
        let Some(grep) = Grep::new(&parsed, query.options) else {
            return Ok(GrepSummary::default());
        };
        let (files, content) = self.content().await?;
        let mut run = Run {
            files: &files,
            content: content.as_deref(),
            limit: query.limit,
            cancel,
            found: &mut found,
        };
        let grep = Arc::new(grep);
        let mut summary = self.grep_files(&mut run, &parsed, &grep).await?;
        // Constraint parsing can swallow tokens meant literally (`!=` as an
        // exclusion): a constrained search that found nothing is retried as
        // the whole raw text, keeping only a file path it named.
        if summary.matches == 0 && summary.stopped.is_none() && !parsed.constraints.is_empty() {
            let literal = parsed.literal();
            if let Some(retry) = Grep::new(&literal, query.options) {
                summary = self
                    .grep_files(&mut run, &literal, &Arc::new(retry))
                    .await?;
            }
        }
        summary.regex_error = grep.regex_error().map(str::to_owned);
        Ok(summary)
    }

    async fn grep_files(
        &self,
        run: &mut Run<'_, impl FnMut(Grepped) + Send>,
        parsed: &FFFQuery<'_>,
        grep: &Arc<Grep>,
    ) -> Result<GrepSummary, String> {
        let (files, content, limit, cancel) = (run.files, run.content, run.limit, run.cancel);
        let candidates = content.and_then(|content| {
            let (filter, overlay) = (Some(&*content.filter), Some(&content.overlay));
            match grep.prefilter() {
                Prefilter::Literal(text) => literal_candidates(filter, overlay, &[text]),
                Prefilter::Regex(text) => regex_candidates(filter, overlay, text),
                Prefilter::Fuzzy(text) => fuzzy_candidates(filter, overlay, text),
            }
        });
        let known = Known {
            candidates: candidates.as_deref(),
            binary: content.map(|content| content.binary.as_slice()),
            unknown: content.map(|content| content.unknown.as_slice()),
        };
        let slots = grep::files_to_search(files, &parsed.constraints, known);
        let mut summary = GrepSummary {
            prefiltered: candidates.is_some(),
            ..GrepSummary::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let _stop_on_drop = SetOnDrop(Arc::clone(&stop));
        let mut chunks = slots.chunks(GREP_CHUNK).map(<[usize]>::to_vec);
        let mut running = std::collections::VecDeque::new();
        loop {
            while running.len() < WINDOW
                && let Some(chunk) = chunks.next()
            {
                let (files, grep, stop) = (Arc::clone(files), Arc::clone(grep), Arc::clone(&stop));
                let cancel = cancel.clone();
                let root = Arc::clone(&self.root);
                // Spawned, so it is admitted now rather than when awaited.
                let job = rt::unblock_with(
                    &*self.rt,
                    BlockingClass::Storage,
                    GREP_JOB_BYTES,
                    move || grep_chunk(&root, &files, &grep, &chunk, &stop, &cancel, limit),
                );
                running.push_back(rt::spawn(&*self.rt, job));
            }
            let Some(next) = running.pop_front() else {
                break;
            };
            let (searched, chunk) = next.await.map_err(|error| error.to_string())?;
            if cancel.is_set() {
                summary.stopped = Some(Stopped::Cancelled);
                return Ok(summary);
            }
            summary.searched += searched;
            for mut grepped in chunk {
                grepped.matches.truncate(limit - summary.matches);
                summary.matched_files += 1;
                summary.matches += grepped.matches.len();
                (run.found)(grepped);
                if summary.matches >= limit {
                    summary.stopped = Some(Stopped::AtLimit);
                    return Ok(summary);
                }
            }
        }
        Ok(summary)
    }
}

/// What one grep reads, and where its matches go.
struct Run<'a, F> {
    files: &'a Arc<Files>,
    content: Option<&'a ContentIndex>,
    limit: usize,
    cancel: &'a Cancel,
    found: &'a mut F,
}

/// What a grep is asked.
#[derive(Clone, Debug)]
pub struct GrepQuery {
    pub pattern: Pattern,
    pub options: GrepOptions,
    /// The most matches it reports.
    pub limit: usize,
}

/// What a grep searches for, and in which files.
#[derive(Clone, Debug)]
pub enum Pattern {
    /// As a person types it, constraints and all: `needle *.rs !tests/`.
    Typed(String),
    /// The pattern as it is, in the files `glob` (relative to the root)
    /// matches, or every file.
    Exact {
        pattern: String,
        glob: Option<String>,
    },
}

impl GrepQuery {
    fn parse(&self) -> FFFQuery<'_> {
        match &self.pattern {
            Pattern::Typed(query) => grep::parse_grep_query(query),
            Pattern::Exact { pattern, glob } => FFFQuery {
                raw_query: pattern,
                constraints: glob.iter().map(|glob| Constraint::Glob(glob)).collect(),
                fuzzy_query: FuzzyQuery::Text(pattern),
                location: None,
            },
        }
    }
}

#[derive(Clone, Copy)]
struct WriteBigrams;

struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// The matches of one chunk of files, and how many it read.
fn grep_chunk(
    root: &Path,
    files: &Files,
    grep: &Grep,
    chunk: &[usize],
    stop: &AtomicBool,
    cancel: &Cancel,
    limit: usize,
) -> io::Result<(usize, Vec<Grepped>)> {
    let _never = sys::fs::NeverFetch::new();
    let mut buf = Vec::new();
    let mut found = Vec::new();
    let mut searched = 0;
    let mut remaining = limit;
    for &slot in chunk {
        if stop.load(Ordering::Acquire) || cancel.is_set() || remaining == 0 {
            break;
        }
        searched += 1;
        let path = files.path(&files.items()[slot]);
        let stopped = || stop.load(Ordering::Acquire) || cancel.is_set();
        let Ok(sys::fs::RegularFile::Open(mut file)) = sys::fs::open_regular(&root.join(path))
        else {
            continue;
        };
        buf.clear();
        let Ok(_) = (&mut file).take(8 << 10).read_to_end(&mut buf) else {
            continue;
        };
        if is_binary(&buf) {
            continue;
        }
        if io::Seek::rewind(&mut file).is_err() {
            continue;
        }
        let matches = if files.items()[slot].size > MAX_FILE_SIZE {
            grep.reader(&mut file, remaining, &stopped)?
        } else {
            // A file may have grown since the walk; never search only its prefix.
            buf.clear();
            if (&mut file)
                .take(MAX_FILE_SIZE + 1)
                .read_to_end(&mut buf)
                .is_err()
            {
                continue;
            }
            if buf.len() as u64 > MAX_FILE_SIZE {
                if io::Seek::rewind(&mut file).is_err() {
                    continue;
                }
                grep.reader(&mut file, remaining, &stopped)?
            } else {
                grep.file_limited(&buf, remaining)
            }
        };
        remaining -= matches.len();
        if !matches.is_empty() {
            found.push(Grepped {
                path: path.to_owned(),
                matches,
            });
        }
    }
    Ok((searched, found))
}

/// Every file under the root, from one blocking job, with the rules it
/// was walked by; each directory it enters is entered in `watching`.
async fn walk_tree(
    rt: &Rt,
    root: Arc<Path>,
    closed: Arc<AtomicBool>,
    watching: Arc<Watching>,
) -> (Arc<Rules>, Files) {
    rt::unblock_with(&**rt, BlockingClass::Storage, 0, move || {
        let rules = Rules::of(&root);
        let files = Files::walked(rules.walk(&|| closed.load(Ordering::Acquire), &|dir| {
            watching.tree.enter(dir);
        }));
        (Arc::new(rules), files)
    })
    .await
}

/// Builds the content index of `files`' walked files, `BUILD_CHUNK` at a
/// time on the blocking pool, `None` if the index went first.
async fn build(
    rt: &Rt,
    root: &Arc<Path>,
    files: Arc<Files>,
    closed: Arc<AtomicBool>,
) -> Option<ContentIndex> {
    let walked = files.walked_count();
    let mut consec = BigramIndexBuilder::new(walked);
    let mut skip = BigramIndexBuilder::new(walked);
    let mut binary = vec![0u64; walked.div_ceil(64)];
    let mut unknown = vec![0u64; walked.div_ceil(64)];
    let mut firsts = (0..walked).step_by(BUILD_CHUNK);
    let mut running = std::collections::VecDeque::new();
    loop {
        while running.len() < WINDOW
            && let Some(first) = firsts.next()
        {
            let (root, files, closed) = (Arc::clone(root), Arc::clone(&files), Arc::clone(&closed));
            let job = rt::unblock_with(
                &**rt,
                BlockingClass::Storage,
                rt::STORAGE_BYTES,
                move || {
                    let len = (files.walked_count() - first).min(BUILD_CHUNK);
                    (first, build_chunk(&root, &files, first, len, &closed))
                },
            );
            running.push_back(rt::spawn(&**rt, job));
        }
        let Some(next) = running.pop_front() else {
            break;
        };
        let (first, chunk) = next.await;
        let (chunk_consec, chunk_skip, chunk_binary, chunk_unknown) = chunk?;
        consec.merge(&chunk_consec, first);
        skip.merge(&chunk_skip, first);
        for (word, bits) in binary[first / 64..].iter_mut().zip(chunk_binary) {
            *word |= bits;
        }
        for (word, bits) in unknown[first / 64..].iter_mut().zip(chunk_unknown) {
            *word |= bits;
        }
    }
    if closed.load(Ordering::Acquire) {
        return None;
    }
    let (filter, binary) = rt::unblock_with(&**rt, BlockingClass::Cpu, 0, move || {
        let mut filter = consec.compress(None);
        filter.set_skip_index(skip.compress(Some(SKIP_INDEX_MIN_DENSITY_PCT)));
        (filter, binary)
    })
    .await;
    Some(ContentIndex {
        id: build_id(),
        filter: Arc::new(filter),
        overlay: BigramOverlay::new(walked),
        binary: Arc::new(binary),
        unknown: Arc::new(unknown),
    })
}

type Chunk = (BigramIndexBuilder, BigramIndexBuilder, Vec<u64>, Vec<u64>);

/// The bigrams of walked files `first..first + len`, and which are binary.
fn build_chunk(
    root: &Path,
    files: &Files,
    first: usize,
    len: usize,
    closed: &AtomicBool,
) -> Option<Chunk> {
    let _never = sys::fs::NeverFetch::new();
    let mut consec = BigramIndexBuilder::new(len);
    let mut skip = BigramIndexBuilder::new(len);
    let mut binary = vec![0u64; len.div_ceil(64)];
    let mut unknown = vec![0u64; len.div_ceil(64)];
    let mut buf = Vec::new();
    for i in 0..len {
        if closed.load(Ordering::Acquire) {
            return None;
        }
        let file = &files.items()[first + i];
        if file.is_deleted() || file.size == 0 {
            continue;
        }
        let path = root.join(files.path(file));
        if read(&path, MAX_INDEXABLE_FILE_SIZE as u64, &mut buf).is_err() {
            unknown[i / 64] |= 1 << (i % 64);
            continue;
        }
        if memchr::memchr(0, &buf).is_some() {
            binary[i / 64] |= 1 << (i % 64);
            continue;
        }
        consec.add_file_content(&mut skip, i, &buf);
    }
    Some((consec, skip, binary, unknown))
}

/// What a change on disk turned out to be, looked at off the lock.
enum Change {
    /// A file the walk would find, with what its content says when the
    /// content index wants it.
    File(Found, Option<Read>),
    /// A directory, walked again: every file it holds now.
    Dir(String, Vec<(Found, Option<Read>)>),
    /// Nothing the walk would find is at this relative path any more.
    Gone(String),
}

impl Change {
    fn paths(&self) -> Vec<String> {
        match self {
            Self::File(found, _) => vec![found.path.clone()],
            Self::Dir(_, found) => found.iter().map(|(f, _)| f.path.clone()).collect(),
            Self::Gone(_) => Vec::new(),
        }
    }
}

enum Read {
    Text(Vec<u16>),
    Binary,
    Unknown,
}

/// What `paths` (absolute) are now: files the walk would find, the
/// directories they are in, or gone.
fn look(
    rules: &Rules,
    paths: &BTreeSet<PathBuf>,
    read_content: bool,
    enter: &dyn Fn(&Path),
) -> Vec<Change> {
    let _never = sys::fs::NeverFetch::new();
    let mut buf = Vec::new();
    let mut content = |found: Found| {
        let read = read_content.then(|| content_of(rules.root(), &found.path, &mut buf));
        (found, read)
    };
    let mut changes = Vec::new();
    let mut files = BTreeMap::<String, Vec<String>>::default();
    let mut entered = HashMap::new();
    for path in paths {
        let Some(relative) = rules.relative(path) else {
            continue;
        };
        let dir = relative.rfind('/').map_or("", |slash| &relative[..slash]);
        if !rules.enters(dir, &mut entered) {
            // Under an ignored directory: nothing the walk would find.
            changes.push(Change::Gone(relative));
            continue;
        }
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => {
                if !rules.enters(&relative, &mut entered) {
                    changes.push(Change::Gone(relative));
                    continue;
                }
                let found = rules
                    .walk_dir(&relative, enter)
                    .into_iter()
                    .map(&mut content)
                    .collect();
                changes.push(Change::Dir(relative, found));
            }
            Ok(_) => {
                let dir = relative
                    .rfind('/')
                    .map_or("", |slash| &relative[..slash])
                    .to_owned();
                files.entry(dir).or_default().push(relative);
            }
            Err(_) => changes.push(Change::Gone(relative)),
        }
    }
    for (dir, names) in files {
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let found = rules.files_in(&dir, &names);
        for name in &names {
            if !found.iter().any(|f| f.path == *name) {
                changes.push(Change::Gone((*name).to_owned()));
            }
        }
        for file in found {
            let (file, read) = content(file);
            changes.push(Change::File(file, read));
        }
    }
    changes
}

/// What the content index wants of the file at `path`: its bigrams.
fn content_of(root: &Path, path: &str, buf: &mut Vec<u8>) -> Read {
    match read(&root.join(path), MAX_INDEXABLE_FILE_SIZE as u64, buf) {
        Ok(()) if memchr::memchr(0, buf).is_none() => Read::Text(extract_bigrams(buf)),
        Ok(()) => Read::Binary,
        Err(_) => Read::Unknown,
    }
}

/// What a walk found that `stored` (the files as the store had them)
/// does not say: each file new, of another size or time, or modified in
/// the second `stored` was taken or later (it may have changed again
/// within that second), read again if `read`; each file gone.
fn differences(
    rules: &Rules,
    stored: &Files,
    walked: &Files,
    taken: u64,
    read: bool,
) -> Vec<Change> {
    let _never = sys::fs::NeverFetch::new();
    let created: HashMap<&str, usize> = (stored.walked_count()..stored.items().len())
        .filter(|&slot| !stored.items()[slot].is_deleted())
        .map(|slot| (stored.path(&stored.items()[slot]), slot))
        .collect();
    let find = |path: &str| {
        let items = stored.items();
        (items[..stored.walked_count()].binary_search_by(|f| stored.path(f).cmp(path)))
            .ok()
            .filter(|&slot| !items[slot].is_deleted())
            .or_else(|| created.get(path).copied())
    };
    let mut buf = Vec::new();
    let mut changes = Vec::new();
    for file in walked.items() {
        let path = walked.path(file);
        let same = find(path).is_some_and(|slot| {
            let known = &stored.items()[slot];
            known.size == file.size && known.modified == file.modified && known.modified < taken
        });
        if !same {
            let found = Found {
                path: path.to_owned(),
                size: file.size,
                modified: file.modified,
            };
            let content = read.then(|| content_of(rules.root(), path, &mut buf));
            changes.push(Change::File(found, content));
        }
    }
    for file in stored.items() {
        let path = stored.path(file);
        if !file.is_deleted() && walked.find(path).is_none() {
            changes.push(Change::Gone(path.to_owned()));
        }
    }
    changes
}

/// A content index's build: unique to the daemon's life and, by its
/// time, to any other daemon's that wrote one.
fn build_id() -> u64 {
    static BUILDS: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            (since.as_secs().wrapping_mul(1_000_000_000))
                .wrapping_add(u64::from(since.subsec_nanos()))
        });
    nanos ^ BUILDS.fetch_add(1, Ordering::Relaxed).rotate_right(16)
}

fn seconds(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a search of a refused root replies.
fn refused(root: &Path, refusal: sys::fs::Refusal) -> String {
    format!(
        "{} is {refusal}, which is never indexed: start in a project directory to search it",
        root.display()
    )
}

/// A change to what decides which files the walk finds: a full walk is
/// the only way to know what it changed.
fn changes_ignore_rules(relative: &str) -> bool {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    matches!(name, ".gitignore" | ".ignore" | ".rgignore") || relative == ".git/info/exclude"
}

/// Reads at most `limit` bytes of the regular file at `path` into `buf`;
/// a cloud placeholder is never opened (it would be fetched to be read).
fn read(path: &Path, limit: u64, buf: &mut Vec<u8>) -> io::Result<()> {
    buf.clear();
    let sys::fs::RegularFile::Open(file) = sys::fs::open_regular(path)? else {
        return Err(io::ErrorKind::InvalidInput.into());
    };
    file.take(limit + 1).read_to_end(buf)?;
    if buf.len() as u64 > limit {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(())
}

/// Whether content looks binary: a NUL early on, as ripgrep decides.
fn is_binary(content: &[u8]) -> bool {
    memchr::memchr(0, &content[..content.len().min(8 << 10)]).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unused_index_configuration_holds_no_open_or_resting_store() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("ws");
        let cache = dir.path().join("cache");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.txt"), "needle").unwrap();
        let indexes = Indexes::new(Limits::default()).kept_in(&cache);
        assert!(indexes.opened.get().is_none());
        assert!(!cache.exists());
        let rt: Rt = Arc::new(rt::Threads::new(2));
        let index = indexes.open(&root, Arc::clone(&rt), Arc::new(Hub::new(Arc::clone(&rt))));
        let opened = indexes.opened.get().expect("first use opens shared state");
        assert!(opened.store.is_some());
        assert_eq!(
            rt::block_on(index.search("a".into(), 10)).unwrap().files[0].path,
            "a.txt"
        );
        let again = indexes.open(&root, Arc::clone(&rt), Arc::new(Hub::new(rt)));
        assert!(
            Arc::ptr_eq(&index, &again),
            "first use still shares one workspace index"
        );
    }

    #[test]
    fn an_unwatched_root_sees_local_changes_at_once_and_remote_ones_by_walking_again() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("alpha.txt"), "needle").unwrap();
        let rt: Rt = Arc::new(rt::Threads::new(2));
        let hub = Arc::new(Hub::new(Arc::clone(&rt)));
        let indexes = Indexes::new(Limits::default());
        let index = indexes.open(&root, rt, hub);
        // As a network volume's root is: no notification can be trusted.
        index.state().tree = Some(Tree::Unwatched(sys::fs::Filesystem::Network));
        let found = |query: &str| -> Vec<String> {
            let found = rt::block_on(index.search(query.to_owned(), 10)).unwrap();
            found.files.into_iter().map(|f| f.path).collect()
        };
        assert_eq!(found("alpha"), ["alpha.txt"]);
        // What is done here, the watcher sees at once.
        assert!(index.state().watching.is_some());
        std::fs::write(root.join("beta.txt"), "needle").unwrap();
        eventually("the watcher finds the new file", REVALIDATE / 2, || {
            found("beta") == ["beta.txt"]
        });

        // What another machine does, it never sees: a walk does, once a
        // search finds the files old.
        let watching = index.state().watching.take();
        drop(watching);
        std::fs::write(root.join("gamma.txt"), "needle").unwrap();
        assert_eq!(found("gamma"), Vec::<String>::new());
        std::thread::sleep(REVALIDATE);
        eventually(
            "the walk again finds the new file",
            Duration::from_secs(10),
            || found("gamma") == ["gamma.txt"],
        );

        // An edit would go unseen by a content index: grep reads every file.
        let query = GrepQuery {
            pattern: Pattern::Typed("needle".to_owned()),
            options: GrepOptions::default(),
            limit: 10,
        };
        let summary = rt::block_on(index.grep(query, &Cancel::default(), |_| {})).unwrap();
        assert_eq!(summary.matched_files, 3);
        assert!(!summary.prefiltered);
    }

    #[test]
    fn resting_compaction_preserves_edits_new_files_and_binary_slots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("ws");
        std::fs::create_dir(&root).unwrap();
        for i in 0..128 {
            std::fs::write(root.join(format!("f{i}.txt")), format!("word{i} abcdef")).unwrap();
        }
        let rt: Rt = Arc::new(rt::Threads::new(2));
        let hub = Arc::new(Hub::new(Arc::clone(&rt)));
        let indexes = Indexes::new(Limits::default()).kept_in(dir.path().join("cache"));
        let index = indexes.open(&root, rt, hub);
        rt::block_on(index.content()).unwrap();
        // Use the real change reader and installer, with the watcher removed
        // so the batch and the rest have a deterministic boundary.
        drop(index.state().watching.take());
        let mut paths = BTreeSet::new();
        for i in 0..64 {
            let path = root.join(format!("f{i}.txt"));
            std::fs::remove_file(&path).unwrap();
            paths.insert(path);
        }
        std::fs::write(root.join("f70.txt"), "newneedle").unwrap();
        std::fs::write(root.join("added.txt"), "newneedle").unwrap();
        std::fs::write(root.join("binary.txt"), b"newneedle\0").unwrap();
        paths.extend([
            root.join("f70.txt"),
            root.join("added.txt"),
            root.join("binary.txt"),
        ]);
        let rules = index.state().rules.clone().unwrap();
        index.install(look(&rules, &paths, true, &|_| {}));
        eventually("the index rests", Duration::from_secs(10), || {
            rt::block_on(index.rest())
        });
        assert_eq!(index.resident(), 0);
        let (_, place) = index.store.as_ref().unwrap();
        let stored = disk::read(place, &root, usize::MAX).unwrap();
        assert_eq!(stored.files.items().len(), 66);
        assert_eq!(stored.files.walked_count(), 66);
        assert_eq!(stored.content.as_ref().unwrap().overlay.modified_count(), 0);
        for query in ["newneedle", "word99", "abcdef"] {
            let query = GrepQuery {
                pattern: Pattern::Typed(query.into()),
                options: GrepOptions::default(),
                limit: usize::MAX,
            };
            let mut filtered = Vec::new();
            rt::block_on(index.grep(query.clone(), &Cancel::default(), |g| filtered.push(g.path)))
                .unwrap();
            index.state().content = Content::Refused;
            let mut scanned = Vec::new();
            rt::block_on(index.grep(query, &Cancel::default(), |g| scanned.push(g.path))).unwrap();
            filtered.sort();
            scanned.sort();
            assert_eq!(filtered, scanned);
            index.state().content = Content::Absent;
            rt::block_on(index.load());
        }
    }

    #[test]
    fn a_resting_index_releases_limited_watches_and_verifies_on_reload() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("ws");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("before.txt"), "hello").unwrap();
        let rt: Rt = Arc::new(rt::Threads::new(2));
        let hub = Arc::new(Hub::new(Arc::clone(&rt)));
        let indexes = Indexes::new(Limits::default()).kept_in(dir.path().join("cache"));
        let index = indexes.open(&root, rt, hub);
        rt::block_on(index.content()).unwrap();
        index.changed_on_disk(vec![Event::Unavailable(Unavailable::Limit)]);
        eventually("the index rests", Duration::from_secs(10), || {
            rt::block_on(index.rest())
        });
        assert!(index.state().watching.is_none());
        std::fs::write(root.join("after.txt"), "hello").unwrap();
        let _ = rt::block_on(index.files()).unwrap();
        eventually(
            "a reload verifies unwatched changes",
            Duration::from_secs(10),
            || {
                index
                    .state()
                    .files
                    .as_ref()
                    .is_some_and(|files| files.find("after.txt").is_some())
            },
        );
    }

    #[test]
    fn watch_pressure_in_one_workspace_releases_other_resting_subscriptions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("resting");
        let active = dir.path().canonicalize().unwrap().join("active");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&active).unwrap();
        std::fs::write(root.join("before.txt"), "hello").unwrap();
        let rt: Rt = Arc::new(rt::Threads::new(2));
        let hub = Arc::new(Hub::new(Arc::clone(&rt)));
        let indexes = Indexes::new(Limits::default()).kept_in(dir.path().join("cache"));
        let resting = indexes.open(&root, Arc::clone(&rt), Arc::clone(&hub));
        rt::block_on(resting.content()).unwrap();
        eventually("the first workspace rests", Duration::from_secs(10), || {
            rt::block_on(resting.rest())
        });
        assert!(resting.state().watching.is_some());
        let index = indexes.open(&active, rt, hub);
        rt::block_on(index.content()).unwrap();
        index.changed_on_disk(vec![Event::Unavailable(Unavailable::Limit)]);
        assert!(resting.state().watching.is_none());
        std::fs::write(root.join("after.txt"), "hello").unwrap();
        rt::block_on(resting.files()).unwrap();
        eventually(
            "the released workspace verifies on reload",
            Duration::from_secs(10),
            || {
                resting
                    .state()
                    .files
                    .as_ref()
                    .is_some_and(|files| files.find("after.txt").is_some())
            },
        );
    }

    #[test]
    fn persistence_keeps_the_observation_watermark_for_racy_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("ws");
        std::fs::create_dir(&root).unwrap();
        let path = root.join("racy.txt");
        std::fs::write(&path, "before").unwrap();
        let observed = seconds(SystemTime::now());
        let time = UNIX_EPOCH + Duration::from_secs(observed);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(time)
            .unwrap();
        let cache = dir.path().join("cache");
        let rt: Rt = Arc::new(rt::Threads::new(2));
        let indexes = Indexes::new(Limits::default()).kept_in(&cache);
        let index = indexes.open(&root, Arc::clone(&rt), Arc::new(Hub::new(Arc::clone(&rt))));
        rt::block_on(index.content()).unwrap();
        drop(index.state().watching.take());
        index.state().observed = observed;
        // Simulate an edit whose event has not reached this snapshot before
        // publication. Size and second-resolution mtime are unchanged.
        std::fs::write(&path, "needle").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(time)
            .unwrap();
        eventually(
            "publication is in a later second",
            Duration::from_secs(3),
            || seconds(SystemTime::now()) > observed,
        );
        assert!(rt::block_on(index.rest()));
        let place = &index.store.as_ref().unwrap().1;
        assert_eq!(
            disk::read(place, &root, usize::MAX).unwrap().taken,
            observed
        );
        drop(index);
        drop(indexes);
        let indexes = Indexes::new(Limits::default()).kept_in(&cache);
        let index = indexes.open(&root, Arc::clone(&rt), Arc::new(Hub::new(Arc::clone(&rt))));
        let mut hits = Vec::new();
        rt::block_on(index.grep(
            GrepQuery {
                pattern: Pattern::Typed("needle".into()),
                options: GrepOptions::default(),
                limit: 10,
            },
            &Cancel::default(),
            |hit| hits.push(hit.path),
        ))
        .unwrap();
        assert_eq!(hits, ["racy.txt"]);
    }

    #[test]
    fn rest_retires_its_keeper_before_a_query_can_reload() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("ws");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.txt"), "before").unwrap();
        let rt: Rt = Arc::new(rt::Threads::new(2));
        let indexes = Indexes::new(Limits::default()).kept_in(dir.path().join("cache"));
        let index = indexes.open(&root, Arc::clone(&rt), Arc::new(Hub::new(Arc::clone(&rt))));
        rt::block_on(index.content()).unwrap();
        assert!(index.state().keeper);
        assert!(rt::block_on(index.rest()));
        assert!(
            !index.state().keeper,
            "retirement is part of the locked rest transition"
        );
        rt::block_on(index.content()).unwrap();
        assert!(index.state().keeper, "reload starts its own keeper");
    }

    #[test]
    fn the_first_grep_after_reload_waits_for_pending_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("ws");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.txt"), "before").unwrap();
        let rt: Rt = Arc::new(rt::Threads::new(2));
        let indexes = Indexes::new(Limits::default()).kept_in(dir.path().join("cache"));
        let index = indexes.open(&root, Arc::clone(&rt), Arc::new(Hub::new(Arc::clone(&rt))));
        rt::block_on(index.content()).unwrap();
        assert!(rt::block_on(index.rest()));
        std::fs::write(root.join("a.txt"), "needle").unwrap();
        std::fs::write(root.join("b.txt"), "needle").unwrap();
        // Deliver the real watch event synchronously so the test controls
        // delivery, while loading and change application remain asynchronous.
        index.changed_on_disk(vec![Event::Changed(vec![
            root.join("a.txt"),
            root.join("b.txt"),
        ])]);
        let mut hits = Vec::new();
        rt::block_on(index.grep(
            GrepQuery {
                pattern: Pattern::Typed("needle".into()),
                options: GrepOptions::default(),
                limit: 10,
            },
            &Cancel::default(),
            |hit| hits.push(hit.path),
        ))
        .unwrap();
        hits.sort();
        assert_eq!(hits, ["a.txt", "b.txt"]);
    }

    fn eventually(what: &str, within: Duration, mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + within;
        while !check() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
