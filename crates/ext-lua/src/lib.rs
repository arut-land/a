//! Sandboxed Lua mods over the extension SDK's JSON ABI. One VM belongs
//! to a mod generation; durable session data belongs to SDK folds.
//! Source changes settle through the host's async delay seam before
//! generation preparation is admitted to its blocking pool.

mod loader;
mod vm;

pub use loader::Loader;
pub use vm::{Limits, Mod};
