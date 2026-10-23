# Source watching and preparation

Linux uses notify's inotify backend. macOS uses FSEvents for existing
source trees and kqueue vnode subscriptions for their parents. Windows
uses overlapped ReadDirectoryChangesW requests awaited through the runtime's
waitable-event reactor. Each request owns a DWORD-aligned 16 KiB buffer and
an OVERLAPPED allocation until completion or joined cancellation. There is
no notification thread or periodic wait on Windows. This avoids notify 8.2's
100 ms idle wait and also surfaces zero-byte overflow and terminal read errors.

WatchEvent carries changes, overflow, rescan requests and unavailable
subscriptions. Uncertainty invalidates all roots, including events without
paths; refresh rearms subscriptions before source inventory is captured.
Root parents remain watched so creation and replacement stay observable.
Events retain canonical paths alongside configured alias spellings so
overlapping canonical and aliased roots all invalidate.
Unavailable subscriptions and failed rearms schedule rescan callbacks with
exponential backoff from 100 ms to five seconds, independently of further
filesystem events. A successful refresh cancels recovery; dropping the
watcher cancels pending recovery and native requests. Healthy watches have
no timer.

Lua resolves home/workspace precedence before opening winning sources.
Skills discover tentative names in priority order before capturing winning
files. Only winners receive full frontmatter validation; losing descriptions,
metadata, bodies and native versions do not participate in comparisons.
Unidentifiable prefixes receive native stability checks and remain raw
snapshot inputs until debounce and re-observation, so malformed UTF-8 or
frontmatter cannot terminate recovery from a changing capture.
Links may resolve only within configured roots, and descendant target edits
invalidate the inventory. Source reads and pathname revalidation use sys's
bounded, nonblocking regular-file opens. Winning and unidentifiable reads
validate native identity, size, modification time and, on Unix, change time
before and after capture. Validated snapshots compare
effective bytes rather than native versions, so identical saves cannot
starve another source. Bytes are captured before UTF-8 decoding.

The I/O-free ext source driver injects capture, wait and compile operations.
It coalesces writes over a 100 ms window and revalidates after compilation.
Each admitted preparation attempt is bounded; changed inputs and transient
I/O failures return a typed retry instead of holding a worker in a loop.
The coordinator schedules cancellable retries with exponential backoff from
100 ms to five seconds. Native sharing/locking conflicts remain retryable
even when successive errors have the same text. Stable rejected content or
permanent source errors retain the working registry and stop recovery until
a new event. Closing a workspace cancels its timer; queued work retains only
a weak owner. Settled sources leave no recurring timer.

A quiet window is a debounce, not a writer-completion signal. Valid partial
sources and temporary absence may publish generations with fewer commands.
Folded values are keyed by mod and fold name: adopting a generation that
omits a fold retains its serialized image and rejection decisions for the
session lifetime. Retained values are included in checkpoints. Returning
registrations rebuild from a matching checkpoint and the log, including
events recorded during absence; retained images never override replay.
Changed reducers replay the prefix using their own semantics. Retention
holds no old reducer or Lua VM. A candidate rejecting previously accepted
changes rejects adoption. A returned fold
can then accept new changes normally; removing a registration never clears
its value. Session disposal ends retention.

File-follow opens, bounded reads and output rendering, cwd-report reads,
and initial source-backed prompt context use admitted Storage work. File
following reuses a fixed 64 KiB buffer. Prompt context shares the existing
per-binding cache allocation with parsed tool inputs.
