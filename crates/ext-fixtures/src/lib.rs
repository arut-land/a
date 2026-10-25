//! For tests: the `echo` agent, which needs no model, `sleep`, a safe call
//! to interrupt and recover, and the `echo` query.
//!
//! The crate is empty unless its `on` feature is: a binary turns that on
//! for its tests (through a dev-dependency, as cargo unifies features) and
//! never for a release, which therefore holds none of this.

/// The test agent's name.
pub const ECHO: &str = "echo";

#[cfg(any(test, feature = "on"))]
mod on;

#[cfg(any(test, feature = "on"))]
pub use on::{Echo, Echoed, Fixtures, Said, Say, Seconds, Sleep, Slept};

/// The fixtures, in a build that has them.
pub fn extension() -> Option<Box<dyn ext::DynExtension>> {
    #[cfg(any(test, feature = "on"))]
    return Some(Box::new(Fixtures));
    #[cfg(not(any(test, feature = "on")))]
    None
}
