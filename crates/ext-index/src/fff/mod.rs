//! A pruned copy of FFF's search core (`fff-search`, `fff-grep` and
//! `fff-query-parser` 0.11.0 at 7353971, MIT, see `LICENSE`), owned here
//! as `llm` owns its copy of rig: the fuzzy scorer, the bigram content
//! prefilter, query constraints, grep over byte slices and the query
//! parser. `searcher` is FFF's simplification of ripgrep's
//! `grep-searcher` (MIT or Unlicense, Andrew Gallant).
//!
//! What FFF ran itself is left out: its threads and pools, watcher,
//! databases, git status and memory maps. Paths live in one contiguous
//! arena ([`types::Files`]) instead of FFF's chunk arena of raw pointers,
//! and nothing here is `unsafe`. The index around it (`crate::index`)
//! decides when this runs and on which thread.

pub mod grep;
pub(crate) mod ignore;
pub(crate) mod index;
mod match_offsets;
pub(crate) mod query;
pub(crate) mod score;
pub(crate) mod searcher;
pub(crate) mod types;
pub(crate) mod walk;

/// The largest file grep reads whole.
pub(crate) const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// The most of a file the bigram index reads: a larger one would only
/// add bigrams that filter nothing.
pub(crate) const MAX_INDEXABLE_FILE_SIZE: usize = 2 * 1024 * 1024;
