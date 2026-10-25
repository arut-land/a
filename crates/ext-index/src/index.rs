//! One [`WorkspaceIndex`] per workspace: opened by the first search or grep that
//! asks about it, walked on the runtime's blocking pool, kept current by
//! the native watcher, and dropped once nothing has used it for a while.
//!
//! The file list is a snapshot ([`Files`] behind an `Arc`): a search
//! reads the one it took, a change copies it only while a search still
//! holds the old one. The content index is built on the first grep, in
//! chunks of files that blocking jobs fill, and only within the memory
//! the workspace may use; above that, grep reads every file instead.

use std::collections::BTreeSet;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use ext::{Cancel, WorkspaceShared};
use rt::{BlockingClass, Rt, WatchEvent};

use crate::fff::grep::{self, Grep, GrepMatch, GrepOptions, Known, Prefilter};
use crate::fff::index::{
    BigramFilter, BigramIndexBuilder, BigramOverlay, SKIP_INDEX_MIN_DENSITY_PCT, build_bytes,
    extract_bigrams, fuzzy_candidates, literal_candidates, regex_candidates,
};
use crate::fff::query::{Constraint, FFFQuery, FuzzyQuery, QueryParser};
use crate::fff::types::{Files, Found, recent_first};
use crate::fff::walk::{Rules, is_git_path};
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
            linger: Duration::from_secs(300),
        }
    }
}

/// The open indexes, one per workspace.
pub struct Indexes {
    open: WorkspaceShared<WorkspaceIndex>,
    limits: Limits,
}

impl Indexes {
    pub fn new(limits: Limits) -> Self {
        Self {
            open: WorkspaceShared::default(),
            limits,
        }
    }

    /// The index of the workspace at `root`, opened now if no one has it
    /// open, and kept open for [`Limits::linger`] from now.
    pub fn open(&self, root: &Path, rt: Rt) -> Arc<WorkspaceIndex> {
        let index = self.open.get_or_make(root, |own| WorkspaceIndex {
            rules: Arc::new(Rules::of(root)),
            rt,
            limits: self.limits,
            state: Mutex::default(),
            changed: rt::Notify::new(),
            closed: Arc::default(),
            own: own.clone(),
        });
        index.used();
        index
    }
}

/// A workspace's files and, once a grep asked for it, its content index.
pub struct WorkspaceIndex {
    rules: Arc<Rules>,
    rt: Rt,
    limits: Limits,
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
    /// Counts the walks installed: what a content index was built from.
    epoch: u64,
    busy: Busy,
    content: Content,
    pending: Pending,
    watcher: Option<rt::Watcher>,
    last_used: Option<Instant>,
    keeper: bool,
    /// The root was missing when last looked at.
    root_missing: bool,
}

/// What changes the file list now: at most one thing at a time.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Busy {
    #[default]
    Idle,
    Walking,
    /// Applying a batch of changes.
    Applying,
}

#[derive(Default)]
enum Content {
    #[default]
    Absent,
    /// Slots that changed while it was built, to be read again after.
    Building(Vec<String>),
    Ready(Arc<ContentIndex>),
    /// Over [`Limits::memory`]: grep reads every file.
    Refused,
}

/// The content prefilter of a walk, and what changed since.
#[derive(Clone)]
pub(crate) struct ContentIndex {
    filter: Arc<BigramFilter>,
    overlay: BigramOverlay,
    /// One bit per walked file: binary, never searched.
    binary: Arc<Vec<u64>>,
}

impl ContentIndex {
    pub(crate) fn heap_bytes(&self) -> usize {
        self.filter.heap_bytes() + self.binary.len() * 8
    }
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
    /// It stopped at its limit with files left to read.
    pub truncated: bool,
    /// The content index narrowed which files were read.
    pub prefiltered: bool,
    pub regex_error: Option<String>,
}

/// Files a grep job reads at once: few enough that a cancel or a full
/// page stops it soon, enough that a job outweighs its scheduling.
const GREP_CHUNK: usize = 256;
/// What a grep job holds at most: the largest file it reads, twice.
const GREP_JOB_BYTES: usize = 20 << 20;
/// Grep and build jobs in flight at once.
const WINDOW: usize = 4;
/// Files one content-index job reads: a multiple of 64, so chunks merge
/// by whole bitset words.
const BUILD_CHUNK: usize = 4096;

