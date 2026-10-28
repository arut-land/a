# Explicit schedule traces

The Infinity survey's section 2 identified missing reproducible decision
artifacts, not an outstanding product defect or a demonstrated product speedup.
The harness runs the real kernel and engine. No production implementation,
dependency, protocol, recorded fixture or product limit changes.

## Decisions and actual batches

`crates/kernel/tests/schedules.rs` supplies a disposable token format shared
by the kernel invariant suite and native admission tests:

| Token | Meaning |
| --- | --- |
| `input` | The workload's `sleep 1` input arrives |
| `progress` | The held safe call reports `ordered` |
| `cancel` | The client interrupts |
| `complete` | The tool completes successfully with `finished` |
| `batch` | Deliver the preceding group together, then finish that driver burst |
| `flush:1:ok` | Deliver the later epoch's successful notification |
| `flush:0:ok` / `flush:0:err` | Resolve the earlier held epoch with the chosen result |
| `crash:N` | Restart from the first N log entries, between closed groups |
| `recovered-complete` | Explicitly complete a safe call restarted by the pure runner; also needs `batch` |

Every ingress group must end with `batch`. Empty groups, unterminated groups,
crashes inside a group and mixed flush/event groups are rejected. Flushes have
their own groups because the real driver handles `Msg::Flushed` outside its
ordinary bursts. These constraints bind the trace to the existing driver seam;
they do not impose product limits.

The pure runner steps Core through every event in a group before carrying its
plans. It admits the group's work only at its boundary, as the real driver
carries a combined burst after all its events. Core itself has no flush API:
flush groups mark native journal decisions, whose behavior is tested by the
native runner rather than a copied journal state machine.

The native test captures the actor future and all spawned async jobs in a
runtime defined entirely in the owned test file. Storage operations use real
files; cache flushes run on the real `rt::Threads` runtime. Only an explicit
actor poll consumes ingress. Progress, cancellation and the tool's actual
completion are queued while the actor is paused, and the batch token then
allows the actor to run. In particular:

```text
cancel complete batch
cancel batch complete batch
```

produce different delivery boundaries. The first delivers completion before
cancellation has reached the actual tool context; the second observes an
already-cancelled context when delivering completion. The test asserts this
against the real context, not just against a local list of token groups.
Progress and sync are likewise queued before one actor poll. There is no
per-token `positions()` or quiescence wait that silently creates extra bursts.
A position read occurs only after group delivery. While finishing a burst,
necessary IO and cancelled-job continuations drain with new external arrivals
frozen; a pending continuation with no runnable job or flush is an error.

The initial input starts one held replay-safe tool after ordering and before
publication. The progress group establishes the later prefix. A later success
notification is explicitly injected while the earlier syscall remains held,
as in the existing admission test. The real native flusher serializes its
syscalls; this tests notification delivery rather than concurrent flush
syscalls. Resolving the earlier epoch releases the real held flushes and
collects their notifications in deterministic task-registration order.

## Bounded coverage

The explorer preserves order within the client, tool and notification sources.
Its tail has twelve event orders. Adjacency permits six additional merged
cancel/completion groups, giving **18 valid groupings per earlier-flush result**:
**36 live schedules** and **117 pure crash traces**. Each successful earlier
epoch explores all entry cuts; an earlier failure certifies none of the new
turn and therefore has cut zero. A recovery completion, when required, is an
explicit token and closed batch.

The native subset has cancellation before completion, with both before the
earlier flush resolves. It covers later notification before, between and after
them, including merged and split cancel/completion delivery: **10 live traces
and 45 native prefix restarts**. Native traces use the existing echo agent and
a held safe tool, keeping every competing completion explicit. The existing
recorded-provider admission tests still run in the full engine suite; the
batch explorer does not claim to control provider/runtime scheduling.

These counts supersede phase 1's 384/1,248 and 12/66 claims. Those included batch
tokens that did not control real bursts. The revised counts enumerate actual,
valid groupings; invalid boundaries are not additional schedule coverage.

## Recovery assertions

Each native trace runs over `Faulty` backed by real JSONL. A real power cut
checks its retained publication prefix. Every selected entry cut is then
loaded through the existing crash-prefix fixture on JSONL.

For each cut the assertions require:

- The entire supplied prefix retains the same sequence numbers and bodies.
  The fixture deliberately assigns the new writer's lease epoch when loading.
- Each retained input owns exactly one `sleep` intent with `seconds: 1`, and
  exactly one result for that call with the scripted `finished` outcome.
- Expected recovery runs a separate real `Core::resume` on that prefix and
  explicitly completes any replay-safe work it requests. Unknown work fails;
  there is no second recovery state machine.
- The native final facts and folded ledger equal that expected recovery.
  Independent input/intent/result assertions still fail if both real Core
  executions erroneously drop retained work.
- The recovered kernel is at rest, and a further resume produces no work or
  new entries. Failed epochs do not publish their speculative suffix.

