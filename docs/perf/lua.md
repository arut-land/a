# Lua runtime selection

Measured 2026-10-21 on macOS arm64, nightly-2026-10-17, at ed4beac.
`mise run size` before changes: 2,963,552 bytes (2894 KiB).

## Method and limits

Four temporary adapters were linked into the real `a`, using its release
profile, build-std, fat LTO, and static vendoring. Each registered an erased
`input` handler and a user tool, `/probe`. The echo agent, transport, policy,
log commits and client replay were real. The handler returned JSON `null`;
the tool returned its JSON input. These small mods measure the embedding and
ABI rather than Lua arithmetic. Production also converts JSON into Lua values
and enforces limits; the probe does neither.

The black-box harness ran three times per configuration, with 200 sequential
handler and tool round trips each, followed by 30 new sessions. Every run used
an isolated home and stopped its daemon on drop. Cold daemon times exclude
first staging: start once, stop, then time `a ls` against the stopped daemon.
RSS is sampled after attach, before work; per-session RSS is the additional
RSS after 30 sessions, with no per-session Lua state. RSS granularity and
allocator effects make these estimates, not allocation counts.

Load was checked before building (1.55/1.86/2.90), and before measurements
(4.68/4.29/3.84; rising to 6.10/4.66/3.99). Builds were stopped during the
measurements. A system security scanner remained active, sometimes over one
CPU. **This was not a quiet machine.** Wall times are observations, not proof
of the latency budgets. Subtracting loaded and unloaded handler times cannot
reliably isolate ABI cost here. Repeat on a quiet machine before enabling Lua
by default.

`mise exec hyperfine@latest -- hyperfine -N --warmup 10 -r 50 ... --version`
measured process startup separately. No VM is constructed on that path.

## Results

Medians of three runs; handler/tool columns are complete client-to-daemon round
trips with one loaded mod, including durable work, in microseconds.

| Candidate | Linked `a`, KiB | Added KiB | `--version`, ms mean | Cold daemon empty / loaded, ms | Handler / tool, µs | Idle RSS empty / loaded, KiB | KiB/session |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Baseline | 2894 | — | 4.1 | 12.43 / — | 300 / — | 7568 / — | 26 |
| mlua 0.12.2 + Lua 5.4 | 3147 | 253 | 4.1 | 13.56 / 11.49 | 227 / 452 | 7648 / 7968 | 20–30 |
| mlua 0.12.2 + LuaJIT | 3531 | 637 | 4.1 | 12.81 / 18.48 | 563 / 1152 | 7584 / 8032 | 19–29 |
| mlua 0.12.2 + Luau | 4112 | 1218 | 5.0 | 17.18 / 23.36 | 446 / 894 | 8384 / 9312 | 18–26 |
| piccolo 0.3.3 | 3251 | 357 | 3.4 | 11.15 / 13.63 | 485 / 961 | 7712 / 7936 | 17–25 |

All comparison binaries include the echo fixtures. Their common baseline is
2,963,600 bytes; deltas use that baseline. The original shipped baseline is
48 bytes smaller. These are minimum adapters, not complete implementations.
Luau already exceeds 4096 KiB with this adapter. Approximate incremental idle
RSS for one mod: 320/448/928/224 KiB respectively. One VM is shared per mod
across sessions; session folds add their JSON payload and engine bookkeeping.

## Capabilities and integration

| Candidate | Sandboxing and limits | Async over `rt` | Windows | Licence |
| --- | --- | --- | --- | --- |
| Lua 5.4 | Select libraries; text-only chunks; memory cap; per-coroutine instruction hooks. Remove protected calls and user coroutines so code cannot swallow budget errors. Native library calls need separate care. | Executor-independent mlua coroutine futures; `send` fits the SDK traits | Upstream supported; selected implementation needs the workspace cross lint | MIT |
| LuaJIT | Remove FFI, package and unsafe libraries. JIT execution complicates instruction hooks; disabling JIT removes its principal advantage here | Same mlua futures | Upstream supports Windows | MIT |
| Luau | Purpose-built sandbox, read-only libraries, interrupts and memory cap | Same mlua futures | Upstream supported | MIT |
| piccolo | Safe host API, incremental GC, fuel-controlled stackless execution; experimental API | Sequences can yield; this probe's non-Send VM needs an owner/mailbox | Portable Rust; not cross-built here | MIT |

Piccolo was compiled and driven by the same daemon harness. Its mailbox blocks
in this comparison adapter; that is an integration cost, not a production
design. An async owner would need a separate design. The other rejected
candidates were not Windows-cross-built either.

`rlua` was checked too: 0.20.1 delegates to mlua and recommends migration, so
it is not an independent runtime candidate.

