# Native release performance

Current hosted per-platform tuning is specified in [hosted.md](hosted.md).
Size is report-only. The dedicated-host observations below do not establish
a cross-platform tuning decision.

Keep `z`, fat LTO, one codegen unit, unwinding, packed debuginfo and
stripping. Retain `memchr2` for JSON CR/LF scans and reuse directory existence
observations. Package membership, retry barriers and allocations are unchanged.

JSON-only large-result instructions fell 2.465 → 1.997 billion; CPU 175 →
157 ms. Combined: 1.992 billion/165 ms. Reload instructions fell 8.821 →
8.157 billion. These support CPU efficiency; contended wall times prove no
universal latency gain. Two directory publication attempts now query twice
instead of four times. Unavailable privileged tracing leaves the historical
191 versus 182 ms cold-creation gap unattributed.

## Host and limits

Apple M3, eight cores, 16 GiB, APFS, macOS 27.0.1, AC power;
nightly-2026-10-17, Hyperfine 2.0.0. Baseline: main `e431c08` plus measurement
harness only. Final adds the two source changes. Saved builds record flags,
revision and source diff.

**The host was contended.** Initial load: 3.96 / 4.89 / 4.46; endpoint
security 113% CPU, Spotlight 63%, Zoom 37%, Core Audio 34%. One-minute
batch loads ranged 1.91–24.03, including completed builds; no compilation
overlapped timing. Raw records retain top consumers and all three load
averages; tables show one-minute load. Native Linux/Windows measurements,
BOLT, x86-64 v2/v3 and GNU/musl trials
remain open. Windows cross-lint does not establish native performance.

CLI: three warmups/30 samples; fresh creation: 30 independent homes. Benches:
one warmup/three runs. Values are medians unless stated. CLI CPU/RSS exclude
the daemon; bench CPU/RSS include setup/replay. Shell stdout spills; the JSON
bench retains its result. The transcript contains 64 real echo turns.

## Per-axis comparison

| Gate or observation | Main | Final |
| --- | ---: | ---: |
| Shipped bytes (`mise run size`, fixtures absent) | 3,246,400 | 3,246,400 |
| Fixture bytes | 3,246,560 | 3,246,544 |
| `--version`, paired, 1,000 samples | 4.96 ms; 3.11 ms CPU; 6,528 KiB RSS | 4.78 ms; 2.98 ms CPU; 6,528 KiB RSS |
| Cold first daemon reply, five samples | 35.04 ms | 32.99 ms |
| Attach p50/p99, 500 samples | 175 / 400 µs | 169 / 259 µs |
| Daemon RSS, five homes: min/median/max | 9,040 / 9,072 / 9,104 KiB | 8,912 / 9,104 / 9,184 KiB |
| Daemon / terminal CPU in 60 seconds | 0.00 / 0.00 s | 0.00 / 0.00 s |
| Existence queries, two publication attempts | 4 | 2 |

Idle CPU resolution is 10 ms. Daemon RSS ranges overlap, at least 1,056 KiB
below the 10,240 KiB RSS cap. Shipped size is reported above. Standalone probe
loads/consumers are in raw records. Allocation counts remain unchanged.

Main → final in every cell:

| Workload | Wall ms | CPU ms | Peak RSS MiB | Load before/after |
| --- | ---: | ---: | ---: | --- |
| Fresh-home `a new` | 154.56→135.44 | 16.17→13.77 | 7.30→7.41 | 3.85/3.72→2.82/3.07 |
| Cold-daemon `a new` | 33.19→32.10 | 5.81→5.34 | 7.22→7.33 | 3.72/3.72→3.07/3.47 |
| Warm `a new` | 16.90→15.27 | 4.96→3.75 | 7.11→7.22 | 3.72/3.72→3.47/3.47 |
| CLI echo | 18.99→19.51 | 5.17→4.99 | 7.88→8.08 | 3.72/3.72→3.47/3.47 |
| Shell: 1 MiB stdout | 104.77→104.20 | 5.52→5.51 | 8.14→8.28 | 3.72/4.14→3.47/3.35 |
| Long transcript resume | 26.07→24.87 | 17.18→16.90 | 10.97→11.16 | 4.14/3.89→3.35/3.35 |
| 500 replay attaches | 353.67→351.01 | 150.67→150.43 | 9.86→9.80 | 2.88/2.88→2.97/2.97 |
| Driver: 100 recorded turns | 990.70→856.01 | 346.21→216.07 | 31.14→31.33 | 3.83/3.52→3.52/3.71 |
| Paired commits: N=1,8,64 | 4750.63→4568.69 | 1460.05→1549.34 | 12.44→12.42 | 1.67/3.05→1.67/3.05 |
| 15,600-entry reload process | 988.71→987.51 | 824.17→795.27 | 80.67→81.00 | 3.90/3.83→3.52/3.52 |
| 20 JSON results + replay | 265.65→264.05 | 175.12→165.02 | 40.61→40.67 | 3.95/4.11→5.65/5.44 |
| Transcript codec + fold | 78.24→81.45 | 76.46→78.81 | 13.33→13.34 | 4.11/4.11→5.44/5.44 |
| 6,000-item TUI render | 118.49→113.04 | 115.21→110.87 | 17.36→17.00 | 4.11/4.11→5.44/5.44 |

