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

## Test temporary directories

Use `tempfile` 3.27 with default features disabled in test infrastructure.
The shared extension rig owns a `TempDir` until its process cleanup completes.
Store and acceptance helpers use exclusive temporary directory creation and
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
The acceptance harness retains its cross-process warm-up lock: its stable name
coordinates different test processes, unlike a temporary isolated directory.
Other individual tests still using incidental paths are not a new shared helper.

## Custom pieces retained for non-size reasons

Byte admission, reserved flush lanes and shutdown accounting express application
resource guarantees, not generic queues. Protocol/LLM codecs express versioned
wire semantics and provider behavior backed by recorded fixtures; a generic
serializer does not replace them. Typed bounded diagnostics enforce privacy and
producer work limits (see [logging.md](logging.md)); log/tracing do not provide
that contract alone. The MCP subset implements byte/page limits and cancellation
on the existing transport (see [mcp.md](mcp.md)). None is retained because a
maintained crate adds executable bytes. This change does not modify those seams.
