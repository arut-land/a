//! Durable extension state: a fold over the session's `ext` entries, so it
//! survives a restart as the rest of the log does.

use std::any::{Any, TypeId};
use std::borrow::Borrow;

use protocol::{Body, Entry, Json};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::Name;

/// Fold an extension keeps per session. Each change is logged as
/// `Body::Ext { ext, state: NAME, data }` and folded in, in log order.
///
/// It is read only inside a closure, so no extension can hold it across an
/// `.await` (where the session's next change would wait for it):
///
/// ```compile_fail
/// # use ext::{Cx, Fold};
/// # #[derive(Default)] struct Count(u64);
/// # impl Fold for Count {
/// #     const NAME: &'static str = "count";
/// #     type Change = u64;
/// #     fn apply(&mut self, n: &u64) { self.0 += n; }
/// # }
/// async fn hold(cx: &Cx) {
///     let count = cx.state(|count: &Count| count);
///     std::future::ready(()).await;
///     let _ = count.0;
/// }
/// ```
pub trait Fold: Default + Send + 'static {
    const NAME: &'static str;
    /// What one change records.
    type Change: Serialize + DeserializeOwned;

    fn apply(&mut self, change: &Self::Change);
}

/// How to make and fold one registered state.
#[derive(Clone, Copy)]
pub(crate) struct Decl {
    pub name: &'static str,
    pub ty: TypeId,
    make: fn() -> Box<dyn Any + Send>,
    fold: fn(&mut dyn Any, &Json) -> Result<(), String>,
}

impl Decl {
    pub fn of<S: Fold>() -> Self {
        Self {
            name: S::NAME,
            ty: TypeId::of::<S>(),
            make: || Box::new(S::default()),
            fold: |state, data| {
                let change = data.parse::<S::Change>().map_err(|e| e.to_string())?;
                if let Some(state) = state.downcast_mut::<S>() {
                    state.apply(&change);
                }
                Ok(())
            },
        }
    }
}

struct Held {
    ext: Name,
    decl: Decl,
    value: Box<dyn Any + Send>,
}

/// One session's extension states.
#[derive(Default)]
pub struct States(Vec<Held>);

impl States {
    pub(crate) fn new(decls: impl IntoIterator<Item = (Name, Decl)>) -> Self {
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

    pub fn get<S: Fold>(&self) -> Option<&S> {
        self.0.iter().find_map(|h| h.value.downcast_ref())
    }

    /// The extension that registered `S`.
    pub(crate) fn ext_of<S: Fold>(&self) -> Option<&Name> {
        let ty = TypeId::of::<S>();
        self.0.iter().find(|h| h.decl.ty == ty).map(|h| &h.ext)
    }

    /// Folds one change in. A change to a state no extension registers now
    /// (one removed since) is skipped, and so, with a warning, is one its
    /// state cannot read.
    pub fn fold(&mut self, ext: &str, state: &str, data: &Json) {
        if let Err(e) = self.try_fold(ext, state, data) {
            eprintln!("warning: {e}");
        }
    }

    fn try_fold(&mut self, ext: &str, state: &str, data: &Json) -> Result<(), String> {
        let held = self.0.iter_mut();
        for held in held.filter(|h| &*h.ext == ext && h.decl.name == state) {
            (held.decl.fold)(&mut *held.value, data)
                .map_err(|e| format!("extension {ext}: a {state} change it cannot read: {e}"))?;
        }
        Ok(())
    }

    /// Folds the change `e` records, if any.
    pub fn apply(&mut self, e: &Entry) {
        if let Body::Ext { ext, state, data } = &e.body {
            self.fold(ext, state, data);
        }
    }

    /// Folds every change `entries` record.
    pub fn replay(&mut self, entries: impl IntoIterator<Item = impl Borrow<Entry>>) {
        for e in entries {
            self.apply(e.borrow());
        }
    }
}

/// Reads state `S` from the states `with` lends, or its default when no
/// extension registered it.
pub(crate) fn read<S: Fold, R>(
    with: impl FnOnce(&mut dyn FnMut(&States)),
    read: impl FnOnce(&S) -> R,
) -> R {
    let mut read = Some(read);
    let mut out = None;
    with(&mut |states| {
        if let Some(read) = read.take() {
            out = Some(match states.get::<S>() {
                Some(state) => read(state),
                None => read(&S::default()),
            });
        }
    });
    out.expect("the session's states are lent once")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Count(u64);

    impl Fold for Count {
        const NAME: &'static str = "count";
        type Change = u64;

        fn apply(&mut self, n: &u64) {
            self.0 += n;
        }
    }

    #[test]
    fn a_change_its_state_cannot_read_is_skipped_with_a_warning() {
        let mut states = States::new([(Name::from("counter"), Decl::of::<Count>())]);
        states.fold("counter", "count", &Json::encode(&2).unwrap());
        let bad = states.try_fold("counter", "count", &Json::encode("two").unwrap());
        assert!(
            bad.unwrap_err()
                .starts_with("extension counter: a count change")
        );
        // Another extension's state of the same name is not this one.
        states.fold("other", "count", &Json::encode(&5).unwrap());
        assert_eq!(states.get::<Count>().unwrap().0, 2);
    }
}
