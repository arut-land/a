//! Line-oriented search over byte slices, with optional multi-line
//! matching: FFF's simplification of ripgrep's `grep-searcher`, which
//! only searches slices (no readers, files or memory maps).

pub use self::matcher::{LineTerminator, Match, Matcher, NoError};
pub use self::searcher::{Searcher, SearcherBuilder};
pub use self::sink::{Sink, SinkError, SinkFinish, SinkMatch};

pub mod lines;
pub mod matcher;
#[allow(clippy::module_inception)]
mod searcher;
mod sink;
