//! The blob store's backends on the file system: [`Files`], the JSONL
//! store's blobs and streams, and [`CacheDir`], blobs as ordinary files.
//! Apart from `blob` so the SDK stays free of I/O, and here rather than in
//! the engine so an extension's tests run on the store the daemon uses.

mod files;
mod tree;

pub use files::Files;
pub use tree::CacheDir;
