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

CPU workers are created on demand up to `max(cores - 1, 1)`; storage is
`clamp(cores / 2, 1, 4)`. `Threads::new` sets async worker count while
blocking defaults use the machine's cores. `Threads::configured` can
set every lane's worker, queue, and byte limits explicitly.

The daemon's default configuration uses effective CPU parallelism. Catalog
recovery shares storage admission. Per-OS policy, QoS, memory pressure and
channel comparisons are recorded in [native runtime resources](resources.md).

Storage admits up to 16 queued jobs and 64 MiB; Network admits 16 and
32 MiB; CPU admits 8 and 64 MiB. Budgets cover queued and running jobs
and do not grow with core count. Seven 8 MiB projections can run together
on the eight-core host. At 32 cores, the same budget permits eight such
projections at once; smaller jobs remain bounded by effective parallelism minus one (at least one worker). The invariant
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

Task wakers hold the executor only while forwarding a wake, so scheduling
finishes before executor destruction performs its final queue drain. Without
that ordering, a late enqueue can leak a scheduler-state/queued-task ownership
cycle. The guard's weak reference avoids retaining the executor through its
pending tasks and makes wakes after executor destruction no-ops. The guard
exists for reclamation; scheduler lifetime safety comes from async-executor's
owned `Arc<State>`, pinned to upstream fix
[`96a1f6a19ceb828c8eb8a10cb4b40daefdbdb945`](https://github.com/smol-rs/async-executor/pull/164).
The private spawn adapter caches one wrapper for the task's fixed scheduler
waker: one Arc allocation on first poll, none on subsequent polls. Scheduler
waker identity is fixed even when `will_wake` returns a false negative. Each
forwarded wake pays one weak upgrade and strong-reference release; no timers
or idle work are added. The `task_wakes_can_race_executor_drop` and
`task_waker_is_reused_when_the_scheduler_waker_is_unchanged` invariants cover
concurrent wakes, late wakes, executor reclamation and allocation reuse.
`mise run test:miri` runs both with leak checking before the sys tests.

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

## Concurrently active sessions

Question: would sessions benefit from running on threads of their own
(an executor per session) instead of as actor futures on the shared async
workers? Answer, measured on every hosted OS/architecture: no. Sessions stay
on the shared `min(cores, 4)` workers; nothing shipped changed.

The harness is `tests::runtime_demand::concurrent_active_sessions` (an
ignored engine test, one fresh process per sample), driven by
[sessions.py](../../scripts/perf/sessions.py):

```sh
CARGO_BUILD_JOBS=2 mise run perf:runtime-demand sessions --rounds 1
gh workflow run perf --ref <branch> -f sessions=true   # every hosted runner
```

Each sample is a daemon in an isolated home serving its real endpoint, with
1, 8, 32 or 128 clients. Every client attaches to a new session over the
endpoint and runs three turns of the default loop. The model is
`openai/streaming_grammar/tool_then_followup_text.yaml`, served unchanged over
loopback HTTP through the real ureq transport, one SSE event every 10 ms. Its
tool call is answered by one real call of the shell extension (`echo`, through
bash or PowerShell), and the recorded answer streams nine text deltas. The
delta latency is the time from the provider writing a delta's event to the
client reading its `delta` frame. Turn latency runs from input sent to
idle. CPU is process CPU while the turns run. Idle RSS and threads are
sampled 12 s after the last turn, once blocking workers have retired; the
clients stay attached. The provider and the client threads are in the same
process and count toward CPU, RSS and threads equally in every layout.

Layouts, interleaved and reversed on alternate rounds, five rounds each:
`A` and `A2` (the default, four async workers; their gap is the A/A noise
band), `cores` (one worker per core, run only where cores differ from four:
macos-latest has three), `threads` (each session actor on a one-worker
executor of its own, with every task spawned from it kept there; the blocking
lanes, flushes and reactor stay shared), and `network` (the default with 128
network-lane workers, a diagnostic and not a candidate). The `threads` layout
is test-only: a `#[cfg(test)]` hook where the actor is spawned
(`driver.rs`, `ACTORS`) and a `Runtime` wrapper in the harness.

Run [40201401042](https://github.com/arut-land/a/actions/runs/40201401042),
2026-10-25. Pooled delta p99 (ms), turn p99 (ms), active CPU (ms) and idle RSS
(MiB) at 128 sessions, as A / A2 / threads. [All rows](runtime-sessions.csv)
cover every layout and session count.

| Runner | Delta p99 | Turn p99 | CPU | Idle RSS |
| --- | ---: | ---: | ---: | ---: |
| ubuntu-latest | 0.32 / 0.31 / 0.45 | 4997 / 5011 / 11249 | 3730 / 3620 / 4600 | 31.0 / 31.1 / 50.1 |
| ubuntu-24.04-arm | 0.29 / 0.31 / 0.28 | 4724 / 4775 / 11542 | 3820 / 3770 / 4480 | 30.1 / 29.7 / 48.7 |
| macos-latest | 1.73 / 1.79 / 2.91 | 27770 / 28428 / 27543 | 6940 / 7163 / 8596 | 35.2 / 35.2 / 44.9 |
| macos-15-intel | 1.96 / 2.12 / 3.91 | 21135 / 20479 / 25369 | 11348 / 10605 / 18994 | 28.0 / 28.0 / 36.0 |
| windows-latest | 0.15 / 0.15 / 0.36 | 17670 / 18618 / 22253 | 5172 / 5062 / 9125 | 41.0 / 39.4 / 50.0 |
| windows-11-arm | 0.80 / 0.73 / 1.16 | 11226 / 11002 / 13154 | 6766 / 6766 / 10016 | 37.2 / 37.3 / 46.4 |

- **Threads per session lose.** Delta p99 is worse than both A runs, beyond
  the A/A band, at 32 sessions on all six runners and at 128 on five
  (ubuntu-24.04-arm is within noise there). Turn p99 roughly
  doubles on Linux at 128. CPU rises by up to 76% (Windows x64), and idle RSS
  grows by 8 to 20 MiB at 128 sessions (one thread and executor per idle
  session). At 1 and 8 sessions every layout is within noise. The adoption
  bar (p99 better beyond noise, no idle-RSS cost) is not met anywhere.
- **More async workers do not help either.** On macos-latest, three workers
  (`cores`) are within the A/A band of four at every count. Delta latency is
  well under a frame interval (p99 0.1 to 3 ms) under all loads.
- **The network lane bounds concurrent turns, not the async workers.** A
  ureq stream holds a network blocking worker for its whole reply, and the
  lane has eight. At 128 sessions, turns wait their turn: ubuntu-latest turn
  p50 is 3945 ms with the default and 532 ms with 128 network workers. The
  cost is delta p99 rising to about 6.5 ms and idle RSS doubling to 68 MiB
  (allocator retention after 128 concurrent streams). Windows is the
  exception: its turns are bound by PowerShell start-up, and the wider lane
  changes nothing there.
- **Under load, live deltas are dropped at the start of a reply.** With 128
  concurrent streams, macos-15-intel lost 9384 of 17280 deltas
  (54%). macos-latest lost 57, and the default layouts lost none.
  Drops cluster on a reply's first delta. This fits `Live::delta` discarding
  deltas until the frame log holds the call's `model_call` entry. The final
  text is unaffected (`model_result` is the fact).
- **Three of about 400 samples stalled** (`failures.jsonl` in the run's
  artifacts): `threads`/32 on macos-15-intel, `A2`/32 on ubuntu-24.04-arm and
  `network`/128 on ubuntu-latest. In the first two, the transport closed a
  provider connection mid-request-body. In all three, a session's turn then
  made no progress for 120 s. The sampler records the stall and moves on.

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
