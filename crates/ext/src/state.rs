//! Durable extension state: a fold over the session's `ext` entries, so it
//! survives a restart as the rest of the log does. A fold is its own
//! image: a snapshot of the session keeps each as it stands, and a load
//! from one folds only the entries after it.

use std::any::{Any, TypeId};
use std::borrow::Borrow;
use std::sync::Arc;

use protocol::{Body, Entry, Json};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::Name;

/// Fold an extension keeps per session. Each change is logged as
/// `Body::Ext { ext, state: NAME, data }` and folded in, in log order.
///
/// It is read only inside a closure, so no extension can hold it across an
/// `.await` (where the session's next change would wait for it):
///
/// ```compile_fail
/// # use ext::{Cx, Fold};
/// # #[derive(Default, serde::Serialize, serde::Deserialize)] struct Count(u64);
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
pub trait Fold: Default + Serialize + DeserializeOwned + Send + 'static {
    const NAME: &'static str;
    /// What one change records.
    type Change: Serialize + DeserializeOwned;

    fn apply(&mut self, change: &Self::Change);
}

/// A durable fold whose name and value are known only at run time.
/// A failed change leaves its previous value intact.
pub trait DynFold: Send + Sync {
    fn name(&self) -> Name;
    fn initial(&self) -> Json;
    fn restore(&self, image: &Json) -> Result<Json, String> {
        Ok(image.clone())
    }
    fn apply(&self, state: &Json, change: &Json) -> Result<Json, String>;
}

type Value = Box<dyn Any + Send>;
type Apply = dyn Fn(&mut dyn Any, &Json) -> Result<(), String> + Send + Sync;
type Encode = dyn Fn(&dyn Any) -> Result<Json, String> + Send + Sync;
type Restore = dyn Fn(&Json) -> Result<Value, String> + Send + Sync;

/// How to make and fold one registered state.
#[derive(Clone)]
pub(crate) struct Decl {
    pub name: Name,
    pub ty: Option<TypeId>,
    make: Arc<dyn Fn() -> Value + Send + Sync>,
    fold: Arc<Apply>,
    image: Arc<Encode>,
    restore: Arc<Restore>,
}

impl Decl {
    pub fn of<S: Fold>() -> Self {
        Self {
            name: Name::from(S::NAME),
            ty: Some(TypeId::of::<S>()),
            make: Arc::new(|| Box::new(S::default())),
            fold: Arc::new(|state, data| {
                let change = data.parse::<S::Change>().map_err(|e| e.to_string())?;
                if let Some(state) = state.downcast_mut::<S>() {
                    state.apply(&change);
                }
                Ok(())
            }),
            image: Arc::new(|state| {
                let state = state.downcast_ref::<S>().ok_or("another state")?;
                Json::encode(state).map_err(|e| e.to_string())
            }),
            restore: Arc::new(|image| {
                let state = image.parse::<S>().map_err(|e| e.to_string())?;
                Ok(Box::new(state))
            }),
        }
    }

