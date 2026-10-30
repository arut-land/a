# Residency waves

`scripts/perf/residency.py` measures ownership across complete daemon/session
lifecycles. This is measurement infrastructure, not a product memory fix.
The motivating Infinity survey identified a missing workload, not a reproduced
leak. There was no earlier version of this workload to measure before adding
it. Both interleaved arms below run the same release binary; A2 minus A is a
noise control, never a claimed optimization.

The default matrix varies **stored histories 1/8/32 with one attached session**,
then **attached sessions 1/4/8 at 32 stored histories**. A one-history,
no-attached-client control exercises the same timing boundaries. Duplicate
matrix points run once per arm. Five fresh-home rounds reverse both arm and
population order, giving five samples per arm per point. Every stored session
first completes four 64 KiB echo turns. Both waves add four turns per attached
session; all histories remain intact. These finite workload sizes impose no
product session, history or message limit.

Each sample records the settled catalog, completed wave while clients remain
attached, normal-client detach, the slow follower alone, acknowledged
hibernation, a second wave, and reopening the earlier sessions. Attached means
resident and observed; the used-stage turn has finished, rather than holding
CPU busy artificially. Inputs are sent to all attached sessions before replies
are consumed. The slow follower stops reading after initial synchronization,
then drains through the last exact reply and idle acknowledgement before
closing. Its entire accumulated entry tail must match the durable log exactly. A slow or disconnected follower is a failed sample, not successful
work with discarded history. This fixture covers finite reader lag, not an
indefinitely stalled consumer.

The public catalog must have exactly the requested population, idle activity,
and the expected resident/client counts. Hibernation requires every session
to report `hibernated` with zero clients, bounded by a failure deadline.
**The public summary also represents the internal stopping state as
hibernated.** Twelve additional seconds allow blocking workers to retire;
this is a settling interval, not proof that an internal weak owner expired.
The existing real-engine `hibernating_frees_the_session` invariant separately
checks that owner. Successful reattachment also crosses the prior actor-exit
barrier, verified by its existing engine invariant. A stronger per-sample actor-exit acknowledgement needs an
engine/protocol interface outside this branch's ownership. Do not infer a leak
or prove its absence from the public summary alone.

Every initial attach, second-wave attach and final reopen decodes with the
real protocol crate and compares the entire replay, including the synced
sequence, against the independently read durable log. Latency starts just
before Attach and ends at Synced, excluding connection/Hello, log comparison
and process startup. Summaries retain the slowest session per sample, then
report its median and full range over rounds. They are not tail percentiles.
The generated probe's invariant rejects incomplete or prematurely synced
replay as a sample. The Python integration invariant runs two real sessions
through both waves, verifies the slow follower pins only its session, and
checks complete history on reopen.

## Accounting and interpretation

Charge comes from the merged `sys::process_memory` reader: macOS
`phys_footprint`, Linux PSS, Windows private commit. Errors fail the sample;
RSS is diagnostic. An external observer samples native charge every 200 ms
through a separate probe process. Its cumulative sampled peak is a lower
bound, not a native lifetime maximum. Available OS lifetime RSS/footprint
peaks from `process.snapshot` are retained separately. The observer does not
attach to any session. The daemon's process CPU, context switches and thread
counts exclude the probes; observer and catalog requests finish before CPU windows. The windows immediately
after detach include lifecycle work; post-hibernation windows follow the
12-second retirement interval and are the settled idle observations. There are no periodic catalog requests during those windows.
Native memory-map diagnostics after the second hibernation aid attribution.

Catalog growth is expected: immutable headers and last summaries remain for
all stored sessions, and files retain complete histories. Compare the settled
`catalog` across stored populations at fixed attached count. Within each
fresh process, compare used-minus-catalog, first-hibernated-minus-catalog and
second-hibernated-minus-first-hibernated. The JSON summary reports these
paired lifecycle differences and separate A2-minus-A differences by round.
An increased charged footprint can include allocator retention, compressed
memory or native caches. It cannot by itself identify a retained actor,
projection, log cursor or client queue. Native map output likewise does not
provide a portable live-allocation census.

Context-graph owns inherited context representation; turn-residency stage B
owns client transcript paging. This workload uses the echo fixture and
protocol clients, so it neither measures inherited model contexts nor claims
TUI/web/FFI paging savings. Interpret their live-history evidence separately;
no files in either product lane are changed here. No product fix, allocator
trim, history cap, new dependency, log change or wire change is included.

## Reproduction

Use Python 3.11+ and psutil 7.2.2. Build outside the checkout, with two Cargo
jobs on a shared host. All builds and checks should finish before timing.
For example, with an absolute external target and output directory:

```sh
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR="$target" mise exec -- \
  python scripts/perf/residency.py --target "$target" --output "$output" --build-only
PYTHONDONTWRITEBYTECODE=1 CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR="$target" mise exec -- \
  python scripts/perf/residency.py --target "$target" --output "$samples" \
    --binary "$output/a-fixtures" --probe "$output/residency-probe"
```