The pure checks also require admission to cover intent, safe work to require
ordering, one settled call per retained input, the scripted outcome, the
selected cancellation ordering, no owned work at rest and identical live and
reloaded ledgers. The existing state matrix independently checks durable
admission for unsafe tools. Different cancellation orders may record different
facts. Entry-cut tests are not claims about interruption of native syscalls at
every byte, nor replacements for native endpoint/process/wakeup tests.

## Replay and shrinking

The exact replay command prints **before** a candidate enters real Core,
including candidates used during shrinking and crash-prefix probing. A Core
panic therefore leaves its candidate identified even under the dev profile's
Cranelift unwinding restriction. A subprocess test triggers a real panicking
gate and asserts that the exact command precedes that panic on stderr.

```sh
A_SCHEDULE_TRACE='input batch progress batch cancel complete batch flush:1:ok batch flush:0:ok batch crash:2 recovered-complete batch' \
  cargo test -p kernel --test invariants every_held_epoch -- --nocapture

A_NATIVE_SCHEDULE_TRACE='input batch progress batch cancel complete batch flush:1:ok batch flush:0:ok batch crash:2' \
  cargo test -p engine scripted_epoch_delivery -- --nocapture
```

Harness invariant failures return explicit errors and shrink by deleting
subsequences and reducing crash cuts. Candidates must preserve causal
prerequisites, bound groups and completion choices. The result is minimal under
those reductions, not necessarily globally shortest. Product panics print their
exact candidate; shrinking them is not claimed to work without unwinding.

## Measurements and verification

This is test infrastructure. No daemon CPU, RSS or latency gain is claimed.
Phase-1 measurements remain historical observations of its superseded harness,
not evidence of real batch exploration. Before review changes, release runs of
that explorer took 375.894/77.867/78.924 ms and its native test took
1978.480/1640.377/1568.809 ms on the shared M3. Cold first observations are
retained, not silently discarded.

Reviewed release measurements alternate A/B and B/A against retained phase-1
executables, reporting medians and observed minimum–maximum spreads. The new
search and native fixture execute different work, so their timings measure
harness cost, not a product speedup. Unchanged recovery/notification tests are
also compared with identical arguments. Raw observations and executable hashes
are retained in `scratchpad/r3/schedule-traces-phase2-measurements.json`.

Seven warm paired groups on the shared Apple M3 (8 CPUs, 16 GiB, macOS 27.0.1)
gave these costs. Cells are median (observed minimum–maximum), in milliseconds.
The first two rows use identical workloads, ten invocations per group. Harness
rows use one invocation per group and execute different search/fixture work.
Both executables in this comparison use production base `f343ff37`; these
measurements precede the later upstream integration.

| Workload | Phase 1 wall ms | Reviewed wall ms | Phase 1 CPU ms | Reviewed CPU ms |
| --- | --- | --- | --- | --- |
| Unchanged kernel recovery | 11.350 (10.457–15.580) | 11.701 (10.270–14.417) | 8.338 (7.767–8.697) | 8.212 (7.595–9.113) |
| Unchanged native notification | 13.505 (12.568–16.572) | 13.834 (12.291–17.682) | 9.967 (9.396–10.692) | 10.287 (7.557–11.011) |
| Pure explorer (coverage revised) | 80.777 (77.329–90.391) | 14.676 (12.877–21.262) | 78.757 (75.190–87.532) | 12.135 (10.859–18.263) |
| Native traces (fixture and coverage revised) | 1527.675 (1475.033–1669.030) | 867.862 (833.885–913.139) | 197.473 (181.695–254.275) | 99.589 (88.736–114.670) |

Separate fresh `/usr/bin/time -l` samples measured peak process RSS (MiB):

| Reviewed workload | Median (minimum–maximum) |
| --- | --- |
| explorer | 7.281 (7.203–7.297) |
| native | 11.312 (11.297–11.359) |

Wall times include process startup. CPU is child user plus system time from
`getrusage(RUSAGE_CHILDREN)`; RSS is process peak, not retained heap or daemon
memory. Unchanged workloads have overlapping spreads. The revised explorer
does less duplicated/invalid search work; its timings cannot be treated as a
product performance improvement.

Native macOS checks use one external target directory and
`CARGO_BUILD_JOBS=2 mise exec --`: touched-crate check/nextest, formatting,
all-target/all-feature clippy with warnings denied, and `mise run check:windows`.
Linux and Windows native results and the single dispatched CI run ID are
recorded in the final report after review fixes are pushed. The target is
cleaned and deleted at handoff.

No product bug or speculative product hardening has been added. A full runtime
simulator remains outside this ownership. An existing rustdoc warning in
unowned `crates/engine/src/lib.rs` links public `Successor` docs to private
`handover`; it affects documentation only and remains owner-pending (small
comment edit, observed on documentation builds).
