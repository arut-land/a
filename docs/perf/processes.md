# Process ownership and capture

The process service distinguishes a live `ProcessLease` from a diagnostic
`RecordedIdentity`. Recovery settles uncertain unsafe work as interrupted and
preserves its committed output; recorded numbers never authorize signaling.

Linux retains pidfds for signaling, observation and final `waitid(P_PIDFD)`
reaping. `WNOWAIT` keeps the leader's identity pinned through group cleanup.
Without pidfds, one event-driven waiter per owned child leaves reaping to the
lease; it is joined before admission is released. Unsupported descendant
watches revoke the remaining cooperative group when its leader exits.

macOS uses `posix_spawn` file actions, default-close inheritance, signal
attributes and session/group policy, with kqueue `NOTE_EXIT` observation.
Windows creates suspended processes with explicit handle and job lists,
verifies job membership before resuming the primary thread, and retains
kill-on-close jobs. Exit watches are checked against the live scope.

Spawn environments select `Exact` or `InheritSnapshot`. Windows preserves
UTF-16 names and values and uses native ordinal name comparison. Tools have
explicit stdio policy; SSH keeps terminal access and visible stderr. Linux
uses glibc closefrom/close_range actions, or a fixed async-signal-safe musl
sequence with a bounded close loop when close_range is unavailable.

Admission covers queued futures as well as running processes, with 32 slots.
Each captured process has one 64 KiB chunk in flight and a hard 64 MiB spill
limit, including background jobs. Spill writes apply storage back-pressure;
following files and enumerating groups run on admitted storage workers.
Observation stops at 4096 members. Rendered head/tail, partial lines and terminal
metadata have independent memory bounds.

Unix groups cover cooperative members; descendants can leave them. A daemon
crash loses Unix leases, so survivors are not killed from their recorded IDs.
macOS and kernels without pidfds refuse numeric-PID daemon force-stop; RPC
shutdown remains available. These are ownership limits, not shell confinement.

## Validation

[CI](https://github.com/arut-land/a/actions/runs/39933755732), [native musl probe](https://github.com/arut-land/a/actions/runs/39927056319), [six-target performance run](https://github.com/arut-land/a/actions/runs/39927063613).

Regressions cover lease retention through cleanup, foreign-reaper revocation,
nonblocking pidfd readiness without changing blocking wait semantics, descriptor
inheritance, verified job membership, Unicode environments and directories,
embedded-NUL rejection, SSH terminal groups, replay isolation, queued admission,
completion releasing capacity, background spill caps and bounded unterminated
terminal metadata. Native process tests run outside Miri; ownership checks that
Miri can model remain enabled. Old-kernel behavior is exercised through forced
fallbacks; the hosted kernels themselves are modern.

No crate was added. The Windows bindings enable `Win32_Globalization` for native
name comparison. Thin adapters use the existing rustix/libc/windows-sys APIs;
musl's missing closefrom spawn action requires the fixed native syscall plan.

## Release observations

Baseline: [39914920583](https://github.com/arut-land/a/actions/runs/39914920583).
Sizes are the shipped build without fixtures. Runtime workloads use the
fixture build to avoid provider/network variation. Values below are medians
from separate hosted runs, not paired causal estimates. Idle CPU is accumulated
over ten seconds after fifteen seconds settling, in five fresh homes; zero
means no counter increment at OS resolution.

| Target | Shipped KiB, before → after | Δ KiB | Cold reply ms, before → after | Idle RSS KiB, before → after | Idle daemon CPU ms, before → after |
| --- | ---: | ---: | ---: | ---: | ---: |
| Linux x86_64 | 4072.531 → 4100.156 | +27.625 | 6.97 → 5.36 | 7300 → 7260 | 0.00 → 0.00 |
| Linux aarch64 | 3510.039 → 3533.664 | +23.625 | 6.68 → 6.13 | 6208 → 6296 | 0.00 → 0.00 |
| macOS arm64 | 3170.281 → 3186.938 | +16.656 | 26.58 → 19.82 | 8608 → 8672 | 0.00 → 0.00 |
| macOS x86_64 | 3788.109 → 3812.570 | +24.461 | 44.77 → 50.79 | 4004 → 4060 | 0.00 → 0.00 |
| Windows x86_64 | 5267.000 → 5272.500 | +5.500 | 22.75 → 20.55 | 8356 → 8324 | 0.00 → 0.00 |
| Windows ARM64 | 4625.000 → 4641.000 | +16.000 | 69.00 → 70.71 | 9660 → 9632 | 0.00 → 0.00 |

Measured runtime source: `82f6658325ed6dffaebd81fbcbe612b4bee36af1`. Raw observations and host/toolchain details
are retained in the performance run artifacts.

## Capture RSS

Each text job writes 8 MiB; eight simultaneous jobs retain 64 MiB in spill files.
The metadata case writes 8 MiB inside an OSC sequence. Daemon and process-tree
RSS are sampled every 5 ms while the workload runs; short spikes may be missed.
Tree RSS includes user tools; kernel pipe memory is outside RSS. Post-burst RSS
is sampled after fifteen seconds, with the resulting sessions retained and no
allocator trimming. Each case uses a fresh isolated `A_HOME` and stops its daemon.

| Target | Daemon peak KiB, 1 text job | Daemon peak KiB, 8 text jobs | Tree peak KiB, 8 text jobs | Daemon peak KiB, metadata | Daemon post-burst KiB, 8 text jobs |
| --- | ---: | ---: | ---: | ---: | ---: |
| Linux x86_64 | 7984 | 12016 | 72828 | 7852 | 12004 |
| Linux aarch64 | 6988 | 10604 | 63216 | 6692 | 10532 |
| macOS arm64 | 10000 | 14160 | 50016 | 9440 | 14224 |
| macOS x86_64 | 5160 | 8748 | 26544 | 4784 | 8088 |
| Windows x86_64 | 9944 | 14496 | 317736 | 9844 | 12616 |
| Windows ARM64 | 11160 | 15388 | 466476 | 10852 | 12172 |

The code and invariants establish admission and retention limits; sampled RSS
characterizes this workload rather than proving a maximum for arbitrary child
programs. No older-kernel machine or signed installed-product lane is implied by
these hosted observations.
