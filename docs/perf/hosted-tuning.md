# Native release tuning

Release code uses level three, with serde_json helpers at level one, fat LTO,
one codegen unit, unwinding and offline crash symbolication. CPU baselines
remain the conservative native toolchain defaults. Size is reported without
a cap; decisions account for idle and peak RSS, CPU, latency and reliability.
The [workflow](hosted.md) preserves raw observations and p10–p90 spreads.

| Native target | Allocator | Native facilities | Release decision |
| --- | --- | --- | --- |
| Linux x86_64 GNU | glibc, two arenas | mold, safe ICF, RELR, epoll/inotify | Level 3 / JSON 1; lower CPU and attach p99 for 1184 KiB more idle RSS |
| Linux aarch64 GNU | glibc, two arenas | mold, safe ICF, RELR, epoll/inotify | Level 3 / JSON 1; lower CPU, cold reply and attach p99 for 1260 KiB more idle RSS |
| Windows x86_64 MSVC | system | MSVC, static CRT, native directory thread-pool I/O | Level 3 / JSON 1; lower JSON CPU for 208 KiB more idle RSS |
| Windows aarch64 MSVC | system | native ARM tools, MSVC, static CRT, native directory thread-pool I/O | Level 3 / JSON 1; lower attach p99 for 136 KiB more idle RSS |
| macOS arm64 | system | Apple linker, compact unwind, kqueue/native directory notifications | Level 3 / JSON 1; lower JSON and driver user CPU for 912 KiB more idle RSS |
| macOS x86_64 | system | Apple linker, compact unwind, kqueue/native directory notifications | Level 3 / JSON 1; lower JSON CPU for 188 KiB more idle RSS |

Linux GNU requires glibc 2.36 for RELR and native OpenSSL 3. Windows retains
an 8 MiB stack for nested decode paths. The system allocator remains native
on Apple and Microsoft platforms. No new instruction-set requirement or
custom compiler wrapper is introduced.

## CPU and memory tradeoffs

[The paired speed-profile run](https://github.com/arut-land/a/actions/runs/39887943724)
compares release level three against level z with the same JSON level-one
override and native backends. Five alternating reference/candidate rounds
finish all builds before timing. Driver processes execute 500 turns; JSON
processes emit 200 large results. Values below are medians; `~` means the
delta is within either cohort's p10–p90 spread, not a significance test.

| Target | Shipped KiB | Driver CPU ms | JSON CPU ms | Idle RSS KiB | JSON peak KiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| Linux x86_64 | 6246.531 | 2223.0 → 1952.3 | 2258.5 → 1230.9 | 7124 → 8308 | 257236 → 261320 |
| Linux aarch64 | 5300.789 | 2041.0 → 1531.7 | 1957.0 → 1166.7 | 6228 → 7488 | 253956 → 258004 |
| Windows x86_64 | 8509.000 | 2843.8 → 2593.8 ~ | 3640.6 → 2578.1 | 8324 → 8532 | 246944 → 247108 ~ |
| Windows aarch64 | 7192.000 | 2062.5 → 1937.5 ~ | 2359.4 → 1968.8 ~ | 9648 → 9784 | 248052 → 248552 ~ |
| macOS arm64 | 5578.141 | 1608.9 → 1119.0 ~ | 1504.4 → 1144.7 | 8656 → 9568 | 255136 → 256512 |
| macOS x86_64 | 6326.906 | 4147.9 → 3790.8 ~ | 3277.5 → 2516.5 | 4068 → 4256 | 260396 → 257652 ~ |

The largest idle cost is 1.23 MiB; every native target remains below
10,240 KiB in every full-suite sample, with no daemon or line-client CPU
counter increment during idle. Linux JSON peak rises about 4 MiB, or 1.6%.
The profile buys 40–45% lower JSON CPU on Linux, 23–24% on macOS and 29% on
Windows x86_64. Windows ARM JSON CPU remains noisy; its attach p99 improves
0.958 → 0.871 ms beyond spread. Linux attach p99 improves
0.477 → 0.378 / 0.412 → 0.309 ms. macOS ARM driver user CPU improves
1169.3 → 757.6 ms beyond spread, while total driver CPU remains noisy.
Cold-reply changes are within noise except Linux ARM's 7.486 → 6.378 ms.
No attach or cold-reply regression is established beyond observed spread.

These are process CPU and peak RSS, including fixture setup, encoding and
replay. The 500-turn fixture captures request history and consumes much more
memory than the regular 100-turn driver benchmark; its approximately
500 MiB peak is not daemon idle memory. Full suites retain the original
100-turn driver and 20-result JSON cases. Native memory definitions differ
between OSes; comparisons apply within a target.

[The complete main comparison](https://github.com/arut-land/a/actions/runs/39885640037)
records the independently retained JSON level-one and Windows-backend
changes against `2a33ad3`. The selected speed-profile evidence applies to the
measured release-3/JSON-1 combination. As
[Cargo documents](https://doc.rust-lang.org/nightly/cargo/reference/profiles.html#overrides-and-generics),
level-three consumers instantiate their own generic code; large decoders
remain in their owning crates. Target-specific package profile environment
overrides are unsupported, so the build uses ordinary Cargo profiles.

## Native idle and alternatives

[Windows subscriptions](windows-watch.md) await a maintained native
thread-pool backend's queue doorbell. Healthy subscriptions have no periodic
timeout. The dependency provides overlapped I/O, cancellation, path fidelity
and overflow/fault recovery behind `rt`. Its 16 target-only dependencies
include generated Windows bindings; the isolated backend comparison adds
36 KiB to the x86_64 image. A [combined main comparison](https://github.com/arut-land/a/actions/runs/39877497068)
measures an ARM idle cost of 72 KiB in one cohort. Native watch invariants pass on both
Windows architectures; resource costs are reported rather than hidden by
binary-size limits.

| Candidate | Decision and measured reason |
| --- | --- |
| JSON-only level 3 with level-z callers | Not selected: the [long Windows ARM comparison](https://github.com/arut-land/a/actions/runs/39867372897) lowers JSON CPU but raises driver user CPU 109.375 → 171.875 ms beyond spread |
| mimalloc | System retained: [paired measurements](https://github.com/arut-land/a/actions/runs/39864532590) raise Linux idle RSS about 6–7 → 24–25 MiB and JSON peak about 43–44 → 69–70 MiB; Windows JSON peak rises about 29–36%. No joint macOS CPU/RSS advantage is established |
| Linux x86-64-v2 | Baseline retained: [paired run](https://github.com/arut-land/a/actions/runs/39849948034) shows no clear CPU/latency gain and raises JSON peak RSS 46536 → 49128 KiB beyond spread |
| Windows alternate linker, ICF, CRT, unwind/debug profiles | Native settings retained: build diagnostics alone do not establish a CPU, RSS or startup benefit |
| Thin LTO, PGO, musl/static | Not selected without a joint runtime benefit across the relevant native targets; no completed all-target PGO comparison is claimed |
| Panic abort | Unwinding retained so one panicking session cannot terminate the daemon |

To reproduce, change one variable on a `perf/<name>` branch and dispatch
`perf.yml` with `paired_ref` set to its full baseline commit. The speed-profile
baseline is
`712b6e4052aa8e7cea9a2f52bad67611359905f7`; the main comparison uses
`2a33ad371a766644c7f11e4090ab80b7f45a2a7e`. Download each `perf-<runner>`
artifact and inspect `paired.json`, `result.json`, native counters and summary.
The [workflow guide](hosted.md#reproduce) gives commands and variant syntax.
