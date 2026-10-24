# Native runtime resources

`rt::Config::for_path` sizes the daemon from the filesystem containing its
session store and `sys::resources` facts. Facts are read once at startup;
there is no CPU, storage or pressure polling loop. Explicit configurations
remain available for hosts and measurement harnesses.

## Admission policy

| Lane | Worker ceiling | Queued jobs | Queued + running reservations |
| --- | ---: | ---: | ---: |
| Async | min(effective CPUs, 4), at least 1 | Executor tasks are caller-owned | Caller-owned |
| CPU | max(effective CPUs − 1, 1) | 8 | 64 MiB |
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

Storage classification is an unqualified heuristic pending slow-disk measurements.
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

Windows and macOS leave storage media unknown. Disk-kind enumeration and the
IOKit lookup add resident memory without a qualified slow-disk benefit; their
measured costs are in the [mechanism comparisons](resource-mechanisms.md).
Explicit configurations can limit a qualified slow filesystem to one worker.

macOS blocking jobs use scoped user-initiated QoS; catalog recovery temporarily
uses utility QoS. The thread-bound guard restores the enclosing class even when
a job unwinds. Async workers keep their normal class because one worker polls
both foreground and maintenance tasks. A single `dispatch2` memory-pressure
source subscribes to warning and critical events. Its callback only wakes the
supervisor; actors then discard optional transcript, snapshot and view caches
when detached and fully idle. Session and process ownership stay with their
actors, including when a new attachment arrives during reclamation. Attached
sessions, pending approvals, active work and process owners remain live. Cancellation releases the source's callback only
after event delivery ends. There is no pressure timer or allocator trim call.

Page size comes from `sysconf(_SC_PAGESIZE)` or `GetSystemInfo`, rather than an
architecture constant. Native Apple silicon and Intel therefore keep their
actual page granularity.

## Allocators

Linux GNU sets glibc `M_ARENA_MAX=2` before starting workers. Other targets use
their system allocator without that call. In particular, musl has no glibc
arena policy; no glibc symbol is referenced in a musl build. The cap bounds
allocator arena growth as workers come and go; it does not guarantee lower RSS
for every allocation topology.

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
Consumer wake callbacks, replaced wakers and discarded payload destructors run after releasing
the queue lock. Bounded senders retain the explicit blocking-only API; async
callers cannot accidentally await a blocking send through the ordinary sender.

Candidate comparisons include `futures-channel` 0.3.34, `async-channel` 2.5.0
and `oneshot` 0.2.1. Construction/handoff, long-lived queues at batch sizes
1/64/1024, and two-slot bounded queues are measured separately. CPU timings use
the system allocator; a separate instrumented execution counts allocations and
bytes so instrumentation does not determine the timing comparison. Primitive
microbenchmarks do not establish contended application throughput.

## Hosted primitive measurements

