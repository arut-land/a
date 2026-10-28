//! Declaration-owned launch settings are implicit; session inputs stay immutable references.

use std::cmp::Ordering;
use std::path::PathBuf;
use std::sync::Arc;

use crate::{Failure, ServerName, config::Scope};

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

/// Shared launch policy belongs to the declaration; current Spawn handlers
/// require session authority, so only an empty chain admits broader scope.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Policy {
    declaration: u64,
    binding: Binding,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Binding {
    Session,
    Declaration,
}

impl Policy {
    pub(crate) fn admit(declaration: u64, scope: Scope, cx: &ext::Cx) -> Result<Self, Failure> {
        if scope.shared() && cx.has_handlers::<ext::point::Spawn>() {
            return Err(Failure::Configuration(
                "shared MCP launches require session scope when spawn hooks are registered".into(),
            ));
        }
        Ok(Self {
            declaration,
            binding: if scope.shared() {
                Binding::Declaration
            } else {
                Binding::Session
            },
        })
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Identity {
    pub policy: Policy,
    pub server: ServerName,
    pub cwd: PathBuf,
    pub env: Environment,
}
