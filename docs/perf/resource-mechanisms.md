# Runtime mechanism costs

The experiment compares one disabled mechanism with the full runtime policy on
the same hosted runner. Its purpose is to separate startup, resident-code and
allocation costs from changes in the active driver path. A mechanism stays only
when it is neutral or better on memory, CPU and latency, or when it fixes a
named native or reliability problem at a cost stated below.

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
relief. Reported live allocator bytes fall by about 72 MiB on Intel, 129 MiB
on macOS 15 ARM and 96.5 MiB on macOS 26 ARM. ARM RSS remains high. Explicit pressure relief reports zero
released bytes and does not justify an allocator policy change. Some ARM zone
counters wrap after relief and the counters are advisory; none is substituted
for RSS. The uninstrumented cache observations remain separate.

In all 20 Intel QoS pairs, `vmmap` reports identical retained allocation counts
and live bytes at its reported precision. Its RSS delta therefore is not
attributed to retained allocation growth. Page residency, image layout and
allocator fragmentation also affect RSS. The driver fixtures do not run the
daemon supervisor or open a persistent catalog; those CPU results compare the
compiled images, rather than demonstrating CPU consumed by an inactive probe.

## Assembled validation

The [assembled comparisons](resource-mechanisms/composed-runs.json) measure the
selected composition against `main` on all six targets, 20 matched rounds
each: first `442f8f69` against `8a0388a7`, then the branch with current `main`
merged (`c6a01c02`) against `main` itself (`8e460c73`). The manifest records
the enabled role, reference revision and the superseded setup and build
failures. Against current `main`:

| Target | Idle RSS, KiB | Driver CPU, % | Large-JSON CPU, % | Cold reply, % | Size, KiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| Linux x86_64 | +7.8 [−16.4, +30.4] | +0.01 [−0.68, +0.78] | −0.12 [−0.76, +0.60] | −13.98 [−51.72, +53.59] | +7.0 |
| Linux aarch64 | +10.8 [6.2, 15.8] | −0.56 [−1.57, +0.12] | −0.46 [−1.64, +0.72] | +1.10 [−2.95, +5.33] | +6.6 |
| Windows x86_64 | +31.8 [10.6, 54.8] | −1.35 [−4.24, +1.57] | +0.76 [−1.75, +3.50] | +1.48 [−0.08, +2.97] | +18.0 |
| Windows aarch64 | +18.8 [11.6, 25.8] | +4.40 [1.28, 7.36] | −0.05 [−1.92, +1.97] | +1.34 [−1.00, +3.61] | +11.5 |
| macOS 15 Intel | +35.4 [23.2, 49.2] | +0.61 [−6.27, +8.14] | +3.51 [−2.20, +9.45] | −2.89 [−12.26, +7.11] | +12.8 |
| macOS 26 Apple silicon | −3.2 [−19.2, +14.4] | −10.93 [−19.67, −1.55] | −10.03 [−17.49, −1.83] | +13.60 [2.47, 28.96] | +17.0 |

The idle RSS that remains is the kept mechanisms' stated cost: Windows CPU
discovery and catalog admission on Windows, QoS, pressure and catalog on the
Macs. Windows aarch64 driver CPU exceeds its same-source control (−2.34%
[−6.63, +2.19]) and no single mechanism lane showed it; it is reported, not
attributed. Idle CPU is zero on every target.

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

Deltas are enabled-minus-disabled over 20 matched pairs with bootstrap 95%
intervals; idle RSS in KiB, CPU and latency in percent. Same-source A/A
controls reach +5.0 KiB [0.8, 9.4] idle RSS on Linux aarch64 and +0.88%
[0.44, 1.30] driver CPU on Linux x86_64, and a mechanism whose code never runs
on a target still moved it by up to 13 KiB. Within-run intervals therefore
understate build-layout variation, and smaller deltas are treated as noise.

