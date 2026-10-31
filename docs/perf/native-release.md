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
