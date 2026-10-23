//! `edit`, and the hashes it keeps to catch stale reads. It edits where
//! commands run, so it needs [`CwdOf`]: a build without the shell does
//! not compile.
//!
//! Storage workers capture a [`sys::fs::EditTransaction`], CPU workers match
//! replacements and build the diff, then storage workers publish the result.
//! A private staging namespace shields content until publication. Metadata and
//! stream copying are bounded and observe cancellation. Links are rejected.
//! Receipts distinguish visibility, durability and conflict protection.
//! Unix exchange verifies displaced content and preserved metadata before reclaiming
//! the original. Other publication paths first move and verify the original, then
//! use no-replace publication; the receipt reports that readers may see an absent
//! target during that interval. Conflicts restore without replacing an arriving
//! file. After a visible swap, rollback retains the displaced version and reports
//! its recovery path, preserving writes to that briefly visible replacement.

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
    const VERSION: u32 = 3;
    type Needs = (Need<dyn CwdOf>, ());

    fn contribute(&self, r: &mut Registrar) {
        r.tool(tool::Edit)
            .fold::<tool::Seen>()
            .text(Text::system("edit", 20, "`edit` changes a file by exact replacement: each `old` must appear exactly once in the file as it was before the call, so include enough of the text around it, and the edits of one call must not overlap. One edit with an empty `old` writes the whole file. Look at a file before editing it: an edit to a file that changed since this session last edited it fails until you look again. Edits replace regular files, preserving metadata; symbolic links and multiple hard links are rejected. Observed concurrent changes cause a conflict; inspect the file and retry. The receipt identifies filesystem conflict protection and whether readers may briefly see an absent target. Existing-target publication verifies displaced content and metadata, and no-replace publication preserves an arriving file. An exchange conflict can be briefly visible before rollback; rollback contention reports retained data for recovery. Publication confirms visibility without promising power-loss durability.").for_tool(tool::Edit::NAME));
    }
}
