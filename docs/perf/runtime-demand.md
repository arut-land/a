# Runtime demand (C09)

`Threads` owns its async and blocking workers and bounds shutdown with a
deadline. Each blocking class has its own queue and byte ledger. Admission is a cancellable
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

| Simulated cores | Async workers | Storage ceiling | Network ceiling | CPU ceiling | Flush ceiling | Admission byte budgets |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 2 | 2 | 1 | 8 | 1 | 1 | 160 MiB |
| 4 | 4 | 2 | 8 | 3 | 1 | 160 MiB |
| 8 | 4 | 4 | 8 | 7 | 1 | 160 MiB |
| 32 | 4 | 4 | 8 | 31 | 1 | 160 MiB |

CPU workers are created on demand up to `max(1, cores - 1)`; storage is
`clamp(cores / 2, 1, 4)`. `Threads::new` sets async worker count while
blocking defaults use the machine's cores. `Threads::configured` can
set every lane's worker, queue, and byte limits explicitly.

Storage admits up to 16 queued jobs and 64 MiB; Network admits 16 and
32 MiB; CPU admits 8 and 64 MiB. Budgets cover queued and running jobs
and do not grow with core count. Seven 8 MiB projections can run together
on the eight-core host. At 32 cores, the same budget permits eight such
projections at once; smaller jobs can use all 31 CPU workers. The invariant
`cpu_capacity_follows_demand_within_the_fixed_byte_budget` exercises both
cases, plus simulated 2/4/8-core configurations.

The flush lane admits one queued leader and one running leader. Leaders
hold metadata, not copied file contents. Shared flush epochs coalesce
requests without timers.

These are ceilings, not eagerly created threads or allocated buffers.
Async workers start together on first spawn; blocking workers start on
demand. Starting workers count against the limit while their first
instruction is still unscheduled. Blocking workers retire after one
ten-second idle wait, releasing their thread resources. No last-worker polling loop remains after retirement.
The shared async-io reactor remains process-scoped. Owner shutdown closes
admission and signals the workers, then waits on their completion until its
deadline. It only joins threads that have already finished. Live threads are
detached at the deadline so uninterruptible native I/O cannot hold the owner.
The runtime destructor does not restart the wait after explicit shutdown.
A last handle dropped on one of its own workers signals shutdown without
joining: the executor must return before its pending futures can
release I/O cancellation guards. This path has a dedicated regression test.

Task wakers pin the executor only while forwarding a wake. A weak reference
breaks the ownership cycle and makes wakes after executor destruction no-ops.
This guards async-executor 1.14.0's borrowed scheduler state against concurrent
executor destruction; the unguarded race reports undefined behaviour in Miri.
The private spawn adapter caches one wrapper for the task's fixed scheduler
waker: one Arc allocation on first poll, none on subsequent polls. Comparing
cloned scheduler wakers with `will_wake` caused false negatives and repeated
allocations in both the native dev build and Miri. Each forwarded wake pays
one weak upgrade and strong-reference release; no timers or idle work are added.
The `task_wakes_can_race_executor_drop` and
`task_waker_is_reused_when_the_scheduler_waker_is_unchanged` tests cover the
race, late wakes, executor reclamation and allocation reuse. `mise run test:miri`
runs both before the existing sys tests.

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
the filesystem work on storage workers. Busy-session recovery marks all
slots as loading, then schedules every independent admission future before
awaiting results. Available storage workers can therefore load later
sessions while the first is stalled.

The daemon owns one runtime for sessions and logging. Shutdown stops
sessions, then gives logging and runtime reclamation one shared 250 ms
deadline. Runtime shutdown closes admission, signals workers, and cancels
async tasks on their executor workers. Healthy workers are reclaimed; an
uninterruptible native closure is detached when the deadline expires so
the daemon can exit. HTTP response cancellation still closes
its socket, including registration after cancellation.

## Session measurements

The reproducible harness is [runtime_demand.rs](../../crates/engine/src/tests/runtime_demand.rs), compiled
as an ignored engine test. It uses 24 file-backed sessions, each containing
1,200 assistant entries of approximately 1 KiB. CPU timing covers concurrent
attach requests through replay, view rebuilding, and snapshot framing.
Storage timing covers concurrent cold session resolution. Both include
queueing and exclude socket/UI transport; filesystem pages are warm.
There are six samples per worker count, with 2/4/7-worker order reversed
on alternate rounds. No remote provider is contacted.