[Native probe](https://github.com/arut-land/a/actions/runs/39980259136), source `1e19ec9e`, normal release profile, 2026-10-23. Each ready-value handoff runs 100,000 times. Queue measurements reuse a channel for 4096 batches; the table uses 1024 messages per batch. Timings come from the execution without allocator instrumentation.

| Target | Old oneshot, ns | futures oneshot, ns | oneshot 0.2, ns | Ring queue, ns/message | futures queue, ns/message | async queue, ns/message |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| macOS x86_64 | 637 | 299 | 214 | 80 | 114 | 193 |
| macOS aarch64 | 245 | 72 | 65 | 25 | 34 | 63 |
| Linux aarch64 GNU | 133 | 131 | 65 | 30 | 45 | 101 |
| Linux x86_64 GNU | 92 | 122 | 40 | 23 | 32 | 85 |
| Windows aarch64 | 194 | 224 | 97 | 36 | 62 | 111 |
| Windows x86_64 | 174 | 131 | 121 | 14 | 52 | 45 |

Native synchronization has different allocation costs. Ready handoffs include one common executor-waker allocation. These are Rust allocator requests, not process RSS.

| Target | Old handoff bytes/op | futures handoff bytes/op | oneshot 0.2 bytes/op | Ring queue allocator bytes | futures queue allocator bytes | async queue allocator bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| macOS x86_64 | 168 | 120 | 56 | 8256 | 100663296 | 68192136 |
| macOS aarch64 | 168 | 120 | 56 | 8256 | 100663296 | 68192136 |
| Linux aarch64 GNU | 96 | 120 | 56 | 8192 | 100663296 | 68191920 |
| Linux x86_64 GNU | 96 | 120 | 56 | 8192 | 100663296 | 68191920 |
| Windows aarch64 | 96 | 120 | 56 | 8192 | 100663296 | 68191920 |
| Windows x86_64 | 96 | 120 | 56 | 8192 | 100663296 | 68191920 |

Queue counters cover 4,194,304 message roundtrips after construction. Byte totals
use `stats_alloc::Stats::bytes_allocated`: allocation sizes plus positive
reallocation growth, excluding bytes copied during reallocation. The ring makes
one allocation plus eight reallocations on Linux/Windows; native mutex
initialization adds an allocation on macOS. futures-channel allocates for each
message. These cumulative counters do not measure peak live memory or RSS.

The retained ring earns its custom implementation through fewer allocation operations and faster sustained messages in these observations. Single-use channel construction can favor futures-channel; these measurements do not claim superiority for every workload. The maintained oneshot reduces both handoff time and allocated bytes.

CPU jobs compare ceilings of 4, 8 and 31 using a simulated 32-core configuration, 64 one-MiB jobs and five alternating-order rounds. The table reports actual effective CPUs alongside the simulated configuration. Medians, ms:

| Target | Effective CPUs | Page bytes | 4 CPU workers | 8 CPU workers | 31 CPU workers |
| --- | ---: | ---: | ---: | ---: | ---: |
| macOS x86_64 | 4 | 4096 | 184.156 | 161.664 | 217.503 |
| macOS aarch64 | 3 | 16384 | 291.205 | 291.716 | 293.980 |
| Linux aarch64 GNU | 4 | 4096 | 143.843 | 143.921 | 145.636 |
| Linux x86_64 GNU | 4 | 4096 | 67.329 | 68.193 | 69.861 |
| Windows aarch64 | 4 | 4096 | 145.599 | 144.150 | 145.117 |
| Windows x86_64 | 4 | 4096 | 110.908 | 111.601 | 112.702 |

These runs measure oversubscription on three/four-CPU VMs and do not justify a CPU cap. The default therefore follows effective parallelism alone: max(effective CPUs − 1, 1). A future cap requires paired measurements on real hosts with more than nine effective CPUs, comparing 4, 8 and cores − 1 workers on representative projection and CPU workloads across supported OS/architecture/libc targets. Evidence must include throughput, CPU, peak/idle RSS and tail latency, with any throughput tradeoff stated. The separate [runtime-demand measurements](runtime-demand.md) show why restricting an eight-core projection workload to four workers can lose throughput.

Raw [primitive samples](resource-primitives.csv) and [CPU samples](resource-cpu.csv) retain the observations. The temporary workflow and comparison example are preserved in the measured revision; they are not production dependencies.

## Paired application measurements

[Hosted run](https://github.com/arut-land/a/actions/runs/40040199282) compares candidate `fbe75b48a7a0ac7963b6b197eaddf4ebb927a12a` with main `4b662d7ab8151802b8417e71378c8ec800834f34` on the same native runner. Each lane completes five alternating reference/candidate rounds. Tables report medians; positive deltas mean more memory or time.

Idle RSS is sampled after three seconds. Idle CPU is a ten-second native counter delta after fifteen seconds settling. Peak RSS and CPU below belong to the named benchmark process, including its retained test log. Primitive allocator counters and process RSS measure different things.

| Target | Shipped bytes, main → candidate | Idle RSS, KiB main → candidate | Idle CPU, ms main → candidate | Cold reply, ms | Attach p99, ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| macOS x86_64 | 3,990,448 → 4,029,032 | 4100 → 4244 | 0.000 → 0.000 | 45.727 → 46.582 (+1.87%) | 1.002 → 1.064 (+6.21%) |
| macOS aarch64 | 3,313,696 → 3,365,056 | 8640 → 8768 | 0.000 → 0.000 | 26.143 → 23.640 (-9.58%) | 0.964 → 0.576 (-40.20%) |
| Linux aarch64 GNU | 3,690,024 → 3,727,528 | 6368 → 6404 | 0.000 → 0.000 | 6.951 → 7.311 (+5.17%) | 0.489 → 0.476 (-2.58%) |
| Linux x86_64 GNU | 4,291,400 → 4,331,464 | 7272 → 7328 | 0.000 → 0.000 | 7.413 → 7.359 (-0.73%) | 0.486 → 0.479 (-1.48%) |
| Windows aarch64 | 4,868,608 → 4,925,952 | 9732 → 9788 | 0.000 → 0.000 | 83.637 → 82.223 (-1.69%) | 0.997 → 0.954 (-4.35%) |
| Windows x86_64 | 5,537,792 → 5,591,040 | 8408 → 8500 | 0.000 → 0.000 | 27.062 → 29.107 (+7.56%) | 1.189 → 1.193 (+0.29%) |

| Target | Driver CPU delta | Driver peak RSS delta | Large-JSON CPU delta | Large-JSON peak RSS delta | Decode control CPU delta |
| --- | ---: | ---: | ---: | ---: | ---: |
| macOS x86_64 | +38.03% | +0.16% | -10.94% | +0.65% | +21.10% |
| macOS aarch64 | -6.26% | -0.00% | +9.83% | +0.00% | -3.68% |
| Linux aarch64 GNU | +1.46% | -0.01% | -32.58% | -0.02% | +2.22% |
| Linux x86_64 GNU | +0.71% | +0.01% | -2.56% | -0.01% | +3.19% |
| Windows aarch64 | -6.33% | +0.03% | -3.77% | +0.08% | +0.00% |
| Windows x86_64 | +10.40% | +0.11% | -1.38% | +0.03% | +5.13% |

The macOS Intel driver CPU median rises 38.03%, with reference samples spanning 3.93–6.01 s and candidate samples 3.99–6.14 s; the unchanged decode control rises 21.10%. These observations do not isolate a cause. Windows x86_64 driver CPU rises 10.40% and cold reply rises 7.56%; its decode control rises 5.13%. The samples are retained without attributing the regressions solely to the runtime change or solely to runner variation.

The decode control uses unchanged provider-decoder code. The tables retain small shifts rather than classifying every median difference as a performance win or loss. These three/four-CPU VMs do not establish physical many-core throughput.

Exact medians and checkpoint samples are in [paired observations](resource-paired.csv); [runner metadata](resource-hosts.csv) records the native image and hardware description. The harness, native accounting and workload definitions are in [hosted measurements](hosted.md).

## Validation

[Integration CI](https://github.com/arut-land/a/actions/runs/40040195493) at
`fbe75b48a7a0ac7963b6b197eaddf4ebb927a12a` passes on Linux, macOS and native
Windows x86_64/aarch64. It includes formatting, clippy, the full test suite,
acceptance, Linux/macOS release budgets, Swift/FFI tests and the app build,
Miri, fuzzing, dependency checks, the Windows platform lint and the no-Tokio
check. The paired run above uses the same source and main baseline.

Invariant tests cover affinity and CPU-set restrictions, QoS restoration,
pressure callback lifetime, single-worker catalog reconstruction, blocked
producer cancellation and cache rebuild equivalence. Reclamation preserves
attached sessions, pending approvals and process ownership. The preservation tests
populate transcript, snapshot and view caches before reclamation, then inspect
retention without invoking a rebuild. Removing the reclamation guards makes
both preservation tests fail with all three caches absent. Independently removing
the idle/ownership guard fails attachment and process retention; removing the
activity guard fails approval retention. Restoring both passes. An unprotected
idle session still releases all three caches. A shared shell
test checks deferred wait completion directly: PID metadata can disappear at
exit before the child is reaped.

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
