# Allocators and memory after a burst

The adoption criterion is lower memory beyond the A/A band, with no CPU or
latency regression. RAM limits that prevent work are not candidates. Allocator
arena counts control allocator organization, rather than how much live session
state the daemon may own.

## Protocol

The sessions campaign compares the system allocator twice (`A`, `A2`), the
system allocator with the runtime's retirement callback (`rest`), and each
replacement allocator. Linux also compares glibc's default arena policy
(`arenas`) against `M_ARENA_MAX=2`. Each process runs three turns at either
1 or 128 concurrently attached sessions. Twenty rounds reverse layout order
on alternate rounds. All builds finish before hosted measurements.

`A`, `A2` and replacement-allocator layouts disable the retirement callback;
`rest` enables it. Every sample also measures an explicit release after the
idle window and another after the clients detach. These diagnostic calls
do not change the earlier active CPU, latency or idle observations. Retired
blocking workers leave no periodic release timer.

The provider and clients run in the same process as the daemon. Their heaps,
threads and CPU count in these measurements, including the 128 attached
client threads. These are allocator comparisons on the same workload, not
measurements of the daemon alone. Paired executable measurements separately
cover the daemon, the line client, startup, attach, driver turns and large
tool results. Their reference is built from the same source with an empty
variant; no allocator feature is passed to that build.

Memory uses KiB. macOS physical footprint and its lifetime peak differ from
RSS: pages the allocator has marked reusable can remain resident without
counting toward the process's physical footprint. Linux and Windows peaks
are lifetime peak RSS/working set. The explicit release duration is wall
time; a lower RSS after release does not establish that subsequent work is
free of page-fault or CPU costs.

[`allocator_effects.py`](../../scripts/perf/allocator_effects.py) matches
rounds, reports the median of candidate-minus-reference differences, and
computes fixed-seed, 10,000-resample percentile 95% bootstrap intervals over
complete pairs. Active CPU and latency use percent differences; memory,
idle CPU and release duration use absolute differences. Latency is the p99
within each run before pairing; individual deltas are not independent
bootstrap samples. The script retains raw pairs, matched round IDs and
separate estimates for the two orders. Intervals describe the measured
host, not variation across hardware. Failed or missing rounds are not
filled with zero. Samples with failed calls or dropped deltas cannot enter
the effect table.

```sh
python3 scripts/perf/allocator_effects.py sessions.jsonl --output effects.json
python3 scripts/perf/allocator_effects.py paired.json --output effects.json
```

## Campaign provenance

The allocator campaign source is `cfd5ae5d`, before the later merge of main.
The local M3 allocator binaries were saved by the preceding build campaign.
Hosted sessions are split to stay within the 150-minute measurement step:

