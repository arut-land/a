//! Sandboxed Lua mods over the extension SDK's JSON ABI. One VM belongs
//! to a mod generation; durable session data belongs to SDK folds.

mod loader;
mod vm;

pub use loader::Loader;
pub use vm::{Limits, Mod};
