# Native allocator release at hibernation

Work in progress on `native-release`; no platform enablement decision yet.

## Experiment

The reference is `5610d9e021bbf83044a80974c7f6eac147fa2564`, the branch base
containing the merged mem-release work. Candidate measurements must use distinct
release executable hashes, five alternating same-runner pairs, and separate
uninstrumented timing and instrumented allocator cohorts. Native charge means
Linux PSS, macOS physical footprint and Windows private commit; it is not RSS.

The experimental hibernation guard schedules a CPU-pool job after residency
locks are released and the last actor has released its state. The residency
index owns a cancellation lease; any session load abandons pending release.
There are no release timers or polling. Cancellation applies before the native
call starts; none of the native APIs makes an in-progress call cancellable.

macOS targets its default zone, Windows its process heap, and glibc its public
trim API. This initial experiment has no guessed capacity threshold. It is a
calibration candidate, not the intended production decision.

## Mechanism constraints

[glibc malloc.c](https://github.com/bminor/glibc/blob/release/2.39/master/malloc/malloc.c)
shows that `malloc_trim(pad)` walks every arena under its mutex and uses
`MADV_DONTNEED` on free interior pages. The pad applies to the main arena's top
chunk; it does not restrict the operation to that chunk or protect non-main
arenas. The public API does not select the retired session's arena. `mallinfo2`
also traverses free bins under allocator locks; its counters are not a lock-free
way to establish that no foreground allocation can pay for maintenance.

[Apple's allocator implementation](https://github.com/apple-oss-distributions/libmalloc/blob/main/src/malloc.c)
allows pressure relief on the default zone without taking the all-zones
registry lock. That still leaves the selected zone's synchronization and
page-reuse costs. Reserved zone capacity is not process charge.

[Microsoft's HeapSetInformation documentation](https://learn.microsoft.com/en-us/windows/win32/api/heapapi/nf-heapapi-heapsetinformation)
describes `HeapOptimizeResources` as optimizing LFH caches and decommitting
where possible, on one heap or all heaps.
[HeapCompact](https://learn.microsoft.com/en-us/windows/win32/api/heapapi/nf-heapapi-heapcompact)
is primarily a debugging operation; ordinary frees already coalesce blocks.
It is not a substitute for targeted decommit. Global allocator tunables remain
unchanged.

## Evidence

Raw builds, executable identities, samples and logs are retained outside the
worktree at the supplied `scratchpad/r/native-release` path. Prior mem-release
results motivate the experiment but are not relabeled as results for this
candidate. The residency harness brackets reopening with native process fault
counters to investigate refault costs separately from release scheduling.

## Decision rule

For the active 32:4 case: require lower charge after each wave in every paired
round, no increase in paired median reopen or turn-batch latency, and zero observed
quiet idle CPU in every uninstrumented candidate sample. Report all paired
spreads even when their medians pass; five rounds cannot prove a universal
zero-cost claim. A platform also needs a measured allocator-counter gate and
verification of the final gated implementation before production enablement.
The gated GNU control should skip reclamation; report its latency and charge
spreads and require no paired median reopen increase or observed idle CPU.

A second diagnostic retains the main arena's entire top with
`malloc_trim(usize::MAX)` and compares directly with the first candidate on
Linux x64. This isolates pad semantics without changing allocator tunables;
interior-bin scanning and page discard remain. It does not claim a top-only
trim mechanism exists.

The pad-only diagnostic did not justify keeping the top: relative to the
corrected full-trim build, charge increased by 2036 [1526, 2626] KiB and
reopen latency changed by +0.530 [-0.819, 2.057] ms. The next GNU candidate
uses full trim with a 4 MiB `mallinfo2().fordblks` floor, derived by rounding
up the five-round control ceiling (below 3.49 MiB). Both GNU architectures
still require a final paired verification of that combination. The batch
scheduler policy is scoped to the maintenance worker and restored before reuse.

The final scheduling owner is an aggregate of the completion reference and
its retirement guard, constructed after the actor's last await. Tuple field
destruction releases the reference before the guard submits work. The guard
is never constructed by the reopen transition. A disabled backend's pending
type contains `Infallible`; its lease is zero-sized, and its compile-time
policy constant prevents even taking an extra scheduling lock.

The gated full-trim candidate missed the reopen criterion on both GNU
architectures (+0.414 [-1.823, 0.542] ms on x64; +0.503 [-0.211, 1.211] ms on
ARM64). The last GNU candidate retains the main arena's top and keeps the
measured free-capacity floor and scoped batch priority. Each architecture is
enabled only if that complete candidate meets the same criteria.

Intel macOS gets one further controlled whole-process relief experiment,
because default-zone relief did not reproduce the earlier whole-process gain.
No capacity threshold is guessed for Apple's reservation statistics. Without
a calibrated predictor, enablement requires consistent benefit in both the
one-session control and active-wave cohorts, with no paired median latency
increase. Apple Silicon and Windows remain disabled unless their measured
criteria are met; the completed corrected timing cohorts currently fail them.
