# Native allocator release: phase 2 decisions

Automatic allocator release is disabled on every target. GNU x64 exceeded the
declared 3 ms turn-latency ceiling; Intel macOS exceeded the reopen ceiling.
The other targets remain disabled from phase 1.

Independent review found foreground CPU-pool contention and an unmeasured native
call overlap window. Maintenance now has its own on-demand worker through rt,
which exits with the job. The regression holds it blocked while a single-worker
CPU job completes; routing it through the old shared pool fails that assertion.

The enabled native backends were removed. Release authority is uninhabited and
the compile-time policy omits engine scheduling entirely. The independent
execution seam and regression remain. The measured experiments used GNU batch
scheduling or Intel utility QoS.

| Target | Final decision | Reason |
| --- | --- | --- |
| GNU Linux x64 | Disable | Turn latency exceeds the declared 3 ms ceiling |
| Intel macOS | Disable | Reopen latency exceeds the declared 3 ms ceiling |
| GNU Linux ARM64 | Disable | Phase-1 turn/reopen criteria failed |
| Windows x64 / ARM64 | Disable | Phase-1 charged-memory/latency criteria failed |
| Hosted Apple Silicon and local M3 | Disable | Phase-1 charged-memory consistency failed |
| Other targets, including musl | Disable | No qualifying native evidence |

## Forced native-call overlap

The 3 ms ceiling was declared before inspecting results. Each action has five
alternating no-release/native-release pairs on one native hosted runner. All numbers
below are milliseconds, median [min, max]; worst added is the maximum paired change.
Full CI does not supply these measurements.

| Target / action | No release | Native release | Paired added | Worst added | Native call | Write starts inside call |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| gnu-x64 / reopen | 31.581 [31.188, 31.909] | 32.152 [32.073, 33.461] | 0.886 [0.244, 1.869] | 1.869 | 0.404 [0.302, 0.456] | 5/5 |
| gnu-x64 / turn | 33.007 [32.680, 34.859] | 35.878 [34.293, 36.426] | 1.285 [1.248, 3.539] | 3.539 | 0.409 [0.263, 0.480] | 4/5 |
| intel-macos / reopen | 44.632 [42.794, 49.239] | 48.070 [47.110, 57.135] | 4.582 [-2.129, 12.502] | 12.502 | 1.460 [0.987, 1.578] | 5/5 |
| intel-macos / turn | 56.809 [44.450, 86.133] | 50.272 [48.168, 67.961] | -7.709 [-35.860, 19.472] | 19.472 | 1.695 [1.274, 1.993] | 5/5 |

Source and harness: c4e9ce57c2547682d60f535132cda502e0ec51e5. Each cohort deliberately
uses one identical fixture executable for both commands; this is a controlled
native-call/no-op comparison, not two production builds. The exact generated
instrumentation patch and binary identities accompany every cohort. Earlier automatic
relief is suppressed equally in both arms to retain the largest heap from two normal 32
stored / 8 active waves, four 64 KiB echo turns per active session. No synthetic
allocator balloon is added.

Only the maintenance worker is held after the real policy gate and final lease check. A
one-byte call-entry marker wakes the prepared probe immediately before the native API.
Native duration uses Instant around the actual call, excluding the socket wake and held
interval. Unix timestamps bracket the call and the real Attach/Input write; strict entry
counts require the write to begin inside the native-call interval. All five GNU turn
request intervals intersected the call; one write began 0.238 microseconds before entry.
Even the strictly after-entry subset reached +3.198 ms, so the negative decision does
not rely on that early write. Connection and durable-log preparation finish before the
marker. The 64 KiB turn frame is also encoded beforehand with the real protocol codec;
the small Attach message keeps its normal encoding path. Reopen latency includes replay;
turn latency includes a real completed echo reply. The turn cohort prepares its attached
session while the uncancellable boundary is held, forcing the already-started side of
the race without adding any pause inside the allocator API.

