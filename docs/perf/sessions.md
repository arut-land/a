# Loading a session, and what its commits cost

Measured by the ignored benches in `crates/engine/src/driver/tests/bench.rs`
(release, Apple M3, APFS; `cargo nextest run --workspace --release
--run-ignored only -E 'test(/bench_/)' --no-capture`, `A_BENCH_ENTRIES` for
the length). The log is the default loop calling a tool and answering from
the recorded `openai/streaming_grammar/tool_then_followup_text` cassette,
seven entries a turn, renumbered to length. "Before" is `main` at 31edb3e,
which folded every load from the first entry.

## Load: snapshot plus suffix

A session at rest with 64 KiB or more of log past its newest snapshot
takes one (`derived/<id>.snap`: the ledger and the extensions' states,
CRC'd, of this build and these extensions); `derived/<id>.idx` holds where
each line ends, so a load places the lines before the snapshot without
reading them and folds only the rest.

| Log | Before, whole | Now, whole | Now, from its snapshot |
| --- | --- | --- | --- |
| 15 600 entries, 8.4 MiB | 18–22 ms | 19–21 ms | 0.14–0.16 ms |
| 100 000 entries, 54 MiB | 116 ms | 117 ms | 0.24 ms |

Medians of five warm loads, the page cache holding the log. A load from a
snapshot reads one line per check (the snapshot's entry, the index's last)
and the suffix; the index is 8 bytes an entry, read whole (800 KB at 100k).

Resident memory: a load folds each entry and drops it, before as now, so
what a loaded session holds is its index in memory, 8 bytes an entry; the
benches' in-process RSS delta (≤ 112 KiB either way) is below what the
allocator keeps from the bench's own setup, so it bounds nothing finer.
What a snapshot saves is reading the log at all: 54 MiB of page cache and
its decoding at 100k entries.

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

## Power-cut promises: shared flush epochs (2026-10-21)

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
`mise run size`: 2,910 KiB before, 2,894 KiB after, both below 4,096 KiB.
