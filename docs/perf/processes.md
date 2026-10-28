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
Each captured process has one 64 KiB chunk in flight and a hard 64 MiB output
limit, including background jobs. Stream writes apply storage back-pressure;
following streams and enumerating groups run on admitted storage workers.
Each observation registers at most 64 member exit watches; the next exit starts
a fresh observation of the remaining group. Membership snapshots have no fixed
count limit, so group size cannot revoke containment. Rendered head/tail, partial
lines and terminal metadata have independent memory bounds. After owned-scope cleanup, capture
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
output caps and bounded unterminated terminal metadata. Native process tests run outside Miri; ownership checks that
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

Each text job writes 8 MiB; eight simultaneous jobs retain 64 MiB in blob streams.
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


## Scoped helper lifetime (round 3)

`Processes::launcher` retains runtime facilities without a call context. Session
launches still apply their spawn hooks; shared launches receive the trusted
declaration's already admitted policy, cwd and exact environment. The declaration
owner supplies the exact identity (including provenance, launch policy and principal
binding) paired with `Scope::{User, Workspace, Session}`. Workspace keys use the
engine's workspace path; this adds no workspace executable configuration reader.

`Scoped<K, T>` reserves weak entries before opening a resource. Its actor advances
from Dormant through Starting to Ready, then Retiring. Concurrent demand joins
startup; a retiring generation finishes native cleanup before a successor starts.
A crash retires that generation; only another explicit acquire may restart it.
Resources implement `ScopedResource`; `OwnedProcess` uses the existing native
process-group supervisor for both exit observation and awaited cleanup.

Request leases and `SessionLifetimes` retain demand. Actors never retain demand or
a session context. Last release cancels immediately without an idle timer. Session
hibernation closes its lease admission and releases its demand; configuration
retirement releases that generation's demand. Shared helpers neither count as
session work nor hold an idle daemon or upgrade open. Daemon shutdown closes
lifetime admission, cancels all actors and awaits their cleanup under the existing
shutdown deadline before runtime shutdown. Replay does not acquire resources.

The later MCP instance integration must keep one registry across its participating
sessions, provide exact trusted identities and deterministic launch inputs, and
retain only activated scope leases. Its shared cancellation drops the individual
request's demand while draining that exchange to its existing deadline; explicit
`ScopeLease::retire` supplies whole-instance retirement at that deadline or for
session-scope cancellation. JSON-RPC, discovery, credentials and request drains are
outside this generic lifetime change. No dependency, wire or log format changed.

### Native release comparison

Apple M3, 16 GiB, macOS 27.0.1 arm64, nightly-2026-10-17. Baseline production
source: `1af5daf3818c9d0f65bda36c70f0eb25285ee743`. Both executables are release
engine test hosts, using the repository's z/fat-LTO/one-codegen-unit profile and
ordinary installed std. The baseline adds only probe/test-context code. These are
measurements of the generic process seam, not an end-to-end MCP or shipped-size
comparison.

Each observation loads a real engine daemon in a fresh temporary home, attaches
1, 32 or 128 retained sessions in one workspace, and acquires the same generic
fixture sequentially through the native process provider. The fixture writes one
readiness byte, then blocks on stdin. Baseline starts one child per session;
candidate acquires one workspace instance and retains session and request leases.
The first acquire includes spawn, pipe registration and fixture readiness. Later
sessions join that already-ready instance; this does not measure an MCP exchange.

The original finding was reproduced before implementation in five baseline rounds.
The comparison then uses five alternating AB/BA rounds at each session count, each
side in a fresh home with the same fixture binary. Samples settle one second before
native memory snapshots. Builds were stopped during sampling, but the M3 is shared;
latency is observational. Values are median [min–max] across the five observations;
latency summarizes each observation's median session-first-acquire time.

| Sessions | Version | Children | Summed child footprint MiB | Parent footprint MiB | Session first acquire ms |
| ---: | --- | ---: | ---: | ---: | ---: |
| 1 | before | 1 | 1.172 [1.172–1.172] | 2.922 [2.907–2.922] | 2.188 [2.029–8.128] |
| 1 | after | 1 | 1.172 [1.172–1.172] | 2.891 [2.860–3.000] | 2.218 [2.088–2.372] |
| 32 | before | 32 | 37.497 [37.450–37.512] | 4.157 [4.125–4.266] | 1.693 [1.639–1.755] |
| 32 | after | 1 | 1.172 [1.172–1.172] | 4.110 [4.079–4.157] | 0.001167 [0.001125–0.001188] |
| 128 | before | 128 | 149.956 [149.940–150.034] | 6.750 [6.719–6.829] | 4.383 [4.363–4.393] |
| 128 | after | 1 | 1.172 [1.172–1.172] | 6.266 [6.188–6.313] | 0.001208 [0.001166–0.001291] |

At 128 sessions the first-ever cold acquire is 3.991 [3.389–6.146] ms before and
4.057 [3.924–4.222] ms after. Sharing removes duplicate launches rather than making
a first process free. Full per-acquire spreads at 128 are 2.776–10.687 ms before
and 0.001041–4.222 ms after; the latter includes the cold acquire in every run.
Summed child RSS is 221.969 [221.938–222.000] MiB before and 1.734 MiB in every
after observation. Footprint uses native `proc_pid_rusage`, RSS uses psutil, and
sums are per-process OS charges rather than total machine RAM. Parent values
include the test harness and retained sessions; they do not isolate registry
allocation cost. User scope across workspaces and workspace/session isolation are
covered functionally, not by a separate latency or memory experiment here.

Five additional 128-session retirement observations each settle for twelve seconds,
then sample ten seconds of native parent CPU. All five have zero counter increment
and zero remaining children. This is no observed idle CPU at OS resolution, not a
claim about arbitrary server CPU or an exhaustive proof of zero wakes.

The probe is `tests::scoped_processes::process_scope_measurement` in the engine
unit-test executable. Build it with `CARGO_BUILD_JOBS=2`, an external
`CARGO_TARGET_DIR`, and `mise exec -- cargo test --release -p engine --lib --no-run
--message-format=json`. Set `SCOPED_FIXTURE` to the generic readiness-byte fixture
and `SCOPED_COUNT` to the session count; run that executable with `--exact
tests::scoped_processes::process_scope_measurement --ignored --nocapture`.
It prints a `SCOPED` JSON record and waits on stdin while sampling occurs. A newline
releases all leases and awaits native cleanup. `SCOPED_IDLE=1` additionally prints
`RETIRED` and waits for another newline before shutdown, allowing post-retirement
native CPU measurements. The external runner asserts every fixture PID is gone.

Raw baseline, AB/BA, idle, build and mutation evidence and the standalone fixture
source are retained outside the repository under the task scratchpad's `r3/` as
`scoped-{baseline,paired,idle}.jsonl`, `scoped-summary.json`, `scoped-host.json`,
`scoped-measure.py`, `scoped-idle.py` and `scoped-fixture.rs`. Native Linux and
Windows runtime/performance results remain unmeasured in phase 1; the Windows
cross-target workspace clippy check passed. Branch CI awaits independent review.