| Comparison | Run |
| --- | --- |
| Smoke, every allocator, 1/8 sessions | [40235381866](https://github.com/arut-land/a/actions/runs/40235381866) |
| mimalloc, 1/128 sessions | [40235406289](https://github.com/arut-land/a/actions/runs/40235406289) |
| jemalloc and snmalloc, 1/128 sessions | [40236725744](https://github.com/arut-land/a/actions/runs/40236725744) |
| rpmalloc and Windows segment heap, 1/128 sessions | [40235412374](https://github.com/arut-land/a/actions/runs/40235412374) |
| Paired system/system A/A | [40235415430](https://github.com/arut-land/a/actions/runs/40235415430) |
| Paired mimalloc | [40235417870](https://github.com/arut-land/a/actions/runs/40235417870) |
| Paired jemalloc | [40235420434](https://github.com/arut-land/a/actions/runs/40235420434) |
| Paired snmalloc | [40236750111](https://github.com/arut-land/a/actions/runs/40236750111) |
| Paired rpmalloc | [40235425211](https://github.com/arut-land/a/actions/runs/40235425211) |
| Paired segment heap, Windows x64 | [40235428021](https://github.com/arut-land/a/actions/runs/40235428021) |
| Paired segment heap, Windows ARM64 | [40235430640](https://github.com/arut-land/a/actions/runs/40235430640) |
| snmalloc release-mode smoke | [40236722742](https://github.com/arut-land/a/actions/runs/40236722742) |
| Read scratch, 1/128 sessions | [40236295274](https://github.com/arut-land/a/actions/runs/40236295274) |

The earlier smoke run
[40234108961](https://github.com/arut-land/a/actions/runs/40234108961)
used `8b529c77`. Both Mac jobs failed in the Python harness tests before
building measurements, and Windows ARM failed compiling snmalloc. Its
completed Linux samples are smoke observations only. The Windows build
environment now removes Git Bash's `MSYSTEM`, which snmalloc checks before
the MSVC target when detecting its compiler.

That correction alone did not fix the Windows ARM compiler error: the new
smoke correctly reported `BUILD_CC=Msvc`, then failed with C1001 at
`freelist.h:934`. snmalloc also interpreted the release profile's line-table
debug information as a request for C++ debug mode. Its replacement campaigns
use `c249a084`, with that dependency's debug information disabled and
upstream's CMake/WaitOnAddress defaults on Windows. The original snmalloc
campaigns 40235408975 and 40235422691 were cancelled before measurement.
Run 40236728631 was superseded by the same comparison with both source
references pinned to `c249a084`.

## Support and dependencies

Candidates are mimalloc 0.1.52, tikv-jemallocator 0.7, snmalloc-rs 0.7.6 and
rpmalloc 0.2.2. Only one replacement can be selected in a build. jemalloc
has no MSVC implementation: its feature selects the system allocator there,
and the sessions campaign skips that comparison. It must not be reported as
a Windows jemalloc result.

The segment-heap executable variant embeds the Windows SegmentHeap manifest.
The sessions probe instead sets an Image File Execution Options key on its
copied test executable. That key alone does not verify that Windows switched
the heap; those sessions observations cannot establish a segment-heap win.

`cargo deny check` passes after merging main: advisories, bans, licenses and
sources are all clear. The unused CDLA-Permissive-2.0 allowance produces a
warning. No allocator feature changes the default allocator during the
campaign.

## Heap attribution and read scratch

On the Apple M3 (eight cores, 16 GiB, macOS 27.0.1), `heap`, `vmmap` and
`malloc_history` with `MallocStackLogging=lite` inspected the sessions process
after its turns, with 128 clients still attached. Symbolized release builds
before and after the read-scratch change gave:

| Diagnostic | Before | After |
| --- | ---: | ---: |
| Live allocator bytes (`heap`, rounded MiB) | 18.8 | 16.3 |
| Idle RSS, KiB | 45728 | 42336 |
| Physical footprint, KiB | 38992.75 | 35584.75 |
| Lifetime peak footprint, KiB | 39472.75 | 36096.77 |
| RSS after explicit pressure relief, KiB | 45728 | 42336 |

The pre-change history attributed 128 allocations totaling 2,621,440 bytes
to the incoming-frame buffer's 16 KiB resize, rounded by the allocator.
Every connection retained scratch while awaiting its next message. The
reader now allocates its fixed scratch on the polling stack and appends only
received bytes to its persistent buffer. Scratch is absent between polls;
incomplete or large messages still grow normally. No frame-size or session
memory limit was added. The regression test sends a small real frame,
checks scratch retention, receives a 64 KiB frame, and checks cooperative
yielding between ready batches.

These single diagnostic samples establish attribution, not timing. The
uninstrumented 20-round comparison uses `1190974f` against `48b9c3a0` on
all six hosted targets. Large transcript, snapshot and view caches remain
reconstructible state; dropping them indiscriminately also creates rebuild
work. The read scratch can be removed without choosing a cache lifetime.

## Prior evidence

The [libc probe](resources.md#allocators) remains a synthetic allocation
topology, rather than application throughput evidence. Its two-arena glibc
case retained more memory after workers exited, and explicit trim returned
much of it. The sessions campaign measures both arena policies against real
session traffic before revisiting the two-arena decision.

The [earlier macOS cache diagnostic](resource-mechanisms.md) found that
pressure relief reported zero bytes returned after dropping large caches.
The retirement callback and per-sample explicit release measure that question
again on the sessions workload. Retained allocations and allocator-held free
memory must be distinguished; a release call cannot discard live caches.
