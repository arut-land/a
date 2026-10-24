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
variation is reported alongside the runtime workloads.

## Variants

| Mechanism | Targets | Disabled behavior |
| --- | --- | --- |
| Scoped QoS | macOS x86_64/aarch64 | Per-job foreground and catalog utility guards |
| Pressure reclamation | All six | Source, supervisor notification allocation and pressure polling branch |
| Storage lookup | All six | IOKit on macOS, disk-kind lookup on Windows, sysfs rotation lookup on Linux; medium becomes unknown |
| Worker sizing | All six | Rotational override and explicit async-worker clamp; core formula remains the pre-O15 formula |
| Effective CPU discovery | Windows x86_64/aarch64 | Additional process CPU-set/job restrictions; standard discovery remains |
| Oneshot | All six | Maintained channel replaced by the original shared type-erased channel |
| Catalog admission | All six | Startup/replay use their original independent execution rather than shared storage admission |

The Linux/macOS effective-CPU implementation already delegates to the standard
library used before O15, so there is no additional discovery mechanism to disable
on those platforms. Non-macOS QoS guards are empty.

## Decisions

Pending hosted results.
