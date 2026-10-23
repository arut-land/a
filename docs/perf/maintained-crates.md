# Maintained infrastructure crates

The production YAML decision and hosted parser comparison live in
[skills.md](skills.md). Size is reported separately from reliability and
runtime cost. Channels belong to the runtime work; paths/XDG and source reload
behavior belong to their respective backends and are outside this change.

## Signal ownership

Keep the native signal source in `sys`, for a lifecycle reason. Linux and macOS
consumers acquire an exclusive lease, coalesce four signal kinds into an atomic
word and use a process-lifetime nonblocking pipe as the wake source. Dropping a
lease restores default dispositions; a later consumer can reacquire it. The
permanent pipe prevents a handler already in flight from writing to a recycled
FD. Linux creates it with `pipe2(CLOEXEC | NONBLOCK)` atomically. Darwin uses
`pipe` plus flags (there is no Darwin pipe2); this does not claim atomic
creation-time inheritance on Darwin. Neither backend polls while idle.

`signal-hook` 0.4.5 is maintained and proportionate in size, but its
[unregister contract](https://docs.rs/signal-hook/0.4.5/signal_hook/low_level/fn.unregister.html)
explicitly leaves its handler installed: unregistering the final action for
SIGTERM effectively ignores SIGTERM. Its
[registration contract](https://docs.rs/signal-hook/0.4.5/signal_hook/low_level/fn.register.html)
also chains an existing handler. Manually restoring sigaction behind that
registry is not a supported substitute for a reacquirable exclusive lease.
Adopting it would retain bespoke disposition ownership while adding a second
process-global registry. That reliability mismatch disqualifies replacement
before timing or size can select it.

| Dimension | Native source | signal-hook candidate |
| --- | --- | --- |
| Memory | Two retained FDs, fixed atomic state; per-lease caught list | Registry, registered closures/IDs plus a wake source and application ownership state |
| CPU | One atomic coalescing update and nonblocking write per caught signal; no idle work | Registry dispatch plus configured actions; self-pipe delivery can also have zero idle work |
| Reliability | Exclusive lease, full-pipe coalescing, default restoration, reacquisition | Safe maintained registration helpers, but final unregister does not restore defaults |
| Dependency tree | Existing libc/rustix | signal-hook-registry and libc, with feature-dependent helpers |
| Size | Included in per-target shipped measurements | Not linked: lifecycle-incompatible; no byte-saving claim |

Windows retains its native console handler and manual-reset event. POSIX signal
registration is not a replacement for console close/logoff/shutdown callbacks.
No signal handlers are installed by embedded library initialization.

The hosted comparison also reports registration/release wall time, sampled RSS,
normal dependency graph and probe bytes for 1,000,000 SIGWINCH leases/hooks per
round (five rounds, Unix only). The hook action sets a shared atomic flag: that result is a lower
bound, excluding the wake source and exclusive ownership still needed for
integration. These are lease churn measurements, not signal-delivery throughput.
The existing full-pipe and exclusivity tests cover coalescing and ownership;
exclusivity also verifies default disposition restoration and CLOEXEC.

## Test temporary directories

Use `tempfile` 3.27 with default features disabled in test infrastructure.
The shared extension rig owns a `TempDir` until its process cleanup completes.
Store, engine, client, FFI, shell-output and crash-test helpers use exclusive temporary directory creation and
transfer cleanup ownership with `keep()`: stores already own cleanup, and the
acceptance harness must stop daemons and retry Windows handle release before
removing homes. CI's RUNNER_TEMP preference remains explicit.

| Dimension | PID/counter paths | tempfile |
| --- | --- | --- |
| Reliability | Predictable reuse across runs; some helpers remove an existing directory before use | Exclusive create with randomized names and collision retries; RAII where ownership permits |
| Memory | Path allocation; some global counters | Path owner and random-name generation; no resident task or queue |
| CPU | Create/remove syscalls; bespoke name formatting | Same filesystem work plus random suffix/collision retry; no idle work |
| Dependencies | std | cfg-if, fastrand, once_cell, rustix on Unix / windows-sys on Windows; existing graph dependencies |
| Shipped size | No test helper linked | Test-only or optional `ext/testing`; absent from the shipped dependency graph |

Default getrandom reseeding is unnecessary for these short-lived test roots;
exclusive creation is the invariant. This is not an API for publishing user
files. Edit transactions and executable staging retain their separate ownership
and metadata/durability contracts; those files belong to other sessions.
The hosted infrastructure probe runs five rounds of 10,000 create/drop pairs
for std PID/counter directories and TempDir, reporting ns/pair, sampled process
RSS and exact probe bytes. Filesystem noise affects both; no daemon runtime
claim follows from test-helper timings.

The acceptance harness retains its cross-process warm-up lock: its stable name
coordinates different test processes, unlike a temporary isolated directory.
Path/backend-specific fixtures and watcher fixtures remain with their owning sessions; individual incidental paths are not a new shared temporary-directory abstraction.

## Custom pieces retained for non-size reasons

Byte admission, reserved flush lanes and shutdown accounting express application
resource guarantees, not generic queues. Protocol/LLM codecs express versioned
wire semantics and provider behavior backed by recorded fixtures; a generic
serializer does not replace them. Typed bounded diagnostics enforce privacy and
producer work limits (see [logging.md](logging.md)); log/tracing do not provide
that contract alone. The MCP subset implements byte/page limits and cancellation
on the existing transport (see [mcp.md](mcp.md)). None is retained because a
maintained crate adds executable bytes. This change does not modify those seams.

## Hosted temporary-directory observation

Linux x86_64, five rounds of 10,000 create/drop pairs:

| Helper | Probe bytes | ns/pair | Sampled RSS KiB |
| --- | ---: | ---: | ---: |
| temp-custom | 93776 | 46401 | 1868 |
| temp-maintained | 108976 | 55499 | 1824 |

The observed tempfile cost is 9.10 µs/pair; the small RSS difference is not a
memory-saving claim. Exclusive creation and workspace ownership remove real
collision/deletion failure modes. Both implementations have zero idle work,
and neither helper is linked into the shipped executable.

## Shipped size and paired runtime resources

[Hosted paired run](https://github.com/arut-land/a/actions/runs/39977920802)
compares production revision `28458988` with main
`065b9331ade244b56e27cb7e457cb8d9a75fcbf0`. Each native runner alternates
reference/candidate order for five rounds. Size excludes fixtures; idle and
loaded workloads use deterministic fixtures. The default catalog is empty, so
header-decoding CPU is assessed separately by the corpus probe above.

| Native target | Main bytes | Candidate bytes | Delta bytes |
| --- | ---: | ---: | ---: |
| aarch64-unknown-linux-gnu | 3626792 | 3668904 | +42112 |
| x86_64-unknown-linux-gnu | 4207520 | 4260512 | +52992 |
| aarch64-pc-windows-msvc | 4763136 | 4811264 | +48128 |
| x86_64-pc-windows-msvc | 5411840 | 5468160 | +56320 |

Median candidate-minus-reference deltas (observations, not speedup claims):

| Native target | Startup ms | Startup CPU ms | Cold reply ms | Attach p50 ms | Idle RSS KiB | Driver CPU ms | Driver peak RSS KiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| aarch64-unknown-linux-gnu | +0.018 | +0.010 | +0.340 | -0.008 | +36.000 | +17.897 | +200.000 |
| x86_64-unknown-linux-gnu | -0.011 | -0.008 | -0.036 | +0.014 | -32.000 | +18.542 | +32.000 |
| aarch64-pc-windows-msvc | -0.069 | +0.000 | -3.939 | +0.022 | +0.000 | -109.375 | +36.000 |
| x86_64-pc-windows-msvc | +0.248 | +0.000 | -0.622 | +0.016 | +32.000 | -62.500 | -256.000 |

| Native target | Large JSON CPU ms delta | Large JSON peak RSS KiB delta | Idle daemon CPU ms (main → candidate) | Idle line-client CPU ms (main → candidate) |
| --- | ---: | ---: | ---: | ---: |
| aarch64-unknown-linux-gnu | +29.625 | -24.000 | 0.000 → 0.000 | 0.000 → 0.000 |
| x86_64-unknown-linux-gnu | +46.727 | -120.000 | 0.000 → 0.000 | 0.000 → 0.000 |
| aarch64-pc-windows-msvc | -46.875 | +56.000 | 0.000 → 0.000 | 0.000 → 0.000 |
| x86_64-pc-windows-msvc | -187.500 | -56.000 | 0.000 → 0.000 | 0.000 → 0.000 |

Short Windows startup CPU samples round to zero at the accounting clock's
resolution; they are not proof of free startup. All other deltas should also
be read with their distributions, not as performance wins. Raw p10–p90,
all loaded workloads and all five rounds remain in each target's artifact.
