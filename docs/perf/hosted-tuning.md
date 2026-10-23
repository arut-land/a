# Native release tuning

Decisions prioritize CPU, idle and peak RSS, latency and reliability. Size is
reported without a cap. The [hosted workflow](hosted.md) records raw native
observations and p10–p90 spreads; a median delta within either cohort's spread
is noise, not a retained speedup.

| Native target | JSON profile | Allocator | Native facilities retained |
| --- | --- | --- | --- |
| Linux x86_64 GNU | serde_json level 1; other crates z | glibc, two arenas | mold, safe ICF, RELR, epoll/inotify |
| Linux aarch64 GNU | serde_json level 1; other crates z | glibc, two arenas | mold, safe ICF, RELR, epoll/inotify |
| Windows x86_64 MSVC | serde_json level 1; other crates z | system | MSVC, static CRT, thread-pool directory I/O |
| Windows aarch64 MSVC | serde_json level 1; other crates z | system | native ARM tools, MSVC, static CRT, thread-pool directory I/O |
| macOS arm64 | serde_json level 1; other crates z | system | Apple linker, compact unwind, kqueue/native directory notifications |
| macOS x86_64 | serde_json level 1; other crates z | system | Apple linker, compact unwind, kqueue/native directory notifications |

All targets retain fat LTO, one codegen unit, unwinding and offline crash
symbolication. CPU baselines remain the conservative native toolchain defaults;
no new instruction-set requirement is introduced. Linux GNU requires glibc
2.36 for RELR and native OpenSSL 3. A musl/static build is a different runtime
and deployment contract; it is not substituted for this GNU measurement.

## JSON CPU and memory

[The same-runner level-one comparison](https://github.com/arut-land/a/actions/runs/39873446213)
alternates five reference/candidate rounds against main `2a33ad3`. Driver
processes execute 500 turns; JSON processes emit 200 large results. Each
process includes fixture setup, encoding and replay, so these are process CPU
and peak RSS, not isolated parser throughput or daemon peak memory. The large
500-turn fixture captures request history and uses substantially more memory
than the regular 100-turn driver benchmark.

| Target | Driver CPU ms | Driver peak KiB | JSON CPU ms | JSON peak KiB |
| --- | ---: | ---: | ---: | ---: |
| Linux x86_64 | 2449.7 → 2252.4 | 513380 → 513432 | 2323.3 → 2294.4 (noise) | 257316 → 257352 |
| Linux aarch64 | 2220.4 → 2025.5 | 512276 → 512248 | 2233.9 → 1953.6 | 253916 → 253900 |
| Windows x86_64 | 2640.6 → 2812.5 (noise) | 490248 → 489884 | 3156.3 → 2906.3 (noise) | 247076 → 247028 |
| Windows aarch64 | 2406.3 → 2375.0 (noise) | 475600 → 475832 | 2500.0 → 2421.9 (noise) | 248108 → 248152 |

Linux driver CPU improves 8.1%/8.8%, beyond observed spreads; user CPU
improves 8.4%/10.9%. Peak and idle RSS changes remain within noise. Windows
shows no established driver CPU gain. ARM attach p99 in this cohort is
0.973 → 1.057 ms, outside its observed spread; cold reply and RSS differences
remain within noise. Per-axis observations remain visible even when another
axis improves.

[Cargo's generic sharing rule](https://doc.rust-lang.org/nightly/cargo/reference/profiles.html#overrides-and-generics)
explains the profile: level one allows generic instantiations to be shared
with z consumers; levels two and three do not. Package profiles are common to
targets in Cargo; target-specific package environment overrides are unsupported.
The release profile does not add a custom compiler wrapper or duplicate crates
to simulate them.

## Native idle and rejected alternatives

[Windows subscriptions](windows-watch.md) await the maintained native
thread-pool backend's queue doorbell rather than notify's 100 ms timed wait.
[Native Windows validation](https://github.com/arut-land/a/actions/runs/39869570230)
passes the focused watch invariants and all five candidate idle CPU windows
on both architectures. Candidate daemon RSS medians are 8456/9716 KiB, with
maxima 8484/9728 KiB. The x86_64 paired comparison is complete; the ARM
historical reference failed a later RPC, so that run cannot supply a complete
paired ARM aggregate.

| Candidate | Decision and measured reason |
| --- | --- |
| serde_json level 3 | Not retained: the [long Windows ARM comparison](https://github.com/arut-land/a/actions/runs/39867372897) lowers JSON CPU but raises driver user CPU 109.375 → 171.875 ms beyond spread; a JSON-only win is insufficient |
| mimalloc | System retained: [paired measurements](https://github.com/arut-land/a/actions/runs/39864532590) raise Linux idle RSS about 6–7 → 24–25 MiB and JSON peak about 43–44 → 69–70 MiB; Windows JSON peak rises about 29–36%. macOS budget collection did not complete, so no macOS allocator benefit is established |
| Linux x86-64-v2 | Baseline retained: [paired run](https://github.com/arut-land/a/actions/runs/39849948034) shows no clear CPU/latency gain and raises JSON peak RSS 46536 → 49128 KiB beyond spread |
| Windows alternate linker, ICF, CRT, unwind/debug profiles | Native defaults retained: build diagnostics alone do not establish a CPU, RSS or startup benefit |
| Thin LTO, PGO, musl/static | Not adopted without a joint runtime benefit across the relevant native targets; no claim of a completed all-target PGO comparison |
| Panic abort | Unwinding retained so one panicking session cannot terminate the daemon |

Size never determines these decisions. To reproduce a single variable, use a
`perf/<name>` branch with only that change and dispatch `perf.yml` with
`paired_ref=2a33ad371a766644c7f11e4090ab80b7f45a2a7e`. Download each `perf-<runner>`
artifact and inspect `paired.json`, `result.json`, native counters and summary.
The [workflow guide](hosted.md#reproduce) gives exact commands and variant syntax.
