//! The static assembly: the extensions a binary links, each checked as it
//! compiles for the services it needs of the ones before it. A build with
//! `edit` and without `shell` is a type error, not a refusal at daemon
//! start; the dynamic path ([`Built::with_dyn`], a Lua mod, a disabled
//! extension's stand-in) keeps the check at install.
//!
//! The list so far is a type, `(Newest, (Older, (.., ())))`, searched by
//! [`Has`] with its index inferred ([`Here`], [`There`]); one provider per
//! service, or the search is ambiguous and that is the error.

use std::marker::PhantomData;

use crate::host::Registry;
use crate::{DynExtension, Extension, Refused, Registrar, Tier};

/// Provides service `S` to the extensions after it, as the type system
/// knows it: `impl Provides<dyn CwdOf> for Shell {}` beside the
/// [`Registrar::provide`] that provides it at run time.
pub trait Provides<S: ?Sized> {}

/// Service `S`, needed: one entry of [`Extension::Needs`].
pub struct Need<S: ?Sized>(PhantomData<fn(&S)>);

/// A list of [`Need`]s, `()`-terminated, declared at run time too for the
/// registries built then.
pub trait Needs {
    fn declare(r: &mut Registrar);
}

impl Needs for () {
    fn declare(_: &mut Registrar) {}
}

impl<S: ?Sized + 'static, R: Needs> Needs for (Need<S>, R) {
    fn declare(r: &mut Registrar) {
        r.need::<S>();
        R::declare(r);
    }
}

/// The index of a provider: the head of the list.
pub struct Here;
/// The index of a provider: `I` into the tail.
pub struct There<I>(PhantomData<I>);

/// A list with a provider of `S` at index `I`.
pub trait Has<S: ?Sized, I> {}

impl<S: ?Sized, H: Provides<S>, T> Has<S, Here> for (H, T) {}

impl<S: ?Sized, H, T: Has<S, I>, I> Has<S, There<I>> for (H, T) {}

/// Every need of a list is met by list `L`, at indices `I`.
pub trait Met<L, I> {}

impl<L> Met<L, ()> for () {}

impl<S: ?Sized, R, L, I, J> Met<L, (I, J)> for (Need<S>, R)
where
    L: Has<S, I>,
    R: Met<L, J>,
{
}

/// A registry being assembled, its extensions so far in `L`.
///
/// ```compile_fail
/// # use ext::host::Built;
/// # use ext::{CwdOf, Extension, Need, Registrar, Tier};
/// struct Edit;
/// impl Extension for Edit {
///     const NAME: &'static str = "edit";
///     const VERSION: u32 = 1;
///     type Needs = (Need<dyn CwdOf>, ());
///     fn contribute(&self, _: &mut Registrar) {}
/// }
/// // Nothing before it provides where commands run.
/// let built = Built::new().with(Tier::Pre, &Edit);
/// ```
pub struct Built<L>(Registry, PhantomData<L>);

impl Default for Built<()> {
    fn default() -> Self {
        Self::new()
    }
}

impl Built<()> {
    pub fn new() -> Self {
        Self(Registry::default(), PhantomData)
    }
}

impl<L> Built<L> {
    /// Registers `ext` in `tier`, after every extension so far: what it
    /// needs, they provide, or this does not compile.
    pub fn with<E, I>(self, tier: Tier, ext: &E) -> Result<Built<(E, L)>, Refused>
    where
        E: Extension,
        E::Needs: Met<L, I>,
    {
        let registry = self.0.with_in(tier, ext)?;
        Ok(Built(registry, PhantomData))
    }

    /// Registers an extension named at run time, its needs checked now.
    pub fn with_dyn(self, tier: Tier, ext: &dyn DynExtension) -> Result<Self, Refused> {
        let registry = self.0.with_dyn(tier, ext)?;
        Ok(Self(registry, PhantomData))
    }

    pub fn registry(self) -> Registry {
        self.0
    }
}
