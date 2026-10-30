# Memory ownership and reclamation

Part A recommendation: **drop the hibernation allocator-release experiment**.
Current paged-protocol measurements do not establish a consistent charged-memory
benefit across the required platforms. The final production code does not trim
at hibernation. Part B keeps the behavior-preserving ownership changes below.

Local component and uninstrumented M3 reference: `8d2b9e8d`. Corrected hosted
comparisons and all census cohorts use reference `455a92e7`. Both use paged
protocol 43. Main `52809dde` and then `b574d6e8` were merged. Their intervening
changes are tests, CI settings and a rustdoc-link correction; the measured
release production paths are unchanged. Measurements retain their actual pinned
source identities rather than being relabeled as builds of the newer commits.
The protocol-37 census report established the hypothesis, not a current result.

## Ownership changes

The actor and mailbox leave a lexical scope before the completion signal.
Their aggregate owner preserves actor-before-mailbox destruction order.
The experimental `AllocationScope<T>` dropped retired live state before the
allocator guard, outside the residency mutex and before catalog publication.
It introduced no timer, polling, allocator-tunable change or additional thread.
It was measured and removed. Its native operation was
`malloc_trim(0)` on glibc, `malloc_zone_pressure_relief` on macOS, and
`HeapSetInformation(HeapOptimizeResources)` on Windows.

Model-only projection fields have one owner, replaced with its default value.
Durable invocation/execution state remains outside that owner. Immutable wire
frames convert their completed Vec into a boxed slice instead of calling
`shrink_to_fit`. Prefix conversion uses `Arc::unwrap_or_clone` to consume a
uniquely owned encoding; shared prefixes retain copy-on-write behavior. Blob
and history authorization/loading leases end at lexical scope boundaries.
Native ACL, COM folder, credential and LocalAlloc resources have private Drop
owners in sys.

## Audit

