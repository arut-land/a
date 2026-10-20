//! An extension's settings: its own `[ext.<name>]` table of the config
//! file, read by the extension alone, so the binary that assembles the
//! extensions knows no key of any.

use crate::{Extension, Refused};

/// An extension's table, `[ext.<name>]`, as the host read it: settings of
/// the few shapes an extension takes. A key of another shape is an error
/// naming `ext.<name>.<key>`.
pub trait Table {
    /// The extension's name, as the table is named.
    fn name(&self) -> &str;
    fn text(&self, key: &str) -> Result<Option<&str>, Refused>;
    fn whole(&self, key: &str) -> Result<Option<u64>, Refused>;
    fn flag(&self, key: &str) -> Result<Option<bool>, Refused>;
    fn list(&self, key: &str) -> Result<Option<&[String]>, Refused>;

    /// Whole number `key`, as the narrower `N`.
    fn number<N: TryFrom<u64>>(&self, key: &str) -> Result<Option<N>, Refused>
    where
        Self: Sized,
    {
        self.whole(key)?
            .map(|n| {
                N::try_from(n)
                    .map_err(|_| Refused(format!("ext.{}.{key} is too large", self.name())))
            })
            .transpose()
    }
}

/// An extension built from its table: every setting it takes is read here,
/// by the crate that defines it.
pub trait Configured: Extension + Sized {
    /// The extension as `table` sets it; `Err` names the key at fault.
    fn configure(table: &impl Table) -> Result<Self, Refused>;
}

/// A table with nothing set.
pub struct Empty<'a>(pub &'a str);

impl Table for Empty<'_> {
    fn name(&self) -> &str {
        self.0
    }

    fn text(&self, _: &str) -> Result<Option<&str>, Refused> {
        Ok(None)
    }

    fn whole(&self, _: &str) -> Result<Option<u64>, Refused> {
        Ok(None)
    }

    fn flag(&self, _: &str) -> Result<Option<bool>, Refused> {
        Ok(None)
    }

    fn list(&self, _: &str) -> Result<Option<&[String]>, Refused> {
        Ok(None)
    }
}
