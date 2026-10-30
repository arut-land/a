# Finite mailbox batches and inbox segments

Measured on 2026-10-30, Apple M3 arm64, 16 GiB, macOS 27.0.1,
nightly-2026-10-17 (rustc 1.101.0-nightly, 5d8fec066). Baseline:
`ea0ce5ff5c97a8516cdf6b531873d044ee4560e3`. The host was shared; samples
alternate A/B and B/A, with every sample retained.

## What changed

The actor already committed finite bursts of at most 64 messages. Its extra
receives each locked the same channel. `Receiver::try_recv_batch` reserves
caller storage before locking, transfers only ready values under one lock,
and wakes producers after unlocking. It exchanges queue ownership when the
whole queue fits the caller's allocation; otherwise it moves the finite
prefix. The capacity check prevents a large historical channel allocation
from moving into the actor's small ready buffer. Blocking producers all get
a chance to use the newly available slots.

The actor still receives the first message and probes for a second through
the scalar path. Only an observed second message enables batch reception.
It never waits for a batch to fill. Thawed messages keep priority over ready
mail, log readers still fence bursts, and a prefetched suffix stays ahead of
later mail. Shutdown consumes flush completions from that suffix before
waiting on the mailbox. Existing freeze and mutation-admission behavior is
unchanged. The ready allocation holds at most 64 messages: 15,360 requested
bytes on this host (`size_of::<Msg>() == 240`), retained until actor teardown.
Message payloads remain owned by their messages.

The client inbox's two growing deques retained a burst's high-water
allocation after every item had been consumed. Each lane now has an inline
head deque and a linked tail of pump-sized segments. Empty head segments
leave as consumption advances; the last head is reused by ordinary traffic.
An empty lane retains at most one 64-item segment. Neither total delivery
nor history is capped: the existing byte/request budgets and oversized
stream admission still govern posting. There is no trim timer.

The tail uses the standard library's `LinkedList`, rather than another
growing outer deque that would retain its own high-water storage. The
measured alternative of putting every segment, including the active head,
in the list cost more CPU; keeping the head inline reduces that cost. No
dependency was added.

## Workloads and measurement boundaries

All performance builds are release builds with opt-level `z`, fat LTO and
one codegen unit. Product binaries use `cargo build-release -p a --features
fixtures`, including the repository's rebuilt standard library. The
standalone channel/inbox probes and engine test executables use installed
std; their absolute numbers are not shipped-binary measurements.

The channel probe compiles the complete real channel module and compares
the unchanged scalar drain with adaptive reception in the same executable.
Both arms share their first two scalar receives; the arm decision occurs
only after a second value exists. This avoids different feature builds
giving different scalar code generation. Each of eleven fresh-process
pairs fills and drains 16,777,216 items at widths 1, 4 and 64. Caller storage
is preallocated in both arms. A separate 65,536-item pass measures each
item from just before burst enqueue through consumption. FIFO checksums
cover every value. CPU is process user plus kernel time from `clock()`;
latency uses `Instant`. Timer resolution is about 41–42 ns.

The inbox probe compares the full baseline and current inbox implementations
with 224-byte values and their real admission, weighting, locking and wake
paths. Eleven interleaved pairs process 1,048,576 items at widths 1, 64 and
32,768. Construction and the same initially empty queue are included in
each arm; allocation instrumentation is not used for these CPU timings.

The whole-turn check uses the existing release
`driver::tests::bench::bench_turns_under_each_barrier_setting`: seven
interleaved pairs, 50 turns per barrier setting, recorded provider replies,
the real tool and JSONL writer. Reported turn times are each process's
median. Process CPU includes setup and both 50-turn settings.

The native TUI check uses five interleaved pairs of fresh 100×30 PTYs,
isolated homes, and 100 replies of 5 MiB each. SSE content is repeated from
the recorded provider corpus, served over a local HTTP server. Every run
checks at least 100 durable completed model results and stops its own
daemon with `A_HOME=<home> a daemon stop`. All ten runs use the same probe
and binaries without allocation-stack logging. Live heap is from `heap -s`;
RSS and charged footprint are measured separately. Turn latency includes
the probe's render-settling check, so it is a workload comparison rather
than an isolated input-latency measurement.

## Results

All values are median [minimum–maximum].