| Site | Previous/current pattern | Shape and disposition |
| --- | --- | --- |
| sys/memory; engine actor completion | Explicit actor/mailbox drop; allocator release existed only as a diagnostic operation | Done: lexical aggregate owner for actor/mailbox. Measured and removed the allocator guard: no consistent charged-memory benefit. Original slot transition restored. |
| engine/projection, driver and prefix/spawn paths | `release_model` reset five cache fields individually | Done: one defaultable `Model` owner, replaced through assignment. |
| protocol/Frame | Completed Vec followed by `shrink_to_fit` | Done: `Vec::into_boxed_slice`, shared through Arc; encoded bytes unchanged. |
| llm/conversation, Prefix → Resident | Always cloned the encoding behind an Arc | Done: standard conversion takes unique ownership with `Arc::unwrap_or_clone`; shared ownership still clones. Existing prefix invariant runs both ownership cases. |
| engine/fetch and history/answer | Explicitly dropped live/slot leases before paced delivery | Done: lexical scopes end leases before delivery. |
| sys/fs/edit Darwin ACLs | Three explicit `acl_free` sites and a cleanup-only closure | Done: private ACL owner; native error captured before destruction where errno matters. |
| sys/paths/windows known folder | Manual COM free on success/error | Done: private owner borrows the loaded deallocator; allocation drops before the library references. |
| sys/windows SID/security allocations | Manual SID-string LocalFree; separate descriptor Drop | Done: one private LocalAlloc owner for both resource kinds. |
| sys/credentials Windows reads | Copy native credential blob, then CredFree | Done: private credential owner, released by Drop. |
| sys/TransientBuffer and PageBuffer | Fallible clear/consume; retained address reservation; initialized heap scratch | Left: clear/consume are logical stream operations, not teardown. Unread/cancelled input must survive a read future. Drop already releases the reservation. Moving fallible discard into Drop would lose its error or discard unread tails. |
| protocol FrameReader/frame_heap, sys ReadBuffer | Borrowed read scratch commits through a guard; cancellation restores the initialized prefix | Left: already borrow-scoped RAII. Completion/consumption returns backing pages through the existing native seam. |
| llm SSE/native and heap buffers | Clear event/line/data at parser boundaries; preserve small scratch | Left: parser transitions are fallible and can retain a partial next event. Replacing buffers would lose the measured reservation/scratch reuse. |
| client/inbox segments | Full segments leave with work; one small head is reused | Left: ownership already releases drained burst storage. Default/take of the entire queue would discard useful small-traffic capacity. |
| client/inbox explicit guard/waker drops | Unlock before waking or dropping arbitrary callbacks | Left: deliberate early Drop is idiomatic synchronization; callback ordering is covered by the existing invariant tests. |
| engine/outbox registrations and queue state | Ownership transfer through take; explicit early guard drops; close/abandon are observable channel transitions | Left: these methods deliver cancellation/disconnection semantics. RAII alone cannot replace a transition while the shared outbox remains alive. |
| engine/history reset, llm writer/reset/close, decoder release/clear, metadata clear | Invalidate a generation, complete a protocol item or normalize a request | Left: semantic state transitions; not native-resource teardown. Drop already owns the underlying data. |
| llm Resident::retain and canonical reconstruction | Drop encoded canonical prefixes; reload canonical data only when requested | Left: the resident-history design intentionally separates encoded and canonical ownership. Retaining both would regress turn-residency. |
| sys ZeroedWords, native handles, CF objects, pressure sources, staging files | Existing Drop/OwnedFd/OwnedHandle owners or native callback destructors | Left: already RAII; OS finalizer callbacks are the foreign ownership boundary. |
| sys page-buffer reservation rollback | Explicit unmap while splitting an OS reservation | Left: native construction rollback before a PageBuffer exists; not an exposed begin/end API. No speculative mapping-error hardening added. |
| sys Linux post-fork descriptor closing | Raw close/exec/_exit path | Left: the child cannot run Rust destructors or allocate after fork. Parent resources already use OwnedFd. |
| sys child handover release | Give a shared child identity to the next executable image | Left: an exec ownership-transfer protocol, not freeing memory; changing it requires a separate handover design. |
| sys/census glibc XML allocation | Manual malloc-buffer free after fclose | Done: private malloc owner drops the transferred buffer. The fallible fclose remains explicit, preserving its published-buffer/error contract. |
| sys/runtime/image copyfile state | Copy result and fallible native free result are both preserved | Left: an infallible Drop alone would suppress the existing cleanup error. No new close/guard abstraction for this short linear fallible boundary. |
| engine JSONL file close before publication | Explicit drop of a standard File before rename | Left: required native publication ordering, already executed through Rust Drop; no manual native free. |
| client drive/remote and other app scopes | Early lock or stdin Drop before callbacks/EOF-sensitive waits | Left: standard ownership sequencing and observable I/O semantics; outside the inbox implementation ownership for this branch. |
| test-only early drops and native fixture cleanup | Deliberately trigger shutdown, EOF, cancellation, mapping release or foreign cleanup | Left: lifetime transitions are the behavior under test. No speculative panic-cleanup changes. |
| engine runtime-demand/context-image measurements | Explicit `sys::release` before/after snapshots | Left: diagnostic interventions distinguish retained from allocated memory; not production lifetime management. |
| CLI startup/sys::small_footprint | Existing glibc arena configuration | Left unchanged: startup policy is not reclamation; this experiment must not change allocator tunables. CLI startup is outside this branch's file ownership. |
| ext-index bigram filter | Builder slab Drop and sparse Vec shrink | Left: outside ownership; compaction also extends the sparse Vec. Box conversion is not a behavior-preserving local substitution. |

## Verification and measurements

Raw evidence, commands and logs are under the assigned
`scratchpad/r/mem-release` directory, outside the worktree. All local commands
use an allowlisted environment with dummy loopback provider settings,
`MISE_NO_ENV=1`, two Cargo jobs, and the single assigned target directory.
Fixture daemons use temporary isolated homes and their normal stop command.

