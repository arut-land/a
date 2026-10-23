# Loading a session, and what its commits cost

Measured by the ignored benches in `crates/engine/src/driver/tests/bench.rs`
(release, Apple M3, APFS; `cargo nextest run --workspace --release
--run-ignored only -E 'test(/bench_/)' --no-capture`, `A_BENCH_ENTRIES` for
the length). The log is the default loop calling a tool and answering from
the recorded `openai/streaming_grammar/tool_then_followup_text` cassette,
seven entries a turn, renumbered to length. "Before" is `main` at 31edb3e,
which folded every load from the first entry.

## Load: snapshot plus suffix

Each session keeps `log.jsonl`, `derived/log.idx` and `derived/log.snap`
in its own directory under `sessions/<id>/`; child sessions use
`children/<id>/` beneath their parent. The disposable index stores line-end
offsets. The CRC-protected snapshot contains the ledger and extension states,
its covered sequence and log offset, and build/extension identity. A session
at rest snapshots after at least 64 KiB beyond its previous snapshot.

Loading validates seals, decoding and sequence/epoch ordering through the
covered prefix before offering a snapshot for restoration. Its sequence and
offset must match that validated prefix. An accepted snapshot skips folding
the prefix; loading still reads and decodes it, then folds the suffix.
An unusable snapshot falls back to whole-log folding. Sealed damage preserves
the log and permits only its validated prefix, read-only; only an unsealed
torn final line may be discarded.

The [current measurements](tuning.md) include this validation: a 15,600-entry,
8,606 KiB log took 54.47 ms whole or 31.24 ms with its snapshot on the final
build (medians of five warm loads on the contended Apple M3 host). The
earlier sub-millisecond snapshot observations predate prefix validation.

## Commits: `[daemon] durability`

