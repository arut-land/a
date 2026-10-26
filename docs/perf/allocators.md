# Allocators and memory after a burst

The adoption criterion is lower memory beyond the A/A band, with no CPU or
latency regression. RAM limits that prevent work are not candidates. Allocator
arena counts control allocator organization, rather than how much live session
state the daemon may own. Qualifying the retirement callback also requires
upper 95% bounds for active CPU and turn/delta p99 latency within the matching
A/A band. A wider interval leaves that target unqualified.

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
variant; no allocator feature is passed to that build. The A/A suite builds
that same revision in both positions. Shipped executable hashes are recorded,
not assumed identical: the Linux ARM64 A/A hashes match, while Apple Silicon's
hashes differ despite identical sizes. Its source, variant and 20-round
control remain explicit; byte identity is not claimed across all targets. The sessions records also
report `system` for every `A`, `A2`, `rest` and `arenas` baseline checked.

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
filled with zero. Executable size is the exact difference between two saved
builds, not a statistically sampled interval; `n=1` for that field. Samples
with failed calls or dropped deltas cannot enter
the effect table.

```sh
python3 scripts/perf/allocator_effects.py sessions.jsonl --output effects.json
python3 scripts/perf/allocator_effects.py paired.json --output effects.json
```

## Campaign provenance

The mimalloc, rpmalloc and segment-heap campaign source is `cfd5ae5d`, before
the later merge of main. The corrected jemalloc/snmalloc sessions block and
snmalloc paired suite use `c249a084`, which includes that merge and the
read-scratch change. Each allocator is compared with its own same-source
system build; absolute footprints across those blocks are not interchangeable.
The local M3 allocator binaries were saved by the preceding build campaign.
The original dispatches split allocators around a 150-minute measurement
step; Apple Silicon retries use the longer window below. To reproduce the
experimental composition, pin `harness_ref=b41ea27b71f0cb4af2a392a5b1b3daef1efcdc36`
and build the pinned measured source from each run. The final branch removes
rejected features. Harness and measured source are separate checkout inputs.

