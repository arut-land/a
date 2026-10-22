# Windows daemon platform

## Transport and completion

The transport uses byte-mode local named pipes selected by the locked daemon
record. Each generation's pipe name includes the home directory's file
identity, the current token's user SID, PID and process creation time. Case
variants and junction aliases therefore find the same daemon. The record
must be locked; clients reject stale identities and verify the pipe server's
PID and SID before writing a hello. Every pipe instance has a protected DACL
allowing only that SID, rejects remote clients, and the first instance refuses
an existing name. Name hashes select an endpoint; they do not authorize access.

`async-io` 2.6.0 supplies Windows socket polling and thread-pool-backed
`Waitable` handles. `sys` submits overlapped `ReadFile`, `WriteFile` and
`ConnectNamedPipe` operations with manual-reset events; `rt` registers
duplicate event handles. An idle connection needs no blocking worker, timer
or dedicated thread. Each operation owns its pinned OVERLAPPED and buffer.
Dropping it cancels and joins the operation before freeing either allocation.

Writes return the accepted count in the submitting poll, without awaiting
completion afterward. A later would-block consumes none of the next buffer.
A separate flush completes accepted writes, including a frame's final chunk;
cancelling and retrying that flush never resubmits bytes. Synchronous client
transports delegate flush to their stream. Async client, daemon and MCP
writers flush at their message boundaries.

The Win32 bindings use `windows-sys` 0.61.2 and `windows-link` 0.2.1, already
in the dependency graph. They avoid hand-maintained ABI layouts.
`interprocess` 2.x requires Tokio for asynchronous named pipes; reusing
async-io's waitable support fits the runtime seam without another executor.

## Process ownership

Daemon startup uses a re-listen handshake under `start.lock`. The starter
closes its record handle, launches the child with `DETACHED_PROCESS`, and
waits at most ten seconds for a stdout binding acknowledgement. The child
locks and publishes its identity before acknowledging readiness, then accepts
clients. Each successor owns a distinct pipe namespace even while an older
client retains a handle. Unix uses the bound-descriptor handoff.

Daemon and shell creation own child stdio duplicates and use
`CreateProcessW` with an explicit `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`.
No transport, lock or unrelated inheritable handle can leak into a child.
The shell's creation attributes assign an unnamed kill-on-close job before
user code can create descendants. Only the daemon retains that job handle.
MCP child stdio uses the same allowlisted spawn path, with synchronous child
ends and overlapped parent ends. Argument escaping has portable tests.

Record locks cover a byte beyond the record's contents so readers can obtain
the identity needed to stop a wedged daemon. Fallback stop retains one
query-and-terminate handle across identity validation and both stop attempts;
it never reopens a PID. Missing or mismatched creation tokens are rejected.
Console callbacks signal a process-lifetime event registered through `rt`,
without a polling thread or callback/handle-close reuse race.

## Shell output and staging

Shell execution uses Git for Windows Bash with redirected output and no
console. Native paths, extended-length and UNC prefixes, shell directory
reports and daemon environment variables are handled in `sys`. Each shell
enters its job at creation; there is no terminal or interactive shell stdin.

Captured output enters an overlapped pipe. `rt` copies bounded 64 KiB chunks
to the spill file on storage workers and signals file-identity-keyed follower
events. NTFS directory notifications can wait until close and cannot provide
live progress. A quiet process holds no copying worker and causes no polling.
Output draining completes independently of descendant reaping. The copier
retains the runtime service, without retaining a call's context or admission.
Unix uses direct file output and native file notifications.

Windows staging uses atomic reference files for `current` and stamps instead
of privileged symlinks. Staged executables have the `.exe` suffix. Files
Windows refuses to remove while running remain for a later prune. Upgrades
drain the old daemon before the successor re-listens.

## Durable namespace publication

Flushed files are published on local NTFS with same-volume `MoveFileExW`,
`REPLACE_EXISTING` and `WRITE_THROUGH`. Content flush precedes the rename;
completion waits for the move on disk. Canonical parent paths retain the
extended-length prefix without following the final component. No cross-volume
copy fallback is allowed. Imports, catalog rewrites and dirty markers use
this path. A new log publishes its flushed header before exposing an append
handle and publication receipt.

Removing a catalog dirty marker durably renames it to an ignored tombstone
before deleting the tombstone. A crash can leave garbage, never a clean
catalog paired with an unpublished rewrite. Torn-tail repair reopens the same
file with write-data access because append-only Windows handles cannot
truncate; it preserves the append handle and read cursor.

Windows directory `FlushFileBuffers` has no persistence contract used here.
`sync_dir` returns Unsupported; directory creation publishes each missing
ancestor from an exclusive sibling with a same-volume write-through rename.
Existing directories retain their contents. Unix creates directories and
flushes their parents, including existing ancestors: existence cannot certify
a previous attempt whose mkdir succeeded but parent barrier failed. A failed
ancestor barrier prevents publishing descendants on every retry. These
operations precede session log commits. The guarantee requires local storage
and devices that honor flushes.

## Validation and measurements

Native Windows CI runs workspace nextest with `--no-fail-fast` and portable
daemon acceptance. It exercises Win32 linking, DACL inspection, completion,
shutdown wakeups, locks, handle inheritance, job containment, staging, daemon
restart and crash recovery, plus shell paths containing spaces and Unicode.
Portable fakes deterministically test cancellation before and after write
acceptance, cancelled flush retries, synchronous flush errors, and retries
after a directory publication barrier fails. Unix Miri covers supported
`sys` operations; it cannot execute Windows kernel FFI.

Cross-account denial and physical power-loss persistence require a suitable
Windows host. No Windows size, RSS or latency measurement has been collected.
On the Apple silicon development host (2026-10-22), `mise run size` measured
the shipped binary at 3,170 KiB against the 4,096 KiB cap, with fixture
exclusion verified. This is an Apple measurement, not a Windows performance
claim.
