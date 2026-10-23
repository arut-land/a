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
Builds finish before timing starts. Both binaries use `cargo build-release`:
rebuilt std with its compact algorithms, the source release profile (main uses level z), unwinding, fat LTO, one codegen unit, stripped
symbols and the source checkout's native linker flags. The default binary
must exclude the sleep fixture and include the edit tool; the second binary
enables fixtures for deterministic echo workloads. Size is measured and reported on every target, without a size gate.
Tuning decisions prioritize latency, CPU, idle and peak RSS, reliability and
native platform behaviour. The JSON records the source and harness
revisions, Rust version, Cargo configuration, profile and variant.

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
| Cold first reply | Five fresh homes; stage once, stop, then line REPL spawn through first session line |
| Attach | Five independent daemons; ten recorded echo turns; 500 connect/hello/attach/replays each, through `Synced` |
| Idle | Five homes; daemon RSS at three seconds; CPU after fifteen seconds settling, then ten seconds with no profiling |
| CLI workloads | Three warmups, twenty runs: version, fresh-home/new, cold/warm new, echo, 1 MiB shell output, long transcript |
| Engine/JSON/TUI benches | One warmup, five runs per saved release benchmark executable |

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
30 days, including measurements from budget failures. Its job summary shows
the same table. Missing measurements stay absent.

The comparison downloads the matching lane from the latest successful
`main` perf run through the GitHub API. Build-only runs and dispatches with
source or compiler overrides cannot become the automatic baseline. Statistics
are recomputed from raw observations before comparison. A missing main artifact is reported
explicitly. `baseline_run` selects a bootstrap run while the workflow is
being introduced. Schemas and architectures must match. A delta no larger
than either run's p10–p90 spread is labeled **within noise**; this conservative
heuristic is not a statistical significance test. Separate hosted VMs can
still differ in CPU, storage or load, so repeat a candidate before retaining
small gains. Size is exact; timing and memory remain observations.

Idle RSS must remain below 10,240 KiB and idle CPU must have no counter
increment. The perf workflow also runs strict Linux latency, terminal first-paint,
PTY-client idle and quiet-shell wake gates; CI owns functional validation.

`build_only` stops after the two release builds and records size, sections and
imports for linker diagnostics. It does not claim a
runtime budget pass. Full measurements remain the default.

For tuning, `paired_ref` additionally builds a reference revision on the same
runner. Reference builds omit the protocol and TUI bench executables, which
these paired windows do not use. Five alternating AB/BA rounds measure startup, cold reply, attach,
idle RSS/CPU, driver turns and large JSON. Paired driver batches use 500 turns (250 per barrier); JSON batches use 200 results
to reduce native CPU counter quantization; the full suite retains its original
20-result JSON and 100-turn driver workloads. Each role uses fresh isolated
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