```sh
A_BENCH_CPU_MIB=32 CARGO_BUILD_JOBS=2 mise run perf:runtime-demand
A_BENCH_CPU_MIB=64 CARGO_BUILD_JOBS=2 mise run perf:runtime-demand
```

Independent review measured these medians on the Apple M3 (8 cores,
16 GiB RAM), release builds, six samples per configuration:

| Operation / byte budget | 2 workers | 4 workers | 7 workers |
| --- | ---: | ---: | ---: |
| Snapshot rebuild / 32 MiB CPU | 52.3 ms | 31.6 ms | 32.3 ms |
| Snapshot rebuild / 64 MiB CPU | 50.9 ms | 30.9 ms | 22.9 ms |
| Cold session load / 64 MiB storage | 40.6 ms | 22.6 ms | 36.5 ms |

Seven CPU workers need the 64 MiB budget for the measured projection
reservations; increasing the worker limit alone left performance flat.
Four storage workers improved throughput; seven varied from 24.2 to
49.5 ms. These results support the core-aware CPU ceiling and the four-worker
storage cap. They do not establish optimal settings on 32-core hardware.
The async worker default remains `min(cores, 4)`.

The follow-up rerun on the same shared M3, six samples per configuration,
produced these medians on 2026-10-21. [Raw samples](runtime-demand.csv) retain
all 72 observations, including storage runs under both CPU-budget settings.

| Operation / byte budget | 2 workers | 4 workers | 7 workers |
| --- | ---: | ---: | ---: |
| Snapshot rebuild / 32 MiB CPU | 52.5 ms | 31.8 ms | 31.4 ms |
| Snapshot rebuild / 64 MiB CPU | 54.7 ms | 31.6 ms | 25.4 ms |
| Cold session load / 64 MiB storage | 40.1 ms | 24.3 ms | 35.6 ms |

With 64 MiB, seven CPU workers ranged from 23.35 to 25.73 ms; seven
storage workers ranged from 25.60 to 44.86 ms. This rerun reproduces the
review's direction: about 53% lower snapshot latency at seven CPU workers
and 39% lower cold-load latency at four storage workers than at two.
These are shared-machine observations, not portable latency budgets.

No dependency was added. The existing pool was extended because the
`blocking` crate's last worker periodically wakes (the original reason
for this seam), and its shared pool does not supply independent storage,
network, CPU, and flush reservations. Unused Smol and Tokio adapters and
their dependency subgraphs were removed. The invariant tests cover worker
startup accounting, reservation release, storage progress under network
saturation, one-worker loading, cold replay, owned shutdown, and idle
wake counts in isolated nextest processes.

## Acceptance and integration

All six required invariants are exercised by name:

- `submission_bursts_count_starting_workers`
- `saturated_network_work_cannot_starve_a_flush`
- `loading_cannot_wait_on_its_own_exhausted_pool`
- `cold_replay_does_not_block_async_workers`
- `runtime_shutdown_reclaims_workers`
- `idle_runtime_has_no_periodic_wakeups`

Cold replay also verifies that swapping a log reader can proceed while the
old reader is stalled. Additional checks cover cancelled byte waiters,
oversized reservations, closed admission, and failure of all flush waiters
when their leader cannot be admitted.

The coordinator expanded C09 ownership for these integration call sites:
`apps/web/src/app.rs`, `crates/ffi/src/live.rs`,
`crates/ext-mcp/src/rpc.rs`, `crates/ext-lua/src/loader.rs`,
`crates/engine/src/server.rs`, `crates/engine/src/driver/side.rs`,
`crates/engine/src/driver/spawn.rs`, and the runtime/config/flush consumers
in `crates/engine/src/driver/tests.rs`, `crates/engine/src/config/tests.rs`,
and `crates/engine/src/store/tests.rs`.

The review regressions live with the corresponding engine invariants:

- `driver::tests::durable::logging_flush_deadline_also_bounds_owned_runtime_shutdown`
- `tests::busy_session_recovery_admits_independent_loads_concurrently`

Additional runtime tests cover expired shutdown deadlines with stalled
closures and demand-created CPU capacity under the fixed byte budget.
The scratch review module and its platform-specific idle observation were
removed. The smaller synthetic yielding example was replaced by the
file-backed session harness used for these decisions.
