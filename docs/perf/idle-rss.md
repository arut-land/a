# Idle daemon RSS (2026-10-21)

The idle budget is **strictly below 10,240 KiB**, measured with `ps -o rss=`
after the terminal has shown `● live` and both processes have been idle for
three seconds. The budget was not changed.

## History

Apple M3, eight cores, 16 GiB RAM, macOS 27.0.1 (26A434),
nightly-2026-10-17. Each row is five independently started daemons, with a
fresh isolated `A_HOME`, an empty workspace, the echo fixture agent, and a
100×30 PTY. No turn is submitted. Builds use `CARGO_BUILD_JOBS=2`, the
repository's `cargo build-release` alias (build-std, size optimization, fat
LTO), and `-p a --features fixtures`. Each build completes before sampling.
The historical checkouts are detached, separate from the working branch.
The rows include every first-parent merge in `eab69fe..9736499`, plus the
baseline and final main. Binary sizes in this table include fixtures.

There was unrelated TypeScript work at the start and security scanning
throughout; another worktree briefly built near the end. These are repeated
RSS observations on a shared machine, not controlled latency comparisons.
No unrelated process was stopped. The raw observations are in
[idle-rss.csv](idle-rss.csv).

| Revision | First-parent milestone | RSS min / median / max, KiB | Binary, KiB |
| --- | --- | ---: | ---: |
| `eab69fe` | baseline | 8016 / 8032 / 8048 | 2991 |
| `b498833` | main into flush | 7968 / 7984 / 8048 | 2910 |
| `00e4d80` | main into flush | 7856 / 7904 / 7936 | 2942 |
| `9b8cb04` | registry adoption | 8160 / 8208 / 8240 | 2991 |
| `7794350` | protocol 4 activity | 8080 / 8144 / 8176 | 2991 |
| `27ac89b` | main into flush | 8208 / 8240 / 8272 | 2991 |
| `e33cd6d` | protocol 7 / fork | 8256 / 8336 / 8384 | 3056 |
| `054a27c` | skills / generation integration | 9792 / 9936 / 9952 | 3268 |
| `b6a11da` | MCP | 9888 / 9920 / 9984 | 3333 |
| `5b577df` | side calls | 9872 / 9936 / 10016 | 3365 |
| `34851ad` | platform publication | 9888 / 9984 / 10080 | 3381 |
| `55b26cc` | agents / protocol 11 | 9984 / 10016 / 10032 | 3559 |
| `254cedb` | C03 | 10112 / 10160 / 10176 | 3559 |
| `0627cca` | C04 | 10096 / 10160 / 10176 | 3559 |
| `e5cbef5` | C05 integration | 10016 / 10096 / 10224 | 3592 |
| `25b6518` | C06 / C07 | 10112 / 10144 / 10224 | 3608 |
| `021d746` | C09 integration | 9984 / 10192 / 10304 | 3657 |
| `7c9c1d4` | C09 integration | 10192 / 10272 / 10336 | 3657 |
| `9736499` | main: runtime sizing / admitted edits | 10160 / 10240 / 10304 | 3673 |
| `batch` | batched watches only | 9984 / 10128 / 10176 | 3705 |

The largest merge-to-merge increase is skills at `054a27c`: median RSS
8,336 → 9,936 KiB (**+1,600 KiB**). Later additions consume the remaining
headroom: final main is 10,240 KiB median, 10,304 KiB maximum. The originally
reported 10,464 KiB is consistent with a build having essentially no margin;
this sweep reproduces the failure without reproducing that exact sample.

## Attribution

`vmmap --summary`, `footprint` and one-second `sample` captures were taken
on the first daemon of each row, **after** its uninstrumented RSS reading.
Representative captures, all KiB except thread counts:

| Revision | Footprint | Live malloc bytes | Malloc resident | Stack resident | Threads | Clean text attributed by footprint |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `eab69fe` | 2688 | 394 | 1200 | 192 | 7 | 1696 |
| `e33cd6d` | 2864 | — | — | 256 | 10 | 1840 |
| `054a27c` | 3632 | 532 | 1760 | 336 | 13 | 1952 |
| `9736499` | 3648 | 532 | 1760 | 336 | 11 | 2160 |
| Batched watches | 3536 | 530 | 1664 | 320 | 10 | 2144 |

The skills merge starts notify's FSEvents stream even for an empty skill
catalog, so it can notice skill directories created later. Samples identify
an FSEvents run-loop thread and two native helper threads. This is useful
work: simply omitting that subscription would break live discovery.
`Watcher::add` also restarts the native stream for every parent path.
Intermediate streams and thread startup touch pages that the allocator keeps
resident after their small live allocations are freed.