Overlap cohorts omit idle-worker retirement waits so the request races the freshly
retired heap while foreground workers are warm. The original steady-idle residency
cohorts retain their waits. These are empirical maxima from five pairs, not universal
tail bounds or an estimate of overlap probability. Request-dispatch overlap does not
prove which individual allocator mutex was contended. No allocation-domain isolation
redesign or speculative hardening was added.

| Cohort | Diagnostic perf run |
| --- | --- |
| gnu-x64 / reopen | [41182262839](https://github.com/arut-land/a/actions/runs/41182262839) |
| gnu-x64 / turn | [41182263786](https://github.com/arut-land/a/actions/runs/41182263786) |
| intel-macos / reopen | [41182264951](https://github.com/arut-land/a/actions/runs/41182264951) |
| intel-macos / turn | [41182266559](https://github.com/arut-land/a/actions/runs/41182266559) |

The first 32:4 probes were cancelled when the default workload maximum was confirmed as
32:8. The first 32:8 attempt stopped after a control sample because the summary expected
charge counters in a timing-only record; that bug was fixed and replayed against its
captured record before collecting these complete cohorts. Partial/superseded cohorts are
excluded. Full source identities, run IDs, raw timestamps, analysis, failed diagnostics
and check logs are retained in scratchpad/r/native-release.

## Verification and completion

Local macOS nextest for sys, rt and engine: 711 passed, 18 skipped after disabling the
backends and removing the now-vacuous queued-release engine test. Native clippy, Rust
documentation, formatting and the workspace Windows lint passed. The public rt
integration suite also passed after relocating the regression. The CI identity and final
target cleanup are recorded in the phase-2 report.

## Phase-1 evidence (historical)

The following measurements and provisional decisions predate the independent review. The
phase-2 table above supersedes their enablement decisions and CPU-pool scheduling
description.

### Native allocator release at hibernation

Keep release on **GNU Linux x64 and Intel macOS**. Disable it on GNU Linux
ARM64, Windows x64/ARM64 and Apple Silicon. Every decision uses native evidence;
no full CI was dispatched in phase 1.

| Target | Decision | Policy / reason |
| --- | --- | --- |
| GNU Linux x64 | Enable | Retain main arena top, return free interior pages; 4 MiB allocator free-capacity floor; scoped batch scheduling |
| GNU Linux ARM64 | Disable | Full trim misses reopen criterion; retaining main top misses turn criterion |
| Windows x64 | Disable | No consistent private-commit gain; reopen median increases |
| Windows ARM64 | Disable | No consistent private-commit gain; turn median increases |
| Intel macOS | Enable | Whole-process native pressure relief at utility QoS; both control and active events consistently pay |
| Hosted macOS ARM64 | Disable | Inconsistent charged-memory benefit |
| Local M3 | Disable | Inconsistent charge; active relief raises reopen and turn medians |
| Other targets, including musl | Disable | No qualifying native evidence |

### Implementation

The actor and mailbox leave their existing aggregate scope first. Its completion
reference and retirement guard form a second aggregate: Rust destroys the session
reference before the guard schedules allocator maintenance. Reopening never owns
that scheduling guard. The residency index issues a release only when it has no
loaded or loading sessions; loading any session drops the owning generation.
Queued jobs hold only a weak generation and abandon expired work. All native
operations run on the runtime's existing CPU blocking pool without residency
locks. No reclamation timer, polling loop, allocator tunable or dependency was
introduced.

Disabled targets have an uninhabited pending-work type and zero-sized lease.
Their compile-time policy constant also omits the scheduling lock. On the M3,
the final candidate does not import malloc_zone_pressure_relief. Existing
sys::release remains available to the repository's diagnostic workloads.

The GNU x64 policy checks mallinfo2().fordblks against a 4 MiB floor on the worker.
The five-round quiet one-session controls stayed below 3.49 MiB; active post-wave
cases exceeded 8 MiB. These are allocator capacity counters, not physical charge.
Batch scheduling is scoped to the current worker and restored before reuse.
The selected GNU x64 policy retains the main arena's top but still releases
free interior pages across arenas. The public API cannot select a retired session's
arena or
trim only the top; pad does not limit interior-page discard.

Intel macOS uses its native whole-process pressure-relief API at utility QoS.
Default-zone relief did not recover the earlier whole-process benefit. Its SDK
reservation statistics overlap between the control and active cases, so there
is no invented reservation threshold. Without a useful counter predictor,
whole-process enablement requires the control and active cases both to pay.

### Method and decision rule

Each release build uses the shipped recipe: size optimization, fat LTO, one
codegen unit and rebuilt std. Every Cargo command goes through mise with two
jobs and the assigned external target. Separate fixture daemons, isolated homes,
dummy loopback provider settings and the real echo agent supply four 64 KiB turns.
The probe verifies paged history against the durable log, holds a slow follower,
waits for truthful actor-exit hibernation, repeats the wave and reopens sessions.
No real model request is made.

Each accepted cohort contains five alternating reference/candidate process pairs
on one native runner. Native charge is PSS on Linux, physical footprint on macOS
and private commit on Windows. Counters and timing run in separate cohorts;
instrumented timings do not establish latency claims. Reopen is the slowest
session's attach including replay; turn latency is the slowest session's
four-turn batch. Quiet CPU covers a one-second interval after the existing
blocking workers' retirement interval. Reported values are median [min, max];
paired differences are computed within rounds, never between hosted machines.

For active 32:4, each wave's charge must fall in every pair. Both turn batches
and reopening must have nonpositive paired median latency changes, and every
candidate quiet-idle CPU observation must be zero. The GNU closed-gate control
must not increase paired median reopen latency. Intel's ungated policy must
also consistently reduce control-case charge. All spreads remain visible;
five rounds cannot establish a universal zero-latency-cost theorem.

### Selected uninstrumented policies

Charge rows use KiB; latency and CPU rows use milliseconds. Each row has five
paired rounds. The worst session's four-turn batch is not a single model turn.

| Cohort / case / metric | Reference | Candidate | Paired change |
| --- | ---: | ---: | ---: |
| gnu-x64 / retained main top / 32:4 / wave1_charge_kib | 15900.000 [15662.000, 16822.000] | 13748.000 [13152.000, 14981.000] | -1953.000 [-3085.000, -919.000] |
| gnu-x64 / retained main top / 32:4 / wave2_charge_kib | 16645.000 [16570.000, 17390.000] | 14708.000 [13916.000, 15193.000] | -2104.000 [-2785.000, -1423.000] |
| gnu-x64 / retained main top / 32:4 / reopen_ms | 32.252 [31.518, 34.104] | 30.399 [29.690, 30.666] | -1.853 [-3.765, -0.852] |
| gnu-x64 / retained main top / 32:4 / wave1_turn_ms | 77.357 [75.447, 79.301] | 74.142 [72.626, 77.232] | -2.070 [-4.731, 1.121] |
| gnu-x64 / retained main top / 32:4 / wave2_turn_ms | 114.436 [111.829, 118.255] | 108.861 [103.297, 110.078] | -5.575 [-14.959, -1.751] |
| gnu-x64 / retained main top / 32:4 / wave2_idle_cpu_ms | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] |
| intel-macos / whole process / 1:0 / wave1_charge_kib | 4388.000 [4348.000, 7936.000] | 1840.000 [1828.000, 1884.000] | -2560.000 [-6096.000, -2464.000] |
| intel-macos / whole process / 1:0 / wave2_charge_kib | 4384.000 [4344.000, 7932.000] | 1852.000 [1836.000, 1880.000] | -2540.000 [-6080.000, -2464.000] |
| intel-macos / whole process / 1:0 / reopen_ms | 18.535 [17.915, 22.002] | 17.810 [16.671, 17.980] | -1.864 [-4.022, -0.105] |
| intel-macos / whole process / 1:0 / wave2_idle_cpu_ms | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] |
| intel-macos / whole process / 32:4 / wave1_charge_kib | 4696.000 [4528.000, 4932.000] | 2276.000 [2236.000, 2280.000] | -2448.000 [-2656.000, -2248.000] |
| intel-macos / whole process / 32:4 / wave2_charge_kib | 4692.000 [4528.000, 4752.000] | 2276.000 [2196.000, 2288.000] | -2404.000 [-2476.000, -2332.000] |
| intel-macos / whole process / 32:4 / reopen_ms | 47.843 [46.653, 49.246] | 45.041 [43.400, 47.989] | -3.506 [-4.443, 0.976] |
| intel-macos / whole process / 32:4 / wave1_turn_ms | 115.177 [111.399, 116.031] | 109.188 [104.123, 114.571] | -6.840 [-11.054, 1.754] |
| intel-macos / whole process / 32:4 / wave2_turn_ms | 163.757 [162.811, 169.390] | 155.010 [148.050, 219.194] | -9.770 [-16.635, 55.869] |
| intel-macos / whole process / 32:4 / wave2_idle_cpu_ms | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] |

The GNU active case uses source 10209ee2. Its closed-gate control path is
unchanged from c50ce7fb: paired reopen -0.600 [-2.496, 4.686] ms, charge +23
[-243, 452] KiB and zero quiet CPU. No reclamation gain is credited to this
closed-gate population. Intel uses source c50ce7fb; later GNU-only changes do
not change its production path.

### Separate selected allocator census

These instrumented results establish allocator/charge behavior, not latency.
Allocated/free capacity is not a partition of physical charge. In particular,
Apple reservation statistics must not be subtracted from process footprint.

| Cohort / case / metric | Reference | Candidate | Paired change |
| --- | ---: | ---: | ---: |
| gnu-x64 / retained main top / 32:4 / wave1_charge_kib | 15708.000 [15402.000, 16241.000] | 14264.000 [14005.000, 14525.000] | -1444.000 [-2236.000, -974.000] |
| gnu-x64 / retained main top / 32:4 / wave2_charge_kib | 16582.000 [16381.000, 17008.000] | 15053.000 [14648.000, 15519.000] | -1488.000 [-1934.000, -1289.000] |
| gnu-x64 / retained main top / 32:4 / wave2_allocated_kib | 1588.611 [1585.762, 1600.541] | 1605.239 [1596.691, 1625.865] | 15.202 [4.698, 37.906] |
| gnu-x64 / retained main top / 32:4 / wave2_free_kib | 9594.301 [9055.482, 10283.629] | 9675.371 [9071.062, 10574.823] | 428.829 [-1212.567, 895.286] |
| gnu-x64 / retained main top / 32:4 / wave2_top_kib | 3203.688 [3202.453, 4097.281] | 3202.719 [3035.406, 3586.531] | -168.281 [-510.750, 257.047] |
| intel-macos / whole process / 1:0 / wave1_charge_kib | 6312.000 [4392.000, 6460.000] | 1896.000 [1892.000, 1952.000] | -4416.000 [-4568.000, -2440.000] |
| intel-macos / whole process / 1:0 / wave2_charge_kib | 6284.000 [4364.000, 6432.000] | 1868.000 [1864.000, 1924.000] | -4416.000 [-4568.000, -2440.000] |
| intel-macos / whole process / 1:0 / wave2_allocated_kib | 389.266 [386.766, 390.516] | 389.500 [387.750, 392.797] | 0.984 [-1.469, 3.531] |
| intel-macos / whole process / 1:0 / wave2_free_kib | 37405.234 [37356.172, 37434.734] | 37388.250 [37353.078, 37434.953] | -16.984 [-31.484, 1.469] |
| intel-macos / whole process / 32:4 / wave1_charge_kib | 4548.000 [4468.000, 4660.000] | 2280.000 [2268.000, 2296.000] | -2280.000 [-2380.000, -2184.000] |
| intel-macos / whole process / 32:4 / wave2_charge_kib | 4584.000 [4540.000, 4672.000] | 2288.000 [2272.000, 2296.000] | -2296.000 [-2400.000, -2244.000] |
| intel-macos / whole process / 32:4 / wave2_allocated_kib | 560.359 [546.266, 568.516] | 560.516 [551.891, 570.266] | 4.156 [-16.625, 14.250] |
| intel-macos / whole process / 32:4 / wave2_free_kib | 37359.641 [37315.891, 37389.734] | 37352.109 [37323.484, 37365.734] | -13.906 [-62.250, 49.844] |

### Rejected targets and variants

These complete uninstrumented candidate cohorts fail the stated criteria.

| Cohort / case / metric | Reference | Candidate | Paired change |
| --- | ---: | ---: | ---: |
| gnu-arm64 / retained main top / 32:4 / wave2_charge_kib | 14787.000 [14447.000, 15447.000] | 12891.000 [12711.000, 13043.000] | -1744.000 [-2556.000, -1596.000] |
| gnu-arm64 / retained main top / 32:4 / reopen_ms | 33.879 [29.934, 38.051] | 32.789 [31.410, 35.451] | -0.268 [-4.519, 1.477] |
| gnu-arm64 / retained main top / 32:4 / wave2_turn_ms | 115.687 [107.499, 118.942] | 114.800 [111.147, 117.773] | 0.371 [-4.141, 6.911] |
| gnu-arm64 / retained main top / 32:4 / wave2_idle_cpu_ms | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] |
| windows-x64 / process heap / 32:4 / wave2_charge_kib | 5492.000 [4096.000, 6048.000] | 4796.000 [4452.000, 5060.000] | -988.000 [-1160.000, 700.000] |
| windows-x64 / process heap / 32:4 / reopen_ms | 52.592 [52.170, 54.665] | 55.099 [54.095, 66.184] | 2.508 [0.313, 11.519] |
| windows-x64 / process heap / 32:4 / wave2_turn_ms | 191.385 [189.822, 197.534] | 200.721 [188.801, 219.104] | 9.337 [-1.021, 21.570] |
| windows-x64 / process heap / 32:4 / wave2_idle_cpu_ms | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] |
| windows-arm64 / process heap / 32:4 / wave2_charge_kib | 3076.000 [3012.000, 3100.000] | 3068.000 [2984.000, 3112.000] | 20.000 [-116.000, 56.000] |
| windows-arm64 / process heap / 32:4 / reopen_ms | 54.813 [47.191, 60.094] | 48.207 [43.909, 54.120] | -6.033 [-16.185, 1.016] |
| windows-arm64 / process heap / 32:4 / wave2_turn_ms | 176.747 [162.908, 194.397] | 194.748 [170.514, 209.084] | 14.687 [-20.628, 31.840] |
| windows-arm64 / process heap / 32:4 / wave2_idle_cpu_ms | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] |
| hosted-macos-arm64 / default zone / 32:4 / wave2_charge_kib | 8529.000 [8129.000, 10593.000] | 10705.000 [7713.000, 11665.062] | 1072.062 [-816.000, 2784.000] |
| hosted-macos-arm64 / default zone / 32:4 / reopen_ms | 54.748 [34.077, 60.346] | 47.593 [36.274, 58.807] | -7.743 [-18.474, 10.740] |
| hosted-macos-arm64 / default zone / 32:4 / wave2_turn_ms | 174.763 [147.720, 213.628] | 145.203 [109.958, 213.606] | -37.762 [-56.949, 38.843] |
| hosted-macos-arm64 / default zone / 32:4 / wave2_idle_cpu_ms | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] |
| m3 / default zone / 32:4 / wave2_charge_kib | 11696.516 [8256.516, 12560.516] | 12064.516 [8640.516, 15008.539] | 624.000 [-3920.000, 3808.000] |
| m3 / default zone / 32:4 / reopen_ms | 24.638 [23.453, 25.202] | 25.941 [25.372, 30.771] | 0.941 [0.491, 6.822] |
| m3 / default zone / 32:4 / wave2_turn_ms | 76.639 [73.795, 76.829] | 80.863 [80.221, 81.004] | 4.126 [3.413, 7.209] |
| m3 / default zone / 32:4 / wave2_idle_cpu_ms | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] |