The final merged-tree checks are recorded below. Earlier development logs
remain in the evidence directory.

The census also records macOS all-zone malloc statistics; the same
fixture-only module is installed in both paired sources and its hash is retained.
No protocol-37 numbers substitute for the requested current-protocol data.

## Native residency results

Focus: 32 stored sessions, four active, after the second hibernation wave.
Each row has five paired rounds. Negative deltas favor A2.

Uninstrumented process charge, KiB:

| Cohort / metric | A | A2 | Paired A2 − A |
| --- | --- | --- | --- |
| m3-timing / charge | 12208.516 [8640.492, 12672.516] | 12640.516 [10720.516, 13392.516] | 351.977 [-1312.023, 4192.000] |
| timing-ubuntu-x64 / charge | 16563.000 [16237.000, 16881.000] | 12689.000 [12376.000, 13662.000] | -3694.000 [-4505.000, -2575.000] |
| timing-ubuntu-arm / charge | 14975.000 [14375.000, 15483.000] | 10671.000 [10215.000, 11307.000] | -4064.000 [-5268.000, -4016.000] |
| timing-windows-x64 / charge | 5356.000 [4208.000, 5756.000] | 4916.000 [3916.000, 5236.000] | -292.000 [-1840.000, 840.000] |
| timing-windows-arm / charge | 3036.000 [3012.000, 3096.000] | 3092.000 [2980.000, 3512.000] | 8.000 [-116.000, 484.000] |
| timing-macos-arm / charge | 11025.188 [10545.062, 11761.062] | 10785.062 [9393.000, 12097.062] | -160.000 [-1664.062, 336.000] |
| timing-macos-x64 / charge | 4660.000 [4608.000, 4688.000] | 2804.000 [2752.000, 2828.000] | -1856.000 [-1936.000, -1796.000] |

Separate instrumented census, KiB:

| Cohort / metric | A | A2 | Paired A2 − A |
| --- | --- | --- | --- |
| m3-census / charge | 7920.492 [6800.516, 10704.492] | 7600.516 [6016.516, 15424.516] | 720.023 [-4687.977, 5584.000] |
| m3-census / allocated | 594.562 [594.219, 594.906] | 594.844 [594.656, 595.031] | 0.406 [0.031, 0.469] |
| m3-census / free capacity | 32173.781 [28077.250, 40365.094] | 36269.219 [36268.969, 40365.344] | 4095.594 [-0.469, 8191.969] |
| census-ubuntu-x64 / charge | 16659.000 [16396.000, 16787.000] | 12744.000 [12129.000, 12979.000] | -3915.000 [-4658.000, -3585.000] |
| census-ubuntu-x64 / allocated | 1584.140 [1555.698, 1623.870] | 1609.489 [1594.394, 1610.899] | 22.834 [-12.971, 39.839] |
| census-ubuntu-x64 / free capacity | 9656.364 [8664.851, 10635.954] | 6006.604 [5690.032, 6177.163] | -3547.839 [-4629.350, -2974.818] |
| census-ubuntu-arm / charge | 15023.000 [14731.000, 15271.000] | 10951.000 [10611.000, 11443.000] | -3804.000 [-4516.000, -3580.000] |
| census-ubuntu-arm / allocated | 1137.531 [1124.006, 1138.755] | 1140.106 [1120.322, 1156.803] | 1.352 [-10.228, 19.930] |
| census-ubuntu-arm / free capacity | 9815.536 [8654.469, 10305.566] | 6655.197 [5495.894, 6728.064] | -3151.858 [-4421.352, -2309.772] |
| census-windows-x64 / charge | 4876.000 [4440.000, 6360.000] | 4980.000 [4568.000, 5308.000] | -172.000 [-1052.000, 540.000] |
| census-windows-x64 / allocated | 656.521 [652.120, 659.816] | 660.163 [651.484, 660.239] | 3.708 [-8.332, 8.119] |
| census-windows-x64 / free capacity | 5123.896 [4575.880, 5716.184] | 3867.837 [3507.761, 4500.516] | -1215.668 [-1719.708, -728.402] |
| census-windows-arm / charge | 3120.000 [3092.000, 3216.000] | 3148.000 [3108.000, 3196.000] | 8.000 [-60.000, 56.000] |
| census-windows-arm / allocated | 646.710 [644.281, 649.127] | 648.387 [644.414, 650.514] | 0.133 [-2.026, 6.058] |
| census-windows-arm / free capacity | 543.995 [539.544, 626.873] | 567.613 [545.486, 614.939] | 10.026 [-59.260, 70.944] |
| census-macos-arm / charge | 10913.250 [10785.188, 11185.188] | 8945.188 [8465.188, 10945.188] | -1904.000 [-2352.062, -208.000] |
| census-macos-arm / allocated | 522.344 [521.625, 522.906] | 522.344 [521.844, 523.125] | 0.531 [-1.062, 1.281] |
| census-macos-arm / free capacity | 19957.656 [15862.375, 19958.156] | 19957.656 [19956.875, 19958.156] | 0.000 [-1.281, 4095.469] |