Inner wall times: driver `step`/`effects` 9.00/9.00 → 7.34/8.43 ms;
snapshot reload 32.79 → 31.24 ms; TUI delta p50/p99 56.71/191.50 →
55.00/177.21 µs. Snapshot loading still validates and decodes the prefix.
Concurrent commits use persistent actors, real JSONL, shared flushing and
30 simultaneous rounds at each N. Paired medians of three runs:

| Sessions | Main turns/s; p50 / p99 ms | Final turns/s; p50 / p99 ms |
| --- | ---: | ---: |
| 1 | 282; 3.92 / 4.00 | 303; 3.63 / 4.03 |
| 8 | 513; 9.79 / 19.63 | 543; 9.98 / 27.70 |
| 64 | 541; 61.86 / 130.17 | 560; 61.79 / 127.55 |

Contrary evidence matters: final paired p99 ranges reach 13.72/69.70/375.60 ms
at N=1/8/64. An independent combined-process batch worsened 4.63 → 5.14 s
with nearly identical instruction counts. Neither concurrency improvement
nor absence of a contention regression is established; N=1 has only 30
samples per process.

## Build decisions

Fat LTO, one codegen unit and unwinding preserve cross-crate optimization and
session panic isolation. Debug metadata supports offline crash symbolication.
The system allocator remains the default; disabling the macOS Nano zone did
not show a robust joint speed/RSS gain in these dedicated-host observations.

Global and per-package optimization levels, CPU baselines, replacement
allocators and PGO require native hosted CPU, latency and memory comparisons.
Executable growth does not disqualify a candidate. PGO training must include
the daemon, not just clients and benchmarks; its profile is specific to the
Rust toolchain and architecture. The [hosted harness](hosted.md) supplies
native comparisons with retained raw samples.

## Reproduce

Copy benches/tasks into an isolated baseline checkout, leaving its production
files unchanged. Finish builds before timing; repeat probes for candidates:

```sh
CARGO_BUILD_JOBS=2 mise run perf:build-variant z --tests
mise run perf:workloads target/perf-variants/z/a main-repeat --output target/perf-tuning/workloads
mise run perf:benches target/perf-variants/z target/perf-tuning/workloads/main-repeat/transcript.jsonl
CARGO_BUILD_JOBS=2 mise run perf:build-variant serde_json3 --tests
# Repeat probes for each saved candidate; label the final workload `final`.
CARGO_BUILD_JOBS=2 mise run perf:build-variant z --label final --tests
mise run perf:idle-rss target/perf-variants/final/a final --samples 5 --idle-seconds 60
# Requires the matching llvm-tools rustup component:
CARGO_BUILD_JOBS=2 mise run perf:pgo
mise run perf:report
```

Hyperfine runs through `mise exec`. `perf:benches --blackbox <executable>`
also runs strict budgets. Daemons use throwaway `A_HOME` and are stopped
with their own binary before cleanup.

[CSV observations](tuning.csv) are regenerated by `perf:report` from ignored
`target/perf-variants/*/{build.json,measurements}` and `target/perf-tuning`
workload, paired and load JSON. Options `--builds`,
`--observations`, `--output` accept archives. Historical CSV regeneration
requires these raw artifacts. Missing optional paired runs are omitted; early
fresh-home trials lack CPU/RSS.
