//! `edit`, and the hashes it keeps to catch stale reads. It edits where
//! commands run, so it needs [`CwdOf`].

use ext::{CwdOf, Extension, Registrar, Text, Tool as _};

pub mod tool;

/// The `edit` extension.
pub struct Edit;

impl Extension for Edit {
    const NAME: &'static str = "edit";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(tool::Edit)
            .fold::<tool::Seen>()
            .need::<dyn CwdOf>()
            .text(Text::system("edit", 20, "`edit` changes a file by exact replacement: each `old` must appear exactly once in the file as it was before the call, so include enough of the text around it, and the edits of one call must not overlap. One edit with an empty `old` writes the whole file. Look at a file before editing it: an edit to a file that changed since this session last edited it fails until you look again.").for_tool(tool::Edit::NAME));
    }
}