Uninstrumented latency and CPU, milliseconds:

| Cohort / metric | A | A2 | Paired A2 − A |
| --- | --- | --- | --- |
| m3-timing / reopen | 27.335 [26.023, 38.494] | 26.451 [25.425, 27.291] | -0.595 [-12.043, 0.150] |
| m3-timing / wave 2 attach | 43.573 [42.555, 68.051] | 41.367 [22.826, 46.009] | -3.472 [-28.775, 2.436] |
| m3-timing / wave 2 turn batch | 83.154 [82.258, 91.158] | 84.010 [82.623, 84.449] | 0.856 [-8.536, 1.730] |
| m3-timing / wave 2 detach CPU | 79.849 [55.956, 85.853] | 83.606 [66.763, 87.873] | 1.477 [-8.938, 27.650] |
| m3-timing / wave 2 quiet idle CPU | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] |
| timing-ubuntu-x64 / reopen | 23.465 [23.298, 23.998] | 25.994 [25.651, 26.617] | 2.345 [1.996, 3.318] |
| timing-ubuntu-x64 / wave 2 attach | 16.361 [16.181, 16.610] | 18.337 [17.820, 37.550] | 2.151 [1.210, 21.369] |
| timing-ubuntu-x64 / wave 2 turn batch | 87.643 [83.503, 88.816] | 100.996 [92.506, 179.415] | 16.970 [4.124, 91.772] |
| timing-ubuntu-x64 / wave 2 detach CPU | 40.000 [40.000, 50.000] | 40.000 [40.000, 50.000] | 0.000 [0.000, 10.000] |
| timing-ubuntu-x64 / wave 2 quiet idle CPU | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] |
| timing-ubuntu-arm / reopen | 30.731 [29.302, 31.183] | 30.308 [29.971, 30.480] | -0.357 [-1.030, 1.178] |
| timing-ubuntu-arm / wave 2 attach | 20.753 [20.066, 20.997] | 21.467 [20.888, 22.612] | 0.715 [-0.110, 1.785] |
| timing-ubuntu-arm / wave 2 turn batch | 107.982 [106.421, 109.815] | 103.604 [102.319, 108.672] | -2.869 [-7.496, 0.690] |
| timing-ubuntu-arm / wave 2 detach CPU | 50.000 [40.000, 60.000] | 50.000 [50.000, 60.000] | 10.000 [-10.000, 10.000] |
| timing-ubuntu-arm / wave 2 quiet idle CPU | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] |
| timing-windows-x64 / reopen | 40.901 [39.376, 43.781] | 39.725 [39.334, 41.132] | -0.576 [-4.055, 0.022] |
| timing-windows-x64 / wave 2 attach | 28.019 [27.678, 29.181] | 27.795 [27.297, 28.446] | -0.466 [-0.735, 0.723] |
| timing-windows-x64 / wave 2 turn batch | 139.648 [137.082, 144.932] | 138.355 [137.212, 141.224] | -1.293 [-7.720, 1.873] |
| timing-windows-x64 / wave 2 detach CPU | 125.000 [78.125, 140.625] | 109.375 [93.750, 109.375] | -15.625 [-31.250, 15.625] |
| timing-windows-x64 / wave 2 quiet idle CPU | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] |
| timing-windows-arm / reopen | 49.417 [44.426, 54.053] | 47.946 [43.744, 53.970] | 1.191 [-10.309, 3.520] |
| timing-windows-arm / wave 2 attach | 32.473 [31.025, 37.608] | 32.150 [31.601, 34.549] | -0.872 [-5.458, 1.571] |
| timing-windows-arm / wave 2 turn batch | 174.895 [163.164, 179.619] | 170.173 [161.490, 194.461] | 4.129 [-17.162, 14.842] |
| timing-windows-arm / wave 2 detach CPU | 125.000 [78.125, 125.000] | 140.625 [93.750, 156.250] | 31.250 [-31.250, 31.250] |
| timing-windows-arm / wave 2 quiet idle CPU | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] |
| timing-macos-arm / reopen | 34.913 [31.998, 37.432] | 41.117 [36.413, 46.425] | 6.936 [-1.019, 14.428] |
| timing-macos-arm / wave 2 attach | 31.759 [26.104, 59.067] | 39.776 [34.592, 60.980] | 6.751 [-20.808, 34.877] |
| timing-macos-arm / wave 2 turn batch | 117.833 [103.383, 128.215] | 102.814 [102.648, 121.786] | -0.764 [-25.525, 17.270] |
| timing-macos-arm / wave 2 detach CPU | 101.980 [89.366, 105.655] | 100.623 [69.912, 107.383] | -1.356 [-26.570, 18.017] |
| timing-macos-arm / wave 2 quiet idle CPU | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] |
| timing-macos-x64 / reopen | 52.322 [49.497, 96.290] | 48.688 [45.873, 54.472] | -3.634 [-49.698, 0.014] |
| timing-macos-x64 / wave 2 attach | 33.248 [32.188, 35.934] | 31.663 [30.974, 46.852] | -1.586 [-3.700, 13.546] |
| timing-macos-x64 / wave 2 turn batch | 173.330 [168.617, 205.964] | 163.498 [155.403, 214.948] | -9.832 [-50.560, 45.447] |
| timing-macos-x64 / wave 2 detach CPU | 75.577 [72.140, 97.843] | 74.434 [70.642, 91.503] | -1.497 [-6.339, 0.120] |
| timing-macos-x64 / wave 2 quiet idle CPU | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] |