GNU full trim with the measured gate missed reopen criteria on x64 (+0.414
[-1.823, 0.542] ms) and ARM64 (+0.503 [-0.211, 1.211] ms). The final retained-top
ARM64 candidate saved charge but its second-wave turn median increased by
+0.371 [-4.141, 6.911] ms, so it remains disabled.

The isolated x64 pad comparison (08892852 versus corrected full-trim owner
1f13b62b) retained +2036 [1526, 2626] KiB more charge and changed reopening by
+0.530 [-0.819, 2.057] ms. Padding was not a universal latency fix. The complete
retained-top policy was then independently verified; only x64 meets all criteria.

Fresh initial async x64 trimming had reopen change +0.101 [-0.162, 7.665] ms,
with +249 [220, 560] minor faults. This does not reproduce mem-release's earlier
consistent +2.3 ms penalty. Initial ARM64 full trim had +561 [555, 573] faults
while its paired median reopen improved. Returned pages can refault; the data
do not isolate a fixed per-fault latency cost or prove that refaults alone
explain the earlier penalty.

The final M3 disabled-backend comparison has reopen change +0.198
[-0.596, 2.742] ms, turn change +0.151 [-0.658, 3.196] ms and zero quiet CPU.
Its mixed spreads do not establish a consistent latency regression or any
reclamation benefit. Its binary does not import malloc_zone_pressure_relief;
no charged-memory saving is credited to a backend that cannot schedule relief.

