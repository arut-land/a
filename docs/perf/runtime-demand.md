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
instruction is still unscheduled. Blocking workers retire after one ten-second idle wait, releasing their
thread resources. No last-worker polling loop remains after retirement.
The shared async-io reactor remains process-scoped. Owner shutdown reclaims the runtime's own workers, not that shared
reactor. A last handle dropped on one of its own workers signals shutdown
without joining: the executor must return before its pending futures can
release I/O cancellation guards. This path has a dedicated regression test.

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
sessions, flushes logging, closes blocking admission, cancels remaining async tasks,
and joins workers after accepted work finishes. Running native
I/O must first be cancelled by its owning operation; Rust cannot forcibly
terminate an arbitrary closure. HTTP response cancellation still closes
its socket, including registration after cancellation.

## Worker comparison

Command: `CARGO_BUILD_JOBS=2 cargo run --release -p rt --example demand`.
Measured on the shared arm64 development machine (8 logical cores, 16 GiB
RAM) on 2026-10-21. This is a
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

Final validation on the merged tree, with `CARGO_BUILD_JOBS=2`:

- `mise run check`: formatting, clippy, and 1,127 tests passed; 29 skipped.
- `mise run acceptance`: 23 passed.
- `mise run check:windows`: Windows cross-target clippy passed.
- `mise run machete`, `check:no-tokio`, and `deny`: passed.
- `mise run test:web`: 6 passed.
- `mise run apple:test`: 14 Swift integration tests passed.
- `mise run size`: 3,624 KiB before, 3,657 KiB after, below 4,096 KiB;
  the final measurement includes the merged main changes.

Budgets and startup timings were deliberately not run. No sys unsafe code
changed, so Miri was not required. Native Windows execution was not tested.
