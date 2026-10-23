//! `edit`, and the hashes it keeps to catch stale reads. It edits where
//! commands run, so it needs [`CwdOf`]: a build without the shell does
//! not compile.

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
    const VERSION: u32 = 1;
    type Needs = (Need<dyn CwdOf>, ());

    fn contribute(&self, r: &mut Registrar) {
        r.tool(tool::Edit)
            .fold::<tool::Seen>()
            .text(Text::system("edit", 20, "`edit` changes a file by exact replacement: each `old` must appear exactly once in the file as it was before the call, so include enough of the text around it, and the edits of one call must not overlap. One edit with an empty `old` writes the whole file. Look at a file before editing it: an edit to a file that changed since this session last edited it fails until you look again. Edits atomically replace regular files, preserving metadata; symbolic links and multiple hard links are rejected. Observed concurrent changes cause a conflict; inspect the file and retry. Publication confirms visibility without promising power-loss durability.").for_tool(tool::Edit::NAME));
    }
}