### Native API constraints

[glibc source](https://github.com/bminor/glibc/blob/release/2.39/master/malloc/malloc.c)
shows that trim walks every arena under its allocator mutex and discards free
interior pages. Pad applies only to the main arena top. It does not select the
retired session's arena or create a top-only trim. Mallinfo2 also traverses bins
under locks. No private arena traversal or allocator tunable was introduced.

[Apple's
allocator](https://github.com/apple-oss-distributions/libmalloc/blob/main/src/malloc.c)
supports one-zone or whole-process relief. Default-zone relief did not recover
the whole-process Intel gain. Reservation/free-capacity ranges overlap between
control and active cases, so no capacity threshold was invented. Both measured
quiescent populations pay for whole-process relief. The M3 high-water field is
zero and cannot be used as touched free memory.

[HeapOptimizeResources](https://learn.microsoft.com/en-us/windows/win32/api/heapapi/nf-heapapi-heapsetinformation)
optimizes LFH caches and decommits where possible. The process-heap experiment
failed the Windows benefit/latency conditions.
[HeapCompact](https://learn.microsoft.com/en-us/windows/win32/api/heapapi/nf-heapapi-heapcompact)
is primarily a debugging facility; ordinary frees already coalesce blocks.
No global decommit thresholds, working-set trick or allocator tunable was added.

### Source and workflow identities

Reference source: 5610d9e021bbf83044a80974c7f6eac147fa2564. All accepted paired
executables have distinct hashes. Source revisions, native identities,
instrumentation patches, hardware, raw samples, commands and analysis scripts
are retained in the assigned scratchpad/r/native-release directory and workflow
artifacts. Local reference production files were restored from the reference
for their builds, with identical census instrumentation in both arms; the exact
reference patch and staged hashes are retained. Harness-only linker changes do
not change release flags or measured production paths.

| Experiment | Source / reference | Run |
| --- | --- | --- |
| Initial native timing | 39384c4e / 5610d9e | [41153342818](https://github.com/arut-land/a/actions/runs/41153342818) |
| Corrected native timing matrix | 08892852 / 5610d9e | [41154680358](https://github.com/arut-land/a/actions/runs/41154680358) |
| Corrected native allocator matrix | 08892852 / 5610d9e | [41154681943](https://github.com/arut-land/a/actions/runs/41154681943) |
| Isolated pad comparison | 08892852 / 1f13b62b | [41155316499](https://github.com/arut-land/a/actions/runs/41155316499) |
| Gated full-trim x64 timing/census | c50ce7fb / 5610d9e | [timing 41158387733](https://github.com/arut-land/a/actions/runs/41158387733), [census 41158389235](https://github.com/arut-land/a/actions/runs/41158389235) |
| Gated full-trim ARM64 timing/census | c50ce7fb / 5610d9e | [timing 41158390776](https://github.com/arut-land/a/actions/runs/41158390776), [census 41160234783](https://github.com/arut-land/a/actions/runs/41160234783) |
| Selected Intel timing/census | c50ce7fb / 5610d9e | [timing 41158393687](https://github.com/arut-land/a/actions/runs/41158393687), [census 41158395150](https://github.com/arut-land/a/actions/runs/41158395150) |
| Selected x64 timing/census | 10209ee2 / 5610d9e | [timing 41161615499](https://github.com/arut-land/a/actions/runs/41161615499), [census 41161617261](https://github.com/arut-land/a/actions/runs/41161617261) |
| Retained-top ARM64 timing/census | 10209ee2 / 5610d9e | [timing 41161618522](https://github.com/arut-land/a/actions/runs/41161618522), [census 41161619822](https://github.com/arut-land/a/actions/runs/41161619822) |

### Local verification

Final local M3 sys/engine nextest: 631 passed, 17 skipped. Native clippy,
formatting and the workspace Windows lint passed after disabling ARM64 release;
Rust documentation also passed. The selected retained-top GNU x64 native run
passed 639 sys/engine tests, 17 skipped, clippy and two probe tests. Its ARM64
experiment passed the same checks before that backend was disabled. The selected
Intel native run passed 633 sys/engine tests, 17 skipped, clippy and two probe
tests. Earlier full-trim GNU runs also passed 639 tests on each architecture.
No full CI was dispatched.

### Limits and owner-pending work

The engine never joins or awaits a release, and the pending generation is
abandoned on load. A native allocator call that has already started cannot be
interrupted and can take allocator-owned locks; later reuse can fault returned
pages. The residency workload waits for worker retirement before its measured
reopen, so it does not bound the overlap window for a request arriving during
a native call. Such overlap is possible only while that one quiescent job is
running; no probability is inferred from these samples. A stronger isolation
claim would require allocation-domain or container ownership changes and native
revalidation, beyond this intervention. No speculative allocator or runtime
hardening was added.

The GNU floor prevents the trim call below the threshold; reading that counter
still needs one small CPU-pool job. Control-case measurements include that cost.
Allocator reserved/free capacity is not a partition of process charge, especially
on Apple and Windows. Neither logical free bytes nor RSS is credited as charged
memory reclaimed.

Independent review and any fixes, the main merge, one full CI dispatch and
Linux/Windows success, and target cleanup belong to the separately authorized
phase 2. The assigned target is retained. No files outside ownership, user ~/.a,
.env.local, recorded fixtures or golden outputs were changed.

### Excluded diagnostics

The first local baseline attempt raced source editing and failed; it produced
no accepted sample. The rebuilt baseline and staged identities replace it.
Run 41153327442 used an abbreviated ref and was cancelled before usable evidence.
Census 41153350995 was superseded and cancelled after the completion-boundary fix;
its partial data are not used. ARM validation 41158391997 hit mold's range-thunk
assertion while linking engine tests; 41159071269 stopped at an LLVM RELR/glibc
loader mismatch. Neither reached an application test. The corrected harness uses
LLVM without packed relocations for ARM validation only; release images retain
mold and the original flags. No full CI failure or rerun occurred.

The full corrected matrix and allocator appendix are in the phase-1 report.
