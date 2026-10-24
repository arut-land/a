# Runtime mechanism costs

The experiment compares one disabled mechanism with the full runtime policy on
the same hosted runner. Its purpose is to separate startup, resident-code and
allocation costs from changes in the active driver path. Runtime policy decisions
remain pending until the measurements complete.

## Method

The full reference is `93344bb06d9096103ecf6ed18d2d78058a30791d`. Each variant
changes one production mechanism and shares measurement harness
`e2b56c644a0f5456c639225352cf219db1069230`. The
[run manifest](resource-mechanisms/runs.json) records exact revisions, native
runners and run IDs; the adjacent patches contain the production differences.

Each lane starts with 20 alternating reference/candidate rounds. The harness
retains individual matched pairs, user/kernel CPU, wall time, peak RSS, idle RSS,
idle CPU, cold reply and attach latency. Runtime facts record effective and
standard CPU counts, page bytes, detected medium and selected pool widths.
Memory-map diagnostics run after the idle accounting window. Compilers and
profilers do not run during timing. No observations are discarded.

The analysis reports enabled-minus-disabled memory costs and paired time ratios,
with bootstrap intervals over complete pairs and both run orders inspected.
Unresolved variation requires more observations; an interval crossing zero alone
does not establish equivalence. Native or reliability benefits must name the
invariant and its test, and state their measured cost.

Additional 20-round comparisons of the full branch against main
`4b662d7ab8151802b8417e71378c8ec800834f34` on Intel macOS and Windows x86_64
check the CPU regressions directly. The decoder is an unchanged control; its
variation is reported alongside the runtime workloads. It is not subtracted
from the runtime results: its source is unchanged, but it still participates in
the build/dependency graph, so a change can also reflect code or dependency
layout.

## Variants

| Mechanism | Targets | Disabled behavior |
| --- | --- | --- |
| Scoped QoS | macOS x86_64/aarch64 | Per-job foreground and catalog utility guards |
| Pressure reclamation | All six | Source, supervisor notification allocation and pressure branch |
| Storage lookup | All six | IOKit on macOS, disk-kind lookup on Windows, sysfs rotation lookup on Linux; medium becomes unknown |
| Worker sizing | All six | Rotational override and explicit async-worker clamp; core formula remains the pre-O15 formula |
| Effective CPU discovery | Windows x86_64/aarch64 | Additional process CPU-set/job restrictions; standard discovery remains |
| Oneshot | All six | Maintained channel replaced by the original shared type-erased channel |
| Catalog admission | All six | Startup/replay use their original independent execution rather than shared storage admission |

The Linux/macOS effective-CPU implementation already delegates to the standard
library used before O15, so there is no additional discovery mechanism to disable
on those platforms. Non-macOS QoS guards are empty.

The pinned Rust Windows implementation of `available_parallelism` reads
`GetSystemInfo`. It does not apply process CPU sets or job quotas. Native child
process tests exercise the additional affinity, CPU-set and hard-quota bounds;
the final decision must report the measured cost of retaining those bounds.

## Cancellation reliability

The original channel calls its receiver waker while holding the slot mutex.
`cancellation_allows_a_waker_to_poll_its_receiver` registers a waker that polls
the receiver immediately. Dropping the sender with the original implementation
fails the bounded test by deadlocking; the maintained implementation and the
compact wrapper pass. Restoring the original channel unchanged would reintroduce
that failure.

An additional compact-oneshot variant retains the maintained state machine and
uses a shared type-erased payload representation. It tests whether sharing the
wake/cancel code across payload types reduces resident code enough to offset
the extra payload allocation. It has the same 20-round, six-target protocol.
Its retention remains pending measurements.

A worker-QoS variant keeps foreground priority for the lifetime of each owned
blocking worker and retains catalog utility scopes. It removes per-job
get/set/restore transitions while leaving the requested class during each job
unchanged. Both native Mac targets use the same 20-round protocol.

A separate [cache-benefit probe](resource-mechanisms/cache-benefit-run.json)
measures process RSS before populating projections, after populating them, and
after an idle reclamation request. It uses a controlled 36 MiB in-memory log,
three repetitions and both native Mac architectures. This measures the cache
release benefit separately from the source subscription's idle cost; it does
not manufacture system-wide memory pressure. Its patch and run provenance are
retained beside the mechanism variants.

## Decisions

Pending hosted results.