Set `RESIDENCY_BINARY`, `RESIDENCY_PROBE` and `PYTHONPATH=scripts/perf`, then
run `python -m unittest residency.ResidencyTests` for the real lifecycle
invariant. Generated probe tests, fmt and clippy run during its build.
`cargo nextest run -p engine --lib -E 'test(hibernat)'` exercises the existing
private release invariants independently. All daemon homes are temporary;
the shared `lifecycle.py` helper stops their exact daemon with `A_HOME` and
`a daemon stop`. Its initial line client is closed before measured stages.
The isolated benchmark sets `hibernate_secs=1`, not a proposed product policy.
After measurements, `cargo clean --target-dir "$target"` and remove that
owned target directory.

The manual `perf` workflow input `residency=true` runs only this workload on
Linux x64/ARM64 glibc, macOS ARM64/x64 and Windows x64/ARM64. Each lane builds
natively, retains raw JSONL, summary distributions, failures and binary
identities, and cleans its target. Cross-compilation is not native evidence;
musl is not covered by these six runner labels. `paired_rounds` must be at
least five. A failed sample fails the job and is retained, never silently
removed from a distribution.

## Evidence

Measured 2026-10-29: **420 primary samples**, 60 on each of seven native
hosts/environments, five fresh-home pairs per matrix point. All six hosted
lanes passed [perf run 40783707912](https://github.com/arut-land/a/actions/runs/40783707912).
Every primary sample passed lifecycle, complete-replay and complete slow-tail
assertions; all native map captures succeeded. The two arms use the same
binary. There is no product before/after saving to claim.

All primary measurements pin harness/source **`b1d6f9c0d181`**, whose product
code is `f343ff37` / PROTO 37. During this long run, `main` advanced to
`855d6062` (MCP questions and lazy dispatch, PROTO 38). The branch merged it;
the resulting tree passed fresh release fixture/probe builds, the real
lifecycle invariant, all seven engine hibernation tests, probe clippy,
workspace Windows clippy and formatting. Those are integration checks;
the tables are **not performance measurements of that later merge**.

Values below are **median [minimum, maximum]**, pooling A and A2 for ten
observations per point (five pairs); separate five-sample arm distributions
and paired A2-minus-A differences remain in the JSON artifacts. No interval
is a significance test. Latency is the slowest Attach-to-Synced session in
that sample, with warm filesystem caches and connection/Hello excluded.

The final post-hibernation CPU windows measured **0 ms in every primary
sample**. Settled threads were 6 on hosted Linux and macOS ARM64, 7 on M3
and Intel macOS, 8–12 on Windows x64 and 9–12 on Windows ARM64. These are
observations of native runtime policies, not portable thread-count limits.

At fixed 32 stored histories, M3 used-wave charge grows with attached
population, but charge remains above catalog baseline after clients leave.
The independent heap census below shows that the multi-MiB difference is
not matched by live malloc allocations. Windows ARM64 private commit returns
near its catalog baseline (32/8: 2.881 MiB catalog, 2.951 MiB after wave two).
Linux shows positive charged growth between hibernations: 32/8 median
+1.127 MiB [0.316, 1.566] on x64 and +0.795 [0.121, 2.070] on ARM64.
That charge remains **unattributed to an execution owner**. The M3 heap
result cannot establish the cause on Linux or Windows.

No unexpected retained execution owner was identified. Catalog summaries
remain intentionally; every reported hibernation has zero attached clients.
Charged memory, allocator/VM retention, small live metadata and execution
ownership remain distinct conclusions. This is not a fresh-idle memory
budget and does not cap stored data to satisfy one.

### Peaks at the largest active population

Sampled charged peaks are lower bounds at 200 ms intervals. Native lifetime
peaks are cumulative process counters; unavailable fields are not zero.

| host, stored=32 / attached=8 | sampled charged peak MiB | native peak RSS MiB | native peak footprint MiB |
| --- | ---: | ---: | ---: |
| M3 | 13.571 [13.172, 15.266] | not exposed | 13.633 [13.235, 15.329] |
| residency-macos-15-intel | 9.520 [8.125, 10.266] | not exposed | 10.375 [8.609, 11.691] |
| residency-macos-latest | 11.368 [10.579, 12.220] | not exposed | 11.431 [10.642, 12.282] |
| residency-ubuntu-24.04-arm | 14.764 [14.046, 16.019] | 17.283 [16.578, 18.543] | not exposed |
| residency-ubuntu-latest | 16.683 [16.141, 17.991] | 20.080 [19.520, 21.375] | not exposed |
| residency-windows-11-arm | 11.812 [11.090, 12.562] | 22.072 [21.688, 22.328] | not exposed |
| residency-windows-latest | 9.809 [8.344, 10.348] | 19.547 [19.422, 19.848] | not exposed |

### M3

macOS-27.0.1-arm64-arm-64bit-Mach-O; Apple M3; source b1d6f9c0d181.

| stored / attached | catalog MiB | used wave 1 MiB | hibernated wave 1 MiB | hibernated wave 2 MiB | idle CPU ms / 1 s | idle threads | reopen ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 / 0 | 9.032 [8.579, 9.610] | 9.032 [8.579, 9.610] | 9.032 [8.579, 9.610] | 6.375 [6.079, 7.110] | 0.000 [0.000, 0.000] | 7.000 [7.000, 7.000] | 7.135 [3.647, 17.016] |
| 1 / 1 | 9.016 [8.501, 9.610] | 9.118 [8.594, 9.875] | 9.165 [8.454, 9.844] | 6.750 [5.501, 10.594] | 0.000 [0.000, 0.000] | 7.000 [7.000, 7.000] | 13.245 [6.549, 31.956] |
| 8 / 1 | 9.297 [8.860, 10.360] | 9.493 [8.891, 10.407] | 9.266 [8.657, 10.391] | 6.844 [4.797, 8.688] | 0.000 [0.000, 0.000] | 7.000 [7.000, 7.000] | 14.259 [6.610, 61.342] |
| 32 / 1 | 11.094 [10.391, 11.375] | 11.047 [9.250, 11.657] | 11.024 [9.219, 11.641] | 8.360 [6.532, 9.375] | 0.000 [0.000, 0.000] | 7.000 [7.000, 7.000] | 7.992 [6.440, 27.251] |
| 32 / 4 | 10.946 [10.438, 11.766] | 12.243 [11.110, 13.891] | 11.438 [10.032, 13.844] | 10.391 [7.860, 11.469] | 0.000 [0.000, 0.000] | 7.000 [7.000, 7.000] | 10.739 [5.912, 31.167] |
| 32 / 8 | 10.696 [10.360, 11.282] | 13.461 [13.110, 14.766] | 13.305 [12.032, 15.188] | 12.305 [9.844, 14.329] | 0.000 [0.000, 0.000] | 7.000 [7.000, 7.000] | 7.900 [6.161, 14.082] |

| stored / attached | wave 2 minus wave 1 hibernated charge MiB | A2 minus A final charge MiB | sampled charged peak MiB | diagnostic final RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| 1 / 0 | -2.578 [-3.406, -1.953] | 0.391 [-0.609, 0.922] | 9.094 [8.657, 9.688] | 14.828 [11.406, 17.344] |
| 1 / 1 | -2.219 [-3.562, 0.750] | -0.531 [-5.094, 2.234] | 9.243 [8.844, 10.641] | 15.008 [12.312, 18.344] |
| 8 / 1 | -2.820 [-4.516, 0.031] | -0.031 [-1.766, 2.109] | 9.586 [8.954, 10.454] | 15.758 [14.812, 18.141] |
| 32 / 1 | -2.477 [-4.375, -1.297] | 0.000 [-2.188, 1.391] | 11.336 [10.500, 11.688] | 17.297 [16.188, 18.359] |
| 32 / 4 | -1.703 [-3.859, 0.031] | 0.625 [-1.547, 2.766] | 12.344 [11.204, 13.938] | 18.828 [17.547, 22.016] |
| 32 / 8 | -1.422 [-2.984, 0.578] | 1.453 [-0.953, 3.531] | 13.571 [13.172, 15.266] | 21.391 [18.688, 22.266] |

Native lifetime peak fields (across completed samples, cumulative per process):
- peak_footprint_kib: 10.579 [8.719, 15.329] MiB (mixed populations; per-case raw distributions retained).


### residency-macos-15-intel

macOS-15.7.9-x86_64-i386-64bit-Mach-O; Intel(R) Core(TM) i7-8700B CPU @ 3.20GHz; source b1d6f9c0d181.

| stored / attached | catalog MiB | used wave 1 MiB | hibernated wave 1 MiB | hibernated wave 2 MiB | idle CPU ms / 1 s | idle threads | reopen ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 / 0 | 7.199 [7.133, 7.684] | 7.199 [7.133, 7.684] | 7.201 [7.133, 7.684] | 7.197 [7.129, 7.680] | 0.000 [0.000, 0.000] | 7.000 [7.000, 7.000] | 12.349 [8.788, 36.592] |
| 1 / 1 | 7.158 [5.668, 7.262] | 7.688 [6.090, 7.824] | 4.896 [4.793, 6.770] | 4.740 [4.699, 6.188] | 0.000 [0.000, 0.000] | 7.000 [7.000, 7.000] | 15.653 [14.651, 23.570] |
| 8 / 1 | 5.617 [4.688, 6.930] | 5.480 [4.773, 6.516] | 5.672 [4.812, 6.609] | 5.889 [5.426, 6.211] | 0.000 [0.000, 0.000] | 7.000 [7.000, 7.000] | 15.682 [12.907, 21.684] |
| 32 / 1 | 5.986 [5.660, 7.301] | 6.139 [5.797, 6.855] | 6.607 [5.832, 7.145] | 6.172 [5.617, 6.742] | 0.000 [0.000, 0.000] | 7.000 [7.000, 7.000] | 17.737 [13.084, 21.259] |
| 32 / 4 | 5.633 [5.125, 6.816] | 5.441 [5.293, 6.160] | 6.842 [6.660, 7.652] | 7.359 [7.004, 9.320] | 0.000 [0.000, 0.000] | 7.000 [7.000, 7.000] | 18.847 [15.024, 28.080] |
| 32 / 8 | 6.312 [5.664, 7.742] | 6.273 [5.980, 6.695] | 6.664 [6.246, 8.352] | 8.822 [6.762, 9.344] | 0.000 [0.000, 0.000] | 7.000 [7.000, 7.000] | 21.819 [17.207, 31.921] |

| stored / attached | wave 2 minus wave 1 hibernated charge MiB | A2 minus A final charge MiB | sampled charged peak MiB | diagnostic final RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| 1 / 0 | -0.004 [-0.004, 0.016] | 0.008 [-0.016, 0.551] | 7.594 [7.527, 7.766] | 11.236 [11.176, 11.719] |
| 1 / 1 | -0.102 [-2.004, 1.121] | -0.027 [-1.441, 1.105] | 7.729 [6.246, 7.922] | 8.830 [8.793, 10.281] |
| 8 / 1 | 0.135 [-0.781, 1.250] | 0.223 [0.102, 0.418] | 8.139 [6.793, 9.906] | 9.992 [9.527, 10.320] |
| 32 / 1 | -0.355 [-1.227, 0.473] | 0.125 [-0.566, 1.125] | 8.977 [7.230, 11.992] | 10.281 [9.734, 10.844] |
| 32 / 4 | 0.609 [-0.633, 2.633] | -0.496 [-2.277, 0.438] | 9.336 [8.305, 10.672] | 11.469 [11.105, 13.438] |
| 32 / 8 | 1.627 [-0.707, 2.820] | -0.465 [-2.133, 2.133] | 9.520 [8.125, 10.266] | 12.932 [10.875, 13.449] |

Native lifetime peak fields (across completed samples, cumulative per process):
- peak_footprint_kib: 9.092 [7.254, 12.496] MiB (mixed populations; per-case raw distributions retained).


### residency-macos-latest

macOS-26.6.2-arm64-arm-64bit-Mach-O; Apple M1 (Virtual); source b1d6f9c0d181.

| stored / attached | catalog MiB | used wave 1 MiB | hibernated wave 1 MiB | hibernated wave 2 MiB | idle CPU ms / 1 s | idle threads | reopen ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 / 0 | 7.899 [7.767, 8.079] | 7.892 [5.470, 8.063] | 7.892 [5.470, 8.063] | 5.649 [5.579, 6.095] | 0.000 [0.000, 0.000] | 6.000 [6.000, 6.000] | 8.823 [4.954, 38.029] |
| 1 / 1 | 8.071 [7.860, 8.438] | 8.259 [7.970, 8.563] | 8.251 [7.954, 8.532] | 6.259 [5.954, 6.563] | 0.000 [0.000, 0.000] | 6.000 [6.000, 6.000] | 10.825 [7.685, 26.513] |
| 8 / 1 | 8.235 [7.829, 8.735] | 8.298 [5.985, 8.892] | 8.282 [5.954, 8.860] | 6.352 [3.954, 6.845] | 0.000 [0.000, 0.000] | 6.000 [6.000, 6.000] | 10.408 [7.832, 18.277] |
| 32 / 1 | 9.720 [7.657, 9.829] | 9.868 [7.813, 10.001] | 9.837 [7.782, 9.970] | 7.845 [7.735, 9.985] | 0.000 [0.000, 0.000] | 6.000 [6.000, 6.000] | 9.953 [7.207, 21.145] |
| 32 / 4 | 9.759 [9.626, 10.423] | 9.899 [9.829, 10.610] | 9.845 [7.860, 9.985] | 7.860 [6.642, 9.985] | 0.000 [0.000, 0.000] | 6.000 [6.000, 6.000] | 9.977 [8.774, 13.891] |
| 32 / 8 | 9.759 [9.704, 10.579] | 10.642 [10.392, 11.173] | 11.329 [10.360, 11.642] | 10.853 [8.345, 12.188] | 0.000 [0.000, 0.000] | 6.000 [6.000, 6.000] | 11.992 [7.196, 15.430] |

| stored / attached | wave 2 minus wave 1 hibernated charge MiB | A2 minus A final charge MiB | sampled charged peak MiB | diagnostic final RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| 1 / 0 | -2.203 [-2.438, 0.109] | 0.016 [-0.438, 0.516] | 7.978 [7.845, 8.142] | 15.906 [14.000, 16.094] |
| 1 / 1 | -2.000 [-2.016, -1.969] | -0.281 [-0.328, 0.594] | 8.282 [7.985, 8.563] | 14.219 [13.922, 14.531] |
| 8 / 1 | -2.000 [-2.281, 0.016] | -0.203 [-2.547, 0.312] | 8.392 [7.954, 8.892] | 14.484 [14.156, 16.266] |
| 32 / 1 | -1.984 [-2.016, 0.031] | 0.016 [-0.125, 2.094] | 9.868 [9.782, 10.017] | 15.836 [15.703, 17.984] |
| 32 / 4 | -1.984 [-3.203, 0.047] | -0.047 [-1.187, 0.047] | 9.899 [9.829, 10.610] | 15.844 [15.750, 17.922] |
| 32 / 8 | 0.070 [-3.172, 0.641] | 0.531 [-1.250, 1.734] | 11.368 [10.579, 12.220] | 18.984 [18.016, 20.156] |

Native lifetime peak fields (across completed samples, cumulative per process):
- peak_footprint_kib: 9.399 [7.876, 12.282] MiB (mixed populations; per-case raw distributions retained).


### residency-ubuntu-24.04-arm

Linux-7.0.0-1017-azure-aarch64-with-glibc2.39; aarch64; source b1d6f9c0d181.

| stored / attached | catalog MiB | used wave 1 MiB | hibernated wave 1 MiB | hibernated wave 2 MiB | idle CPU ms / 1 s | idle threads | reopen ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 / 0 | 8.007 [7.378, 8.737] | 8.007 [7.378, 8.737] | 8.007 [7.378, 8.737] | 8.022 [7.394, 8.753] | 0.000 [0.000, 0.000] | 6.000 [6.000, 6.000] | 3.596 [3.346, 3.796] |
| 1 / 1 | 8.071 [7.218, 8.565] | 9.202 [8.894, 10.233] | 9.647 [9.015, 10.233] | 10.761 [10.397, 11.132] | 0.000 [0.000, 0.000] | 6.000 [6.000, 6.000] | 6.563 [6.385, 7.085] |
| 8 / 1 | 10.079 [9.511, 10.339] | 10.319 [9.729, 11.265] | 10.374 [10.046, 11.265] | 10.972 [10.632, 11.710] | 0.000 [0.000, 0.000] | 6.000 [6.000, 6.000] | 6.602 [6.337, 6.787] |
| 32 / 1 | 12.673 [12.437, 13.604] | 12.704 [12.499, 13.632] | 12.704 [12.503, 13.632] | 12.983 [12.585, 13.698] | 0.000 [0.000, 0.000] | 6.000 [6.000, 6.000] | 6.464 [6.314, 6.723] |
| 32 / 4 | 12.788 [12.214, 12.991] | 12.817 [12.503, 13.088] | 12.923 [12.503, 13.593] | 13.474 [13.144, 14.037] | 0.000 [0.000, 0.000] | 6.000 [6.000, 6.000] | 6.613 [6.407, 8.332] |
| 32 / 8 | 12.767 [12.253, 13.058] | 13.913 [13.076, 14.698] | 13.991 [13.146, 15.163] | 14.764 [14.046, 16.019] | 0.000 [0.000, 0.000] | 6.000 [6.000, 6.000] | 6.634 [6.463, 6.936] |

| stored / attached | wave 2 minus wave 1 hibernated charge MiB | A2 minus A final charge MiB | sampled charged peak MiB | diagnostic final RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| 1 / 0 | 0.016 [0.016, 0.016] | -0.027 [-0.680, 0.117] | 8.101 [7.651, 8.757] | 10.555 [9.926, 11.230] |
| 1 / 1 | 1.117 [0.879, 1.426] | -0.113 [-0.323, 0.430] | 10.765 [10.397, 11.132] | 13.297 [12.930, 13.664] |
| 8 / 1 | 0.543 [0.348, 1.004] | -0.031 [-0.301, 0.656] | 10.972 [10.632, 11.710] | 13.504 [13.164, 14.188] |
| 32 / 1 | 0.158 [0.004, 0.621] | -0.004 [-0.176, 0.586] | 12.983 [12.585, 13.698] | 15.512 [15.062, 16.230] |
| 32 / 4 | 0.604 [0.332, 0.941] | 0.332 [0.051, 0.597] | 13.474 [13.144, 14.037] | 16.000 [15.676, 16.555] |
| 32 / 8 | 0.795 [0.121, 2.070] | -0.871 [-1.973, 0.054] | 14.764 [14.046, 16.019] | 17.283 [16.578, 18.543] |

Native lifetime peak fields (across completed samples, cumulative per process):
- peak_rss_kib: 14.625 [11.535, 18.543] MiB (mixed populations; per-case raw distributions retained).


### residency-windows-latest

Windows-2025Server-10.0.26100-SP0; AMD64 Family 25 Model 17 Stepping 1, AuthenticAMD; source b1d6f9c0d181.

| stored / attached | catalog MiB | used wave 1 MiB | hibernated wave 1 MiB | hibernated wave 2 MiB | idle CPU ms / 1 s | idle threads | reopen ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 / 0 | 2.809 [2.742, 3.133] | 2.973 [2.742, 3.238] | 2.812 [2.742, 3.133] | 2.723 [2.688, 3.742] | 0.000 [0.000, 0.000] | 9.000 [8.000, 10.000] | 6.109 [5.248, 6.382] |
| 1 / 1 | 2.812 [2.746, 2.883] | 5.746 [5.598, 6.543] | 2.859 [2.793, 2.926] | 3.039 [2.891, 4.102] | 0.000 [0.000, 0.000] | 8.500 [8.000, 10.000] | 10.181 [9.143, 10.845] |
| 8 / 1 | 3.145 [3.086, 3.258] | 4.805 [4.215, 8.051] | 3.227 [3.121, 3.570] | 3.383 [3.246, 4.457] | 0.000 [0.000, 0.000] | 11.000 [11.000, 12.000] | 9.300 [8.939, 9.828] |
| 32 / 1 | 3.721 [3.289, 5.605] | 4.740 [4.477, 5.176] | 3.920 [3.492, 5.266] | 5.012 [3.789, 5.480] | 0.000 [0.000, 0.000] | 11.000 [11.000, 12.000] | 8.883 [8.777, 9.223] |
| 32 / 4 | 4.309 [3.484, 6.379] | 7.477 [5.605, 7.867] | 4.797 [3.469, 5.594] | 5.205 [3.875, 5.777] | 0.000 [0.000, 0.000] | 11.000 [10.000, 12.000] | 9.013 [8.618, 9.813] |
| 32 / 8 | 3.826 [3.270, 5.066] | 7.611 [6.785, 10.113] | 3.975 [3.566, 5.613] | 5.016 [4.156, 5.445] | 0.000 [0.000, 0.000] | 11.000 [11.000, 12.000] | 8.999 [8.899, 9.220] |

| stored / attached | wave 2 minus wave 1 hibernated charge MiB | A2 minus A final charge MiB | sampled charged peak MiB | diagnostic final RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| 1 / 0 | -0.088 [-0.121, 0.848] | 0.031 [-0.074, 1.055] | 3.418 [3.148, 3.938] | 11.393 [11.332, 11.637] |
| 1 / 1 | 0.176 [0.023, 1.309] | 0.062 [-1.031, 0.121] | 4.648 [3.656, 6.668] | 11.697 [11.598, 12.418] |
| 8 / 1 | 0.129 [-0.016, 0.887] | 0.020 [-0.344, 1.059] | 4.924 [4.285, 6.172] | 11.980 [11.863, 12.742] |
| 32 / 1 | 0.373 [-0.805, 1.469] | -0.398 [-1.293, 1.066] | 7.182 [6.789, 7.496] | 13.332 [12.445, 13.602] |
| 32 / 4 | 0.529 [-1.449, 1.770] | -0.129 [-0.988, 0.270] | 7.023 [6.742, 8.949] | 13.510 [12.527, 13.789] |
| 32 / 8 | 0.977 [-0.938, 1.742] | 0.109 [-0.234, 1.152] | 9.809 [8.344, 10.348] | 13.582 [12.707, 13.699] |

Native lifetime peak fields (across completed samples, cumulative per process):
- peak_rss_kib: 18.436 [15.398, 19.848] MiB (mixed populations; per-case raw distributions retained).


### residency-ubuntu-latest

Linux-7.0.0-1017-azure-x86_64-with-glibc2.43; AMD EPYC 9V74 80-Core Processor; source b1d6f9c0d181.

| stored / attached | catalog MiB | used wave 1 MiB | hibernated wave 1 MiB | hibernated wave 2 MiB | idle CPU ms / 1 s | idle threads | reopen ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 / 0 | 9.646 [9.007, 10.058] | 9.646 [9.007, 10.058] | 9.646 [9.007, 10.058] | 9.653 [9.015, 10.065] | 0.000 [0.000, 0.000] | 6.000 [6.000, 6.000] | 3.868 [3.518, 4.630] |
| 1 / 1 | 9.419 [8.961, 9.827] | 10.572 [10.296, 11.719] | 11.101 [10.714, 11.832] | 11.945 [11.085, 12.504] | 0.000 [0.000, 0.000] | 6.000 [6.000, 6.000] | 7.114 [6.722, 7.445] |
| 8 / 1 | 11.904 [11.560, 12.433] | 12.086 [11.815, 12.460] | 12.151 [11.958, 12.507] | 12.546 [12.401, 12.765] | 0.000 [0.000, 0.000] | 6.000 [6.000, 6.000] | 7.204 [6.913, 7.744] |
| 32 / 1 | 14.550 [13.879, 14.930] | 14.643 [13.918, 15.137] | 14.709 [13.906, 15.375] | 14.854 [14.059, 15.504] | 0.000 [0.000, 0.000] | 6.000 [6.000, 6.000] | 7.329 [7.032, 7.764] |
| 32 / 4 | 14.628 [14.185, 15.062] | 14.799 [14.231, 15.344] | 14.925 [14.530, 15.305] | 15.322 [14.976, 15.653] | 0.000 [0.000, 0.000] | 6.000 [6.000, 6.000] | 7.083 [6.946, 7.791] |
| 32 / 8 | 14.374 [13.874, 14.770] | 15.496 [14.651, 16.440] | 15.684 [15.038, 16.405] | 16.644 [16.102, 17.952] | 0.000 [0.000, 0.000] | 6.000 [6.000, 6.000] | 7.181 [6.938, 7.445] |

| stored / attached | wave 2 minus wave 1 hibernated charge MiB | A2 minus A final charge MiB | sampled charged peak MiB | diagnostic final RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| 1 / 0 | 0.008 [0.008, 0.012] | -0.211 [-0.977, 0.829] | 9.681 [9.265, 10.116] | 13.068 [12.383, 13.480] |
| 1 / 1 | 0.756 [0.273, 1.309] | -0.429 [-1.200, 0.289] | 12.148 [11.124, 12.523] | 15.326 [14.492, 15.891] |
| 8 / 1 | 0.391 [0.250, 0.660] | 0.001 [-0.215, 0.303] | 12.649 [12.421, 12.831] | 15.930 [15.770, 16.168] |
| 32 / 1 | 0.088 [0.000, 0.375] | -0.068 [-0.649, 0.228] | 14.874 [14.078, 15.523] | 18.219 [17.438, 18.918] |
| 32 / 4 | 0.506 [0.000, 0.742] | 0.142 [-0.335, 0.678] | 15.361 [15.015, 15.673] | 18.676 [18.359, 19.027] |
| 32 / 8 | 1.127 [0.316, 1.566] | -0.512 [-1.287, 0.370] | 16.683 [16.141, 17.991] | 20.041 [19.480, 21.336] |

Native lifetime peak fields (across completed samples, cumulative per process):
- peak_rss_kib: 16.828 [14.121, 21.375] MiB (mixed populations; per-case raw distributions retained).


### residency-windows-11-arm

Windows-11-10.0.26200-SP0; ARMv8 (64-bit) Family 8 Model D49 Revision   0, MICROSOFT CORPORATION; source b1d6f9c0d181.

| stored / attached | catalog MiB | used wave 1 MiB | hibernated wave 1 MiB | hibernated wave 2 MiB | idle CPU ms / 1 s | idle threads | reopen ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 / 0 | 2.584 [2.535, 2.609] | 2.715 [2.684, 2.750] | 2.584 [2.543, 2.609] | 2.496 [2.441, 2.531] | 0.000 [0.000, 0.000] | 11.000 [9.000, 11.000] | 7.822 [7.320, 10.027] |
| 1 / 1 | 2.561 [2.516, 2.594] | 3.982 [3.891, 6.242] | 2.580 [2.520, 2.617] | 2.588 [2.516, 2.617] | 0.000 [0.000, 0.000] | 11.000 [9.000, 11.000] | 11.466 [11.032, 12.512] |
| 8 / 1 | 2.688 [2.641, 2.957] | 4.297 [4.070, 6.633] | 2.688 [2.641, 2.961] | 2.684 [2.637, 2.961] | 0.000 [0.000, 0.000] | 11.000 [11.000, 11.000] | 14.550 [13.178, 15.656] |
| 32 / 1 | 2.871 [2.832, 2.922] | 4.643 [4.523, 4.895] | 2.875 [2.836, 2.926] | 2.869 [2.836, 2.922] | 0.000 [0.000, 0.000] | 11.000 [11.000, 12.000] | 11.899 [11.182, 15.399] |
| 32 / 4 | 2.879 [2.871, 2.941] | 8.611 [6.219, 8.965] | 2.889 [2.867, 2.941] | 2.896 [2.863, 3.027] | 0.000 [0.000, 0.000] | 11.000 [11.000, 12.000] | 13.973 [11.327, 15.323] |
| 32 / 8 | 2.881 [2.844, 2.938] | 8.699 [8.566, 10.859] | 2.908 [2.875, 2.980] | 2.951 [2.875, 3.219] | 0.000 [0.000, 0.000] | 11.500 [11.000, 12.000] | 13.796 [11.742, 17.495] |

| stored / attached | wave 2 minus wave 1 hibernated charge MiB | A2 minus A final charge MiB | sampled charged peak MiB | diagnostic final RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| 1 / 0 | -0.068 [-0.141, -0.055] | 0.023 [-0.047, 0.070] | 3.625 [3.328, 3.754] | 12.518 [10.789, 12.539] |
| 1 / 1 | 0.000 [-0.012, 0.016] | -0.043 [-0.074, 0.012] | 4.326 [3.898, 5.355] | 12.650 [12.164, 12.672] |
| 8 / 1 | -0.004 [-0.004, 0.004] | -0.020 [-0.324, 0.047] | 5.916 [5.035, 6.566] | 12.754 [12.246, 12.789] |
| 32 / 1 | -0.004 [-0.008, 0.012] | -0.020 [-0.086, 0.047] | 8.682 [7.438, 11.527] | 12.934 [12.473, 12.992] |
| 32 / 4 | 0.000 [-0.004, 0.125] | -0.008 [-0.145, 0.043] | 8.711 [8.223, 11.754] | 12.963 [12.930, 13.098] |
| 32 / 8 | 0.012 [-0.004, 0.309] | -0.039 [-0.293, 0.332] | 11.812 [11.090, 12.562] | 13.014 [12.941, 13.230] |

Native lifetime peak fields (across completed samples, cumulative per process):
- peak_rss_kib: 18.805 [17.043, 22.328] MiB (mixed populations; per-case raw distributions retained).

### M3 allocation attribution (separate from timing)

Five additional fresh-home alternating 1/8 attached pairs, fixed stored=32,
repeat the same lifecycle and inspect `heap -s <daemon-pid>` at catalog,
used, and both hibernation boundaries. All ten samples passed all lifecycle
and replay assertions. Native `All zones` allocated bytes are below; these
instrumented samples are excluded from the 420-sample timing cohort.

| previously attached, stored=32 | catalog live KiB | used wave 1 live KiB | hibernated wave 1 live KiB | hibernated wave 2 live KiB |
| --- | ---: | ---: | ---: | ---: |
| 1 | 561.422 [559.922, 567.203] | 1035.516 [1033.312, 1040.375] | 573.812 [572.094, 579.156] | 585.844 [584.344, 597.562] |
| 8 | 560.922 [560.297, 561.422] | 2746.062 [2740.875, 2747.062] | 586.438 [585.938, 586.938] | 598.562 [598.062, 599.062] |

The used eight-session heap falls from 2,746 KiB to 599 KiB after wave two,
close to the 561 KiB catalog census, while charged footprint can remain in
the multi-MiB range. A small tens-of-KiB live increase over catalog is not
attributed to a specific owner here. The census supports release of the
large malloc-backed execution state on this workload; it does not prove
that every VM mapping or native cache disappeared. No trim was requested.

### Remaining ownership questions

- The public hibernation summary also covers internal stopping. A successful
  reattach crosses the engine's prior-actor-exit barrier, tested separately,
  but the earlier memory sample has no direct per-actor-exit certificate.
  Exact per-sample attribution would require an engine event/counter seam
  outside this branch's ownership. No delayed/stuck teardown was observed;
  its likelihood is unquantified. Estimated cost is tens to low hundreds of
  lines, plus protocol/fixture work if exposed on the wire. Owner-pending.
- Linux PSS growth and Windows x64 private-commit growth are measured charge,
  not identified live session state. A native allocation/stack census is
  needed before proposing a product change; the M3 census cannot substitute.
  Expected cost is a profiled native run per affected architecture and
  profiler/harness wiring, with no shipped dependency required. Cause and
  likelihood of an execution leak remain unestablished. Owner-pending.
- Model-context inheritance, renderer paging, suspended external waits and
  musl runtime behavior are outside this echo/protocol-client experiment.
  Context-graph and stage B retain their product ownership.

### Validation and artifacts

The generated validator's missing-history mutation fails its invariant and
restored code passes. The real two-session lifecycle invariant passed on
both measured PROTO 37 and merged PROTO 38. All seven existing hibernation
invariants passed before and after the merge. Probe fmt/test/clippy with
`-D warnings`, native `cargo check -p protocol -p sys`, workspace
`mise run check:windows`, and `cargo fmt --all -- --check` passed.
The existing Python harness suite passed (50 tests, 12 skipped).

Hosted artifacts are `residency-<runner>` on the linked run, with raw JSONL,
full arm/paired distributions, native identities, source/toolchain/hardware
metadata and maps. The owner's phase-1 report retains the M3 raw cohort,
heap output/driver, all validation logs and binary hashes outside the repo.
An earlier M3 pilot and cancelled hosted run were excluded when complete
slow-tail validation was added; their evidence was retained. An initial
workflow dispatch failed syntax before measurements. A heap diagnostic
print-format error occurred after its first valid record; it was fixed and
the same record retained while the remaining pairs completed.

The single local Cargo target was cleaned (1.5 GiB removed) and its absence
verified. Hosted jobs clean their external targets as their final step.
Only the four owned paths differ from `origin/main`. No product source fix,
new dependency or branch-owned wire change was made. CI has not been
dispatched; phase 2 awaits independent review.


### Opt-in Linux and Windows allocator census

The `perf` workflow's `alloc_census=true` input selects an instrumented
residency cohort. With `runner=all`, it runs on native Ubuntu x64/ARM64 and
Windows x64/ARM64 runners. Five alternating A/A rounds remain the minimum;
the lifecycle, equal stored histories, slow follower assertions and reopen
checks are unchanged. This cohort must not be used for latency, idle CPU or
thread-count comparisons with the uninstrumented workload.

Only `a --features fixtures` includes `sys::census`. With the feature off,
the CLI initialization call and census module are compiled out. With fixtures
on, only the internal daemon entry point checks for an `alloc-census`
directory under an explicit `A_HOME`; the harness creates it inside its
isolated home. This survives the daemon startup environment allowlist. The hook publishes a PID
specific loopback port there and serves one native census per connection.
One extra diagnostic thread persists for the daemon's lifetime. The harness
warms the endpoint before seeding histories, samples every lifecycle boundary,
and stops the daemon through that home's ordinary stop command.

On glibc, `malloc_info(0, ...)` supplies raw XML for all arenas, with no
preload or replacement allocator. The report derives free capacity from the
root `fast` and `rest` totals, allocator capacity from the root current
system bytes plus `mmap` bytes, and allocated bytes by subtraction. Raw
`mallinfo2` allocated/free counters are also retained as a cross-check.
Allocator-accounted allocated bytes include metadata and per-thread cached
blocks; they are not an exact count of application-live payloads. Free
capacity can include pages already discarded by the allocator and therefore
is not a measurement of resident free pages.

On Windows, `GetProcessHeaps` enumerates process heaps and `HeapSummary`
records each heap's allocated, committed and reserved bytes. The report
sums allocated bytes and committed capacity, with their difference labeled
free capacity; that difference also includes heap overhead. These totals
do not include direct VM allocations outside the heaps. See Microsoft's
[HeapSummary contract](https://learn.microsoft.com/en-us/windows/win32/api/heapapi/nf-heapapi-heapsummary)
and [counter definitions](https://learn.microsoft.com/en-us/windows/win32/api/heapapi/ns-heapapi-heap_summary).

Every raw response carries the daemon PID and charged bytes immediately
before and after collecting allocator statistics. The external charge probe
continues to report PSS or private commit separately. Neither the native
heap collection nor the charge bracket freezes all daemon threads; only
stable lifecycle boundaries should be compared. Raw XML/per-heap records, per-stage VM maps,
all stage distributions and paired hibernation deltas remain in JSON/JSONL
artifacts. Summary tables give median [min, max] in KiB.

The platform guidance is to preserve allocator hysteresis and profile
before tuning. glibc's [trim and mmap thresholds](https://sourceware.org/glibc/manual/latest/html_node/Memory-Allocation-Tunables.html)
adapt when left at their defaults; setting thresholds explicitly disables
that adaptation. Microsoft's [LFH guidance](https://learn.microsoft.com/en-us/windows/win32/memory/low-fragmentation-heap)
says modern Windows enables the policy as needed and recommends profiling
to assess its benefit. This census changes no arena limit, threshold,
heap policy or reclamation schedule. Native findings and any proposed policy
experiment must be supported by the resulting cohort, not by the M3 result.
