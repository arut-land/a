# Explicit schedule traces

The Infinity survey's section 2 identifies a reproducibility gap, not an
outstanding product defect or a measured product speedup. At baseline
`f343ff3708e9f70823788798fe089342f6e6dfcf`, kernel invariants already crash at
entry cuts and native admission tests already deliver flush notifications out
of order. They do not share an explicit decision artifact or an explorer that
shrinks those choices. This change adds that test infrastructure. It changes
no production code, dependency, protocol, log format or product limit.

## Workload and boundaries

`crates/kernel/tests/schedules.rs` supplies the vocabulary used by both the
kernel invariant suite and the engine admission suite:

| Token | Decision |
| --- | --- |
| `input` | The workload's one input arrival |
| `progress` | The held safe tool reports progress |
| `complete` | Deliver that tool's successful completion |
| `cancel` | Deliver the client's interrupt |
| `batch` | End a driver batch; the native runner requests sync and drains preceding ingress |
| `flush:1:ok` | Deliver the later flush notification |
| `flush:0:ok` / `flush:0:err` | Resolve the earlier held flush with the selected result |
| `crash:N` | Reload only the first N entries |
| `recovered-complete` | Explicitly complete the safe call restarted by the pure kernel |

The payloads are fixed by this workload: `sleep 1` and `finished` at the pure
boundary; the existing held harbor tool and recorded OpenAI cassette in the
native rig. There is no random seed, generated provider wire data, wall clock
choice or implicit response to an unfamiliar effect. Invalid or exhausted
scripts are rejected. The parser also rejects unknown tokens.

The explorer merges client cancellation, tool completion and the two flush
notifications, retaining order within each source. The initial input and
progress each have an explicit batch boundary. Twelve tail orders, sixteen
additional batch placements and two earlier-flush results give **384 live
schedules**. The real `Core::step`, `Core::resume` and fold run each schedule;
there is no copied transition function. Successful earlier epochs explore
all entry cuts; an earlier failure certifies none of the new turn, so its
recovery cut is zero. This gives **1,248 crash traces**, each spelling out a
recovery completion if one is needed.

`Core` does not accept flush notifications or know driver batches. Its trace
projection checks real admission requirements and recovery, and deliberately
leaves journal choices to the native runner. It does not simulate durability
with a second journal state machine. Six selected delivery/batch combinations
per earlier-flush result run through the actual engine, `Faulty` over a real
JSONL store, the held safe tool and the recorded provider fixture. These
include later notification before, between and after cancellation/completion,
with and without extra batch boundaries. A real power cut checks the retained
prefix, and each selected resulting log is restarted at every entry cut on
JSONL using the existing fixture's crash-prefix loading seam: 12 selected live
traces and 66 native prefix restarts in total.

The native flusher serializes its syscalls. As in the pre-existing epoch test,
the later notification is injected independently while the earlier syscall is
held. This exercises notification delivery, not concurrent native flush
syscalls. Prefix restarts are entry-cut recovery tests, not a claim to have
interrupted a filesystem syscall at every byte. Native readiness and process
lifetime tests remain necessary; this is not a runtime simulator.

The checks require safe work to wait for ordering, unsafe tool admission to
require durability (in the existing state matrix), one result for each retained
call, the selected completion outcome, the selected cancellation order, no
publication beyond an unresolved prefix, no follow-up after interruption,
no work at rest and the same ledger after folding the final log again. A
settled log must produce no new admitted work or facts on resume. Different
cancellation orders are allowed to record different facts.

## Replay and shrinking

A pure invariant failure prints the full trace and a minimized replay command:

```sh
A_SCHEDULE_TRACE='input batch progress batch cancel complete flush:1:ok flush:0:ok crash:2 recovered-complete' \
  cargo test -p kernel --test invariants every_held_epoch -- --nocapture
```

Native traces print the same token format, with a separate entry point:

```sh
A_NATIVE_SCHEDULE_TRACE='input batch progress batch cancel complete flush:1:ok flush:0:ok crash:2' \
  cargo test -p engine scripted_epoch_delivery -- --nocapture
```

Native replay supports the selected family: cancellation before tool
completion, both before the earlier flush resolves. Its provider/tool fixture
behavior is fixed; recovery has no competing test-owned completions. Pure
replay supports the whole explored family. Entry numbers belong to each
backend's log, since the native recorded-provider run also logs model facts.

Shrinking deletes decisions without rearranging surviving source events and
lowers crash cuts. Candidates must retain causal prerequisites and explicit
completion choices; invalid candidates do not count as reproduced invariant
failures. The shrinker is deletion-minimal under those constraints, not a
claim of globally shortest traces. Its tests use the real Core's cancellation
fact as a retained witness and check parsing, source order, crash-cut reduction
and unbound work rejection. Harness invariants return explicit errors, so
shrinking also works under the dev profile's Cranelift unwinding restriction.
A product panic is not converted into a harness invariant failure.

## Measurements

Measurements use release executables on the shared Apple M3, with
`CARGO_BUILD_JOBS=2` through `mise exec --`. Baseline executables were retained
before edits. Final comparisons alternate A/B and B/A, rather than collecting
all baseline samples before all candidate samples. Reported spread is the
observed minimum–maximum, not a significance interval. Builds and checks are
excluded from the timed workload.

