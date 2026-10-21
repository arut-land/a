//! Durable extension state: a fold over the session's `ext` entries, so it
//! survives a restart as the rest of the log does. A fold is its own
//! image: a snapshot of the session keeps each as it stands, and a load
//! from one folds only the entries after it.

use std::any::{Any, TypeId};
use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::sync::Arc;

use protocol::{Body, Entry, Json, Seq};
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
    /// Session configuration that a child kind may inherit.
    const SETTINGS: bool = false;
    /// What one change records.
    type Change: Serialize + DeserializeOwned;

    fn apply(&mut self, change: &Self::Change);
}

/// A typed identity for model context, sealed with its text in one entry.
pub trait Publication {
    const NAME: &'static str;
    type Key: Serialize + ?Sized;
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
    settings: bool,
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
            settings: S::SETTINGS,
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
            settings: false,
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
    rejected: BTreeMap<Seq, String>,
}

impl Held {
    fn image(&self) -> Result<Image, String> {
        let image = (self.decl.image)(&*self.value)
            .map_err(|e| format!("extension {}: encoding {}: {e}", self.ext, self.decl.name))?;
        Ok(Image {
            ext: self.ext.to_string(),
            state: self.decl.name.to_string(),
            image,
            rejected: self.rejected.clone(),
        })
    }
}

/// One session's extension states.
#[derive(Default)]
pub struct States {
    held: Vec<Held>,
    publications: protocol::publication::Publications,
}

impl States {
    pub(crate) fn new(decls: impl IntoIterator<Item = (Name, Decl)>) -> Self {
        Self {
            publications: protocol::publication::Publications::default(),
            held: decls
                .into_iter()
                .map(|(ext, decl)| Held {
                    value: (decl.make)(),
                    ext,
                    decl,
                    rejected: BTreeMap::new(),
                })
                .collect(),
        }
    }

    /// Only declared settings of registrations present in the child travel.
    pub fn inherit_settings(&mut self, parent: &Self) -> Result<(), String> {
        for held in &mut self.held {
            if held.decl.settings
                && let Some(source) = parent
                    .held
                    .iter()
                    .find(|s| s.ext == held.ext && s.decl.name == held.decl.name)
            {
                held.value = (held.decl.restore)(&(source.decl.image)(&*source.value)?)?;
            }
        }
        Ok(())
    }

    /// A child receives configuration, without conversation publications or resources.
    pub fn settings_image(&self) -> Result<Checkpoint, String> {
        Ok(Checkpoint {
            folds: self
                .held
                .iter()
                .filter(|h| h.decl.settings)
                .map(Held::image)
                .collect::<Result<_, _>>()?,
            publications: protocol::publication::Publications::default(),
        })
    }

    pub fn restore_settings(&mut self, image: &Checkpoint) -> Result<(), String> {
        for setting in &image.folds {
            let held = self
                .held
                .iter_mut()
                .find(|h| h.decl.settings && *h.ext == setting.ext && *h.decl.name == setting.state)
                .ok_or_else(|| format!("unregistered setting {}:{}", setting.ext, setting.state))?;
            held.value = (held.decl.restore)(&setting.image)?;
        }
        Ok(())
    }

    /// Sets a registered setting while constructing a kind's admission image.
    pub fn set<S: Fold>(&mut self, value: S) {
        const { assert!(S::SETTINGS, "only settings can seed an agent kind") };
        if let Some(held) = self
            .held
            .iter_mut()
            .find(|h| h.decl.ty == Some(TypeId::of::<S>()))
        {
            held.value = Box::new(value);
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
        self.reduce(ext, state, data, None);
    }

    fn reduce(&mut self, ext: &str, state: &str, data: &Json, seq: Option<Seq>) {
        for held in self
            .held
            .iter_mut()
            .filter(|h| &*h.ext == ext && &*h.decl.name == state)
        {
            if let Err(error) = (held.decl.fold)(&mut *held.value, data) {
                let error = format!("extension {ext}: a {state} change it cannot read: {error}");
                eprintln!("warning: {error}");
                if let Some(seq) = seq {
                    held.rejected.insert(seq, error);
                }
            }
        }
    }

    /// Folds or skips the change `e` records, exactly as live execution does.
    pub fn apply(&mut self, e: &Entry) {
        self.publications.apply(e);
        if let Body::Ext { ext, state, data } = &e.body {
            self.reduce(ext, state, data, Some(e.seq));
        }
    }

    pub fn registered(&self, ext: &str, state: &str) -> bool {
        self.held
            .iter()
            .any(|held| &*held.ext == ext && &*held.decl.name == state)
    }

    /// Reports the live fold's decision for an entry already applied.
    pub fn mutation_result(&self, entry: &Entry) -> crate::MutationResult {
        if let Body::Ext { ext, state, .. } = &entry.body {
            let held = self
                .held
                .iter()
                .find(|held| &*held.ext == ext && &*held.decl.name == state)
                .ok_or(crate::MutationError::Unregistered)?;
            if let Some(error) = held.rejected.get(&entry.seq) {
                return Err(crate::MutationError::Rejected(error.clone()));
            }
        }
        Ok(entry.seq)
    }

    pub fn publications(&self) -> &protocol::publication::Publications {
        &self.publications
    }

    /// A candidate may fix or share live rejections, but cannot reject an
    /// entry the live generation accepted. Checkpoints retain these decisions.
    pub fn validate(&self, live: &Self) -> Result<(), String> {
        for held in &self.held {
            let previous = live
                .held
                .iter()
                .find(|h| h.ext == held.ext && h.decl.name == held.decl.name);
            for (seq, error) in &held.rejected {
                if !previous.is_some_and(|h| h.rejected.contains_key(seq)) {
                    return Err(error.clone());
                }
            }
        }
        Ok(())
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
    pub fn image(&self) -> Result<Checkpoint, String> {
        let folds = self
            .held
            .iter()
            .map(Held::image)
            .collect::<Result<_, _>>()?;
        Ok(Checkpoint {
            folds,
            publications: self.publications.clone(),
        })
    }

    /// The states as `images` left them: exactly one image per registered
    /// state. One missing, extra, or with an image its state
    /// cannot read fails the lot, the states left as they were: the
    /// snapshot is of another build, and the log is folded instead.
    pub fn restore(&mut self, checkpoint: &Checkpoint) -> Result<(), String> {
        let images = &checkpoint.folds;
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
                        .map(|value| (value, i.rejected.clone()))
                        .map_err(|e| format!("extension {}: reading {}: {e}", h.ext, h.decl.name)),
                    None => Err(format!(
                        "extension {}: missing {} image",
                        h.ext, h.decl.name
                    )),
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.publications = checkpoint.publications.clone();
        for (held, (value, rejected)) in self.held.iter_mut().zip(restored) {
            held.value = value;
            held.rejected = rejected;
        }
        Ok(())
    }
}

/// A complete SDK checkpoint, including named model context identities.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub folds: Vec<Image>,
    pub publications: protocol::publication::Publications,
}

/// One state as a snapshot keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    pub ext: String,
    pub state: String,
    pub image: Json,
    /// Skipped entries, kept so validation survives a checkpoint.
    pub rejected: BTreeMap<Seq, String>,
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

