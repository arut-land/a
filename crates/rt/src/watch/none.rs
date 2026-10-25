//! No native watcher on this OS: every root is unsupported.

use std::path::Path;
use std::sync::Weak;

use super::{Shared, Sub, Unavailable};
use crate::Rt;

pub(super) struct Native;

#[allow(
    clippy::unused_self,
    clippy::unnecessary_wraps,
    reason = "one interface for every backend"
)]
impl Native {
    pub(super) fn new(_: Weak<Shared>, _: Rt) -> Self {
        Self
    }

    pub(super) fn arm(&mut self, _: u64, _: &Sub) -> Result<(), Unavailable> {
        Err(Unavailable::Unsupported)
    }

    pub(super) fn disarm(&mut self, _: u64) {}

    pub(super) fn enter(&mut self, _: u64, _: &Path) -> Result<(), Unavailable> {
        Err(Unavailable::Unsupported)
    }

    pub(super) fn leave(&mut self, _: u64, _: &Path) {}

    pub(super) fn grow(&mut self, _: u64, _: &Sub, _: &Path) -> Result<(), Unavailable> {
        Err(Unavailable::Unsupported)
    }

    pub(super) fn watches(&self) -> usize {
        0
    }
}
