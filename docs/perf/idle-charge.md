# Idle daemon charged memory

The gate is strictly below **10 MiB (10,485,760 bytes)**. RSS remains a
diagnostic, with no assertion. On 2026-10-28 the owner chose charged memory,
keeping the 10 MiB limit, after the round-3 investigation found that clean
resident code pages could cross the old RSS threshold while the retained
allocation set stayed flat.

`sys::process_memory::charged_bytes` owns native accounting and returns
bytes or an error. Missing, inaccessible or undecodable counters never
become a zero measurement. No platform is selected by the callers.

| OS | Counter | Meaning |
| --- | --- | --- |
| macOS | `proc_pid_rusage`, `rusage_info_v4.ri_phys_footprint` | The kernel's physical footprint charge, including compressed memory |
| Linux | `/proc/<pid>/smaps_rollup`, `Pss` | Resident private pages plus the proportional share of shared pages |
| Windows | `K32GetProcessMemoryInfo`, `PROCESS_MEMORY_COUNTERS_EX.PrivateUsage` | Private commit charge, including committed pages outside the working set |

Windows uses private commit rather than private working set because the
latter changes when the memory manager evicts pages. Microsoft's
[counter mapping](https://learn.microsoft.com/en-us/windows/win32/memory/memory-performance-information)
identifies `PrivateUsage` as Private Bytes / Task Manager Commit Size, and the
[structure documentation](https://learn.microsoft.com/en-us/windows/win32/api/psapi/ns-psapi-process_memory_counters_ex)
defines it as the process's commit charge. These are native policies with
different semantics; the table does not claim a single portable definition
of charged memory.

The Unix terminal gate in `apps/cli/tests/blackbox/budgets.rs` reads the
daemon's charge after three idle seconds. The Windows adapter in
`scripts/perf/budget-task.py` gates `idle_charged_kib`; its shared hosted
sampler invokes the native reader through the `perf-attach --memory <pid>`
example. Both continue reporting RSS. These are the two idle RSS assertions
replaced by this change; historical survey RSS observations stay unchanged.

## Investigation behind the accounting decision

The first-phase comparison used historical survey revision `d6bb6cce` and
candidate `1af5daf3`, with five fresh-home release pairs in alternating
AB/BA order on each native host. It found no MiB-scale private-memory
regression. On the M3, the historical revision already failed the exact
RSS budget in all five samples: median 10,544 KiB [10,480–10,704], versus
10,656 [10,592–10,704] for the candidate. Instrumented TUI footprint was
3,408.45 [3,264.49–3,440.49] versus 3,360.52 [3,312.49–3,408.47] KiB;
live malloc allocations rounded to 443 KiB in both. The
[raw M3 samples, exact-test results and build identity](idle-charge-investigation.json)
retain every observation, including the preliminary control below.

The hosted endpoint comparisons attributed the observed growth to resident
executable pages. Linux paired RSS growth was median +176 KiB [−32,+248],
with executable text +180 [−12,+244], heap 0 [−144,+104], and no settled
thread-count increase. Hosted macOS paired RSS grew +64 KiB [48,144] and
executable `__TEXT` +96 [96,96], while footprint changed −32 [−64.06,+48.06].
[Per-sample mapping attributions](idle-charge-investigation.json) accompany
the full native artifacts in [Linux run 40692028455](https://github.com/arut-land/a/actions/runs/40692028455)
and [macOS run 40692035085](https://github.com/arut-land/a/actions/runs/40692035085).
These map/charge diagnostics were collected after settling, separately from
the three-second RSS read; they are not simultaneous decompositions of it.
This endpoint comparison did not isolate a per-merge culprit.

In a preliminary unchanged-daemon control (`pilot/samples.json` in the
[raw evidence](idle-charge-investigation.json)), RSS fell from 10,560 to
8,352 KiB while footprint moved only from 3,360.49 to 3,328.49 KiB.
That roughly 2 MiB RSS drop with flat footprint illustrates reclaimable
residency, rather than an allocation fix. RSS would still be a legitimate
contract for total residency; the owner instead selected native charged
memory to gate process memory pressure. The numeric limit is unchanged,
and RSS remains visible so executable residency is still observable.

## Reproduction

`cargo nextest run -p sys --test process_memory` starts a real child and
checks that touching and retaining 4 MiB raises its native charge by at
least 3 MiB. A second invariant checks that an inaccessible process cannot
be reported as zero. The subprocess fixture is ignored when run directly.

The manual `perf` workflow with `idle_charge=true` exercises native Linux x64, macOS
arm64, Windows x64 and Windows arm64. `scripts/perf/idle-charge.py` builds
the normal fixture daemon and the memory probe, then copies repository
inputs to a temporary source tree outside the checkout. Only that copy is
modified to retain a touched 16 MiB buffer for the lifetime of `engine::serve`.
There is no allocation switch or test buffer in the shipped daemon.

All release builds finish before five fresh-home pairs alternate baseline
and retained-build order. Each sample starts the echo daemon with an idle
line client and reads charge and RSS after three seconds. The native Rust
counter is cross-checked against psutil's PSS/private bytes or an independent
libproc read on macOS. The sampler invokes the actual Windows adapter assertion. Every baseline
must pass the 10 MiB gate and every retained build must fail it. Sixteen MiB is used for the gate rejection
because adding 4 MiB to a roughly 3 MiB footprint would correctly remain
under the limit; the separate invariant tests the 4 MiB sensitivity.

Run locally with `CARGO_BUILD_JOBS=2` through `mise exec --`, passing
`--output` and `--target` paths outside the worktree. The target must follow
the task's `CARGO_TARGET_DIR` policy. The workflow retains raw samples,
executable identities, medians and full ranges as JSON. Native validation
results are recorded below after the campaign completes.

## Native observations

Apple M3, macOS 27.0.1; five fresh-home AB/BA pairs. Values are KiB, median
[min–max]. Builds finish before sampling, with two Cargo build jobs.

| Build | Charged memory | Diagnostic RSS | Gate outcomes |
| --- | ---: | ---: | --- |
| Main runtime | 3,424.49 [3,344.49–3,456.52] | 10,800 [10,720–10,832] | 5/5 pass |
| Throwaway +16 MiB retained | 19,824.49 [19,776.49–19,856.49] | 27,248 [27,200–27,280] | 5/5 rejected |

The main runtime's worst-case charged margin is 6,783.48 KiB (6.62 MiB).
The old RSS assertion would reject all five normal samples. No product
allocation or runtime policy changed.

All four hosted lanes in [perf run 40706095506](https://github.com/arut-land/a/actions/runs/40706095506)
passed at implementation revision `68c9c191`. Each lane ran the real-child
4 MiB reader invariants, then five interleaved normal/retained daemon pairs.
Every normal sample passed the actual Windows adapter memory assertion;
every retained build was rejected. Values below are KiB, median [min–max].

| Native target | Normal charge | Retained-build charge | Normal RSS (diagnostic) | Worst normal margin |
| --- | ---: | ---: | ---: | ---: |
| M3 macOS ARM64 | 3,424.49 [3,344.49–3,456.52] | 19,824.49 [19,776.49–19,856.49] | 10,800.00 [10,720.00–10,832.00] | 6,783.48 |
| Linux x64 | 7,431.00 [7,391.00–7,462.00] | 23,847.00 [23,473.00–23,973.00] | 10,556.00 [10,516.00–10,604.00] | 2,778.00 |
| Hosted macOS ARM64 | 2,704.94 [2,576.88–2,736.94] | 19,169.00 [19,120.94–19,217.00] | 10,448.00 [10,304.00–10,480.00] | 7,503.06 |
| Windows x64 | 2,136.00 [2,104.00–2,272.00] | 18,560.00 [18,528.00–18,692.00] | 9,848.00 [9,812.00–9,908.00] | 7,968.00 |
| Windows ARM64 | 2,184.00 [2,172.00–2,188.00] | 18,604.00 [18,588.00–18,620.00] | 10,604.00 [10,592.00–10,664.00] | 8,052.00 |

[Raw samples](idle-charge.csv) include the independent native-library
cross-check for each observation. All five targets used the same reader
source SHA-256, `c36380eb4826e5b63f220a525d8c61e6940b46c2d0c0a35aa2f4d1e5d3209e82`. M3 was measured
in the working tree before the implementation commit; its normal runtime
is main `63ac410a`. The reader is called by the observer and test process,
not by the daemon. The hosted lanes measured the clean implementation commit.
No product allocation reduction is claimed.

The actual Unix terminal budget test was also exercised in five fresh-home
pairs on the M3: all five normal daemons passed (printed charge 3,328–3,488
KiB), and all five retained daemons failed (19,824–19,904 KiB). Terminal
client CPU remained zero in every three-second window. Its worst-case
normal charged margin exceeds 6.59 MiB.
