# Source watching and preparation

## One watcher: `rt::watch::Hub`

The daemon owns one `Hub` (engine `Shared::hub`, offered to calls and
queries as a service); a client process makes its own. A feature
subscribes a root with a scope, `Watch::tree(root)` or
`Watch::entered(root)`, and paths under it that never wake it
(`.excluding(".git")`), and gets a `Subscription`; dropping it
unsubscribes, and the last subscriber of a native watch closes it. An
entered subscription hears of the root's entries and of each directory
it `enter`s, as the index enters what its walk does not ignore.

| OS | Native watch | Shared how | Above the root |
| --- | --- | --- | --- |
| Linux | one inotify descriptor per hub, read by a task on the runtime's reactor; one watch per directory | counted per directory across subscriptions (inotify gives one inode one watch) | the nearest existing parent, for its entries coming and going |
| macOS | FSEvents streams (`FileEvents`, `WatchRoot`, `NoDefer`, latency 0) on one serial dispatch queue in `sys` | one stream per outermost root; a nested root rides it, filtered by prefix; a stream opened for an outer root starts now, and the nested ones are flushed (`FSEventStreamFlushSync`) as they close, so what they held is delivered and nothing before the outer subscription is replayed | nothing: a stream follows its path, and `WatchRoot` reports the root going and coming back |
| Windows | overlapped `ReadDirectoryChangesW` (`sys::DirectoryWatch`) awaited through the reactor | one recursive handle per outermost root, as on macOS; a closing handle tells what completed in its buffer first; a missing root inside another's tree holds that tree | a non-recursive handle on the nearest existing parent, unless a tree covers it; a handle follows its directory, so a root replaced at its path is opened again |

The router runs under the hub's lock, on a runtime task (macOS hands
its dispatch-queue batches to one), and decides everything before a
subscriber is woken: an event outside its root, excluded, in a directory
an entered subscription has not entered, or naming a sibling of the root
in a watched parent wakes nothing. An event at the root or above it
compares the root's identity (device and inode, volume and file id)
with what it was: unchanged is nothing; replaced is rearmed and told
`Rescan`; missing is told `Unavailable::Gone` and followed from
its nearest existing parent until it is back (`Rescan`). A change
under a root believed gone counts as its return, so a coalesced creation
is not missed. Paths come in the filesystem's spelling (links resolved,
letter case as on disk, which is how FSEvents and Windows report them)
and are told in the subscriber's, so a differently cased root on APFS or
NTFS hears its changes.

On Linux a directory that appears in a tree is watched, with everything
in it, before its change is told: a subscriber that looks at it then sees
what was made before the watch, and hears of the rest. A directory moved
out drops its watches, which would name paths it no longer has. A queue
overflow rearms every subscription and tells `Rescan`. A directory
removed or moved away loses its watches and leaves what was entered:
made again, it is entered again when the subscriber looks at it. However
many watches the OS drops in one read, a subscription out of watches
walks again once.

Not observed: a directory above a root's parent renamed or replaced, or
a linked root retargeted, until something at the root itself changes
(macOS's `WatchRoot` follows the whole path). A tree is walked and armed
under the hub's lock on Linux, which suits the small trees subscribed
whole (skills, Lua); the index's large roots are entered as its walk,
off the lock, goes.

Sinks run on runtime tasks, never on a native thread, one batch at a
time: a busy sink's next batch holds what came meanwhile (consecutive
changes merged). There is no debounce in the hub. Only an I/O failure
retries, rearming after 100 ms doubling to five seconds until it holds;
running out of watches (`ENOSPC`, or `EMFILE` on the descriptor) is told
once as `Unavailable::Limit` and waits for a watch to be freed (inotify
dropping one of its own accord: a directory removed) or for a new
subscription. Permission denied is `Unsupported`, as is a filesystem
without notifications; neither has a timer. A healthy hub schedules
nothing: `an_idle_subscription_never_wakes_the_runtime`.

Subscribers never refresh a watch. The generation layers (skills, Lua)
name their roots (`GenerationLayer::roots`) and the coordinator
subscribes each as a tree through the engine's `Watch` seam before the
first capture; the index subscribes its root entered (and
`.git/info`, for `exclude`), entering each directory as its walk does,
and each rescan walks into a new subscription that replaces the old once
installed, so ignore rules that changed leave no stale watches; the
client's machine list watches the directories of its two files entered.

