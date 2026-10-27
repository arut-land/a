# Hosted release measurements

`.github/workflows/perf.yml` measures one source revision and one optional
variant per run. Pushes to `main` and `perf/**` run all six standard hosted
lanes; dispatch can select one lane. The runner labels are checked against
[GitHub's public runner table](https://docs.github.com/en/actions/reference/runners/github-hosted-runners):

| Runner | Native target |
| --- | --- |
| ubuntu-latest | x86_64-unknown-linux-gnu |
| ubuntu-24.04-arm | aarch64-unknown-linux-gnu |
| windows-latest | x86_64-pc-windows-msvc |
| windows-11-arm | aarch64-pc-windows-msvc |
| macos-latest | aarch64-apple-darwin |
| macos-15-intel | x86_64-apple-darwin |

The harness and measured checkout are siblings, so Cargo cannot inherit
experimental configuration from the harness when measuring another source
revision. Baseline source receives only the protocol probe and the benchmark's
portable RSS adapter. Provider cassettes and encoded fixtures are unchanged.

## Build and accounting

Build, measurement and optional reference rounds are separate job steps.
Builds finish before timing starts. Both binaries use the release arguments in
`scripts/perf/build.py`, equivalent to `cargo build-release`:
rebuilt std with its compact algorithms, the source release profile (main uses level z), unwinding, fat LTO, one codegen unit, stripped
symbols and the source checkout's native linker flags. The default binary
must exclude the sleep fixture and include the edit tool; the second binary
enables fixtures for deterministic echo workloads. Size is measured and reported on every target, without a size gate.
Tuning decisions prioritize latency, CPU, idle and peak RSS, reliability and
native platform behaviour. The JSON records the source and harness
revisions, Rust version, Cargo configuration, profile and variant.

`mise run size` and `mise run startup` consume Cargo's `compiler-artifact`
executable path, including when `CARGO_TARGET_DIR` or the target triple changes
the output location. `mise run bloat` shares the rebuilt-std arguments; its
only profile difference is retaining the symbols needed for attribution.
Staging retains matching dSYM, DWP and PDB sidecars, with an identity manifest
containing the image and symbol-file SHA-256 hashes and native UUID, ELF notes
or PE debug-directory output. The upload includes the symbol bundles themselves.

`host.json` is written before builds and measurements, so failed runs retain
hardware and toolchain metadata. Failed prep commands keep their argv, exit
status, stdout and stderr separately from the idle line client's stderr.
These captures and native map diagnostics run outside timing windows.

The mbx store and Cargo downloads are cached by OS, architecture and
configuration. Rustup installs an explicitly native host toolchain in a private runner-local
directory outside the cache, so a
cancelled installation cannot leave a partial toolchain in subsequent runs. Intel macOS uses rust-cache for native Cargo artifacts because
mbx 1.24.1 publishes no Intel macOS executable. Windows ARM installs native
Hyperfine and mbx from the checked-in mise lockfile. Release and benchmark
builds use LLVM; they do not need a Cranelift component.

Hyperfine 2.0.0 collects wall time, process CPU and Unix peak RSS.
On Windows, five separate launches per workload retain the child handle
through exit and query `GetProcessMemoryInfo` for lifetime peak working set
and `GetProcessTimes` for CPU. Hyperfine does not support Windows peak RSS;
these native observations are stored separately and supply that report axis. psutil 7.2.2
provides native RSS/working-set and accumulated user+kernel CPU counters,
including Windows, for the idle daemon and line client. Optional per-thread
diagnostics are Windows-only; macOS process accounting uses libproc without
requesting a debug task port. CPU reports separate
user and kernel time as well as their total; a reduction in one workload must
not conceal a regression in another. It is a measurement
dependency, not a shipped Rust dependency. `/proc` and macOS resident sets
include shared resident pages; Windows uses working set, not private bytes.
Do not compare memory values between OSes as if they shared one definition.

Snapshots retain page-fault counters: Linux minor/major faults, macOS faults
and page-ins, and Windows page faults. Unix fresh-home launches also retain
`wait4` minor/major faults; Windows launches retain `GetProcessMemoryInfo`'s
fault count. These are lifetime counters, not proof of disk-cold startup.
`memory-*.json` includes executable-image and executable-code residency:
Linux uses smaps, macOS uses the rounded `vmmap -resident` columns, and Windows
uses `QueryWorkingSetEx`'s valid-page bit. On Windows psutil's map field named
`rss` is `VirtualQueryEx.RegionSize`; artifacts rename it `mapped_bytes` and
store the separately queried `resident_bytes`. Whole-process working set
remains `rss_kib`. Mappings and process counters are sequential observations,
so their totals need not match exactly. See the native
[QueryWorkingSetEx contract](https://learn.microsoft.com/en-us/windows/win32/api/psapi/nf-psapi-queryworkingsetex)
and [working-set flags](https://learn.microsoft.com/en-us/windows/win32/api/psapi/ns-psapi-psapi_working_set_ex_block).

## Round-2 survey workloads

Normal hosted runs include `scripts/perf/evidence.py`. Dispatch with
`survey=true` to build only the fixture binary, survey probe and engine load
measurement; `runner=linux-windows` selects the two x64 lanes in one dispatch.
Dispatch with `memory=true` to build only the fixture binary and the small
protocol/sys memory probe: it skips the CPU probe and all saved benchmark
executables. The corresponding local commands are `hosted.py --phase survey`
and `hosted.py --phase memory`, with explicit `--source`, `--output` and
`--runner` arguments. `CARGO_TARGET_DIR` is respected by these builds.

Three rounds reverse the complete control/load ordering on alternating rounds.
Every completed observation is appended immediately to `samples.jsonl`;
`evidence.json` keeps raw observations, timing medians and min/max/p10/p90
spreads, skipped capabilities and failures. Numeric measurements do not gate
the run. Failed tool outcomes, incomplete workloads or wrong output sizes do.
Tool validation reads the engine's real log, joining results to agent calls;
user configuration calls such as `--mode auto` are separate. A successful CLI
exit alone is insufficient. Large shell output must hold the expected exact
1,048,576-byte blob; image reads must each complete and hold their image.

| Workload | Control / load |
| --- | --- |
| quiet shell | 1 MiB output / quiet `true` or PowerShell `exit 0`, through the native shell tool |
| reply projection | 0 / 50,000 prior items, 4,096 32-byte deltas through the real reducer and reply stream |
| edit | 1 / 64 MiB real UTF-8 file, one exact replacement |
| TUI paragraph | newline / unfinished paragraph, 2,000 32-byte updates at width 100 |
| web title | 0 / 50,000 prior items, 4,096 delta batches through the actual page and patch code |
| subscriptions | 20,000 list / watch-disconnect cycles on a quiet daemon |
| attachments | 20,000 list / attach-sync-detach cycles |
| image reads | 1 / 8 concurrent reads of a 4,000×3,000, 36,000,054-byte BMP |
| replay buffers | 240 bare / environment-bearing session attachments held idle, then detached |
| PTY cache | 32 / 5,000 validated echo turns with a real 100×30 terminal attached; two Ctrl-O toggles restore the display mode |

The Rust probe is built under the target directory against workspace crates
and actual web source modules; it adds no production APIs or dependencies to
the shipped graph. Memory-only mode includes just its protocol/sys portion.
Each daemon uses a fresh isolated home. Memory observations distinguish
resident/footprint samples from live-heap attribution: RSS remaining after
cleanup is not by itself evidence of a leak. Windows PTY-cache observations
are explicitly marked unavailable until a ConPTY launcher is exposed by the
sys test seam; there is no pipe-rendering substitute.

The many-session runner samples native counters from its parent. In an idle
window the measured process blocks once, while the parent takes both snapshots
five seconds apart, then returns the observations. No accounting child is
launched by the measured process. `phases` preserves provider writes and
client receive/decode/observation timestamps per session, turn and delta, in
microseconds from one process-local monotonic origin. Receive means the last
read by the protocol decoder, including buffered reads. The reader delegates
framing and decoding to protocol; it does not duplicate the wire format.

Provider receive/decode, daemon visibility and server-outbox timestamps remain
explicitly listed in `unobserved_phases`. Collecting those requires test hooks
in `crates/llm/src`, `crates/engine/src/driver.rs`, `projection.rs` and
`outbox.rs`/`server.rs`, outside this harness branch's ownership. The available
client timestamps cannot attribute those internal phases or establish the
cause of Windows throughput and Intel macOS tails.

All daemons use isolated homes and stop through their own binary and `A_HOME`.
Collection waits for the captured daemon PID to exit before deleting its home;
Windows retains executable image mappings through process exit. A failed graceful stop is recorded and only that captured daemon PID can be
terminated. Measured commands have a five-minute deadline; cleanup stops only
the owned command and its descendants. Measurement and paired job steps have
twenty- and forty-five-minute deadlines; long Python phases emit a stack
snapshot every five minutes. The tool
never addresses the user's default home.

## Samples and comparisons

| Measurement | Repetitions and boundary |
| --- | --- |
| Startup | 10 warmups, 100 launches each of shipped `a --version` and a native empty Rust executable |
| First run | Five fresh homes; initial `ls`, including immutable build installation and daemon startup |
| Cold first reply | Five fresh homes; stage once, stop, then line REPL spawn through first session line |
| Attach | Five independent daemons; ten recorded echo turns; 500 connect/hello/attach/replays each, through `Synced` |
| Idle | Five homes; daemon RSS at three seconds; CPU after fifteen seconds settling, then ten seconds with no profiling |
| CLI workloads | Three warmups, twenty runs: version, fresh-home/new, cold/warm new, echo, 1 MiB shell output, long transcript |
| Engine/JSON/LLM/TUI benches | One warmup, five runs per saved release benchmark executable; LLM decodes recorded provider cassettes |

The CLI workload generates its transcript through the engine's encoder.
The engine driver executes 100 recorded turns across the step/effects barrier
settings; concurrency, 15,600-entry reload, large JSON, protocol codec/reducer
and 6,000-item TUI rendering retain their existing workload definitions. The
concurrent runtime sizing workload also measures 24 file-backed sessions with
two, four and seven workers over six alternating rounds. Benchmark output,
including the inner worker and latency observations, is retained in JSON.
Blocking workers retire after ten seconds idle; CPU settling excludes this finite startup work. Process-level driver time includes setup and replay. Attach p99 is calculated
from 500 samples per daemon; the report gives the median and spread of the
five p99 estimates. Idle CPU zero means no increment at the OS counter's
resolution, not a claim about instructions below that resolution.

`result.json` preserves raw samples, median, min/max and p10/p90 for every
axis. Spreads use nearest-rank percentiles, so a five-observation cohort's
p10/p90 includes its min/max. Each lane uploads its shipped binary, JSON, workload/load records and `summary.md` for
30 days, including partial measurements from harness failures. Its job summary shows
the same table. Missing measurements stay absent.

The comparison downloads the latest successful matching measurement job from
completed `main` perf runs through the GitHub API. A failing sibling lane does
not prevent healthy lanes from advancing their baselines. Build-only runs and dispatches with
source or compiler overrides cannot become the automatic baseline. Statistics
are recomputed from raw observations before comparison. A missing main artifact is reported
explicitly. `baseline_run` selects a bootstrap run while the workflow is
being introduced. Schemas and architectures must match. A delta no larger
than either run's p10–p90 spread is labeled **within noise**; this conservative
heuristic is not a statistical significance test. Separate hosted VMs can
still differ in CPU, storage or load, so repeat a candidate before retaining
small gains. Size is exact; timing and memory remain observations.

Perf fails only on harness errors, including invalid or incomplete replay; numeric
measurements never fail a lane. Idle CPU reports the maximum, every window
count and the number of nonzero windows, including Windows watcher ticks.
CI gates idle RSS below 10,240 KiB, cold reply and attach latency (strict on
Linux, with CI slack on macOS), attach replay length, terminal first-paint,
PTY-client idle and quiet-shell wakes.

`build_only` stops after the two release builds and records size, sections and
imports for linker diagnostics. It does not claim a
runtime budget pass. Full measurements remain the default.

For tuning, `paired_ref` additionally builds a reference revision on the same
runner. Reference builds omit the protocol and TUI bench executables, which
these paired windows do not use. Five alternating AB/BA rounds measure startup, cold reply, attach,
idle RSS/CPU, driver turns, large JSON and recorded-provider decode. Paired driver batches use 500 turns (250 per barrier); JSON batches use 200 results
to reduce native CPU counter quantization; the full suite retains its original
20-result JSON and 100-turn driver workloads. Paired rounds also run those
regular-size cases so long batches cannot hide a resident-code cost in peak RSS.
Each role uses fresh isolated
homes. All builds finish before these paired windows. Both raw observations
and `paired.json` remain in the artifact, with a second summary table. Use
this comparison when different hosted CPUs or storage obscure a small change.
Each completed role is checkpointed atomically with its completed-round count.
A failed reference leaves partial observations explicitly incomplete; daemon
JSONL logs are retained on command failures.

## Reproduce

Use separate `perf/<name>` branches for single-variable source/config changes.
A push runs the complete matrix; dispatch selects an optional overlay without
editing the release configuration:

```sh
gh workflow run perf.yml -R arut-land/a --ref codex-ciperf \
  -f runner=ubuntu-latest \
  -f variant='{"config":["profile.release.package.serde_json.opt-level=2"]}'
gh workflow run perf.yml -R arut-land/a --ref codex-ciperf -f source_ref=main
# With a successful bootstrap artifact, compare a final run to that run ID:
gh workflow run perf.yml -R arut-land/a --ref codex-ciperf -f baseline_run=RUN_ID
gh workflow run perf.yml -R arut-land/a --ref perf/x86-v2 \
  -f runner=ubuntu-latest -f paired_ref=FULL_40_CHARACTER_REVISION
gh run view RUN_ID -R arut-land/a --log-failed
gh run download RUN_ID -R arut-land/a -n perf-ubuntu-latest
```

`variant` is one JSON string with `rustflags` (extra compiler arguments) and/or
`config` (Cargo `--config` assignments). Compiler arguments append to the
release profile, retaining its address-significance metadata and target
linker flags. Do not repeat a committed profile change as an overlay.

For a dedicated measurement host, install the pinned Python accounting
dependency and run `mise exec -- python scripts/perf/hosted.py --runner LABEL`.
`size` and `startup` also work natively on Windows; `startup` builds the native
process floor. `perf:idle-rss` measures the line client on every OS; macOS PTY
and profiler evidence remains the separate historical measurement in
[idle-rss.md](idle-rss.md).

Optional `profile=true` attributes baseline instructions with
[Callgrind](https://valgrind.org/docs/manual/cl-manual.html) on Linux x86_64,
after all timing windows. It builds symbol-bearing engine and LLM benchmark
executables and profiles recorded turns, large JSON and provider cassettes.
Exclusive and inclusive instruction summaries and raw traces are artifacts.
These instrumented observations select packages to investigate; they do not
establish native speedups. `build_only=true` can skip timing for attribution.

CI owns functional checks, acceptance, platform lints, the Apple build and
release budget gates on pushes to main and pull requests. Perf owns native
release measurements; gates are not duplicated there.

Attach samples compare every replay entry and the Synced sequence against the
persisted log before accepting a timing. Bloat uses the measured variant’s
compiler flags and package overrides, retaining symbols for attribution.
