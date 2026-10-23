# Lua runtime selection

Lua remains opt-in (`--features lua`). No timing result here establishes a
startup or call-overhead budget: those measurements are deferred until a quiet
machine is available. Earlier runs competed with builds and a security scanner;
their timings have been withdrawn.

## Comparison and limits

The original comparison linked small adapters into the real daemon, using its
release profile, build-std, fat LTO and vendored runtimes. Each adapter registered
an erased input handler and a user tool, driven through the echo agent, policy,
log commits and client replay in isolated homes. The adapters passed JSON text
without the production conversion, sandbox or full primitives. These are
historical minimum-embedding sizes, not current production sizes.

| Candidate | Linked binary, KiB | Added KiB | Approximate first-mod RSS delta, KiB |
| --- | ---: | ---: | ---: |
| Fixture baseline | 2894 | — | — |
| mlua 0.12.2 + Lua 5.4 | 3147 | 253 | 320 |
| mlua 0.12.2 + LuaJIT | 3531 | 637 | 448 |
| mlua 0.12.2 + Luau | 4112 | 1218 | 928 |
| piccolo 0.3.3 | 3251 | 357 | 224 |

The common baseline was 2,963,600 bytes. RSS includes code pages, allocator
behavior and coarse sampling; it is not a linear per-mod heap cost. Sessions
share mod VMs; their folds add JSON values and engine bookkeeping. There is no
reliable comparative per-session memory or call-latency result. Piccolo's
experimental, non-Send VM used a blocking mailbox in the temporary adapter;
that integration limitation cannot establish production performance.

| Candidate | Sandbox and limits | Async over `rt` | Windows / licence |
| --- | --- | --- | --- |
| Lua 5.4 | Selected libraries, text-only chunks, allocation cap, coroutine instruction hooks; native calls need separate limits | Executor-independent mlua coroutine futures, `send` | Upstream support; native CI lints production feature / MIT |
| LuaJIT | FFI/package removed; JIT complicates instruction hooks; disabling it loses its main advantage | mlua futures | Upstream support, comparison not cross-built / MIT |
| Luau | Read-only libraries, sandbox, interrupts, allocation cap | mlua futures | Upstream support, comparison not cross-built / MIT |
| piccolo | Safe host API, fuel-controlled execution, incremental GC, experimental API | Yielding sequences need an async owner bridge | Portable Rust, comparison not cross-built / MIT |