Census control cases (one stored session, zero active) and both wave
boundaries are included in the raw samples and the full report appendix.

Timing run: [41099572836](https://github.com/arut-land/a/actions/runs/41099572836).
Census run: [41099575124](https://github.com/arut-land/a/actions/runs/41099575124).
Both use source c8aa2e48, reference 455a92e7 and harness 22c763aa.
The later harness adds rejection of identical paired source images;
every accepted cohort here has distinct executable hashes.

Final-code Linux x64 check, with trimming removed (charge in KiB; other metrics in ms):

| Cohort / metric | A | A2 | Paired A2 − A |
| --- | --- | --- | --- |
| final-ubuntu-x64 / charge | 16702.000 [16231.000, 16780.000] | 16670.000 [16332.000, 16832.000] | 125.000 [-388.000, 439.000] |
| final-ubuntu-x64 / reopen | 18.089 [17.384, 18.801] | 17.868 [17.470, 18.407] | -0.289 [-0.933, 1.023] |
| final-ubuntu-x64 / wave 2 attach | 12.661 [12.363, 13.279] | 12.749 [12.456, 12.864] | 0.202 [-0.541, 0.247] |
| final-ubuntu-x64 / wave 2 turn batch | 63.453 [60.045, 73.515] | 65.619 [63.368, 110.512] | 3.323 [-1.212, 44.189] |
| final-ubuntu-x64 / wave 2 detach CPU | 30.000 [30.000, 40.000] | 30.000 [20.000, 40.000] | -0.000 [-10.000, 10.000] |
| final-ubuntu-x64 / wave 2 quiet idle CPU | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] | 0.000 [0.000, 0.000] |

This is a separate interleaved comparison against main, not a subtraction
between different hosted machines. Source and harness: 966c2123;
reference: 455a92e7; run [41104221513](https://github.com/arut-land/a/actions/runs/41104221513).

## Interpretation

The experimental build reduces second-wave charge on both Linux
architectures and the extra Intel macOS lane. Windows and the M3 do not
show a consistent reduction. Hosted Apple Silicon improves in the separate
instrumented cohort, but that benefit is not consistent across its
uninstrumented cohort and the local M3. No cohorts are pooled.

Linux census allocated bytes stay roughly level while free capacity and
charge decrease. Windows x64 free capacity decreases without a reliable
charge reduction. macOS zone capacity can stay level while native charge
changes; reserved capacity is not physical charge. These results do not
support treating allocator counters as a partition of process charge.

The combined experimental build also slows Linux x64 reopening in every
pair: +2.345 [1.996, 3.318] ms. The final-code comparison without trimming
does not reproduce a consistent reattach cost: -0.289 [-0.933, 1.023] ms.
Each comparison is interleaved against its own same-runner control;
subtracting results across hosted machines would not isolate a syscall.
Quiet idle CPU is zero in both arms of every accepted uninstrumented
second-wave sample. The cross-platform benefit and latency conditions
are not met, so the global guard was removed.

There are 220 accepted lifecycle samples: 50 local M3, 60 hosted timing
(including the extra Intel lane), 100 hosted census, and 10 final-code
Linux samples. Hosted timing focuses on 32:4; census adds the 1:0 control.
Local timing also includes 32:1. Each population has five alternating
pairs. The required matrix is Ubuntu x64/ARM64, Windows x64/ARM64, hosted
macOS and the local M3. Intel macOS timing is additional evidence; its
partial census was cancelled after the required matrix completed and
contributes no claim. The census workflow therefore ends cancelled
despite complete required cohorts.

## Component comparisons

M3, release z/fat-LTO, one codegen unit, two build jobs; both local arms use
the toolchain std (the hosted residency harness rebuilds std). Builds finish
before timing. Frame rows use eleven alternating process pairs; other rows
use five. Values are milliseconds, median [min, max].

Frame and prefix controls build the actual main versions of the two owned
production files with identical benchmark sources, then restore the candidate
files automatically. Engine controls use the saved full-main and candidate
release test binaries. The native framing feature is enabled. No network model
is contacted. Raw sources, binary hashes, commands and samples are retained.

| Workload | Main | Candidate | Paired candidate − main |
| --- | ---: | ---: | ---: |
| frame encode: 1,000,000 × 32 B | 217.280 [210.441, 229.360] | 212.973 [209.533, 221.926] | -2.859 [-14.194, 6.823] |
| frame decode: 1,000,000 × 32 B | 421.850 [419.418, 427.909] | 425.347 [421.825, 439.831] | 2.370 [-4.326, 19.197] |
| frame encode: 8,192 × 64 KiB | 159.358 [157.979, 165.697] | 159.060 [155.361, 160.888] | -0.388 [-5.811, 2.170] |
| frame decode: 8,192 × 64 KiB | 69.507 [65.459, 73.445] | 69.657 [65.616, 70.684] | -0.457 [-3.923, 4.399] |
| frame encode: 128 × 5 MiB | 211.426 [208.451, 214.617] | 213.535 [208.018, 214.881] | 1.884 [-6.530, 5.838] |
| frame decode: 128 × 5 MiB | 94.745 [90.097, 100.322] | 94.661 [89.001, 99.556] | -0.268 [-10.955, 4.565] |
| transfer 1,000 unique prefixes | 0.147 [0.135, 0.345] | 0.102 [0.097, 0.126] | -0.045 [-0.219, -0.036] |
| recorded actor turn, Step barrier | 10.000 [9.009, 11.724] | 10.994 [9.044, 13.803] | 0.034 [-0.696, 3.803] |
| recorded actor turn, Effects barrier | 10.167 [9.028, 11.998] | 11.028 [8.991, 15.953] | -0.027 [-1.175, 3.955] |
| load 15,600-entry log, whole | 62.328 [53.057, 63.114] | 57.800 [55.159, 64.494] | -1.763 [-4.676, 2.166] |
| load 15,600-entry log, snapshot | 34.061 [32.611, 39.664] | 32.730 [30.696, 42.251] | -0.715 [-5.396, 9.640] |
| large JSON tool result, 200 turns | 12.006 [10.991, 25.845] | 11.033 [10.997, 25.246] | -0.000 [-0.973, 0.900] |

Boxed frames reduce requested live allocation by exactly 8 bytes per frame
at all three sizes; allocation counts are unchanged. This is allocator-requested
metadata, not a claim of 8 bytes less process charge. The longer frame cohort
has process-tree CPU of 1203.827 [1188.838, 1228.804] ms versus 1204.082
[1194.985, 1215.005] ms, paired change 5.586 [−24.583, 17.815] ms. Its spreads
do not establish a consistent CPU or latency cost. Prefix conversion is faster
in every pair, with 1,000 allocations in both arms.

The existing long-transcript benchmark also runs all four encoders, whole and
incremental, in five alternating pairs. Its existing helper reports the fastest
of twenty inner operations; full per-encoder values and allocation counts are
in `component-summary.json` in the evidence directory. These component results
do not establish the reclamation experiment’s charged-memory benefit.

After removing the allocator guard and preserving actor-before-mailbox field
destruction, the final actor implementation was rebuilt and measured in five
fresh alternating pairs against the same saved control (500 turns per barrier):

| Final actor metric | Control | Final implementation | Paired change |
| --- | --- | --- | --- |
| Step, ms/turn | 10.020 [9.418, 10.867] | 10.566 [10.021, 11.014] | 0.302 [-0.798, 1.362] |
| Effects, ms/turn | 10.021 [9.732, 11.122] | 10.871 [10.067, 10.905] | 0.234 [-1.055, 1.139] |
| Process-tree CPU, ms | 6242.689 [5637.582, 6790.346] | 6333.654 [5958.076, 6757.268] | -176.427 [-516.288, 1119.686] |

The paired spreads include both improvements and regressions; these noisy
samples do not prove zero cost. They do not establish a consistent regression.
Raw final-code samples and executable hashes are in `final-turns/`.

## Residency experiment method

Each pair uses separate fixture daemons, isolated homes and the real echo agent;
four turns carry 64 KiB each. The probe checks paged replay against the durable
log, holds a slow follower through detach, waits for truthful actor-exit
hibernation, repeats the wave, and reopens the sessions. Five outer rounds
alternate A/A2 order. A is main; A2 contains the ownership work and the
experimental allocator guard. Final code removes the guard rather than shipping
an unsupported optimization.

Hosted timing and census use the shipped release recipe, including rebuilt std,
fat LTO and one codegen unit. Local M3 timing uses toolchain std; its separate
census uses rebuilt std. Results from these recipes are not pooled. Census
instrumentation runs separately from timing and is identical in both arms.
Executable hashes, native identities, source revisions, instrumentation patches,
hardware, raw samples and analysis scripts are retained in the evidence folder.

Charged memory is the existing platform probe's native process charge, not RSS.
Allocator allocated bytes count requested/in-use blocks, including allocator
rounding and runtime storage; they are not a precise application-live census.
Free capacity is an allocator statistic, not resident or charged memory. On
macOS all-zone allocated size includes reservations; on Windows HeapWalk's busy
and free totals do not partition process private commit. These counters must not
be subtracted from native process charge to claim a physical-memory identity.

Detach CPU includes slow-follower drain, actor shutdown and worker retirement;
it does not isolate the relief syscall. Reopen latency includes paged replay.
Turn latency is the slowest active session's four-turn batch. Quiet idle CPU is
measured over the probe's one-second hibernated interval. All values below are
median [min, max]; paired deltas are computed within each round, not by
subtracting the two medians.

## Excluded diagnostic attempts

Runs `41081536470` and `41093491911` used abbreviated checkout refs and produced
no usable cohort. The latter was cancelled. Windows attempts `41082781831`,
`41084736170`, `41084738867` selected Git Bash's link.exe; native PowerShell fixed
that. Intel macOS initially lacked a locked mr-boxington binary; native Cargo
fixed that. Windows attempts `41087567556`, `41087570230` and the Windows lanes
of `41093549095` failed writing generated Rust through cp1252; explicit UTF-8
fixed that. These failures produced no timing evidence.
Final-code ablation dispatch `41104196668` contained a mistyped full revision;
it was cancelled and replaced by `41104221513` before any probe evidence.

Artifact validation then found identical A/A2 executable hashes in the hosted
cohorts from `41082781831`, `41084741857`, `41093549095` and the four Windows runs
`41094503799`, `41094508892`, `41094515401`, `41094520893`. Cargo reused the other
checkout's release image in the shared target directory. Those samples are
excluded from every A/B claim. The corrected harness stages the reference, then
cleans that single target's release cache before compiling the candidate. The
final harness also rejects identical paired source images (explicit binary
controls remain available for intentional A/A work). The M3 controls used source
replacement with fresh mtimes and have distinct hashes. Remaining invalid
in-flight jobs were cancelled after this discovery.

## Final local verification

On the final production code, after merging main's tests:

- `cargo nextest run -p sys -p engine -p protocol -p llm --features sys/fixtures`:
  1,055 passed, 33 skipped.
- `cargo nextest run --workspace -E 'package(a)' --no-fail-fast`:
  147 passed, 10 skipped. The first attempt lacked the pinned Playwright module;
  the repository's browser setup resolved it before the complete rerun.
- Native clippy for all targets of those four crates, with sys fixtures and
  warnings denied; Cargo formatting check; `mise run check:windows`: passed.
- The real macOS census opt-in daemon test passed. Python syntax passed; the
  two optional Python integration tests skip without explicit binary/probe paths.

Every Cargo command used `mise exec --`, two jobs and the assigned external
target directory. No provider credentials, real model requests, user home data,
allocator tunables, dependencies or recorded fixtures were changed. No full CI
workflow was dispatched. Phase 2 remains owner-controlled; the target directory
is retained until that phase's prescribed cleanup.

## Disposition and remaining owner-controlled work

Drop Part A; keep Part B. A global hibernation trim needs a repeatable benefit
on every required platform as well as acceptable timing. The current M3 cohort
alone does not meet that condition. No platform-specific fallback or allocator
tuning is introduced to rescue the experiment.

No speculative hardening was added or awaits approval. Sites left unchanged are
listed with their reasons in the audit. No additional non-test file outside the
assigned ownership was needed. The independent review and any fixes, the final
main merge, one full CI dispatch and its Linux/Windows results, and target cleanup
belong to the separately authorized phase 2.

Process note: merge `d28b3f61` retained Git's generated merge message rather than
the lower-case area convention. It was not rewritten, in accordance with the
history rule; the subsequent merge uses the required lower-case message.
