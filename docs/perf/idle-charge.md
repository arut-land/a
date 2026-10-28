# Idle daemon charged memory

The gate is strictly below **10 MiB (10,485,760 bytes)**. RSS remains a
diagnostic, with no assertion. The owner changed the accounting policy after
the round-3 investigation found that clean resident code pages could cross
the old RSS threshold while the retained allocation set stayed flat.

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
allocation or runtime policy changed. Hosted results follow when the four
native lanes complete.