The baseline-to-main live heap increase is only **138 KiB**. Malloc residency
increases **560 KiB**, resident stacks **144 KiB**, and attributed clean text
**464 KiB**. Framework initialization, native mappings and allocator metadata
account for more of RSS; these tools' categories are not an additive
reconstruction of `ps` RSS. In particular vmmap includes shared-cache library
pages in its large resident total; that total must not be compared to the
10 MiB `ps` budget. Footprint counts dirty/private cost, not RSS.

The runtime already creates blocking workers on demand and retires them
after ten seconds. Its byte reservations are admission limits, not allocated
buffers. Registry tool specifications and prompt caches are already lazy.
There is no eagerly decoded model catalog. Logging's bounded 256-record
queue and the empty skills catalog cannot explain megabytes of live heap.
An optimized `-Zprint-type-sizes` build puts `Actor::run` at 10,328 bytes,
so boxing that future would not recover the missing margin either.

## Fix and rejected experiments

Batched FSEvents registration alone measured 9,984 / 10,128 / 10,176 KiB
(min / median / max, five fresh daemons): only 112 KiB saved at the median.
One async worker plus batching measured 10,000 / 10,112 / 10,160 KiB. This
provided no useful margin over batching and was rejected; the four-worker
default and every blocking lane's sizing remain unchanged. The raw CSV
retains both experiments (`batch`, `one`).

The fix makes tree subscriptions follow their actual roots. Missing roots
hold nonrecursive watches on their nearest existing ancestors. On macOS
these reuse `sys::WriteWatch` through the existing async reactor and owned
runtime tasks, without starting a recursive FSEvents stream or native helper
threads. WriteWatch also reports deletion, rename and revoke, so moving an
ancestor causes a resubscription. When a skill/Lua root appears, refresh
installs a recursive notify subscription before scanning it; disappearance
releases it. Parent watches remain to observe replacement at the same path.
Ancestor events invalidate unchanged path names too, because an inode may
have been replaced. All initial recursive paths are installed in one batch.

Skills and Lua supply their actual roots rather than recursively watching
the whole workspace and home. Source preparation still runs off async
workers. The SDK receives a pure async scheduling callback alongside its
existing blocking preparation callback, keeping the SDK and kernel I/O-free.
Dropping a subscription cancels its ancestor tasks and releases the watches.
The runtime regression checks missing-root laziness, recursive changes after
creation, replacement at the same path, and release after removal.

No feature is disabled, no worker ceiling or queue capacity is reduced, and
no package is added. The existing sys dependency of rt is also used on
macOS. An all-kqueue notify backend was rejected after source inspection:
its directory Write/Link handling expands watches even for nonrecursive
parents, defeating the intended scope. We retain notify's maintained
recursive backends and reuse the existing native parent-watch seam instead.
Populated roots still pay for their live recursive subscriptions and catalogs;
the idle budget scenario has empty roots, not a bound on arbitrary session
history or user-supplied catalogs.

Ten fresh daemons with the fix measured **9248 / 9344 / 9424 KiB**
(min / median / max). Median RSS falls **896 KiB**; the worst sample has
**816 KiB** of headroom. The first profile has **8 threads**, **3,328 KiB
footprint**, **472 KiB live malloc**, **1,664 KiB malloc resident**, and **256 KiB
resident stacks**. The final merged build is checked separately below.

## Reproduction

List the history with:

```sh
git log --first-parent --oneline eab69fe..9736499
git log --reverse --first-parent --merges --format=%h eab69fe..9736499
```

In a disposable detached checkout of each revision, build sequentially,
copy the executable before the next build, then use the macOS probe:

```sh
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/tmp/a-rss-build \
  mise exec -- cargo build-release -p a --features fixtures
# Copy /tmp/a-rss-build/release/a to a revision-specific path first.
python3 docs/perf/idle-rss.py /absolute/path/to/saved/a revision --output target/rss
```

Use the probe from the final tree when measuring old checkouts. It waits for
the live terminal, samples after three seconds, profiles only afterward,
and stops its own daemon with its own `A_HOME` before removing that home.
It never reads the developer's home or `.env.local`. The probe uses macOS
profiling commands; the maintained budget test remains the cross-platform
gate. Cargo may reorder old lockfile entries; restore only those generated
scratch-checkout changes before checking out the next revision.