| Mechanism | Per-target delta | Decision |
| --- | --- | --- |
| Storage-media lookup (sysfs, Windows disk kinds, IOKit) | Idle RSS Windows x86_64 +55.4 [31.6, 80.2], aarch64 +18.2 [10.0, 26.8], Intel macOS +56.0 [43.0, 68.4], Apple silicon +40.0 [10.4, 69.6] KiB; Linux x86_64 +22.2 [−3.0, +51.2] KiB and attach p99 +16.75% [−0.88, +47.15]. No hosted runner selected a different pool width, and no slow-disk benefit is qualified | Drop on every OS |
| Worker override (rotational storage width, async clamp in `Threads::new`) | No pool width changes on any runner; Linux x86_64 driver CPU +3.14% [2.22, 4.02] against a +0.88% control; Intel macOS large-JSON CPU +3.47% [0.24, 7.06] with idle RSS −14.8 KiB | Drop; `main`'s sizing remains |
| Maintained `oneshot` 0.2 | Idle RSS Linux aarch64 +31.0 [26.4, 36.0], x86_64 +46.2 [3.0, 91.4], Windows aarch64 +11.6 [6.2, 16.6], Intel macOS +17.2 [1.0, 34.4] KiB; Windows x86_64 large-JSON CPU +3.16% [1.24, 5.00]; driver CPU Apple silicon −2.40%, Linux aarch64 large-JSON CPU −4.98% | Drop; the shared slot returns with its wake and displaced-waker drop outside the lock |
| Compact oneshot revision | Idle RSS +18.4 to +39.6 KiB on three non-Mac targets, −17.4 and −59.2 KiB on the Macs; Linux aarch64 cold reply +7.08% | Drop: no consistent gain over the shared slot it was meant to beat |
| Generic and independent pressure revisions | Linux aarch64 large-JSON CPU about +2.5% and +4.8% relative to the selected implementation, no RSS saving | Drop |
| Effective CPU discovery (Windows CPU sets and job rate) | Idle RSS +21.0 [14.4, 28.2] aarch64, +22.2 [2.0, 43.8] x86_64 KiB; CPU and cold reply within noise; x86_64 attach p99 −5.75% | Keep: the standard query ignores CPU sets and job hard caps, so a limited process would oversubscribe. Cost: about 21 KiB idle RSS on Windows |
| Catalog admission | Idle RSS Linux aarch64 +12.8, Windows aarch64 +7.8, Intel macOS +46.4 [31.8, 60.2], Apple silicon −35.2 [−58.4, −12.8] KiB, x86_64 Linux/Windows within noise; driver CPU Linux aarch64 −1.23%, Windows aarch64 −4.89% | Keep: replay and rebuild share the storage limit and complete with one worker. Cost: up to 13 KiB idle RSS off the Mac; the two Macs disagree in sign |
| Scoped QoS (macOS) | Idle RSS Intel −39.6 [−59.8, −23.6], Apple silicon +44.0 [15.2, 72.0] KiB; Intel cold reply −9.97% [−24.18, −0.40]; CPU within noise on both | Keep: foreground jobs at user-initiated and catalog work at utility is how macOS schedules for responsiveness and energy. Cost: up to 44 KiB idle RSS on Apple silicon |
| Pressure reclamation (macOS dispatch source) | Idle RSS Intel −6.4 [−21.8, +9.2], Apple silicon +35.2 [11.2, 61.6] KiB; Apple silicon attach p99 +36.97% [16.10, 63.75] (sub-millisecond); other targets compile it out and move only by layout noise | Keep: a populated 36 MiB log releases 74 MiB RSS on Intel macOS 15 and 72–129 MiB of live allocator bytes on Apple silicon. Cost: about 35 KiB idle RSS and attach p99 on Apple silicon |
| Displaced receiver waker dropped outside the queue lock | Inside the assembled comparison; 10,000 Windows resumes without the pre-existing timeout | Keep: reliability, `consumer_wakes_and_discarded_values_never_run_under_the_queue_lock` |
| Worker-lifetime QoS revision | Relative to scoped QoS: idle RSS Intel +9.8 [−6.6, +27.6], Apple silicon +0.0 [−30.4, +29.6] KiB, CPU within noise | Not adopted: no gain over scoped QoS |

`cancellation_allows_a_waker_to_poll_its_receiver` deadlocks with the shared
slot as it was on `main` and passes once the waker runs after unlocking. That
change costs nothing measurable: the oneshot variant above already measures
the shared slot's code.

The 5-round CPU regressions that started this attribution do not reproduce
with 20 matched rounds: driver CPU is −2.88% [−8.72, +3.65] on Intel macOS
and +0.27% [−4.35, +5.08] on Windows x86_64 for the full branch against
`main`. Its idle RSS cost does reproduce, +124.2 KiB [101.6, 145.8] and
+86.2 KiB [62.0, 108.6]; storage lookup, the maintained oneshot and Windows CPU
discovery account for most of it.

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