impl WorkspaceIndex {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn me(&self) -> Option<Arc<Self>> {
        self.own.upgrade()
    }

    /// Keeps the index open for [`Limits::linger`] from now: one task
    /// holds it, sleeping until the last use is that old.
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
                    if due <= now {
                        state.keeper = false;
                        break;
                    }
                    due
                };
                me.rt.sleep(due - now).await;
            }
            drop(me);
        }));
    }

    /// The files as the last walk and the changes since left them, after
    /// the first walk if it has not finished.
    async fn files(&self) -> Arc<Files> {
        self.used();
        let walk = {
            let mut state = self.state();
            if let Some(files) = &state.files {
                return Arc::clone(files);
            }
            let idle = state.busy == Busy::Idle;
            if idle {
                state.busy = Busy::Walking;
            }
            idle
        };
        if walk {
            self.walk();
        }
        let mut files = None;
        self.changed
            .until(|| {
                files.clone_from(&self.state().files);
                files.is_some()
            })
            .await;
        files.expect("checked")
    }

    /// Walks the root again on the blocking pool, the watcher armed first
    /// so nothing that changes during the walk is missed.
    /// What the caller set [`Busy::Walking`] for, under the lock it then let go:
    /// the watcher may call back into the index as it arms.
    fn walk(&self) {
        let watcher = match self.state().watcher.take() {
            Some(mut watcher) => {
                let _ = watcher.refresh();
                Some(watcher)
            }
            None => self.watch().ok(),
        };
        self.state().watcher = watcher;
        let weak = Weak::clone(&self.own);
        let (rt, rules, closed) = (
            Arc::clone(&self.rt),
            Arc::clone(&self.rules),
            Arc::clone(&self.closed),
        );
        self.rt.spawn(Box::pin(async move {
            let files = walk(&rt, rules, closed).await;
            let Some(index) = weak.upgrade() else { return };
            {
                let mut state = index.state();
                state.files = Some(Arc::new(files));
                state.epoch += 1;
                state.busy = Busy::Idle;
                // Its bit positions were the old walk's.
                state.content = Content::Absent;
            }
            index.changed.notify();
            index.apply();
        }));
    }

    /// The watcher's callback runs on the watcher's own thread, where
    /// rearming or dropping the watcher would wait on that thread: events
    /// go to an inbox, which a task on the runtime empties.
    fn watch(&self) -> io::Result<rt::Watcher> {
        let rt = Arc::clone(&self.rt);
        let spawn_rt = Arc::clone(&self.rt);
        let weak = Weak::clone(&self.own);
        let inbox: Arc<Mutex<(Vec<WatchEvent>, bool)>> = Arc::default();
        rt::Watcher::new(
            Arc::new(move |task| spawn_rt.spawn(task)),
            [self.rules.root()],
            move |event| {
                let mut queued = inbox.lock().unwrap_or_else(PoisonError::into_inner);
                queued.0.push(event);
                if std::mem::replace(&mut queued.1, true) {
                    return;
                }
                let (inbox, weak) = (Arc::clone(&inbox), Weak::clone(&weak));
                rt.spawn(Box::pin(async move {
                    let events = {
                        let mut queued = inbox.lock().unwrap_or_else(PoisonError::into_inner);
                        queued.1 = false;
                        std::mem::take(&mut queued.0)
                    };
                    if let Some(index) = weak.upgrade() {
                        for event in events {
                            index.changed_on_disk(event);
                        }
                    }
                }));
            },
        )
    }

    fn changed_on_disk(&self, event: WatchEvent) {
        {
            let mut state = self.state();
            match event {
                WatchEvent::Changed(paths) | WatchEvent::RescanPaths(paths) => {
                    for path in paths {
                        if self.rules.root().starts_with(&path) {
                            // The root or a directory above it: the root
                            // may be gone, or back after it was.
                            let missing = !self.rules.root().is_dir();
                            if missing != state.root_missing {
                                state.root_missing = missing;
                                state.pending.rescan = true;
                            }
                            continue;
                        }
                        let Some(relative) = self.rules.relative(&path) else {
                            continue;
                        };
                        if changes_ignore_rules(&relative) {
                            state.pending.rescan = true;
                        } else if !is_git_path(&relative) {
                            state.pending.paths.insert(path);
                        }
                    }
                }
                WatchEvent::Overflow | WatchEvent::Rescan | WatchEvent::Unavailable(_) => {
                    state.pending.rescan = true;
                }
            }
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
            self.walk();
            return;
        }
        if state.pending.paths.is_empty() {
            return;
        }
        let paths = std::mem::take(&mut state.pending.paths);
        let read = matches!(state.content, Content::Ready(_) | Content::Building(_));
        state.busy = Busy::Applying;
        drop(state);
        let rules = Arc::clone(&self.rules);
        let seen = rt::unblock_with(&*self.rt, BlockingClass::Storage, 0, move || {
            look(&rules, &paths, read)
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
            ..
        } = &mut *state;
        let Some(files) = files else { return };
        let files = Arc::make_mut(files);
        let walked = files.walked_count();
        let mut ready = match content {
            Content::Ready(ready) => Some(Arc::make_mut(ready)),
            Content::Building(touched) => {
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
                    Some(Read::Text(bigrams)) => {
                        Arc::make_mut(&mut ready.binary)[slot / 64] &= !bit;
                        ready.overlay.modify_bigrams(slot, bigrams);
                    }
                    Some(Read::Binary) => Arc::make_mut(&mut ready.binary)[slot / 64] |= bit,
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
        let root = self.rules.root();
        pending
            .paths
            .extend(unread.into_iter().map(|path| root.join(path)));
    }

    /// The content index, built now if no grep has asked for it since the
    /// walk, or `None` when it would take more than the workspace may use.
    async fn content(&self, files: &Arc<Files>) -> Option<Arc<ContentIndex>> {
        loop {
            let build = {
                let mut state = self.state();
                match &state.content {
                    Content::Ready(content) => return Some(Arc::clone(content)),
                    Content::Refused => return None,
                    Content::Building(_) => None,
                    Content::Absent => {
                        let estimate = build_bytes(files.walked_count()) + files.heap_bytes();
                        if estimate > self.limits.memory {
                            state.content = Content::Refused;
                            return None;
                        }
                        state.content = Content::Building(Vec::new());
                        Some(state.epoch)
                    }
                }
            };
            if let Some(epoch) = build {
                self.build(Arc::clone(files), epoch);
            }
            self.changed
                .until(|| !matches!(self.state().content, Content::Building(_)))
                .await;
        }
    }

    fn build(&self, files: Arc<Files>, epoch: u64) {
        let rt = Arc::clone(&self.rt);
        let root = self.rules.root().to_owned();
        let closed = Arc::clone(&self.closed);
        let memory = self.limits.memory;
        let weak = Weak::clone(&self.own);
        self.rt.spawn(Box::pin(async move {
            let built = build(&rt, root, Arc::clone(&files), closed).await;
            let Some(index) = weak.upgrade() else { return };
            let touched = {
                let mut state = index.state();
                if state.epoch == epoch {
                    let touched = match std::mem::take(&mut state.content) {
                        Content::Building(touched) => touched,
                        _ => Vec::new(),
                    };
                    state.content = match built {
                        Some(content) if content.heap_bytes() + files.heap_bytes() <= memory => {
                            Content::Ready(Arc::new(content))
                        }
                        _ => Content::Refused,
                    };
                    touched
                } else {
                    state.content = Content::Absent;
                    Vec::new()
                }
            };
            index.changed.notify();
            // What changed while it was built is read again into it.
            let root = index.rules.root().to_owned();
            index
                .state()
                .pending
                .paths
                .extend(touched.into_iter().map(|path| root.join(path)));
            index.apply();
        }));
    }

    /// The best `limit` files for `query`, as an @-mention completes.
    pub async fn search(&self, query: String, limit: usize) -> Searched {
        let files = self.files().await;
        rt::unblock_with(&*self.rt, BlockingClass::Cpu, 0, move || {
            let parsed = QueryParser::default().parse(&query);
            let found = score::search(&files, &parsed, limit);
            let paths: Vec<&str> = (found.items.iter())
                .map(|(slot, _)| files.path(&files.items()[*slot]))
                .collect();
            let ranges = score::match_ranges(&parsed, &paths);
            Searched {
                files: (paths.iter().zip(&found.items).zip(ranges))
                    .map(|((path, (_, score)), ranges)| SearchedFile {
                        path: (*path).to_owned(),
                        score: score.total,
                        ranges: ranges.into_vec(),
                    })
                    .collect(),
                matched: found.total_matched,
                total: files.live_count(),
            }
        })
        .await
    }

    /// The files the glob `pattern` matches, most recently modified first:
    /// a pattern with a `/` matches whole relative paths, one without
    /// matches file names.
    pub async fn glob(&self, pattern: String, limit: usize) -> Result<Searched, String> {
        let files = self.files().await;
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
        found: impl Fn(Grepped) + Send + Sync,
    ) -> Result<GrepSummary, String> {
        let files = self.files().await;
        let parsed = query.parse();
        let Some(grep) = Grep::new(&parsed, query.options) else {
            return Ok(GrepSummary::default());
        };
        let content = self.content(&files).await;
        let run = Run {
            files: &files,
            content: content.as_deref(),
            limit: query.limit,
            cancel,
            found: &found,
        };
        let grep = Arc::new(grep);
        let mut summary = self.grep_files(&run, &parsed, &grep).await;
        // Constraint parsing can swallow tokens meant literally (`!=` as an
        // exclusion): a constrained search that found nothing is retried as
        // the whole raw text, keeping only a file path it named.
        if summary.matches == 0 && !summary.truncated && !parsed.constraints.is_empty() {
            let literal = parsed.literal();
            if let Some(retry) = Grep::new(&literal, query.options) {
                summary = self.grep_files(&run, &literal, &Arc::new(retry)).await;
            }
        }
        summary.regex_error = grep.regex_error().map(str::to_owned);
        Ok(summary)
    }

    async fn grep_files(
        &self,
        run: &Run<'_, impl Fn(Grepped) + Send + Sync>,
        parsed: &FFFQuery<'_>,
        grep: &Arc<Grep>,
    ) -> GrepSummary {
        let Run {
            files,
            content,
            limit,
            cancel,
            found,
        } = *run;
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
                let root = self.rules.root().to_owned();
                // Spawned, so it is admitted now rather than when awaited.
                let job = rt::unblock_with(
                    &*self.rt,
                    BlockingClass::Storage,
                    GREP_JOB_BYTES,
                    move || grep_chunk(&root, &files, &grep, &chunk, &stop),
                );
                running.push_back(rt::spawn(&*self.rt, job));
            }
            let Some(next) = running.pop_front() else {
                break;
            };
            let (searched, chunk) = next.await;
            if cancel.is_set() {
                summary.truncated = true;
                return summary;
            }
            summary.searched += searched;
            for grepped in chunk {
                summary.matched_files += 1;
                summary.matches += grepped.matches.len();
                found(grepped);
                if summary.matches >= limit {
                    summary.truncated = true;
                    return summary;
                }
            }
        }
        summary
    }
}