The kernel ends each step's appends with `Effect::Commit(Barrier | Lazy)`:
a barrier before anything external, whatever the setting; `step` (the
default, today's rule) makes every step but one of notes alone a barrier,
`effects` only those before something external and a turn's end.

| | Turn (median of 50) | Entries a second |
| --- | --- | --- |
| Before | 0.94–2.5 ms | 2 200–6 400 |
| `step` | 1.05–1.36 ms | 5 100–5 800 |
| `effects` | 0.98–1.58 ms | 3 100–7 000 |

Within noise of each other: in this turn every step but the last is
followed by an external effect (a model call sent, a tool run), so both
settings sync the same steps. `effects` saves syncs only where a step
learns something and starts nothing (a call beginning to wait, a decision
alone), which a turn of tool calls does not have.

## Initial shared-flush measurements (2026-10-21)

These busy-machine timings describe the initial four-flush tool-turn
implementation. The replay-aware admission policy below supersedes its
admission rules; these timings are not measurements of that revision.

A write barrier orders every Apple commit, including deferred notes. The
store requests a drive-cache flush only for `Commit(Barrier)` and before
publishing deferred entries. One daemon-wide `rt::Flusher` serves the
JSONL store: each request returns a `FlushEpoch` future. Requests queued
before a flush starts share it; requests arriving during that syscall
share the next epoch. A lone request starts work immediately, without a
timer. Only the flush worker blocks; session actors await their epochs.
An error fails every promise in that epoch. Cancelling a waiter does not
cancel other sessions' promises.

The file and its directory pass their barriers before the request. The
log path uses a directory barrier because Rust's Apple `sync_all` itself
issues `F_FULLFSYNC`. Loading also orders and flushes before replay: a
process restart can read cache-resident entries that no client has seen,
including from a log now kept read-only. Lazy commits do not request a
flush. `step` and `effects` retain their kernel-defined promise points.
Other targets return no flush epoch: the existing sync already flushes,
with no additional flush syscall or blocking hop per commit.

This reuses `rt`'s blocking pool and oneshots, adding no dependency or
configuration. It needs only an epoch queue, one selected flush operation,
and failure fan-out; no scheduling timer, dedicated thread, or storage
backend is introduced. `Faulty` now keeps barrier-written entries apart
from flushed entries and can cut power between the two. Tests cover that
cut, publication and effect admission, cached replay, shared epochs,
flush errors, and cancelled waiters.

Measured on an 8-core Apple M3, APFS, macOS 27.0.1. **The machine was not
quiet.** Load averages started at 7.92 / 10.13 / 9.90, were 4.90 / 7.85 /
8.97 before the per-commit flush run, and reached 13–14 during builds and
other work. The final sequential CLI comparison began at 10.40 / 11.10 /
10.23; a subsequent sample showed 0% CPU idle. These are observations on a
contended host, not a quiet-machine performance claim.

The baseline is `main` at `7ce25f7`. The per-commit variant replaces its
`sys::sync_data` barrier with `File::sync_data` (`F_FULLFSYNC` on Apple),
leaving its existing directory sync in place. The grouped implementation
is `575ceea`. CLI binaries were built separately from each source variant
with `CARGO_TARGET_DIR=target/fixtures cargo build-release -p a --features
fixtures`, then copied aside before the next build. Every daemon used a
throwaway `A_HOME` containing `[daemon]` and `agent = "echo"`; each was
stopped with its own `A_HOME` and `a daemon stop`, then its home removed.

The final CLI runs used `mise exec hyperfine@latest -- hyperfine`, three
warmups and 30 samples, in baseline / per-commit / grouped order. `a new`
measures the command against a running daemon. The echo command starts a
Python helper and `a resume` on a fresh session prepared outside the
measurement, sends `hello`, waits for the committed `a    hello` line,
then detaches. Its timings include Python/client startup and teardown.

| CLI wall time, mean ± sample standard deviation | Barrier only | Full flush per commit | Grouped promises |
| --- | --- | --- | --- |
| `a new` | 22.56 ± 11.72 ms | 23.05 ± 9.96 ms | 22.39 ± 4.72 ms |
| Complete echo command | 61.00 ± 11.70 ms | 70.18 ± 16.04 ms | 66.38 ± 15.67 ms |

The driver's recorded model/tool turn bench remains
`cargo test --release -p engine bench_turns_under_each_barrier_setting --
--ignored --nocapture --test-threads=1` (same fixture as above, medians of
50 turns). Those runs had unequal host contention; their ranking is not
reliable. The final grouped repeat below ran at load 12.96 / 11.65 / 10.46.

| Driver turn | Barrier only | Full flush per commit | Grouped promises |
| --- | --- | --- | --- |
| `step` | 0.943 ms; 6,096 entries/s | 13.951 ms; 400 entries/s | 19.794 ms; 313 entries/s |
| `effects` | 0.770 ms; 8,901 entries/s | 11.967 ms; 617 entries/s | 19.198 ms; 313 entries/s |

For concurrent commits, persistent real CLI attachments each warm up with
one echo. A thread barrier releases N clients together, each sends a
numbered echo and waits for its committed reply. Across 100 rounds,
latency runs from the release to publication; throughput divides N times
100 by the sum of round durations, including waiting for the slowest
client. Session/client setup and thread creation are outside the timing.
This is a closed workload, with 100 / 800 / 6,400 latency samples. The
N=1 p99 is an observed tail from only 100 samples.

| Sessions | Policy | Turns/s | p50 | p99 |
| --- | --- | --- | --- | --- |
| 1 | Barrier only | 2,063 | 0.40 ms | 1.17 ms |
| 1 | Full flush per commit | 366 | 2.93 ms | 7.01 ms |
| 1 | Grouped promises | 327 | 2.97 ms | 4.02 ms |
| 8 | Barrier only | 4,101 | 1.47 ms | 2.51 ms |
| 8 | Full flush per commit | 724 | 8.00 ms | 15.67 ms |
| 8 | Grouped promises | 752 | 7.32 ms | 14.92 ms |
| 64 | Barrier only | 8,425 | 4.04 ms | 12.43 ms |
| 64 | Full flush per commit | 680 | 63.67 ms | 106.40 ms |
| 64 | Grouped promises | 925 | 39.68 ms | 82.82 ms |

The driver-side `bench_concurrent_session_commits` provides a second
reproducible workload over real actors and JSONL, without CLI processes:
30 simultaneous rounds at N = 1, 8, 64, using the same shared store. Run
it alone with `cargo test --release -p engine
bench_concurrent_session_commits -- --ignored --nocapture
--test-threads=1`. Initial exploratory results were:

| Sessions | Barrier only: turns/s; p50 / p99 | Full per commit: turns/s; p50 / p99 | Grouped: turns/s; p50 / p99 |
| --- | --- | --- | --- |
| 1 | 1,884; 0.399 / 1.967 ms | 159; 5.960 / 18.167 ms | 161; 5.123 / 16.455 ms |
| 8 | 7,307; 0.869 / 1.348 ms | 515; 10.477 / 25.348 ms | 392; 12.918 / 57.165 ms |
| 64 | 9,543; 4.873 / 13.316 ms | 613; 73.235 / 117.614 ms | 764; 46.699 / 120.178 ms |

The promise has a real cost: the persistent client's lone echo median
rises by about 2.56 ms over barrier-only. At 64 clients the grouped run
has 36% more throughput than full flush per commit and a lower observed
tail, while remaining far slower than barrier-only. Grouping amortizes
cache flushes under concurrency; it cannot amortize an isolated promise.
The host contention and small number of independent runs warrant a quiet
repeat before treating these ratios as stable. Shipped binary size from
`mise run size`: 2,910 KiB before, 2,894 KiB after.

After these measurements, `main` advanced to `6744d05` and was merged as
`b498833`. The tables identify the pre-merge variants; the incoming client
changes were not remeasured. On the merged tree, `mise run check` passed
839 tests, acceptance passed 12, seeded crash checks passed eight
(`A_CRASH_SEED=20261021`), and all five release budget checks passed.
Windows clippy, machete and the no-Tokio check passed; shipped size was
still 2,894 KiB. `mise run test:miri` stopped at the existing unsupported
CoreFoundation `CFStringCreateWithCString` call in `sys::awake`; skipping
that also encounters an unsupported process-spawn call in `sys::crash`.
A supported subset (config, log and login parsing) passed 10 tests. The
full prescribed Miri task therefore remains unverified. Benchmark homes
and processes were checked absent after cleanup.

## Replay-aware admission after the design review

The kernel now declares an `Admission { through, requirement }` before
work, including steps that append nothing. Model preparation and sending,
timers and replay-safe tools require `Ordered`; unsafe tools require
`Durable`, derived from the existing callable `Replay` metadata. Numbered
publication, input acknowledgements and unsafe effects share the same
durable prefix. The `step` and `effects` settings still choose write
barriers; replay safety independently decides which work can overlap the
cache flush. No configuration or dependency was added.

The driver tracks `ordered_seq` separately from `durable_seq`. A burst
crosses its write barriers once, and registers any required epoch from
that blocking append job. An elected leader seals its captured prefix,
returns ordered completion, then reuses the same thread for the full
flush. Safe work continues while that flush runs. A successful epoch
advances only its captured prefix; completion notifications are consumed
in registration order so a later success cannot hide an earlier failure.
Publication and durable acknowledgements remain behind that watermark.

Cold model transcripts include ordered, unpublished entries, and warm
transcripts fold them as they arrive. Deltas carrying a call ID are
suppressed until that call's intent is published. Cancellation settles an
unadmitted call without running it. Recovery cancels speculative work,
waits for outstanding epochs, and fences callbacks, host reports and
observer contexts by generation before any sequence can be reused.
Observer reactions declare `Ordered` or `Durable` and enter the same
admission queue. An attach waiting on failed publication receives an
error instead of waiting indefinitely.

`sys::fs::FlushDomain` records the store directory's device and checks
opened log descriptors on Apple. A log on another device is refused
before loading or writing it, including an existing cross-device symlink.
Thus one store's shared full flush cannot falsely cover another device.
Linux and Windows retain their existing sync path, without an extra
flush epoch, flush syscall or blocking hop per commit.

Flush counts are tested through the fault store's shared epoch flusher
in `crates/engine/src/driver/tests/admission.rs`, using the recorded
`openai/streaming_grammar/tool_then_followup_text` model replies. The safe
and unsafe variants differ only in callable replay metadata. Counters
exclude initialization, cached replay and recovery. Both `step` and
`effects` meet the following counts:

| Turn | Before review | After review | Required full flushes |
| --- | --- | --- | --- |
| Two model calls and one safe tool | 4 | 2 | Input acknowledgement; final publication |
| Two model calls and one unsafe tool | 4 | 3 | Input acknowledgement; unsafe intent; final publication |
| Plain echo | 1 | 1 | Input and echo published together |

The initial counter regression failed with `[4, 4, 1]`; the revised test
passes with `[2, 3, 1]`. It disables publication timers so these are
logical promise counts, independent of host scheduling. The existing
50 ms publication deadline can add intermediate flushes during longer
turns; the flusher itself has no batching timer. Held epochs separately
prove that safe work and observers proceed, unsafe work and publication
wait, writes arriving during a syscall need a later epoch, and stale
reports cannot settle a new call after sequence reuse. Error tests also
hold the final flush after an unsafe tool has run and verify that recovery
does not run it again.

The earlier 19.79 versus 13.95 ms comparison had unequal contention and
does not isolate scheduling overhead. No quiet-machine latency claim is
made for this revision; the coordinator's quiet-machine run comes later.
`mise run size` on an isolated checkout of the starting merge `00e4d80`
measured 2,942 KiB; the revised binary measured 2,958 KiB.

Validation of this revision: `mise run check` passed 863 tests, acceptance
passed 12, seeded crash checks passed eight (`A_CRASH_SEED=20261021`), and
all five release budget checks passed. Windows clippy, machete and the
no-Tokio check passed. The supported Miri subset passed its 10 config,
log and login tests; the new device-domain test also passed separately
under Miri. The temporary baseline worktree was removed after its size
measurement.

## Protocol 4 activity with shared flush epochs

The merge of `main` through `9e13330` retains protocol 4's activity frame.
Current-ledger activity at N is eligible only when the ledger and visible
frames both end at N and `durable_seq >= N`. The same predicate gates
live certification and attach, after either snapshot or `since` replay.
The last emitted certification is tracked separately from the summary;
an ineligible prefix never replaces it with an earlier durable sequence.

Advancing the durable prefix exposes entries and certifies activity before
admitting waiting attachments, so an attachment does not receive both its
initial certification and a duplicate broadcast. Recovery certifies the
rebuilt ledger, and shutdown publishes again after draining pending epochs.
Activity retains the outbox's `send_after(N, ...)` ordering. Replay-safe
admission continues to use `ordered_seq`; publication still requires the
durable watermark. No additional wire change, dependency or setting was
introduced beyond main's protocol 4 fixtures and reducer vectors.

The held-flush regression
`activity_waits_for_the_current_ledgers_flush_on_live_snapshot_and_since_replay`
checks that speculative idle activity cannot certify a durable prefix of a
longer ledger, that live and attaching clients receive activity only after
the last epoch completes, and that snapshot and suffix attaches receive
one certification each. `final_sync_publishes_activity_after_its_entries`
covers shutdown's final drain. The pipe renderer regression
`a_turn_ends_when_idle_arrives_after_its_last_entry` checks that the final
activity ends the process without querying the daemon. Failed attachments
retain the durability design's explicit error; reconnect after recovery
gets activity for the recovered prefix.

The branch also merged main's platform and canonical-design changes through
`a420694`. On that final tree, `mise run check` passed 918 tests (28 skipped);
acceptance passed 12, web passed five, and Apple passed 10. Windows clippy,
machete and the no-Tokio check passed. `CARGO_BUILD_JOBS=3` was set for every
build. `mise run size` measured 3,007 KiB before the merges and 2,991 KiB
after, including the fixture-exclusion check.
Budgets and startup timing were deliberately left to the coordinator on a
quiet machine; no timing claim is made under the shared host's load.

The final release fixture binary was built with
`CARGO_TARGET_DIR=target/fixtures cargo build-release -p a --features fixtures`.
One isolated `A_HOME` selected the `echo` agent. Sequential runs of `a -p hi`,
`a -p 'sleep 0'` and `a --mode auto -p '$ true'`, 1,000 each with a 10-second
per-process timeout, completed with zero hangs and zero nonzero exits in
all three groups. The daemon was stopped through that home and the home
removed; the fixture build directory and temporary stress script were
removed. The loop is not committed.

## Windows sync classes

On Windows, Rust's `File::sync_data` and `File::sync_all` both call
`FlushFileBuffers`. Thus an ordered barrier already requests a file cache
flush, and `SEPARATE_CACHE_FLUSH` is false: there is no additional shared
flush epoch or blocking hop. Lazy writes can remain cached until the next
barrier; publication and unsafe effects still await the durable prefix.
This is stronger, and potentially more expensive, than Apple's separate
ordering barrier. No Windows latency measurement has been made here.

Namespace durability is separate from flushing file contents. On NTFS,
`sys::fs::publish` moves a flushed temporary file within its volume using
`MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`: return waits for the move
on disk. Cross-volume copy fallback is disabled. Imports and catalog rewrites
use this operation before acknowledging publication. Windows creates a new
log by flushing and publishing its header before opening its append handle;
Unix retains the deferred directory barrier on the first durable entry.
The Windows path adds a header flush and rename per new session; this is a
correctness cost, with no local Windows timing claim.

The catalog dirty marker is also published before logs may change. Cleaning
renames that marker to a tombstone with the same durable operation, then
reclaims the tombstone. A surviving tombstone cannot make a stale catalog
look clean. Unix publication uses rename followed by parent-directory fsync.
The former successful Windows `sync_dir` no-op now returns Unsupported;
Windows publication never depends on it. Microsoft does not document
`FlushFileBuffers` on a directory opened with BACKUP_SEMANTICS as a directory
persistence contract, so that is not used as evidence.

These guarantees target local NTFS with storage that honors flush requests.
Crash/restart tests establish logical recovery, not persistence under power
loss. Native Windows CI must execute rename/error handling and new-log tests;
physical power-cut verification requires a suitable filesystem/storage host.

## Composable spawn authority codecs

On the shared development machine, `CARGO_BUILD_JOBS=3 mise run size`
measured 3559 KiB before the independent-review fixes and 3527 KiB after
those fixes and the codec reduction, before merging approval explanations.
Both measurements use the same shipped release task; no timing claims are
made under concurrent worktree load.

The persisted authority uses tuples for admitted request fields and frozen
routing, rather than decoding the public SDK request and its unresolved runner
and context enums. The protocol owns one image-envelope decoder, shared by the
transcript and publication reducers; engine authority remains opaque there.
Awaited fresh and prefix tools share their async body, and prefix capture uses
sequence keys for both sets. The net reduction is 32 KiB despite the added
recovery and Lua inheritance logic. This is a shipped-binary comparison, not a
claim that all 21.6 KiB attributed to the former spawn codecs disappeared.
No dependencies were added.

After merging `main`'s approval explanations and regenerating protocol 11's
corpus and view vectors, the same size task measured 3559 KiB. The review
baseline and final merged build therefore have equal total size; the isolated
pre-merge reduction above is 32 KiB.