| Channel width | Scalar CPU, ns/item | Adaptive CPU, ns/item | Scalar p99, µs | Adaptive p99, µs |
| --- | ---: | ---: | ---: | ---: |
| 1 | 34.39 [33.45–36.24] | 34.18 [33.39–35.04] | 0.042 [0.042–0.083] | 0.042 [0.042–0.042] |
| 4 | 24.40 [23.47–27.33] | 24.18 [23.40–24.68] | 0.167 [0.167–0.292] | 0.208 [0.167–0.250] |
| 64 | 21.11 [20.26–24.22] | 11.57 [11.05–11.78] | 3.583 [3.209–6.000] | 2.958 [2.791–3.459] |

Width-64 CPU falls 45.2%; p99 falls 17.4%. Singleton CPU differs by
−0.6%, with overlapping ranges and the same 42 ns p50/p99 medians. Width-4
CPU is similar; its p99 median moves by one approximately 42 ns timer tick.
The prototype’s absolute timings are not reused as product measurements.

| Inbox width | Original CPU, ns/item | Segmented CPU, ns/item |
| --- | ---: | ---: |
| 1 | 45.78 [42.70–46.88] | 50.79 [48.49–53.31] |
| 64 | 44.89 [41.39–45.95] | 51.35 [48.05–52.47] |
| 32768 | 46.46 [42.86–49.73] | 54.84 [51.21–58.18] |

Segmentation costs about 5–8 ns/item (11–18%) in this component probe. It
exchanges a permanent backlog-sized allocation for finite reusable heads
and temporary tail segments. The native workload below includes this cost.

| Recorded whole-turn workload | Before | After |
| --- | ---: | ---: |
| Step, ms/turn | 7.092 [6.976–8.003] | 7.006 [6.942–7.995] |
| Effects, ms/turn | 7.003 [6.927–8.001] | 7.002 [6.980–7.701] |
| CPU per 100-turn process, ms | 151.389 [144.214–179.399] | 149.878 [137.747–169.003] |

Ranges overlap: no whole-turn speedup is established.

| Native TUI after 100 × 5 MiB replies | Before | After |
| --- | ---: | ---: |
| Live heap, MiB | 508.216 [508.216–515.529] | 500.966 [500.966–500.966] |
| Charged footprint, MiB | 534.407 [533.673–542.360] | 530.314 [528.954–530.892] |
| RSS, MiB | 367.141 [178.047–387.109] | 369.312 [325.688–423.469] |
| TUI CPU for 100 turns, s | 86.815 [83.679–110.401] | 84.180 [83.471–89.463] |
| Daemon CPU for 100 turns, s | 65.379 [61.346–86.391] | 61.742 [61.422–65.318] |
| Settled turn p50, ms | 1064.595 [1046.639–1298.030] | 1062.163 [1041.195–1080.991] |
| Slowest settled turn of 100, ms | 1546.945 [1362.547–2007.256] | 1312.441 [1246.768–1428.550] |

Paired live-heap savings are 7.250 [7.250–14.562] MiB. The 7,424 KiB
allocation is present in four baseline runs, with a larger high-water
allocation in the fifth; it is absent in every candidate run. All five
candidate live heaps are equal to the byte. RSS does not show a reliable
decrease. TUI CPU and settled-turn latency show no regression in this
workload, but the overlapping ranges and shared host do not establish a
general end-to-end latency improvement.

## Verification and limits

The drained-inbox retention test fails against the baseline. The new
shutdown-suffix regression fails when suffix completion handling is
removed, using a real JSONL flush with controlled completion delivery.
Tests also cover finite FIFO append, closed queues, storage exchange
without inheriting a large allocation, two blocked producers, wakes
outside the channel lock, freeze/thaw ordering and log-read fences.

Phase 1 runs `cargo check`, all 610 tests and all-target/all-feature
Clippy with `-D warnings` for `rt`, `client` and `engine`, their rustdoc,
format checks, and `mise run check:windows`. Windows is a cross-platform
compile/lint check, not a native performance measurement. Linux and
Windows CI await independent review; no hosted workflow is dispatched in
phase 1. Wire and log formats are unchanged.

Current main still uses protocol 40 and retains the conversation payload
in the TUI. Removing inbox capacity does not remove that payload or
promise a matching RSS/footprint decrease: native allocator charge and
memory pressure are separate observations. The initial diagnostic run
with allocation-stack logging changed backlog formation and is excluded
from the ten uninstrumented native samples. An interrupted native run is
also excluded, rather than treated as a completed sample.

Raw evidence and the external reproduction probes are retained under the
round-3 scratchpad's `batch-mail/`: `probe/`, `micro.py`, `final-ab.json`,
`turn-ab.json`, `native-restart.py`, `tui.py`, native heap/maps/snapshots,
and validation logs. Product build artifacts live only in the designated
external `perf2/tgt-batch-mail`, retained for phase-2 review and cleanup.