/// What one grep reads, and where its matches go.
struct Run<'a, F> {
    files: &'a Arc<Files>,
    content: Option<&'a ContentIndex>,
    limit: usize,
    cancel: &'a Cancel,
    found: &'a F,
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
) -> (usize, Vec<Grepped>) {
    let mut buf = Vec::new();
    let mut found = Vec::new();
    let mut searched = 0;
    for &slot in chunk {
        if stop.load(Ordering::Acquire) {
            break;
        }
        searched += 1;
        let path = files.path(&files.items()[slot]);
        if read(&root.join(path), MAX_FILE_SIZE, &mut buf).is_err() || is_binary(&buf) {
            continue;
        }
        let matches = grep.file(&buf);
        if !matches.is_empty() {
            found.push(Grepped {
                path: path.to_owned(),
                matches,
            });
        }
    }
    (searched, found)
}

/// Every file under the root, from one blocking job.
async fn walk(rt: &Rt, rules: Arc<Rules>, closed: Arc<AtomicBool>) -> Files {
    rt::unblock_with(&**rt, BlockingClass::Storage, 0, move || {
        Files::walked(rules.walk(&|| closed.load(Ordering::Acquire)))
    })
    .await
}

/// Builds the content index of `files`' walked files, `BUILD_CHUNK` at a
/// time on the blocking pool, `None` if the index went first.
async fn build(
    rt: &Rt,
    root: PathBuf,
    files: Arc<Files>,
    closed: Arc<AtomicBool>,
) -> Option<ContentIndex> {
    let walked = files.walked_count();
    let mut consec = BigramIndexBuilder::new(walked);
    let mut skip = BigramIndexBuilder::new(walked);
    let mut binary = vec![0u64; walked.div_ceil(64)];
    let mut firsts = (0..walked).step_by(BUILD_CHUNK);
    let mut running = std::collections::VecDeque::new();
    loop {
        while running.len() < WINDOW
            && let Some(first) = firsts.next()
        {
            let (root, files, closed) = (root.clone(), Arc::clone(&files), Arc::clone(&closed));
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
        let (chunk_consec, chunk_skip, chunk_binary) = chunk?;
        consec.merge(&chunk_consec, first);
        skip.merge(&chunk_skip, first);
        for (word, bits) in binary[first / 64..].iter_mut().zip(chunk_binary) {
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
        filter: Arc::new(filter),
        overlay: BigramOverlay::new(walked),
        binary: Arc::new(binary),
    })
}

type Chunk = (BigramIndexBuilder, BigramIndexBuilder, Vec<u64>);

/// The bigrams of walked files `first..first + len`, and which are binary.
fn build_chunk(
    root: &Path,
    files: &Files,
    first: usize,
    len: usize,
    closed: &AtomicBool,
) -> Option<Chunk> {
    let mut consec = BigramIndexBuilder::new(len);
    let mut skip = BigramIndexBuilder::new(len);
    let mut binary = vec![0u64; len.div_ceil(64)];
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
            continue;
        }
        if memchr::memchr(0, &buf).is_some() {
            binary[i / 64] |= 1 << (i % 64);
            continue;
        }
        consec.add_file_content(&mut skip, i, &buf);
    }
    Some((consec, skip, binary))
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
}