`rlua` 0.20.1 delegates to mlua and recommends migration; it is not an independent
runtime candidate. References: [mlua](https://github.com/mlua-rs/mlua),
[LuaJIT platforms](https://luajit.org/status.html),
[Luau sandbox](https://luau.org/sandbox/),
[piccolo](https://github.com/kyren/piccolo),
[rlua](https://github.com/mlua-rs/rlua).

Vendored Lua 5.4 through mlua is the smallest measured adapter, has allocation
and instruction limits, and fits the existing async runtime.
This choice makes no claim about comparative timings.

## Generation preparation

File events schedule preparation off the async worker. Owned workspace
subscriptions expose an in-memory generation counter; unchanged bursts perform
no source scan, log read or blocking dispatch. The index holds weak references.
Calls retain their immutable registry and their generation's state view.

Compilation validates the whole candidate before publication. Initial loading
omits invalid mods with diagnostics; an invalid edit retains the published
candidate. `/lua-reload` uses the same preparation path and acknowledges it.
Preparation is serialized per workspace, independently of other workspaces.
There are at most 128 mods, 4096 directory entries per root and 1 MiB of combined
source; compilation also has a one-second aggregate budget in addition to VM
budgets. Filesystem calls themselves do not have an OS-enforced deadline.

Session folds prepare off the worker from the latest session snapshot and its
complete suffix, including folded-but-unsynced entries. Sequence and final
boundary checks reject an incomplete read. Preparation is bounded to 100,000
suffix entries and one second; rejection retains the old binding and reports an
error. Pending folds receive subsequent changes once while adoption waits.
The binding activates its state view at the kernel's safe boundary; the driver
adopts registry, handlers and binding there before dispatching effects.

The SDK's added seams are generic: owned workspace subscriptions with a blocking
preparation callback, fallible erased fold restoration/application, and binding
activation. The SDK remains I/O-free. There are no Lua knobs in the kernel.

notify 8.2 (CC0-1.0) supplies directory creation, replacement and removal events across
supported platforms. The existing sys watcher only follows writes to one file
and has no Windows backend. The generation/watcher changes added 68,608 bytes
before tuning, including notify and the SDK/engine changes; this does not isolate
notify's individual contribution. The maintained implementation avoids implementing three additional watcher backends by hand. Its Windows backend pins
windows-sys 0.60 while sys/the reactor use 0.61; the deny exception is scoped
to 0.60.2. No Windows types cross between these backends.

## Release size experiments

All numbers use the same merged main and release flags on macOS arm64 with
nightly-2026-10-17. Size is reported without a cap.

| Build or experiment | Bytes | Decision |
| --- | ---: | --- |
| Merged baseline, default | 3,029,872 | Baseline |
| Merged baseline, Lua | 3,570,256 | Baseline |
| Generation/watcher fixes, Lua | 3,638,864 | Required correctness work |
| Narrow rejection callback and shared invocation adapters | 3,622,368 | Keep: saves 16,496 |
| Integer handles instead of userdata carriers | 3,605,840 | Keep: saves another 16,528 |
| Direct bounded JSON-to-Lua conversion | 3,605,840 | Discard: no measured reduction |
| Final default, including preparation bounds and lint fixes | 3,029,888 | +16 versus baseline |
| Final Lua | 3,605,792 | +35,536 versus baseline |

Invocation handles live in a per-VM map and are removed by a drop guard on
completion or cancellation. Saved callbacks retain no call context. The direct
conversion experiment integrated depth/node accounting with table construction;
it did not reduce the binary, so the bounded serde conversion remains.

## Reproduction

The old adapter archive has been removed from the current tree. For the
historical comparison, use a disposable detached checkout of `96ce0e7`, whose
archive and integration patch match the SDK at that revision. Apply its
`docs/perf/lua-probe/integration.patch`, copy its `client.rs` into
`apps/cli/tests/lua_probe.rs`, then build the four adapters:

```sh
cargo build-release -p a --features fixtures
cargo build-release -p a --features probe54,fixtures
cargo build-release -p a --features probejit,fixtures
cargo build-release -p a --features probeau,fixtures
cargo build-release -p a --features probepure,fixtures
cargo test -p a --test lua_probe --features probe54 --no-run
```

Save each release binary before the next build. Locate the test executable from
Cargo's output, rather than a hard-coded build hash, and run it with `A_BIN`
pointing at each saved binary, with and without `LUA_PROBE_LOADED=1`. Do not use
the historical measure.py's local paths. Remove the disposable checkout and its
build output afterward. These historical adapters require fresh integration
before making any comparison against the current SDK.

For current production sizes and behavior:

```sh
mise run size
cargo build-release -p a --features lua
wc -c < target/release/a
cargo nextest run -p a --features lua,fixtures --test blackbox -E 'test(/^lua::/)'
mise run check
mise run acceptance
mise run check:windows
mise run machete
```

For future timing measurements, check load first, stop other builds, use real
client-to-daemon handler/tool round trips and isolated homes, and run the strict
release budgets alone (`mise run budgets`). Pull temporary tools through
`mise exec hyperfine@latest -- hyperfine ...`. Stop each owned daemon using
`A_HOME="$home" "$binary" daemon stop` before deleting its temporary home.
No quiet-machine timing claims are made by this change.

## Review validation

- `mise run check`: 867 passed, 27 skipped; includes 19 Lua host invariants.
- `mise run acceptance`: 12 passed. Lua-feature black-box tests: 6 passed.
- Native CLI Lua-feature clippy, `machete`, `deny`, and `check:no-tokio`: pass.
- `mise run check:windows`: pass, using plain MSVC-target clippy with ext-lua
  excluded and the CLI Lua feature off. No cargo-xwin or licence-acceptance
  mechanism runs. Native Windows Lua lint is configured in CI; it was not
  executed from this Mac.
- Default `mise run size` and the identically configured Lua release size
  check: pass, with no fixtures in either shipped binary.
- `mise run budgets`: 4 daemon/client checks and the shell zero-wake check pass.
  It ran alone locally. Load averages before/after were 3.61/3.08/4.60 and
  3.36/3.08/4.53; security scanning remained active. This was not a controlled
  quiet-machine timing comparison.

The pending-state/adoption regression failed on the merged old driver (a running
call read zero after one folded change). The inbound JSON and sandbox regressions
also failed before their fixes. A delegated-result test was additionally checked
against the old unbounded decoder: it failed there and passed with bounded
conversion restored. The new subscription API is covered by controlled queued
publication, no-blocking-hop bursts, weak-lifetime tests and a real OS file-event
test; it deliberately replaces the polling contract rather than preserving it.
