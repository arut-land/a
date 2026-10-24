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
profilers do not run during timing. Run provenance retains failures, superseded setup variants and partial rounds.
Primary comparisons use complete matched lanes; incomplete lanes remain diagnostic
evidence and are not silently retried into successful measurements.

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

The retained [analysis script](resource-mechanisms/analyze.py) accepts an
artifact directory grouped as `mechanism/runner`, the run manifest and an output
directory. It validates source revisions and run IDs, then emits raw paired
observations, geometric-mean time ratios, arithmetic-mean memory differences,
paired medians, run-order estimates and bootstrap 95% intervals. A fixed random
seed makes the interval calculation reproducible. Size has one observation per
build; its repeated-looking interval is not a sampling confidence claim.

```sh
python3 docs/perf/resource-mechanisms/analyze.py ARTIFACTS --output REPORT
```

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

The queue's lock-boundary invariant also covers replacement of a registered
receiver waker. The expanded invariant fails when replacement drops that waker
under the queue mutex and passes when the displaced waker is released after
unlocking. This fix is included in the final composition; it is not assumed to
explain the Windows timeout.

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
not manufacture system-wide memory pressure. The first probe releases
74,064–74,068 KiB of RSS on Intel macOS. Apple silicon shows no immediate
RSS release in these observations; the allocator retains freed memory. The
[one-session follow-up](resource-mechanisms/cache-reuse-observations.json) also
shows increasing Apple-silicon RSS across repeated rebuild/reclaim cycles, with
no delayed release after one second. This is not classified as a resource win.
A native allocator probe measures live versus reserved bytes and an explicit
pressure-relief diagnostic before deciding whether the hook is useful there.
[Raw cache observations](resource-mechanisms/cache-benefit-observations.json)
retain all three repetitions per architecture. These hosted images also differ
in OS version: Intel runs macOS 15 and ARM runs macOS 26; the allocator behavior
is not attributed to architecture alone. Its patch and run provenance are
retained beside the mechanism variants.

Full-versus-full A/A comparisons on all six targets build the identical
`e2b56c644a0f5456c639225352cf219db1069230` revision in both positions. These
validate build-position effects and the observed runtime-control variation.
The harness records binary hashes for both builds. This control was added after
the first worker-sizing comparisons showed CPU differences despite matching
worker counts.

The [native allocator diagnostic](resource-mechanisms/cache-allocator-observations.json)
records cache population, clearing, a one-second delay and explicit pressure
relief. Reported live allocator bytes fall by about 72 MiB on Intel and 129 MiB
on macOS 15 ARM. ARM RSS remains high. Explicit pressure relief reports zero
released bytes and does not justify an allocator policy change. Some ARM zone
counters wrap after relief and the counters are advisory; none is substituted
for RSS. The uninstrumented cache observations remain separate.

In all 20 Intel QoS pairs, `vmmap` reports identical retained allocation counts
and live bytes at its reported precision. Its RSS delta therefore is not
attributed to retained allocation growth. Page residency, image layout and
allocator fragmentation also affect RSS. The driver fixtures do not run the
daemon supervisor or open a persistent catalog; those CPU results compare the
compiled images, rather than demonstrating CPU consumed by an inactive probe.

## Observations

The [per-target table](resource-mechanisms/observations.md) reports complete
matched lanes and their intervals. [Raw paired values](resource-mechanisms/effects.csv)
include peak RSS, steady idle after retirement, regular-size workloads, CPU
components, run order and image size. Results are conditional on each runner;
within-host bootstrap intervals do not establish variation across hardware.

## Pressure implementation revisions

Both zero-state pressure revisions are rejected. The generic revision increases
Linux ARM large-JSON CPU by about 2.5% relative to the full implementation; the
independent-signal revision increases it by about 4.8% and increases Linux x64
driver CPU by about 1.8%. Neither establishes an RSS saving. Their completed
lanes remain in the table; the generic revision's queued Mac lanes are cancelled
because that implementation is no longer selected.

The supervisor retains its small shared observer. Its event counter is captured
before native subscription so a warning delivered while subscribing stays
pending. This does not add another public runtime interface.

## Decisions

Windows storage lookup is removed. Its enabled-minus-disabled idle cost is
55.4 KiB [31.6, 80.2] on x86_64 and 18.2 KiB [10.0, 26.8] on aarch64
(20 paired rounds, bootstrap 95% intervals). It changes neither target's selected
pool widths; ARM reports unknown media even with the query. The unadjusted x86_64 cold-reply delta is 1.88% [0.91, 2.95], while the
same-source control is 2.43% [0.68, 4.46]; no cold-latency benefit is attributed
to removing the query. The RSS cost has no qualified slow-disk benefit.
The remaining decisions await the complete comparisons and same-build controls.

## Incomplete runs

The initial catalog-off probe incorrectly started four replay threads even for
an empty store. The corrected probe retains the original `min(headers, 4)`
bound. The run manifest identifies both revisions and their supersession reason.

Run 40052161443 failed after eight pairs when the unchanged full-reference
Windows daemon stopped responding to a resume request. Its daemon log has no
panic. A separate isolated availability campaign compares full, main, compact
oneshot and a foreground traced daemon, preserving failures before cleanup.
The cause is unresolved; it is not attributed to catalog admission.
The [availability campaign](resource-mechanisms/availability-observations.json)
reproduces the same timeout on main after 6,126 resume attempts, and on full,
traced-full and compact builds. All four answer a later probe. This establishes
that the failure pre-exists O15, not that its frequency is unchanged. A separate
campaign exercises the displaced-waker fix. The initial diagnostic's copy of a
held endpoint claim file fails with WinError 32; collected phase records and
stacks survive. The later probe records copy errors without losing its summary. Its
[waker-drop campaign](resource-mechanisms/additional-runs.json) completes all
200 phases and 10,000 resumes with no timeout. The native hosts differ, so this
is supporting evidence rather than causal proof that the lock-boundary fix
resolves the transient timeout.

Run 40052186159 failed in the catalog-off Windows ARM benchmark after five
pairs. Hyperfine did not expose the benchmark's failure output, so its cause is
unresolved. The corrected catalog probe is measured separately.

Run 40052168784 stalled in macOS `psutil.memory_full_info`, after idle timing,
until the job timeout. Subsequent Mac diagnostics use a bounded `vmmap` subprocess
and record diagnostic timeouts without extending the measured idle window.
Queued and unfinished Mac lanes using the unbounded probe were superseded;
completed lanes retain their original observations.