Primary references: [mlua features and futures](https://github.com/mlua-rs/mlua),
[LuaJIT platforms](https://luajit.org/status.html),
[Luau sandbox](https://luau.org/sandbox/),
[piccolo design](https://github.com/kyren/piccolo),
[rlua migration](https://github.com/mlua-rs/rlua).

## Choice

Use vendored Lua 5.4 through mlua, with `send`, `async` and JSON conversion.
It is the smallest measured adapter, uses the existing runtime, supports
per-coroutine hooks and memory limits, and leaves room for the real extension
API. There is no observed end-to-end reason to spend more size on a JIT.
Luau exceeds the size budget. Piccolo's experimental API and ownership bridge
do not buy a size advantage here. Keep the CLI feature opt-in pending quiet
startup measurements.

## Reproducing the comparison

The archived adapter and harness in `lua-probe/` target ed4beac, before the SDK's
fallible-handler seam changed. In a clean checkout of that commit, copy that
directory into `docs/perf/lua-probe`, apply `integration.patch`, and copy
`client.rs` to `apps/cli/tests/lua_probe.rs`. Build each `probe54`, `probejit`,
`probeau`, `probepure` feature with `fixtures`, using `cargo build-release -p a
--features <feature>,fixtures`, saving each binary outside the next build's
output. Build a `fixtures` baseline too. `cargo test -p a --test lua_probe
--no-run --features probe54` builds the harness; point `A_BIN` at a saved
release binary and run the ignored test with/without `LUA_PROBE_LOADED=1`.
`measure.py` retains the exact local commands and paths; `results.txt` and
`startup.json` retain the raw results. Delete temporary binaries and target
directories afterward.

## Implementation and final verification

The complete implementation uses mlua's `lua54`, `vendored`, `send`, `async`
and `serde` features. The CLI feature is `lua`, currently opt-in.

| Release configuration after merging main | Bytes | KiB, as `mise run size` displays |
| --- | ---: | ---: |
| Default, before this work | 2963552 | 2894 |
| Default, after this work | 2963664 | 2894 |
| Lua enabled | 3487536 | 3405 |
| Lua enabled with fixtures | 3504128 | 3421 |

The minimum adapter comparison above excludes JSON value conversion, the
sandbox, loader and full primitive implementations; these final sizes include
all of them. Both Lua builds remain below 4194304 bytes.

The SDK changes are general seams: fallible erased handlers apply the point's
`ON_ERROR` without a Rust panic; `DynFold` registers JSON state named at run
time; workspace `Sources` return registry generations; delegated calls use
existing policy hooks. Agent JSON codecs live in the SDK that owns their
large types. There are no Lua settings in the kernel or engine.

Final validation after merging main:

- `mise run check`: 811 tests passed, 25 skipped.
- `mise run acceptance`: 12 passed; `mise run test:web`: 5 passed.
- Lua feature: 17 CLI unit tests and 6 black-box tests passed; 12 runtime
  invariant tests are included in the workspace check.
- `mise run size`, `deny`, `machete`, `check:no-tokio`: passed.
- `mise run check:windows`, plus CLI clippy with `--features lua`: passed.
  This Mac needed C headers and a COFF archiver for vendored Lua. The first
  plain cross check failed on missing `errno.h`; the successful check used
  `mise exec cargo:cargo-xwin@latest` (0.23.1), its clang backend/sysroot and
  rustup's LLVM tools (`llvm-lib` driver via `llvm-ar`). No platform cfg was
  added above sys/rt. `check:windows` now sets this up itself on cross hosts,
  including a temporary archiver driver directory that it removes on exit;
  native Windows keeps its normal compiler. The pinned cargo-xwin tool adds
  no runtime dependency or binary bytes. This checks compilation, not Windows
  execution.
- `mise run budgets`: all four strict checks passed. The final Lua release
  with no mods also passed all four strict checks, one test at a time.

| Strict budget observation | Default | Lua enabled, no mods |
| --- | ---: | ---: |
| Cold first daemon reply, median | 17.47 ms | 16.63 ms |
| Process floor | 1.58 ms | 1.45 ms |
| Attach p99 | 0.433 ms | 0.178 ms |
| Idle daemon RSS | 8448 KiB | 8688 KiB |
| TUI first frame, median | 5.60 ms | 5.53 ms |

An earlier Lua budget run failed cold start at 62.82 ms (6.77 ms floor) while
other worktrees compiled and load exceeded 14. A repeat passed all four;
the final observations above are from the rebuilt, quota-limited runtime.
Load averages were still elevated (11.38/12.19/10.43) before the final run.
These passing checks do not turn the experiment into a controlled quiet-machine
comparison. Lua remains opt-in pending that repeat.

The first-mod RSS deltas above include code pages and allocator effects; they
are not linear per-mod heap estimates. Piccolo's ownership constraint was also
confirmed with a compile check: `assert_send::<piccolo::Lua>()` is rejected
because its arena's metrics contain a non-Send `Rc`.

Interactive or deferred approval for a *delegated* `cx.tool` call remains
unsupported: a pending ask is refused. Lua agent tool actions use normal engine
approval. Arbitrary Rust trait services stay on the typed side; Lua delegates
to named tools as its service interface. The active-poll time limit is checked
by instruction hooks, not an OS-enforced hard deadline for native code.

## Review correction: generation preparation

The merged-main baseline is 3,029,872 bytes by default and 3,570,256 bytes
with Lua. The generation/watcher implementation before size tuning is
3,638,864 bytes with Lua (+68,608 bytes). This includes notify 8.2 and the
SDK/engine changes; it does not isolate notify's individual contribution.
notify supplies directory creation, replacement and removal events on all
three supported platforms. The existing sys write watcher handles file writes
and has no Windows implementation. This maintained dependency fits the release
budget; extending that seam by hand would require additional platform backends.

File events schedule bounded preparation off the async worker. The SDK exposes
owned workspace subscriptions and in-memory generation counters, keeping I/O
out of the SDK and the kernel. A binding activates its prepared state view at
the kernel's existing safe boundary. The driver then adopts its registry and
observers; old contexts retain their own state view. State preparation uses the
last session snapshot plus the complete suffix, including unsynced changes,
with contiguous sequence and end-boundary checks. Workspace indexes are weak.

Size tuning, same release flags: sharing the `(Value, Value)` invocation
adapter across agents, hooks, reducers and tool coroutines, and narrowing the
read-only rejection callback to `()`, reduced 3,638,864 to 3,622,368 bytes
(-16,496). Retained.
