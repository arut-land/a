# Native runtime resources

`rt::Config::for_path` sizes the daemon from the filesystem containing its
session store and `sys::resources` facts. Facts are read once at startup;
there is no CPU, storage or pressure polling loop. Explicit configurations
remain available for hosts and measurement harnesses.

## Admission policy

| Lane | Worker ceiling | Queued jobs | Queued + running reservations |
| --- | ---: | ---: | ---: |
| Async | min(effective CPUs, 4), at least 1 | Executor tasks are caller-owned | Caller-owned |
| CPU | clamp(effective CPUs − 1, 1, 8) | 8 | 64 MiB |
| Storage | clamp(effective CPUs / 2, 1, 4); 1 for rotational media | 16 | 64 MiB |
| Network | 8 | 16 | 32 MiB |
| Flush | 1 | 1 | Metadata only |

Blocking workers start on demand and retire after one ten-second idle wait.
Starting workers count against the ceiling. Queue and byte admission is shared
by all sessions on the runtime; these reservations do not bound the complete
process heap. `Threads::new` also caps async workers at four; explicit
configurations outside 1–4 async workers are rejected.

Catalog startup, including reconstruction after an unclean exit, uses ordinary
storage admission for preparation, log reads and publication. Up to four replay
jobs share the runtime's queue, byte budget and actual storage-worker limit;
there is no independent rebuild pool. The coordinator awaits their completion
outside storage admission, so reconstruction also completes with one worker.
Catalog records themselves remain retained state, not a promise of an
eight-MiB catalog size bound.

Unknown media retain the core-based storage ceiling; a media label does not
establish filesystem latency or durability. Device-mapper, remote mounts and
virtual disks may report unknown media or an inaccurate rotation flag. Explicit
`Config` values can represent a qualified slow filesystem without changing
platform-independent callers.

## Native facts and scheduling

Linux uses Rust's `available_parallelism`, whose implementation intersects
`sched_getaffinity` with cgroup v1/v2 quotas, including ancestor limits and
non-root controller mounts. This avoids another cgroup parser. Storage discovery
stats the selected path, resolves its block device in sysfs, and reads the
queue rotation flag on that device or its partition parent. Failure is an
explicit unknown, not an SSD claim.

Windows intersects process affinity and process-default CPU-set IDs within the
calling processor group, then bounds capacity by the immediate job's CPU rate
or maximum rate. Weight-based job scheduling is not a quota. A missing native
query falls back to the available facts, with at least one worker. The standard
scheduler's processor-group limit remains in effect. Parent job rates require
parent handles and cannot be discovered through the public current-job query;
this is an approximation in nested quota hierarchies. No thread is pinned to a
guessed performance or efficiency core.

Windows and macOS disk classification uses maintained `sysinfo` with only its
`disk` feature and kind refresh enabled. It owns the native IOCTL and IOKit
integration; process sampling, periodic refresh, capacity and I/O-rate refresh
are disabled. The longest canonical mount prefix selects the store's disk.

macOS blocking jobs use scoped user-initiated QoS; catalog recovery temporarily
uses utility QoS. The thread-bound guard restores the enclosing class even when
a job unwinds. Async workers keep their normal class because one worker polls
both foreground and maintenance tasks. A single `dispatch2` memory-pressure
source subscribes to warning and critical events. Its callback only wakes the
supervisor; actors then discard optional transcript, snapshot and view caches
when detached and fully idle. Session and process ownership stay with their
actors, including when a new attachment arrives during reclamation. Attached sessions, pending approvals, active work and
process owners remain live. Cancellation releases the source's callback only
after event delivery ends. There is no pressure timer or allocator trim call.

Page size comes from `sysconf(_SC_PAGESIZE)` or `GetSystemInfo`, rather than an
architecture constant. Native Apple silicon and Intel therefore keep their
actual page granularity.

## Allocators

Linux GNU sets glibc `M_ARENA_MAX=2` before starting workers. Other targets use
their system allocator without that call. In particular, musl has no glibc
arena policy; no glibc symbol is referenced in a musl build.

The [hosted libc probe](https://github.com/arut-land/a/actions/runs/39863001600)
used native x86_64 and aarch64 Ubuntu runners and Alpine 3.22 / musl 1.2.5
containers. Thirty-two workers each touched four MiB, freed 15/16 while staying
alive, then exited. RSS, KiB:

| Policy | x86_64: allocated / 1⁄16 live / exited | aarch64: allocated / 1⁄16 live / exited |
| --- | ---: | ---: |
| glibc default | 133268 / 129684 / 6052 | 133128 / 129544 / 5728 |
| glibc two arenas | 133168 / 132944 / 13220 | 133032 / 132800 / 14892 |
| musl | 150600 / 127204 / 656 | 150528 / 135188 / 584 |

A subsequent explicit `malloc_trim(0)` reduced the two-arena observations to
1980/1660 KiB. This synthetic topology does not establish application throughput
or justify a periodic trim. Musl released more memory after worker exit but had
higher allocated RSS. Worker lifetime and allocation topology matter alongside
the allocator; no replacement allocator is selected from idle RSS alone.

## Channel policy

The single-value handoff wraps maintained `oneshot` 0.2 with its async and std
features. The adapter preserves `Option<T>` cancellation and a consuming send,
without type erasure or a second payload allocation. The crate has no runtime
dependencies.

The streaming queue retains a growable ring under one mutex. It counts blocked
producers so a receive signals space only when someone is actually waiting.
Consumer wake callbacks and discarded payload destructors run after releasing
the queue lock. Bounded senders retain the explicit blocking-only API; async
callers cannot accidentally await a blocking send through the ordinary sender.

Candidate comparisons include `futures-channel` 0.3.34, `async-channel` 2.5.0
and `oneshot` 0.2.1. Construction/handoff, long-lived queues at batch sizes
1/64/1024, and two-slot bounded queues are measured separately. CPU timings use
the system allocator; a separate instrumented execution counts allocations and
bytes so instrumentation does not determine the timing comparison. Primitive
microbenchmarks do not establish contended application throughput.

## Sources and limits

- [Rust available parallelism](https://doc.rust-lang.org/std/thread/fn.available_parallelism.html)
- [Windows CPU sets](https://learn.microsoft.com/en-us/windows/win32/procthread/cpu-sets)
- [Windows job CPU rates](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_cpu_rate_control_information)
- [Current-job query and nested jobs](https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-queryinformationjobobject)
- [Apple pressure source](https://developer.apple.com/documentation/dispatch/dispatch_source_type_memorypressure)
- [sysinfo disk interface](https://docs.rs/sysinfo/0.39.6/sysinfo/struct.Disks.html)

Hosted VM storage flags are not real-HDD latency tests. A simulated 32-core
configuration on a small VM measures oversubscription, not a 32-core host.
Hardware power loss, battery/thermal policy and injected system-wide memory
pressure require separate hardware qualification. These checks preserve
application state under a pressure request without manufacturing host pressure.
