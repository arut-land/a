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
