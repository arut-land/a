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
through fixed buffers. The matcher uses `memchr` with an overlap window
proportional to the supplied `old`, including overlapping occurrences. Worker reservations account for
replacement arguments and the overlap window rather than the file's size. The
detailed formatter retains at most 256 KiB of source text under its existing work
budget; larger sources retain line counts with the bounded `[diff cut]` display.
Output size does not determine whether a diff is formatted. The filesystem
transaction owns any caller-requested size policy for capture and publication;
the edit tool requests no file-size ceiling.

Windows native security updates distinguish stored inheritance state from the
`SE_DACL_AUTO_INHERIT_REQ` command bit. When copying a DACL marked
`SE_DACL_AUTO_INHERITED`, the edit transaction supplies that command to
`NtSetSecurityObject`, then removes the command from its comparison descriptor.
It verifies the original owner, group, DACL entries and control bits, including
explicit inheritance protection, against the stored result. It does not ask for
SACL access or enable privileges.

This matters for files created inside an existing isolated home after daemon
startup secures that home with `private_dir`/`SetSecurityInfo`. Ordinary Rust
unit-test temporary directories did not exercise that setup. Native Windows x64
reproduction found identical owner, group and DACL bytes after the old setter,
but descriptor control changed from `0x8404` to `0x8004`: Windows cleared the
source's automatic-inheritance state. Both volumes were NTFS. A regression
creates and secures an existing home through the real filesystem implementation,
then checks successful publication, exact contents and the complete access
policy, including the automatic-inheritance bit.


Hosted Windows x64 [qualification and paired measurements](https://github.com/arut-land/a/actions/runs/40449265793)
use the survey's release build policy and the unchanged 1/64 MiB edit workload.
Three rounds alternate reference/candidate, candidate/reference,
reference/candidate on one runner. All six reference attempts fail with the ACL
error; all six candidate edits complete and pass content validation. The final
regression fails with the reference setter; all 23 candidate native edit tests
and native clippy pass. Local macOS verification passes 185 sys tests and the
workspace Windows cross-lint.

Tool-attempt wall time in ms, median [min–max] over three samples per size:

| Input | Reference (failed) | Candidate (completed) |
| --- | --- | --- |
| 1 MiB | 86.754 [80.533–94.732] | 136.896 [135.178–139.146] |
| 64 MiB | 431.260 [430.649–431.789] | 553.856 [548.135–565.709] |

Daemon CPU ms for reference/candidate are respectively 0.000 [0.000–15.625] /
0.000 [0.000–31.250] at 1 MiB, and 359.375 [359.375–375.000] /
437.500 [406.250–437.500] at 64 MiB. The native CPU timer is coarse; zero readings
do not imply zero work. Failed attempts and completed edits do different work,
so these observations do not establish a speedup or regression. Raw paired
observations and both executables are in the run's `acl-paired` artifact.