### Before and after

`watchbench` (old: `rt::Watcher` over notify 8.2 on main 7b6f16c5; new:
the hub) on one tree of 4,400 directories, 400 of them kept and 4,000
under an ignored `target/`; the entered rows enter the 400 and their
parents only, as the index does. Arm is subscribing (and entering);
latency is a write to the sink seeing it, 200 writes; threads and RSS
are the whole process after arming, a two-worker runtime included.
Median of five alternating rounds.

| Host | Watcher | Arm | inotify watches | RSS added | Threads after | Latency p50 / p99 |
| --- | --- | --- | --- | --- | --- | --- |
| Linux x86_64 (ubuntu-latest) | notify, tree | 99.9 ms | 4,476 | 1,780 KiB | 2 | 86 / 136 µs |
| | hub, tree | 64.0 ms | 4,476 | 1,040 KiB | 4 | 191 / 246 µs |
| | hub, entered | 2.2 ms | 423 | 272 KiB | 4 | 183 / 231 µs |
| Linux aarch64 (ubuntu-24.04-arm) | notify, tree | 53.5 ms | 4,476 | 1,604 KiB | 2 | 185 / 240 µs |
| | hub, tree | 37.0 ms | 4,476 | 936 KiB | 4 | 336 / 597 µs |
| | hub, entered | 1.4 ms | 423 | 164 KiB | 4 | 343 / 638 µs |
| macOS arm64 (macos-latest) | notify, tree | 3.6 ms | — | 1,024 KiB | 6 | 14.4 / 18.8 ms |
| | hub, tree | 1.8 ms | — | 112 KiB | 2 | 14.2 / 18.9 ms |
| | hub, entered | 2.1 ms | — | 160 KiB | 2 | 15.0 / 19.0 ms |
| Windows x86_64 (windows-latest) | old `rt::Watcher`, tree | 0.5 ms | — | 460 KiB | 6 | 166 / 384 µs |
| | hub, tree | 0.6 ms | — | 500 KiB | 6 | 171 / 362 µs |
| | hub, entered | 1.2 ms | — | 556 KiB | 6 | 175 / 390 µs |

Threads are the process's: the old Linux harness never starts the
runtime's workers (notify calls its sink on its own thread, the one
thread added), while the hub routes and delivers on the runtime, whose
reactor and two workers the daemon runs anyway; the hub adds no thread of
its own anywhere. On macOS notify's run-loop thread and its helpers go.
Windows already used `sys::DirectoryWatch` before, so its numbers are
the same backend under the new router. Linux latency gains about
100 µs (two runtime hops, reactor to router task to the sink's task,
where notify called the sink on its thread); every consumer debounces or
walks for longer than that. macOS latency is fseventsd's own, 12 to 19 ms
at latency 0, the same either way. An entered subscription, as the index makes, takes a
tenth of the watches on a tree with an ignored `target/`.

The release `a` (perf workflow, paired against main 6efcaf01, three
rounds): Linux x86_64 −25.9 KiB, Linux aarch64 −22.3 KiB, macOS
−1.6 KiB, Windows aarch64 +28.5 KiB (the router, where nothing was
removed). Idle daemon RSS and CPU and attach latency stay within the
rounds' spread on all four (an idle daemon holds no subscription).

The hand-rolled backends replace notify (and mio, inotify, kqueue,
fsevent-sys, walkdir and notify's own windows-sys 0.60 with them) because
notify cannot share a watch between subscriptions, keeps a thread (Linux,
macOS) or a timed wait (Windows) of its own, watches every directory of a
recursive root on Linux, and reports through callbacks on its threads,
which the old watcher had to bounce back to the runtime.

## Preparation

The generation coordinator arms its watches before the first capture.
macOS and Windows cover descendants continuously and Linux watches a new
directory before reporting it; coalesced or dropped batches are a full
rescan. No callback debounce suppresses an
event because preparation is running: it marks another capture pending,
including when that attempt rejects or retries. Events arriving after an
attempt finishes schedule new work under the same coordinator lock.

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
For a source on a filesystem supporting native notifications, a write after
capture starts remains covered by an armed subscription: its later delivered
event (or overflow/rescan) forces another capture. Thus once writes stop and
their queued notifications are delivered, settled content is captured and
applies if valid; invalid content retains the working generation. No finite
quiet window proves completion, and healthy sources are never polled.
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
