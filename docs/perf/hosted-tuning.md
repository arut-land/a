# Native release tuning

Release uses main's level-z profile, fat LTO, one codegen unit, unwinding,
packed debuginfo and native allocators. CPU baselines remain conservative.
Size is report-only, but growth must be proportionate to the benefit.

A package override is retained only when CPU improves beyond observed spread
on most native targets and idle and peak RSS do not rise beyond noise.
Measurements use the [hosted workflow](hosted.md), five alternating reference
and candidate rounds, native counters and raw p10–p90 spreads. All other
packages retain main's profile.

## Package selection

The [hosted baseline attribution](https://github.com/arut-land/a/actions/runs/39897488558)
uses Callgrind 3.22 on Linux x86_64. Recorded-provider decode executes
29.35 billion instructions; LLM-namespaced functions account for about 61%
of exclusive instruction cost. SSE framing, JSON depth tracking and typed
chat decoding dominate. serde_json accounts for about 42% of the large-result
profile and 27% of the driver profile. Kernel-namespaced fold functions are
about 0.1% of driver instructions, so kernel optimization is not selected
without a different workload demonstrating its importance.

Attribution includes fixture setup and standard-library work. It selects
packages for native comparisons; instrumented instruction counts do not
establish a latency improvement. Each trial changes one package level with
every other package left at main's settings. No tested override satisfies
the CPU and memory rule, so the release profile matches `fdf3da9` exactly.

## Native decisions

| Target | Release profile | Allocator and native build settings |
| --- | --- | --- |
| Linux x86_64 GNU | Main level z; no package overrides | glibc, two arenas; clang/mold, safe ICF, RELR |
| Linux aarch64 GNU | Main level z; no package overrides | glibc, two arenas; clang/mold, safe ICF, RELR |
| Windows x86_64 MSVC | Main level z; no package overrides | System; static CRT, native MSVC ABI, 8 MiB stack |
| Windows aarch64 MSVC | Main level z; no package overrides | System; native ARM tools, static CRT, 8 MiB stack |
| macOS arm64 | Main level z; no package overrides | System; Apple linker and compact unwind |
| macOS x86_64 | Main level z; no package overrides | System; Apple linker and compact unwind |

The same-runner comparisons below use main
`fdf3da9c5e3a4588bc353a4a5c8c3988e709a576` as reference. Five alternating
AB/BA rounds preserve all raw samples. The Linux ARM measurements already
disqualify every candidate under the memory rule; a CPU win on other targets
cannot erase that cost. Sizes and all native target observations remain in
the linked per-run artifacts, including measurements from failing gates.

| Single-package override | Linux ARM shipped KiB | Decoder CPU ms | Memory reason not retained |
| --- | ---: | ---: | --- |
| [llm level 3](https://github.com/arut-land/a/actions/runs/39898553619) | 3510.039 → 3815.289 | 2499.556 → 1319.816 | Idle RSS +184 KiB versus 16 KiB spread; decoder peak +620 KiB versus 64 KiB spread |
| [llm level 1](https://github.com/arut-land/a/actions/runs/39898557053) | 3510.039 → 3864.414 | 2507.117 → 2012.627 | Idle RSS +352 KiB versus 24 KiB spread; decoder peak +476 KiB versus 4 KiB spread |
| [serde_json level 1](https://github.com/arut-land/a/actions/runs/39898560741) | 3510.039 → 3531.289 | 2506.230 → 2153.502 | Decoder peak 5724 → 5820 KiB: +96 KiB versus 72 KiB spread |

CPU and RSS values are medians. Spread is the larger cohort's p10–p90 range;
with five samples it includes that cohort's extrema. Decoder CPU includes
recorded-cassette setup and 50 decode iterations; it is not network latency.
Peak RSS is the native lifetime resident maximum. Long driver/JSON cases
also include setup and replay; their approximately 500/250 MiB fixture peaks
must not be confused with idle daemon memory. The final harness additionally
pairs the regular 100-turn driver and 20-result JSON cases.

Whole-workspace level three is not used: its 51–76% image growth, increased
idle RSS and approximately 4 MiB greater Linux JSON peak RSS are not a joint
win for network-bound turns. Replacement allocators, higher CPU baselines,
PGO and alternate linkers remain unselected without proportionate CPU,
latency and memory evidence. Windows uses main's notify watcher and adds no
watcher dependency.

## Reproduce

Use one `perf/<name>` branch per package setting. Add only
`[profile.release.package.llm]` with `opt-level = 3`, for example; leave the
rest of main's profile unchanged. Dispatch the workflow with `paired_ref`
set to the full reference above. [Workflow commands](hosted.md#reproduce)
also support a single JSON config overlay. Inspect each native artifact's
`paired.json`, `result.json`, profile configuration and raw counters.

Retain a candidate only when CPU improves beyond spread on at least four
of six targets, no measured CPU workload regresses beyond spread, and idle
and peak RSS do not rise beyond noise on any target. Check both regular and
long batches before promotion. Size is reported per target and must remain
proportionate to the benefit. Do not rerun a rejected memory tradeoff until
it happens to pass.

## Open issues

- Resolved: notify is gone. The watcher is `rt::watch::Hub` over each OS's own notifications, with no timed wait anywhere ([watchers](watchers.md)).