/// What `paths` (absolute) are now: files the walk would find, the
/// directories they are in, or gone.
fn look(rules: &Rules, paths: &BTreeSet<PathBuf>, read_content: bool) -> Vec<Change> {
    let mut buf = Vec::new();
    let mut content = |found: Found| {
        let read = read_content.then(|| {
            match read(
                &rules.root().join(&found.path),
                MAX_INDEXABLE_FILE_SIZE as u64,
                &mut buf,
            ) {
                Ok(()) if memchr::memchr(0, &buf).is_none() => Read::Text(extract_bigrams(&buf)),
                _ => Read::Binary,
            }
        });
        (found, read)
    };
    let mut changes = Vec::new();
    let mut files = std::collections::BTreeMap::<String, Vec<String>>::default();
    let mut entered = std::collections::HashMap::new();
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
                    .walk_dir(&relative)
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

/// A change to what decides which files the walk finds: a full walk is
/// the only way to know what it changed.
fn changes_ignore_rules(relative: &str) -> bool {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    matches!(name, ".gitignore" | ".ignore" | ".rgignore") || relative == ".git/info/exclude"
}

/// Reads at most `limit` bytes of the regular file at `path` into `buf`.
fn read(path: &Path, limit: u64, buf: &mut Vec<u8>) -> io::Result<()> {
    buf.clear();
    let sys::fs::RegularFile::Open(file) = sys::fs::open_regular(path)? else {
        return Err(io::ErrorKind::InvalidInput.into());
    };
    file.take(limit).read_to_end(buf)?;
    Ok(())
}

/// Whether content looks binary: a NUL early on, as ripgrep decides.
fn is_binary(content: &[u8]) -> bool {
    memchr::memchr(0, &content[..content.len().min(8 << 10)]).is_some()
}