    pub fn erased(fold: Arc<dyn DynFold>) -> Self {
        let initial = fold.initial();
        let restore = Arc::clone(&fold);
        Self {
            name: fold.name(),
            ty: None,
            make: Arc::new(move || Box::new(initial.clone())),
            fold: Arc::new(move |state, change| {
                let state = state.downcast_mut::<Json>().ok_or("another state")?;
                *state = fold.apply(state, change)?;
                Ok(())
            }),
            image: Arc::new(|state| {
                state
                    .downcast_ref::<Json>()
                    .cloned()
                    .ok_or_else(|| "another state".into())
            }),
            restore: Arc::new(move |image| Ok(Box::new(restore.restore(image)?))),
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
pub struct States {
    held: Vec<Held>,
    rejected: Option<String>,
}

impl States {
    pub(crate) fn new(decls: impl IntoIterator<Item = (Name, Decl)>) -> Self {
        Self {
            held: decls
                .into_iter()
                .map(|(ext, decl)| Held {
                    value: (decl.make)(),
                    ext,
                    decl,
                })
                .collect(),
            rejected: None,
        }
    }

    pub fn get<S: Fold>(&self) -> Option<&S> {
        self.held.iter().find_map(|h| h.value.downcast_ref())
    }

    pub fn erased(&self, ext: &str, name: &str) -> Option<&Json> {
        self.held
            .iter()
            .find(|h| &*h.ext == ext && &*h.decl.name == name && h.decl.ty.is_none())
            .and_then(|h| h.value.downcast_ref())
    }

    /// The extension that registered `S`.
    pub(crate) fn ext_of<S: Fold>(&self) -> Option<&Name> {
        let ty = TypeId::of::<S>();
        self.held
            .iter()
            .find(|h| h.decl.ty == Some(ty))
            .map(|h| &h.ext)
    }

    /// Folds one change in. A change to a state no extension registers now
    /// (one removed since) is skipped, and so, with a warning, is one its
    /// state cannot read.
    pub fn fold(&mut self, ext: &str, state: &str, data: &Json) {
        if let Err(e) = self.try_fold(ext, state, data) {
            eprintln!("warning: {e}");
            self.rejected.get_or_insert(e);
        }
    }

    fn try_fold(&mut self, ext: &str, state: &str, data: &Json) -> Result<(), String> {
        let held = self.held.iter_mut();
        for held in held.filter(|h| &*h.ext == ext && &*h.decl.name == state) {
            (held.decl.fold)(&mut *held.value, data)
                .map_err(|e| format!("extension {ext}: a {state} change it cannot read: {e}"))?;
        }
        Ok(())
    }

    /// Folds or skips the change `e` records, exactly as live execution does.
    pub fn apply(&mut self, e: &Entry) {
        if let Body::Ext { ext, state, data } = &e.body {
            self.fold(ext, state, data);
        }
    }

    /// Candidates require every reduction since creation or restore to
    /// succeed; live sessions keep serving after a skipped reduction.
    pub fn validate(&self) -> Result<(), String> {
        self.rejected.as_ref().map_or(Ok(()), |e| Err(e.clone()))
    }

    /// Folds every change `entries` record.
    pub fn replay(&mut self, entries: impl IntoIterator<Item = impl Borrow<Entry>>) {
        for e in entries {
            self.apply(e.borrow());
        }
    }

    /// Every state as it stands, by the extension that registered it and
    /// its name: what a snapshot keeps. A state that cannot be encoded
    /// fails the lot: a snapshot is whole or not taken.
    pub fn image(&self) -> Result<Vec<Image>, String> {
        self.held
            .iter()
            .map(|h| {
                let image = (h.decl.image)(&*h.value)
                    .map_err(|e| format!("extension {}: encoding {}: {e}", h.ext, h.decl.name))?;
                Ok(Image {
                    ext: h.ext.to_string(),
                    state: h.decl.name.to_string(),
                    image,
                })
            })
            .collect()
    }

    /// The states as `images` left them: exactly one image per registered
    /// state. One missing, extra, or with an image its state
    /// cannot read fails the lot, the states left as they were: the
    /// snapshot is of another build, and the log is folded instead.
    pub fn restore(&mut self, images: &[Image]) -> Result<(), String> {
        if images.len() != self.held.len() {
            return Err("checkpoint fold coverage differs".into());
        }
        let restored = (self.held.iter())
            .map(|h| {
                let image = images
                    .iter()
                    .find(|i| i.ext == *h.ext && i.state == *h.decl.name);
                match image {
                    Some(i) => (h.decl.restore)(&i.image)
                        .map_err(|e| format!("extension {}: reading {}: {e}", h.ext, h.decl.name)),
                    None => Err(format!(
                        "extension {}: missing {} image",
                        h.ext, h.decl.name
                    )),
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        for (held, value) in self.held.iter_mut().zip(restored) {
            held.value = value;
        }
        self.rejected = None;
        Ok(())
    }
}

/// One state as a snapshot keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    pub ext: String,
    pub state: String,
    pub image: Json,
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

    #[derive(Default, Serialize, Deserialize)]
    struct Count(u64);

    impl Fold for Count {
        const NAME: &'static str = "count";
        type Change = u64;

        fn apply(&mut self, n: &u64) {
            self.0 += n;
        }
    }

    /// A state restored from its image folds on as the one imaged did;
    /// an image a state cannot read restores nothing.
    #[test]
    fn a_state_restored_from_its_image_is_the_state_imaged() {
        let mut states = States::new([(Name::from("counter"), Decl::of::<Count>())]);
        states.fold("counter", "count", &Json::encode(&2).unwrap());
        let image = states.image().unwrap();
        let mut restored = States::new([(Name::from("counter"), Decl::of::<Count>())]);
        restored.restore(&image).unwrap();
        restored.fold("counter", "count", &Json::encode(&3).unwrap());
        assert_eq!(restored.get::<Count>().unwrap().0, 5);
        let bad = vec![Image {
            ext: "counter".into(),
            state: "count".into(),
            image: Json::encode("two").unwrap(),
        }];
        assert!(
            restored
                .restore(&bad)
                .unwrap_err()
                .contains("reading count")
        );
        assert_eq!(restored.get::<Count>().unwrap().0, 5, "left as it was");
    }

    #[test]
    fn a_checkpoint_covers_exactly_the_registered_folds() {
        let mut states = States::new([(Name::from("counter"), Decl::of::<Count>())]);
        states.fold("counter", "count", &Json::encode(&2).unwrap());
        assert!(states.restore(&[]).is_err());
        let mut extra = states.image().unwrap();
        extra.push(Image {
            ext: "other".into(),
            state: "count".into(),
            image: Json::default(),
        });
        assert!(states.restore(&extra).is_err());
        assert_eq!(states.get::<Count>().unwrap().0, 2);
    }

    #[test]
    fn a_skipped_change_keeps_its_value_and_rejects_candidate_validation() {
        let mut states = States::new([(Name::from("counter"), Decl::of::<Count>())]);
        states.fold("counter", "count", &Json::encode(&2).unwrap());
        states.fold("counter", "count", &Json::encode("two").unwrap());
        assert!(
            states
                .validate()
                .unwrap_err()
                .starts_with("extension counter: a count change")
        );
        // Another extension's state of the same name is not this one.
        states.fold("other", "count", &Json::encode(&5).unwrap());
        assert_eq!(states.get::<Count>().unwrap().0, 2);
        states.fold("counter", "count", &Json::encode(&3).unwrap());
        assert_eq!(states.get::<Count>().unwrap().0, 5);
        assert!(
            states.validate().is_err(),
            "later successes cannot validate a rejected candidate"
        );
        states.restore(&states.image().unwrap()).unwrap();
        assert!(
            states.validate().is_ok(),
            "a restore begins a new replay suffix"
        );
    }
}
