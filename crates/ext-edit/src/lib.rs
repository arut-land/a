//! `edit`, and the hashes it keeps to catch stale reads. It edits where
//! commands run, so it needs [`CwdOf`]: a build without the shell does
//! not compile.
//!
//! Storage workers capture a [`sys::fs::EditTransaction`], CPU workers match
//! replacements and build the diff, then storage workers publish the result.
//! A private staging namespace shields content until publication. Metadata and
//! stream copying are bounded and observe cancellation. Links are rejected.
//! Receipts distinguish visibility, durability and conflict protection.
//! The target is always present across publication and process termination. Unix
//! exchange verifies the displaced file and swaps back on conflict; other paths
//! atomically replace after the final check. Receipts document the remaining
//! window before a non-exchange rename and Unix writes through old descriptors.

use ext::{Configured, CwdOf, Extension, Need, Refused, Registrar, Table, Text, Tool as _};

pub mod tool;

/// The `edit` extension.
pub struct Edit;

impl Configured for Edit {
    fn configure(_: &impl Table) -> Result<Self, Refused> {
        Ok(Self)
    }
}

impl Extension for Edit {
    const NAME: &'static str = "edit";
    const VERSION: u32 = 4;
    type Needs = (Need<dyn CwdOf>, ());

    fn contribute(&self, r: &mut Registrar) {
        r.tool(tool::Edit)
            .fold::<tool::Seen>()
            .text(Text::system("edit", 20, "`edit` replaces exact text in regular files, preserving metadata. Each nonempty `old` must match exactly once; include enough surrounding text and do not overlap edits. One empty `old` replaces the whole file. Inspect a file before editing; after a stale-read conflict, inspect it again. Links are rejected. Publication is atomic; the receipt reports durability and concurrency limits. On conflict, inspect the file and retry.").for_tool(tool::Edit::NAME));
    }
}
