# Edit publication

`sys::fs::EditTransaction` captures a regular file through an owned handle.
Final-component symlinks, reparse points, multiple hard links, directories,
devices and FIFOs are rejected. No advisory flock lease is needed.

Staging uses an exclusive named file inside an owned private directory beside
the target. Directory-relative operations publish and clean up the owned object
even if another directory writer substitutes the public staging name. Restrictive
macOS staging ACLs are established before content is written. The source mode,
owner and access policy are restored before publication; a preservation failure
aborts the transaction. Linux xattrs, macOS xattrs/resource forks and Windows
alternate streams share the transaction size policy and observe cancellation while
copying. Windows copies the source security policy and streams natively.

The final check compares identity, content and preserved metadata. Linux uses
`renameat2(RENAME_EXCHANGE)` where supported; macOS uses the directory-relative
native rename swap. The displaced file is verified against the capture and
staged metadata snapshot. A mismatch triggers an atomic swap back and a typed
conflict; failure to swap back is a distinct `RollbackFailed` conflict. Successful
transactions remove displaced originals. Filesystems without exchange use a
plain atomic rename after the final check. Windows denies write sharing on the
source through revalidation and uses native `FileRenameInformationEx` with
`REPLACE_IF_EXISTS | POSIX_SEMANTICS` through owned handles. The Win32
`SetFileInformationByHandle(FileRenameInfoEx)` wrapper expands relative names;
calling its native operation directly preserves the destination directory anchor,
as in [Go's directory-relative rename](https://go.dev/src/internal/syscall/windows/at_windows.go). Unsupported native
publication returns a typed error. Creating a new target never replaces an
existing entry.

The target remains present throughout existing-file publication, including
process termination at any publication boundary. Readers see complete old or
new content. This is a namespace guarantee, separate from power-loss persistence.
Subprocess tests terminate at staging, content, metadata, sync, final check,
replace, displaced verification, rollback and cleanup boundaries. The forced
non-exchange path has the same target-presence invariant.

Edit version 4 receipts report `visible`, `durability`, `protection` and
`limitations`. `exchange_verified` includes verification after the atomic swap;
`revalidated_atomic_replace` records the final complete check before rename.
Two concurrency exclusions are explicit:

- `change_after_check`: any change to the target in the instant between the
  final check and a non-exchange rename is overwritten: new content, a path
  replacement, or an in-place mode, xattr, ACL or attribute change. Exchange
  verifies the displaced file and swaps it back instead.
- `writes_to_old_inode` (Unix only): a writer with a descriptor still open on
  the old inode can write to that inode after publication. Such writes do not
  reach the newly published file. Preventing them requires mandatory locking.
  Windows denies write sharing from capture, so it never reports this.

A swap back that fails, or that finds the name replaced again since the first
exchange, never deletes what it displaced: the staging directory is kept and
`RollbackFailed` names the retained file. A sharing violation on the Windows
rename is a `Busy` conflict.

Durability is independently `not_requested`, `confirmed`, or `failed` with the
namespace already visible. Durable saves sync the staged file and parent;
macOS full barriers are requested only on this durable path. Windows edit
publication currently refuses a requested durability guarantee. `write_private`
provides atomic private replacement without a durability barrier;
`write_private_durable` is the explicit configuration-save choice.

Unix endpoint discovery uses a capability for the owned 0700 runtime directory.
Its owner, mode and macOS ACL policy are checked when the capability is created;
records publish relative to that directory handle. The reconstructible record
needs no persistence barrier. General-purpose private saves still establish
restrictive staging ACLs for arbitrary parent directories.

Capture and transformed output use private temporary files from `tempfile`,
which already supplies the repository's temporary-file lifecycle in tests. It
handles native deletion and permissions without a new cleanup protocol. Native
publication still uses rustix/libc and windows-sys; all unsafe code stays in sys.

The edit tool imposes no file-size or matching-work ceiling. Capture, hashing,
UTF-8 validation, exact matching, content comparison and publication stream
through fixed buffers. The matcher retains a prefix table proportional to the
supplied `old`, including overlapping occurrences. Worker reservations account for
replacement arguments and the matcher table rather than the file's size. Files
and combined replacement text up to 64 KiB use the detailed formatter; larger
edits retain line counts with the bounded `[diff cut]` display. The filesystem
transaction owns any caller-requested size policy for capture and publication;
the edit tool requests the representable size rather than a memory limit.
