//! Grep over byte slices. `grep.rs` compiles a query into one [`Grep`]
//! for every mode (plain, regex, fuzzy) and searches a file's bytes;
//! `prefilter` picks the files to read; `regex` and `fuzzy_grep` hold
//! the mode-specific machinery on top of the shared `sink`.

mod fuzzy_grep;
#[allow(clippy::module_inception)]
mod grep;
mod prefilter;
mod reader;
mod regex;
mod sink;
mod types;

pub(crate) use grep::{Grep, Prefilter, parse_grep_query};
pub(crate) use prefilter::{Known, files_to_search};
pub use types::{Casing, GrepMatch, GrepMode, GrepOptions};
