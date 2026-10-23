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
sequence with a bounded close loop when close_range is unavailable. The range
path never inspects procfs; an old-kernel launch without a trustworthy descriptor
inventory fails closed. Child directories and executable paths resolve to
absolute paths before spawning.

Admission covers queued futures as well as running processes, with 32 slots.
Each captured process has one 64 KiB chunk in flight and a hard 64 MiB spill
limit, including background jobs. Spill writes apply storage back-pressure;
following files and enumerating groups run on admitted storage workers.
Observation stops at 4096 members. Rendered head/tail, partial lines and terminal
metadata have independent memory bounds. After owned-scope cleanup, capture
finishes accepted disk writes and drains at most 1 MiB with a 100 ms pipe-drain
deadline. Escaped pipe holders cannot retain completion or admission.

Unix groups cover cooperative members; descendants can leave them. A daemon
crash loses Unix leases, so survivors are not killed from their recorded IDs.
macOS and kernels without pidfds refuse numeric-PID daemon force-stop; RPC
shutdown remains available. These are ownership limits, not shell confinement.

## Validation

[CI](https://github.com/arut-land/a/actions/runs/39943730570), [native musl probe](https://github.com/arut-land/a/actions/runs/39943739759), [six-target performance run](https://github.com/arut-land/a/actions/runs/39943735276).

Regressions cover lease retention through cleanup, foreign-reaper revocation,
nonblocking pidfd readiness without changing blocking wait semantics, descriptor
inheritance, verified job membership, Unicode environments and directories,
embedded-NUL rejection, SSH terminal groups, replay isolation, queued admission,
completion releasing capacity, escaped pipe holders during timeout and
cancellation, relative cwd/PATH combinations, procfs-free spawning, background
spill caps and bounded unterminated terminal metadata. Native process tests run outside Miri; ownership checks that
Miri can model remain enabled. Old-kernel behavior is exercised through forced
fallbacks; the hosted kernels themselves are modern.

The escaped-holder and relative-path regressions failed before their fixes.
The [procfs-free musl chroot before the fix](https://github.com/arut-land/a/actions/runs/39943446868)
failed to launch; the native musl run linked above passes the same chroot case.

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
| Linux x86_64 | 4072.531 → 4108.906 | +36.375 | 6.97 → 55.46 | 7300 → 7356 | 0.00 → 0.00 |
| Linux aarch64 | 3510.039 → 3541.789 | +31.750 | 6.68 → 6.64 | 6208 → 6308 | 0.00 → 0.00 |
| macOS arm64 | 3170.281 → 3187.078 | +16.797 | 26.58 → 25.64 | 8608 → 8624 | 0.00 → 0.00 |
| macOS x86_64 | 3788.109 → 3816.695 | +28.586 | 44.77 → 50.53 | 4004 → 4032 | 0.00 → 0.00 |
| Windows x86_64 | 5267.000 → 5285.000 | +18.000 | 22.75 → 20.26 | 8356 → 8328 | 0.00 → 0.00 |
| Windows ARM64 | 4625.000 → 4651.500 | +26.500 | 69.00 → 84.45 | 9660 → 9644 | 0.00 → 0.00 |

Measured runtime source: `394a546aa4e0f9db9342d4065377e3b7d406a4cc`. Raw observations and host/toolchain details
are retained in the performance run artifacts. The baseline-to-current table
includes the endpoint ownership merge from `origin/main`.

## Capture RSS

Each text job writes 8 MiB; eight simultaneous jobs retain 64 MiB in spill files.
The metadata case writes 8 MiB inside an OSC sequence. Daemon and process-tree
RSS are sampled every 5 ms while the workload runs; short spikes may be missed.
Tree RSS includes user tools; kernel pipe memory is outside RSS. Post-burst RSS
is sampled after fifteen seconds, with the resulting sessions retained and no
allocator trimming. Each case uses a fresh isolated `A_HOME` and stops its daemon.

| Target | Daemon peak KiB, 1 text job | Daemon peak KiB, 8 text jobs | Tree peak KiB, 8 text jobs | Daemon peak KiB, metadata | Daemon post-burst KiB, 8 text jobs |
| --- | ---: | ---: | ---: | ---: | ---: |
| Linux x86_64 | 8108 | 11880 | 72616 | 7812 | 11868 |
| Linux aarch64 | 6968 | 10660 | 63256 | 6680 | 10640 |
| macOS arm64 | 10112 | 14160 | 49984 | 9392 | 14352 |
| macOS x86_64 | 5048 | 8760 | 26204 | 4784 | 8144 |
| Windows x86_64 | 9972 | 14396 | 295852 | 9628 | 12304 |
| Windows ARM64 | 11180 | 15576 | 517216 | 10872 | 12112 |

The code and invariants establish admission and retention limits; sampled RSS
characterizes this workload rather than proving a maximum for arbitrary child
programs. Old-kernel paths were forced-fallback tested on modern hosted kernels.

## Review-fix release comparison

Before: `4a6005db23fbfcc4626b20c55318bd5d7e8fb41d`
([run](https://github.com/arut-land/a/actions/runs/39936472393)).
After: the measured runtime source above. These are separate hosted runs.

| Target | Δ shipped KiB | Δ cold reply ms | Δ idle RSS KiB |
| --- | ---: | ---: | ---: |
| Linux x86_64 | +3.500 | +40.50 | +40 |
| Linux aarch64 | +2.750 | -2.30 | +12 |
| macOS arm64 | +0.000 | -2.49 | +16 |
| macOS x86_64 | +4.008 | +10.18 | -24 |
| Windows x86_64 | +4.500 | +2.42 | -12 |
| Windows ARM64 | +4.000 | +7.26 | +4 |

Linux x86_64 cold replies ranged from 6.60 to 82.41 ms, versus
6.27 to 44.18 ms before the fixes. The increased median above
is an observation; these unpaired, variable runs do not establish its cause.
