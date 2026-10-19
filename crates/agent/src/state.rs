//! Durable extension state: a fold over the session's `ext` entries, so it
//! survives a restart as the rest of the log does.

use std::any::{Any, TypeId};
use std::path::{Path, PathBuf};

use protocol::{Body, Entry};
use serde_json::Value;

/// State an extension keeps per session. Each change is logged as
/// `Body::Ext { ext, state: NAME, data }` and folded in, in log order.
pub trait State: Default + Send + 'static {
    const NAME: &'static str;
    fn fold(&mut self, data: &Value);
}

/// How to make and fold one registered state.
#[derive(Clone, Copy)]
pub(crate) struct Decl {
    pub name: &'static str,
    pub ty: TypeId,
    make: fn() -> Box<dyn Any + Send>,
    fold: fn(&mut dyn Any, &Value),
}

impl Decl {
    pub fn of<S: State>() -> Self {
        Self {
            name: S::NAME,
            ty: TypeId::of::<S>(),
            make: || Box::new(S::default()),
            fold: |state, data| {
                if let Some(state) = state.downcast_mut::<S>() {
                    state.fold(data);
                }
            },
        }
    }
}

struct Held {
    ext: &'static str,
    decl: Decl,
    value: Box<dyn Any + Send>,
}

/// One session's extension states.
#[derive(Default)]
pub struct States(Vec<Held>);

impl States {
    pub(crate) fn new(decls: impl IntoIterator<Item = (&'static str, Decl)>) -> Self {
        Self(
            decls
                .into_iter()
                .map(|(ext, decl)| Held {
                    ext,
                    decl,
                    value: (decl.make)(),
                })
                .collect(),
        )
    }

    pub fn get<S: State>(&self) -> Option<&S> {
        self.0.iter().find_map(|h| h.value.downcast_ref())
    }

    /// The extension that registered `S`.
    pub fn ext_of<S: State>(&self) -> Option<&'static str> {
        let ty = TypeId::of::<S>();
        self.0.iter().find(|h| h.decl.ty == ty).map(|h| h.ext)
    }

    /// Folds one change in. A change to a state no extension registers now
    /// (one removed since) is skipped.
    pub fn fold(&mut self, ext: &str, state: &str, data: &Value) {
        for held in &mut self.0 {
            if held.ext == ext && held.decl.name == state {
                (held.decl.fold)(&mut *held.value, data);
            }
        }
    }

    /// Folds every change `entries` record.
    pub fn replay(&mut self, entries: &[Entry]) {
        for e in entries {
            if let Body::Ext { ext, state, data } = &e.body {
                self.fold(ext, state, &data.value());
            }
        }
    }
}

/// Where the session's last command left off, and the client directory it
/// started from, as the shell records it.
#[derive(Debug, Default)]
pub struct Cwd {
    dir: Option<PathBuf>,
    base: Option<PathBuf>,
}

impl Cwd {
    /// The change recording that a command run from `base` ended in `dir`.
    pub fn change(dir: &Path, base: Option<&Path>) -> Value {
        serde_json::json!({ "cwd": dir, "base": base })
    }

    /// The last command's directory, unless the client has since moved
    /// away from where it ran.
    pub fn after(&self, base: Option<&Path>) -> Option<&Path> {
        self.dir.as_deref().filter(|_| self.base.as_deref() == base)
    }
}

impl State for Cwd {
    const NAME: &'static str = "cwd";

    fn fold(&mut self, data: &Value) {
        let path = |key: &str| data[key].as_str().map(PathBuf::from);
        self.dir = path("cwd");
        self.base = path("base");
    }
}