| Comparison | Run |
| --- | --- |
| Smoke, every allocator, 1/8 sessions | [40235381866](https://github.com/arut-land/a/actions/runs/40235381866) |
| mimalloc, 1/128 sessions | [40235406289](https://github.com/arut-land/a/actions/runs/40235406289) |
| jemalloc and snmalloc, 1/128 sessions | [40241553286](https://github.com/arut-land/a/actions/runs/40241553286) |
| rpmalloc and Windows segment heap, 1/128 sessions | [40235412374](https://github.com/arut-land/a/actions/runs/40235412374) |
| Paired system/system A/A | [40241525439](https://github.com/arut-land/a/actions/runs/40241525439) |
| Paired mimalloc | [40241528406](https://github.com/arut-land/a/actions/runs/40241528406) |
| Paired jemalloc | [40241531006](https://github.com/arut-land/a/actions/runs/40241531006) |
| Paired snmalloc | [40241541308](https://github.com/arut-land/a/actions/runs/40241541308) |
| Paired rpmalloc | [40241533786](https://github.com/arut-land/a/actions/runs/40241533786) |
| Paired segment heap, Windows x64 | [40241536281](https://github.com/arut-land/a/actions/runs/40241536281) |
| Paired segment heap, Windows ARM64 | [40241538698](https://github.com/arut-land/a/actions/runs/40241538698) |
| snmalloc release-mode smoke | [40241550744](https://github.com/arut-land/a/actions/runs/40241550744) |
| Retirement hook, separate daemon | [40241543773](https://github.com/arut-land/a/actions/runs/40241543773) |
| Segment heap, ARM64 sessions only | [40241548413](https://github.com/arut-land/a/actions/runs/40241548413) |
| Read scratch, 1/128 sessions | [40241546207](https://github.com/arut-land/a/actions/runs/40241546207) |
| System/system A/A, Windows ARM64 retry | [40249311731](https://github.com/arut-land/a/actions/runs/40249311731) |
| Retirement hook, Windows ARM64 retry | [40253490613](https://github.com/arut-land/a/actions/runs/40253490613) |
| mimalloc, Apple Silicon, 128-session retry | [40258918237](https://github.com/arut-land/a/actions/runs/40258918237) |
| rpmalloc, Apple Silicon, 128-session retry | [40258921054](https://github.com/arut-land/a/actions/runs/40258921054) |

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
The first pinned-revision dispatches used abbreviated commit IDs, which
checkout interpreted as branch names. They were cancelled before measurement
and replaced with full-hash inputs. The table lists the replacements. Waiting comparisons were then dispatched
with the runner matrix computed directly, removing an Ubuntu-only selector
job that blocked their native runner queues; their measured sources and
paired protocol are unchanged.

The original Apple Silicon mimalloc and rpmalloc sessions steps hit their
150-minute limit with one-session samples complete, but only 19 complete
128-session control pairs. Those incomplete pairs remain separate evidence.
The longer-window retries measure only 128 sessions; there is no pairing
across runners. The first mimalloc retry, 40252160653, was cancelled while
queued to avoid repeating its completed one-session measurements.

## Allocator decision

Keep the native system allocator. The replacement allocators trade memory for
throughput in the separate Linux daemon, while retaining substantially more
memory in the sessions workload. For example, on Linux ARM64, mimalloc changes
idle daemon RSS by +18,814 KiB [18,808, 18,820] and driver CPU by
−20.624% [−20.696, −20.373]; jemalloc changes them by +2,110 KiB
[2,098, 2,122] and −12.476% [−12.822, −12.032]. Corrected release-mode
snmalloc increases both: +9,802 KiB [9,776, 9,840] and +15.073%
[14.563, 15.325]. These are 20-pair 95% intervals, not smoke estimates.
A CPU improvement alone does not meet this campaign's memory criterion.

The four experimental allocator features, their native build dependencies and
the forwarding global-allocator seam have been removed. Their pinned source
commits above preserve reproducibility. The final binary uses Rust's native
system allocator directly; measurement helpers continue to label it `system`.
The Windows segment-heap manifest is also rejected: idle RSS changes by
−92 KiB [−112, −52] on x64, overlapping its A/A band of ±80 KiB, and
+8 KiB [−4, 28] on ARM64. These are 20 matched daemon pairs. Its experimental
feature and manifest rewrite have been removed. The retirement callback is
a separate decision.

The release-mode snmalloc smoke completed on Linux ARM64 and Windows x64;
its remaining queued jobs were cancelled because the same-source full paired
campaign covers the build and runtime checks on those targets.
Windows ARM64's full paired build confirms the remaining C1001 failure.

The local Apple M3 cleanup build at `40f8663b`, using the normal
`cargo build-release -p a` profile and two build jobs, is 5,243,824 bytes
(5,120.922 KiB). The shipped-byte fixture exclusion scan passes. This is an
exact build observation; allocator size differences below compare matching
sources within each paired campaign.

## Support and dependencies

Candidates are mimalloc 0.1.52, tikv-jemallocator 0.7, snmalloc-rs 0.7.6 and
rpmalloc 0.2.2. Experimental builds admitted one replacement at a time. jemalloc
has no MSVC implementation: its feature selects the system allocator there,
and the sessions campaign skips that comparison. It must not be reported as
a Windows jemalloc result.

rpmalloc fails to link on Windows ARM64: its native object references the
x86 `_mm_pause` intrinsic (`LNK2019`, `_rpmalloc_global_cache_extract_spans`).
This is an unsupported target for the measured crate, rather than a runtime
sample. The shared rpmalloc/segment sessions job failed before it could
measure either; a separate segment-only ARM64 campaign preserves that
comparison.

snmalloc also fails on Windows ARM64 with upstream's CMake build in release
mode: MSVC 14.51 reports C1001 at `freelist.h:934`. Clearing `MSYSTEM` and
disabling C++ debug mode did not resolve that compiler failure. It is not
qualified on this runner/toolchain; no performance result is substituted.

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

## Retirement callback decision

Do not retain the automatic callback, its runtime configuration field or its
worker accounting. On Linux x64, the separate daemon suite changes large-JSON
CPU by +1.314% [1.042, 1.552], entirely beyond its A/A band of ±0.572%.
That violates the adoption criterion. These short driver/large-JSON workloads
do not wait for worker retirement: their CPU results compare compiled runtime
variants and bookkeeping, rather than timing `malloc_trim` itself.

Memory return alone is insufficient. After the read-scratch change, the
128-session callback returns 626 KiB [510, 796] on Linux ARM64 and 1,556 KiB
[1,262, 2,046] on Windows x64. Their upper turn-p99 bounds (+2.894% and
+5.355%) extend beyond their respective A/A bands (±1.940% and ±3.497%).
Those targets also fail the conservative latency qualification. The M3
shows no memory return beyond A/A.

The native `sys::release()` entry point remains an explicit diagnostic used
by the measurement executable. Production has no automatic trim, extra
retirement timer, callback field or worker counter. The experiment and its
lifecycle invariant test remain available in the pinned source commits.
The current sessions harness removes the rejected callback layout and its
`rest=none` option; the experimental campaign uses the pinned harness above.

## Local M3 paired results

Twenty alternating rounds at 128 sessions; values are median paired changes
with 95% bootstrap intervals. Memory is KiB, CPU and latency are percent.
The four-allocator block has a shared A/A control; the corrected snmalloc and
read-scratch block has its own control.

| Comparison | Idle RSS, KiB | Active CPU, % | Turn p99, % |
| --- | ---: | ---: | ---: |
| A/A, original block | −64 [−352, 296] | −0.521 [−5.556, 8.861] | −0.094 [−1.879, 1.098] |
| mimalloc | 56040 [55400, 56248] | 1.187 [−2.434, 6.007] | 1.642 [−1.720, 3.448] |
| jemalloc | 46104 [45912, 46264] | 0.370 [−5.111, 4.953] | 1.862 [−1.195, 5.037] |
| rpmalloc | 92320 [91944, 92520] | −1.206 [−8.469, 2.319] | 0.587 [−1.740, 3.365] |
| A/A, corrected block | 80 [−200, 304] | 0.515 [−1.953, 5.870] | 1.027 [−1.420, 2.540] |
| snmalloc, release mode | 59504 [59064, 59592] | −1.266 [−6.644, 2.865] | −0.596 [−1.613, 1.660] |
| Read scratch on polling stack | −2256 [−2440, −1952] | 0.452 [−2.861, 2.869] | −0.900 [−1.974, 0.774] |
| Retirement callback, corrected block | 184 [−408, 360] | −0.054 [−3.090, 2.616] | 0.655 [−1.251, 2.594] |

Physical footprint in the corrected block falls by 2,247.988 KiB
[1,944, 2,448] after the read-scratch change. Its A/A interval is
[−216.012, 304.023] KiB. Both RSS and footprint beat that control band;
CPU and latency stay within it. Every sample completes without failed calls
or dropped deltas. The original block overlapped local checks, profiling and
builds during its early rounds, so its noisy CPU intervals cannot establish
a throughput win. The original snmalloc binary had C++ debug mode enabled;
its measurements are retained for provenance and superseded by release mode.

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
