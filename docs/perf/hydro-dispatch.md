# Lazy dispatch locals

Measured on 2026-10-29 on the shared Apple M3, arm64, macOS 27.0.1.
Baseline: `dbd55d3df0f5bf6f6cab89babcef8d51be3cf70d`, including the
merged MCP elicitation SDK changes. The finding in section 1 of the Hydro
survey is confirmed in the real SDK: both `Hooks::run` and `ask_from`
entered an eagerly allocated `InvocationContext`, even without local use.
This was allocation churn, not retained session memory.

## Representation

TLS now distinguishes `Outside`, `Active(None)` and an active owned
context. Entering an outer dispatch installs `Active(None)`; nested
entries leave that scope alone. The first `dispatch_local` materializes
the context, then releases the TLS borrow before calling an initializer.
This gives nested handlers one budget even when the inner handler is the
first local user. The existing guard restores the previous state on
return and unwind, including an unused active scope. Explicit invocation
contexts retain their ownership and can still carry budgets across workers.
There is no new dependency, bound, protocol change or platform-specific
production code.

## Method

Rust nightly-2026-10-17 (`5d8fec066`), release opt-level z, fat LTO,
one codegen unit, installed standard library. These are release SDK and
engine-test builds, not the shipped CLI's rebuilt-std size configuration.
All Cargo commands use `CARGO_BUILD_JOBS=2` through `MISE_NO_ENV=1 mise exec --`
and `CARGO_TARGET_DIR` outside the worktree.

Five baseline rounds ran before the production edit. Final comparisons
use seven fresh-process pairs per workload in alternating AB/BA order,
with both builds finished before timing. Native timings use an external
System-allocator executable linked to this checkout's actual `ext` and
`protocol` crates. It calls `Hooks::run<SessionEnd>` with no handlers,
one no-op Rust handler, a nested Rust handler, or a handler asking for a
local. Explicit-context samples reuse a warmed owned context around each
call. Inputs and hook registration are outside the timed region; outputs
are consumed with `black_box`.

The local workload has 262,144 events per process; other microbenchmarks
have 1,048,576, matching the survey's event counts. CPU is libc `clock()`
user + system time inside the loop, divided by events (macOS clock ticks
are microseconds). Wall throughput is also retained. No individual-event
latency percentile is inferred from these aggregate timings.

Allocation runs are separate from native timings. The ignored SDK test
`point::tests::bench_dispatch` wraps System with a thread-local counter;
construction and warm-up are excluded. Its instrumented timings are not
used as CPU evidence. The normal invariant checks both `run` and
`ask_from` without locals, and fails against the eager implementation.

The whole-turn workload is the existing engine benchmark
`driver::tests::bench::bench_turns_under_each_barrier_setting`: 50 turns per
process with the recorded OpenAI tool-call/follow-up cassette, a real tool,
and JSONL writes. Both Step and Effects durability settings run; each
sample is that process's median turn wall time. It uses isolated temporary
storage and no live provider or daemon. Baseline and candidate use exactly
`cargo test --release -p ext -p engine --lib --no-run`; Lua budget validation
runs separately to avoid feature-unification differences in the comparison.

## Results

CPU ns/event, median [minimum–maximum] across seven pairs:

| Workload | Eager | Lazy | Allocations/event, eager → lazy |
| --- | ---: | ---: | ---: |
| empty | 28.64 [28.38–29.44] | 13.47 [13.46–13.71] | 1 → 0 |
| native | 30.68 [29.46–31.62] | 14.54 [14.52–14.72] | 1 → 0 |
| nested | 37.85 [37.03–39.02] | 22.26 [21.81–23.71] | 1 → 0 |
| local | 122.71 [121.28–126.13] | 123.20 [120.61–131.28] | 5 → 5 |
| explicit | 31.96 [31.47–33.27] | 29.97 [29.89–31.88] | 0 → 0 |

The native-handler dispatch uses 52.6% less CPU at the median. Local-state
CPU differs by +0.4%, with overlapping ranges; its five allocations/event
are unchanged. Explicit-context samples exclude owned-context creation and
reuse a warmed local; their per-event allocation count is zero in both arms.

Whole-turn wall time in milliseconds, median [minimum–maximum] of the
seven process medians (50 turns each):

| Durability setting | Eager | Lazy |
| --- | ---: | ---: |
| Step | 7.996 [6.993–8.996] | 7.685 [6.970–8.992] |
| Effects | 7.061 [7.007–8.820] | 7.814 [7.007–8.977] |

There is no consistent end-to-end latency gain: the small Step reduction
and Effects increase sit within overlapping ranges. No turn-speedup claim
is justified on this workload and shared host.

These measurements establish removal of one allocation from non-local
outer dispatches. They do not establish a reduction in daemon RSS, idle
CPU, or a universal turn-latency improvement. Other OS/architecture native
performance remains unmeasured; the production state machine is portable
Rust, with Windows cross-lint and Linux/Windows CI recorded separately.

## Validation and reproduction

The invariant failed on the baseline with one allocation for a native
handler dispatch, and passes after the fix. Behavioral tests cover local
creation in an inner handler, release at the outer scope boundary, fresh
state on the next dispatch, reentrant initializers, explicit contexts,
and restoration of unused/initialized/outside scopes after panic.
Existing Lua tests exercise shared callback-chain and invocation budgets.

Phase-1 checks passed: SDK dev tests (87 passed, 3 skipped), SDK release
library tests (85 passed, 1 skipped), the combined SDK/Lua release suite
(133 passed, 1 skipped), `cargo check -p ext`, formatting, clippy with all
SDK targets/features and warnings denied, library documentation, and
`mise run check:windows`. The release suites run the unwind tests that the
default Cranelift dev configuration skips. Ignored measurements are run
explicitly, rather than serving as timing gates in the normal suite.

Commands, with the external target directory and two build jobs exported:

```sh
MISE_NO_ENV=1 mise exec -- cargo check -p ext
MISE_NO_ENV=1 mise exec -- cargo nextest run -p ext
MISE_NO_ENV=1 mise exec -- cargo nextest run --release -p ext -p ext-lua
MISE_NO_ENV=1 mise exec -- cargo clippy -p ext --all-targets --all-features -- -D warnings
MISE_NO_ENV=1 mise run check:windows
```

The ignored allocation workload accepts `A_DISPATCH_WORKLOAD` with
`empty`, `native`, `nested`, `local`, or `explicit`, plus `A_DISPATCH_COUNT=1`.
Use `cargo test --release -p ext --lib point::tests::bench_dispatch -- --exact --ignored --nocapture`.

Raw observations and harness source are retained outside the worktree in
`scratchpad/r3/lazy-dispatch/` under the task's scratch root: `before.json`,
`paired.json`, `summary.json`, `measure.py`, `summarize.py`, `native.rs`,
and its standalone `Cargo.toml`/lockfile. An earlier `paired-initial.json`
records the rejected comparison that built Lua with the candidate only;
its whole-turn and counted SDK samples are excluded from the final table.
The native timing executable had its own identical manifest in both runs.

No speculative hardening was added; no owner-pending hardening item was
identified in these owned paths. Phase 1 does not dispatch CI. The target
and preserved A/B binaries stay outside the worktree until phase 2 cleanup.