Before editing, seven groups reproduced the two existing invariants. Each
kernel group ran twenty fresh processes; each engine group ran five. Including
the cold first group, kernel recovery was median 7.640 ms per invocation
(6.799–35.314 ms); reordered native notification was 7.524 ms
(7.145–90.964 ms). These reproduce the existing paths and their shared-host
spread. Neither baseline has an explorer to time.

Final measurements used seven alternating A/B–B/A groups after one warm-up
per executable. The host was an 8-core Apple M3 with 16 GiB RAM, macOS 27.0.1
(26A434). Wall time includes spawning fresh test processes; child user plus
system CPU comes from `getrusage(RUSAGE_CHILDREN)`. Each cell below is median
(minimum–maximum), in milliseconds. There were twenty invocations per group
for recovery and ten for notification delivery; both versions ran exactly one
matching test per invocation.

| Same workload | Before wall ms | After wall ms | Before CPU ms | After CPU ms |
| --- | --- | --- | --- | --- |
| `recovery_is_just_resume` | 6.703 (5.856–8.985) | 7.018 (6.172–8.925) | 4.313 (3.673–5.234) | 4.064 (3.729–4.388) |
| `later_epoch_notifications_cannot_skip_an_unresolved_prefix` | 20.163 (12.620–22.720) | 20.343 (14.834–23.766) | 9.622 (8.715–10.738) | 9.257 (9.167–10.516) |

The overlapping observed ranges provide no evidence of a product performance
change. Initial and final host-load levels differ, which is why the comparison
uses final interleaved observations rather than subtracting the initial cold
reproduction numbers.

The larger suites deliberately execute more work. Seven paired groups ran
three invocations of the kernel invariant suite and one of the admission suite
per group. These measure the coverage cost, not identical test sets:

| Suite | Tests before → after | Before wall ms | After wall ms | Before CPU ms | After CPU ms |
| --- | --- | --- | --- | --- | --- |
| Kernel invariants | 17 → 20 | 390.796 (354.270–441.398) | 525.872 (413.633–549.904) | 324.429 (305.885–354.527) | 439.936 (391.642–459.450) |
| Native admission | 21 → 22 | 121.220 (50.175–261.614) | 3042.936 (2938.105–3412.042) | 64.409 (50.830–82.833) | 327.607 (307.912–386.578) |

The new workloads were also run individually seven times, alternating with
the corresponding baseline reference. Separate fresh `/usr/bin/time -l` runs
measured process peak RSS; this is not retained heap or daemon memory:

| Added workload | Wall ms | CPU ms | Peak RSS MiB |
| --- | --- | --- | --- |
| 384 live schedules plus 1,248 crash traces | 89.189 (75.108–99.650) | 85.921 (71.678–95.849) | 3.500 (3.484–3.516) |
| 12 native traces plus 66 JSONL restarts | 2466.254 (1577.463–4058.196) | 235.386 (166.940–275.391) | 12.609 (12.500–12.703) |

There is no pre-change explorer workload to compare against. The fixed search
is small enough for ordinary tests; the native cost includes real files,
flushes and repeated engine startup. It is not a daemon CPU, RSS or latency
improvement.

To reproduce, retain the release executables from the baseline, rebuild with
`cargo test --release -p kernel --test invariants --no-run` and
`cargo test --release -p engine --lib --no-run`, then alternate the old and new
executables with `--test-threads=1`. The first table uses `--exact` with its
listed test names (the native name has the prefix
`driver::tests::admission::`). The suite table uses no filter for invariants
and the filter `driver::tests::admission::` for engine. The new tests are
`--exact schedule_checks::every_held_epoch_trace_settles_once_and_reloads_the_same_fold`
and `--exact driver::tests::admission::scripted_epoch_delivery_and_crash_cuts_preserve_native_admission`.
Always set the external `CARGO_TARGET_DIR` and use `CARGO_BUILD_JOBS=2 mise exec --`.
Raw paired samples, CPU observations, RSS samples, test counts and executable
SHA-256s are retained with the phase-1 report in
`scratchpad/r3/schedule-traces-measurements.json`.

## Verification and pending work

Phase 1 passed `cargo check -p kernel -p engine`, the touched crates' full
nextest run (506 passed, 14 skipped), formatting, all-target/all-feature clippy
with warnings denied and `mise run check:windows`. Release measurements also
reran both complete measured suites in every paired group. Linux and Windows native
execution belongs to phase 2 after independent review; CI is not dispatched
in phase 1. The only target directory is outside the worktree and is cleaned
at handoff.

No product bug has been found by this workload. No product hardening is added.
A full native scheduler simulator or cross-backend trace lowering with
identical log entry numbers would require broader test/runtime seams outside
this ownership; neither is claimed here. Their failure coverage, likelihood
of exposing another bug and implementation cost are unmeasured. They remain
owner-pending rather than a reason to change engine production files.

An additional `cargo doc --no-deps -p kernel -p engine` completed with an
existing warning: public `Successor` documentation links to private `handover`
in `crates/engine/src/lib.rs:1267`. It affects generated documentation, not
runtime behavior; it reproduces on documentation builds. Fixing the link would
be a small documentation edit outside the owned paths, so it is left for the
owner.
