# Runtime demand (C09)

`Threads` owns and joins its async and blocking workers. Each blocking
class has its own queue and byte ledger. Admission is a cancellable
future: a full queue or byte budget suspends its caller without occupying
a worker. Both queued and running jobs retain their byte reservations.
Oversized reservations and admission after shutdown fail. Pending futures
and retained session state remain caller-owned; these limits are not a
whole-daemon RSS bound (A07 owns aggregate outstanding work).

## Sizing

`Config::for_cores` is exercised with simulated core counts by
`memory_budgets_do_not_scale_with_core_count`. Limits are explicit and
configurable through `Threads::configured`; RAM is never inferred from
processor count.

| Simulated cores | Async workers | Storage workers | Network workers | CPU workers | Reserved flush workers | Reserved bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 2 | 2 | 2 | 8 | 2 | 1 | 128 MiB |
| 4 | 4 | 2 | 8 | 2 | 1 | 128 MiB |
| 8 | 4 | 2 | 8 | 2 | 1 | 128 MiB |
| 32 | 4 | 2 | 8 | 2 | 1 | 128 MiB |

Storage admits up to 16 queued jobs and 64 MiB; Network admits 16 and
32 MiB; CPU admits 8 and 32 MiB. The flush lane admits one queued leader
and one running leader. Leaders hold flush metadata, not copied file
contents. Shared flush epochs still coalesce requests without timers.

These are ceilings, not eagerly created threads or allocated buffers.
Async workers start together on first spawn; blocking workers start on
demand. Starting workers count against the limit while their first
instruction is still unscheduled. Blocking workers remain parked until
work or shutdown; no retirement timer or periodic wake is installed.
The shared async-io reactor remains process-scoped. Runtime shutdown
reclaims the runtime's own workers, not that shared reactor.

Network reservations include request payload and bounded stream buffers.
Logging reserves its fixed-record batch plus encoding scratch. Generic
storage and projection jobs reserve an explicit 8 MiB working allowance;
this is admission accounting, not an allocator-enforced bound on a decoded
session. Large retained projections still need A07's end-to-end limits.
CPU projection rebuilding and Lua recompilation cannot consume the
storage or flush lanes. A loader runs an elected flush on its current
worker rather than submitting and waiting on its own pool. A follower's
cold log reads and projection rebuilds are awaited off async workers.
Config lookups clone immutable snapshots; demand-triggered refreshes do
the filesystem work on storage workers.

The daemon owns one runtime for sessions and logging. Shutdown stops
sessions, flushes logging, cancels remaining async tasks, closes blocking
admission, and joins workers after accepted work finishes. Running native
I/O must first be cancelled by its owning operation; Rust cannot forcibly
terminate an arbitrary closure. HTTP response cancellation still closes
its socket, including registration after cancellation.

## Worker comparison

Command: `CARGO_BUILD_JOBS=2 cargo run --release -p rt --example demand`.
Measured on the shared development machine on 2026-10-21. This is a
fixed-workload scheduling comparison, not a startup or resource-budget
measurement. No budgets or startup timings were run.

The async workload schedules 2,048 tasks with 100 cooperative yields each.
The CPU workload submits 256 jobs, each with 100,000 dependent integer
updates, through the same two-worker CPU lane in every configuration.

| Explicit async workers | Async workload | CPU workload |
| --- | ---: | ---: |
| 2 | 24,310 µs | 43,154 µs |
| 4 | 17,665 µs | 32,709 µs |
| 32 | 34,790 µs | 33,183 µs |

This single shared-machine sample gives no reason to change the default
`min(cores, 4)` or increase the CPU lane. Repeat on quiet target hardware
before tuning either. The workload and configurations are reproducible;
these timings are not claimed as portable performance budgets.

No dependency was added. The existing pool was extended because the
`blocking` crate's last worker periodically wakes (the original reason
for this seam), and its shared pool does not supply independent storage,
network, CPU, and flush reservations. Unused Smol and Tokio adapters and
their dependency subgraphs were removed. The invariant tests cover worker
startup accounting, reservation release, storage progress under network
saturation, one-worker loading, cold replay, owned shutdown, and idle
wake counts in isolated nextest processes.