    #[test]
    fn fresh_resources_never_arrive_through_settings_inheritance() {
        #[derive(Default, Serialize, Deserialize)]
        struct Setting(u64, #[serde(skip)] Arc<()>);
        impl Fold for Setting {
            const NAME: &'static str = "setting";
            const SETTINGS: bool = true;
            type Change = u64;
            fn apply(&mut self, value: &u64) {
                self.0 = *value;
            }
        }
        let fresh = || {
            States::new([
                (Name::from("extension"), Decl::of::<Setting>()),
                (Name::from("extension"), Decl::of::<Count>()),
            ])
        };
        let mut parent = fresh();
        parent.fold("extension", "setting", &Json::encode(&7).unwrap());
        parent.fold("extension", "count", &Json::encode(&42).unwrap());
        let image = parent.settings_image().unwrap();
        assert_eq!(image.folds.len(), 1);
        let mut child = fresh();
        child.inherit_settings(&parent).unwrap();
        let mut restored = fresh();
        restored.restore_settings(&image).unwrap();
        for states in [&child, &restored] {
            assert_eq!(states.get::<Setting>().unwrap().0, 7);
            assert_eq!(states.get::<Count>().unwrap().0, 0);
            assert!(!Arc::ptr_eq(
                &states.get::<Setting>().unwrap().1,
                &parent.get::<Setting>().unwrap().1
            ));
        }
        let mut grandchild = fresh();
        grandchild.inherit_settings(&restored).unwrap();
        assert_eq!(grandchild.get::<Setting>().unwrap().0, 7);
        assert_eq!(grandchild.get::<Count>().unwrap().0, 0);
        assert!(!Arc::ptr_eq(
            &grandchild.get::<Setting>().unwrap().1,
            &restored.get::<Setting>().unwrap().1
        ));
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
            rejected: BTreeMap::new(),
        }];
        assert!(
            restored
                .restore(&Checkpoint {
                    folds: bad,
                    ..Checkpoint::default()
                })
                .unwrap_err()
                .contains("reading count")
        );
        assert_eq!(restored.get::<Count>().unwrap().0, 5, "left as it was");
    }

    #[test]
    fn a_checkpoint_covers_exactly_the_registered_folds() {
        let mut states = States::new([(Name::from("counter"), Decl::of::<Count>())]);
        states.fold("counter", "count", &Json::encode(&2).unwrap());
        assert!(states.restore(&Checkpoint::default()).is_err());
        let mut extra = states.image().unwrap();
        extra.folds.push(Image {
            ext: "other".into(),
            state: "count".into(),
            image: Json::default(),
            rejected: BTreeMap::new(),
        });
        assert!(states.restore(&extra).is_err());
        assert_eq!(states.get::<Count>().unwrap().0, 2);
    }

    #[test]
    fn candidate_validation_compares_the_same_entries_across_checkpoints() {
        let make = || States::new([(Name::from("counter"), Decl::of::<Count>())]);
        let change = |seq, data| {
            Entry::new(
                Seq::new(seq).unwrap(),
                protocol::Epoch::default(),
                Body::Ext {
                    ext: "counter".into(),
                    state: "count".into(),
                    data,
                },
            )
        };
        let good = change(1, Json::encode(&2).unwrap());
        let bad = change(2, Json::encode("two").unwrap());
        let later = change(3, Json::encode(&3).unwrap());
        let mut live = make();
        live.apply(&good);
        let mut candidate = make();
        candidate.replay([&good, &bad, &later]);
        assert_eq!(candidate.get::<Count>().unwrap().0, 5);
        assert!(candidate.validate(&live).is_err());
        live.apply(&bad);
        assert!(candidate.validate(&live).is_ok());
        let checkpoint = Json::encode(&live.image().unwrap()).unwrap();
        let mut restored = make();
        restored
            .restore(&checkpoint.parse::<Checkpoint>().unwrap())
            .unwrap();
        assert!(candidate.validate(&restored).is_ok());
        // The seq identifies the decision; an equal payload at another
        // position was never rejected by the live generation.
        candidate.apply(&change(4, Json::encode("two").unwrap()));
        assert!(candidate.validate(&restored).is_err());
    }
}
