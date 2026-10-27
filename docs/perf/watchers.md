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

## Coverage and routing follow current demand

Requested roots and native coverage have separate ownership. On macOS the
backend derives the outermost requested roots on subscription changes;
on Windows each holder retains its requested root alongside the shared
handle's path and generation. Removing an outer subscription restores
coverage for the surviving roots. Every replacement stream or directory
read is armed before releasing broader coverage; macOS flushes pending
callbacks and Windows drains the closing handle's completed buffer. If
replacement admission fails, the working broader coverage remains until
a later subscription change can narrow it. A failed initial admission owns
no native demand; a failure while narrowing keeps its covering stream and
allows unrelated roots to arm. This adds no idle timer.

Two surviving siblings can therefore need two recursive watches instead
of retaining one watch on their former ancestor. This is the original
child-only resource layout, and prevents unrelated sibling writes from
entering the process. Linux already counts the requested directories and
roles independently, so its native ownership needs no change.

Hubs with one or two subscriptions route changed paths directly through the
subscription map, avoiding index queries, ID-list allocation and secondary
subscription lookups for a root and its overlapping parent. Larger hubs use
a canonical-root `BTreeMap<PathBuf, BTreeSet<u64>>` index. The index remains
current across both paths, so subscribing or dropping across the threshold
needs no reconstruction.
A changed path selects subscriptions at its ancestors and in its descendant
range before applying visibility, exclusions, entered-directory scope and
alias translation. Coalesced invalidations and failures use the same index;
global invalidations still reach every subscription. Rearming updates the
index after resolving the root, including unavailable roots, and dropping
a subscription removes its entry. Index memory grows with requested roots
and subscriptions, with no cap on either. Routing costs depend on path depth,
index lookup and affected subscriptions instead of scanning unrelated roots.
Predecessor lookups jump between indexed ancestors or their common prefixes;
paths and range bounds are borrowed, avoiding a lookup for every absolute-path
component and avoiding a temporary path allocation per event.

### Round 2 reproduction

Apple M3, macOS 27.0.1 arm64; baseline `02f5ef52`. Both executables use
`cargo build-release`, two build jobs, the same external probe and lockfile,
and a four-worker runtime. The external writer's CPU is excluded.

* Coverage: subscribe to `keep`, optionally subscribe to its parent and
  immediately drop that subscription, then create 20,000 one-byte files in
  sibling `noise`. Six rounds reverse narrow/broaden order and executable
  order.
* Routing: retain one parent stream and 1/32/128 child subscriptions, then
  create 20,000 one-byte files in `keep`. Five rounds reverse layout and
  executable order.
* Each fresh process settles for two seconds before its native accounting
  snapshot. After writing, allow three seconds for delivery, take the final
  snapshot, and sample another two seconds to check for late work. Fresh
  temporary directories are outside the worktree. Only one watcher workload
  runs at a time on this shared host.

Before editing the implementation, the unchanged release probe reproduced
routing CPU medians of 480 / 1,068 / 2,688 ms for 1/32/128 children, with
ranges 317–551 / 909–1,174 / 2,287–2,720 ms (five reversed rounds).

Phase-1 interleaved routing results (median CPU; min–max in parentheses):

| Child subscriptions | Before, ms | After, ms |
| --- | ---: | ---: |
| 1 | 298 (282–446) | 328 (321–490) |
| 32 | 734 (732–955) | 346 (337–521) |
| 128 | 1616 (1570–1728) | 546 (345–582) |

The phase-1 index reduces the 32- and 128-child medians by 53% and 66%.
The one-child median increases by 10% (about 31 ms CPU per 20,000
creations); this phase-1 cost motivated the review fix below. Native delivery
batching and context switches also vary, so these are whole-process
workload results rather than isolated lookup timings.

Phase-1 coverage results (six pairs; median with min–max):

| Surviving subscription / history | CPU before, ms | CPU after, ms | Switches before | Switches after |
| --- | ---: | ---: | ---: | ---: |
| Child only | 0 (0–0) | 0 (0–0) | 0 (0–0) | 0 (0–0) |
| Child after dropping parent | 99.94 (92.33–254.06) | 0.064 (0.021–0.121) | 16,061 (15,262–21,017) | 1 (1–3) |

Every two-second follow-up window in all 54 final samples recorded zero
CPU and context-switch increments. The idle-runtime invariant also passes.

The index has a measured memory cost: with 128 children, whole-process
physical footprint before writing was 2,112 KiB (2,112–2,144) before and
2,304 KiB (2,272–2,448) after. After the burst it was 2,816 KiB
(2,752–2,912) before and 2,720 KiB (2,640–2,784) after. These include the
runtime and native backend; they do not isolate index allocations.

The private invariants have regression tests for restoring sibling coverage
without losing queued changes, inspecting only related roots, and removing
old alias targets and dropped subscriptions from routing. Those three fail
against the previous implementation. Additional tests cover common-prefix
jumps past unrelated predecessors and shared-parent recovery after narrowing.
Failed-admission tests use the real FSEvents boundary to check that both
initial failures and failed narrowing leave unrelated admission available.
Existing tests cover alias spelling,
root replacement, missing-root recovery, exclusions, scope, handoff and
zero runtime wakes while idle.

### Phase 2: direct routing for small hubs

The review fix bypasses changed-path selection for at most two subscriptions
and shares the existing delivery logic between direct and indexed traversal.
The invariant `one_root_and_its_parent_route_without_index_selection` fails
on the phase-1 implementation and passes with the direct path. The existing
129-subscription invariant still visits only its two related subscriptions.
Windows cancellation/overflow handling is unchanged by the review fix.

Same M3, release flags, saved main probe, writer and settling windows as
above. Five reversed 1/32/128 layout rounds were followed by five additional
one-child pairs to check the common-case result against host variation.
All executable orders alternate; medians and min–max are milliseconds CPU.

| Child subscriptions | Pairs | Main | Review fix |
| --- | ---: | ---: | ---: |
| 1 | 10 | 519.59 (285.28–639.29) | 519.50 (448.97–703.81) |
| 32 | 5 | 1,176.97 (1,003.22–1,240.12) | 617.93 (560.06–661.70) |
| 128 | 5 | 2,656.89 (2,322.26–4,030.04) | 485.53 (313.47–769.98) |

The one-child medians are equal within observed variation. Its paired
review-minus-main differences have median +15.34 ms and range −84.54 to
+215.11 ms; native batching and scheduling remain variable on this shared
host. The 32/128-child median reductions are 47%/82%.

Six fresh coverage pairs retain the other gain: after dropping a parent,
sibling-write CPU is 249.22 ms (100.33–330.59) on main and 0.058 ms
(0.052–0.090) with the review fix. Context switches are 19,426.5
(4,872–26,444) versus 1 (1–2). Child-only controls record zero CPU and switches.
One of 64 two-second follow-ups records 0.047 ms CPU and one switch; the
others record zero. The idle-runtime wake invariant passes.

With 128 children, ready-state whole-process physical footprint is
2,160 KiB (2,112–2,176) on main and 2,352 KiB (2,320–2,384) after the fix.
The +192 KiB median index cost remains. These totals include native/runtime
allocation and do not isolate the index.

Windows and Linux watcher performance remain unmeasured. The hosted perf
workflow has no watcher probe; adding one requires files outside this branch's
owned paths. Native platform tests run in the post-review CI workflow;
macOS is checked locally on the M3.

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
