//! Declaration-owned launch settings are implicit; session inputs stay immutable references.

use std::cmp::Ordering;
use std::path::PathBuf;
use std::sync::Arc;

use crate::ServerName;

#[derive(Clone, Default)]
pub(crate) struct Environment(pub Option<Arc<protocol::Environment>>);

impl PartialEq for Environment {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for Environment {}
impl PartialOrd for Environment {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Environment {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .as_ref()
            .map(|env| &env.vars)
            .cmp(&other.0.as_ref().map(|env| &env.vars))
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Identity {
    pub declaration: u64,
    pub server: ServerName,
    pub cwd: PathBuf,
    pub env: Environment,
}
