# Admission deduplication

## Decision

Stage 4 is measured and closed. The child keeps its frozen authority copy,
so child start, reload and restart stay a local read with no parent lookup.
The parent's admission record stays the durable fact written first, and
stage 3's retention rule is unchanged.

The measured ceiling is under 0.43% of retained bytes on the stage-3 workload,
so no hybrid can pay for a lookup there. Reopen only if measured real sessions
show copied authority above about 2% of retained bytes: the many-children,
short-output shape measured at 2.2% on echo.

2026-11-03. Branch `admit-dedup`; fetched main and measured baseline
`624e76a58abb85ff05135e67ad7aabc4cb42c918`.

The complete stage-3 construction workload would save only **0.385–0.392%**
of retained logical bytes. Even deleting every entire `Spawned` row and its
index position, charging nothing for a reference, projects less than 0.426%.
The echo-only matrix is a counterexample to calling every saving negligible:
it projects **2.17–2.19%**, roughly 203 KiB per root at 32 turns. That is near
the brief's example threshold, rather than well under it.

## Verified finding

`driver/spawn.rs::SpawnIntent` freezes the request, budgets, model and routing,
installed generation set, selected extensions, settings, admission time,
refusal and inherited context metadata. Ordinary task admissions persist that
intent in parent `Body::Link`; `lib.rs::owned_conversation` imports it again
as child `Body::Spawned` at sequence 1. `driver/spawn.rs::restore` reads that
child copy on load. The child's header already records the parent session and
admission position.

There are two canonical source shapes:

- Agent children refer to the parent's `Link`, selected by `Header.parent.call`.
  The source child id and entire copied intent match.
- Persisted model-only sessions refer to a parent `ModelCall` containing a
  `ThreadImage`. `driver/side.rs::model_turn` copies its authority but clears
  the child's per-turn `ContextCut`. The benchmark preserves that existing
  transformation before comparing recovered values.

The existing `Jsonl::cut` boundary can retrieve either exact source fact.
Stage 3's retained hard links supply the approved lifetime rule. No authority,
context, payload, watch, budget, cache, protocol or log code was changed.

## Method and evidence

The raw evidence was scratch-only and has since been deleted; the tables here
are the record. Raw evidence, stopped homes, baseline binary, scripts and
temporary test sources were stored at:

```text
/private/tmp/claude-502/-Users-rajaniraiyn-work-a/57b9c5f0-cf3a-47ba-ba38-f417726d005b/scratchpad/r/admit-dedup
```

Call this directory `E`. `build.json`, `gate_matrix.py`, `matrix_support.py`,
`extend_echo.py`, `lookup-helper.rs`, `memory-helper.rs`, `memory.py`,
`audit.py`, `summarize.py` and their JSON/log outputs recorded the method.
The original stage-3 helper was reused for its loopback SSE fixture.

Host: native macOS 27.2 arm64, Mac15,13, 16 GiB. The fixture binary was built
through `mise exec -- cargo build-release -p a --features fixtures`, opt-z,
fat LTO, one codegen unit and rebuilt std. Its size is **7,821,104 bytes**;
SHA-256 `d5fda8ca320e7c372a0922a25790aeab4b84161fca187ed9ef32cc2e73f0032d`.
The boundary tests use the same release optimization profile through
`cargo nextest run --release -p engine` (ordinary toolchain std).
Both A/B arms execute the same test binary and production store/authority codec.

All Cargo commands use `mise exec --`, `CARGO_BUILD_JOBS=2`, and only:

```text
/private/tmp/claude-502/-Users-rajaniraiyn-work-a/57b9c5f0-cf3a-47ba-ba38-f417726d005b/scratchpad/tgt-admit-dedup
```

`E/run.py` enters mise from an external configuration without the repository's
dotenv loader, then uses absolute toolchain binaries. Runtime probes construct
allowlisted environments: isolated `HOME`/`A_HOME`, dummy OpenAI credentials,
loopback base URLs and `NO_PROXY=127.0.0.1,localhost`. Provider/proxy variables
are not inherited. Echo uses port 1; SSE uses an owned loopback server. No real
model request, user's `~/.a` access, `.env.local` read/print, or broad kill.
Every daemon stops with its own `A_HOME` and `a daemon stop`.

Five fresh homes per scale: 1/4/8 roots ×32 turns and 1 root ×64 turns.
Scale order alternates between rounds. Echo 1×32 was the initial control;
the remaining echo scales followed in a separate alternating five-round series.
The stage-3 construction portion is reproduced with its five 128 KiB agent responses, eight tools per root turn,
fresh/inherited/background children, nested grandchildren, title side threads,
file/skill tools, a shell background job, and the extra live turn. Its 20 homes
contain the same 19,900 sessions / 9,880 child links as one arm of log-cuts.
TUI/FFI/browser attachment phases were not needed for this storage gate.

The brief requests echo, while the historical log-cuts matrix uses SSE.
Therefore both were measured, rather than inserting large child prompts to
make echo manufacture SSE-sized replies and inflating the duplication under
study. Echo uses 128 KiB root text plus short fresh/nested, inherited and
background task prompts in each logical turn. It has four task calls per
logical turn, no model-only title threads, and an extra finishing input.
Its storage profile is deliberately reported separately.

Disk accounting deduplicates file identities across sessions, blobs and the
catalog index, as stage 3 did. These are logical file bytes, not allocated disk
blocks. Existing log records are emitted by the real daemon, never hand-written.
The inventory checks every copied authority against its canonical parent fact.
Net savings are **projections**: replace the current intent value with compact
JSON `{session, through}`. This example reference costs 62–65 bytes. Header
reuse could avoid that repetition; the whole-row projection bounds its effect
on this conclusion. No candidate log format or second daemon binary exists.

## Bytes

All table entries are median [min–max], n=5 homes. Bytes are exact integers.
Child counts include persisted model-only sessions where present.

| Workload | Roots × turns | Children/home | Retained bytes | Projected bytes saved | Projected percent saved |
| --- | --- | ---: | ---: | ---: | ---: |
| stage-3 SSE | 1 × 32 | 265 | 109,764,112 [109,764,074–109,764,265] | 422,905 [422,892–422,907] | 0.38528 [0.38527–0.38529]% |
| stage-3 SSE | 1 × 64 | 521 | 215,801,884 [215,801,608–215,805,813] | 831,640 [831,633–831,665] | 0.38537 [0.38536–0.38538]% |
| stage-3 SSE | 4 × 32 | 1,060 | 432,563,968 [432,563,594–432,566,645] | 1,691,598 [1,691,564–1,691,619] | 0.39106 [0.39106–0.39107]% |
| stage-3 SSE | 8 × 32 | 2,120 | 862,964,467 [862,962,668–862,966,150] | 3,383,197 [3,383,151–3,383,206] | 0.39204 [0.39204–0.39205]% |
| echo | 1 × 32 | 128 | 9,554,264 [9,554,259–9,554,277] | 207,536 [207,534–207,540] | 2.17218 [2.17216–2.17222]% |
| echo | 1 × 64 | 256 | 18,979,484 [18,979,455–18,979,503] | 415,141 [415,130–415,149] | 2.18731 [2.18726–2.18735]% |
| echo | 4 × 32 | 512 | 38,217,112 [38,217,070–38,217,119] | 830,163 [830,146–830,165] | 2.17223 [2.17219–2.17223]% |
| echo | 8 × 32 | 1,024 | 76,434,166 [76,434,137–76,434,182] | 1,660,304 [1,660,291–1,660,310] | 2.17220 [2.17219–2.17221]% |

Complete copied intent size and net per-child projection, pooling the five homes at each scale:

| Workload | Runner | Children | Intent bytes | Reference bytes | Projected saved bytes/child |
| --- | --- | ---: | ---: | ---: | ---: |
| stage-3 SSE | Agent | 9,880 | 1,617.5 [1,609.0–2,026.0] | 64.0 [63.0–65.0] | 1,553.5 [1,546.0–1,961.0] |
| stage-3 SSE | ModelOnly | 9,950 | 1,600.0 [1,596.0–1,605.0] | 62.0 [62.0–63.0] | 1,538.0 [1,534.0–1,542.0] |
| echo | Agent | 9,600 | 1,659.5 [1,644.0–1,773.0] | 64.0 [62.0–65.0] | 1,595.5 [1,582.0–1,708.0] |

Across all 20 stage-3 SSE homes: **8,105,478,109 retained bytes**, **31,646,638 projected bytes saved** (0.39044%).

Across all 20 echo homes: **715,925,073 retained bytes**, **15,565,689 projected bytes saved** (2.17421%).

This does not count any incremental ancestor bytes that admission retention
would keep after visible deletion. That can increase retained storage for a
fresh-only tree; the owner-approved retention rule remains necessary. Larger
custom prompts/settings may change the tradeoff; they are not a measured win here.

## Lookup A/B

A is current main: decode its existing copy. At start it is already available;
reload/restart read the child's first fact. B retrieves the exact parent fact
through the production indexed cut reader, then decodes the same typed value.
Reload/restart B also performs the child first-fact read, as a reference-bearing
child would require. Its on-disk first fact still contains today's authority,
so B is a conservative boundary prototype, not a latency measurement of a new
format. Equality is checked outside timing for every recovered authority.

Each home receives five paired passes, alternating A/B order. A pass times all
children and divides by child count. Tables summarize the five per-home medians:
median [min–max], n=5 independent homes. **Microseconds per child.** Restart
means a fresh `Jsonl` store with common header location prepared before timing;
it excludes catalog discovery, process startup and model/context execution.
Filesystem caches are not purged. Echo 1×32 was timed in the initial series;
other echo scales followed separately. Compare paired arms within a row, not
absolute latency across those series. No build or workload construction from this
lane overlaps these timed passes.

### Start boundary

| Workload | Roots × turns | A: copy, µs | B: reference, µs |
| --- | --- | ---: | ---: |
| stage-3 SSE | 1 × 32 | 34.9 [34.6–37.7] | 278.1 [226.9–322.3] |
| stage-3 SSE | 1 × 64 | 34.6 [32.6–40.0] | 321.2 [272.8–370.6] |
| stage-3 SSE | 4 × 32 | 34.2 [25.2–41.5] | 308.1 [196.7–416.1] |
| stage-3 SSE | 8 × 32 | 31.9 [26.2–40.0] | 318.6 [257.6–404.3] |
| echo | 1 × 32 | 39.9 [36.5–47.5] | 359.5 [281.0–556.1] |
| echo | 1 × 64 | 14.5 [14.4–14.6] | 115.1 [113.7–115.9] |
| echo | 4 × 32 | 14.5 [14.4–15.1] | 114.1 [113.9–117.3] |
| echo | 8 × 32 | 14.5 [14.4–14.6] | 114.4 [113.8–114.7] |

### Reload boundary

| Workload | Roots × turns | A: copy, µs | B: reference, µs |
| --- | --- | ---: | ---: |
| stage-3 SSE | 1 × 32 | 94.7 [87.7–140.5] | 365.0 [264.2–385.9] |
| stage-3 SSE | 1 × 64 | 129.6 [92.3–157.7] | 377.1 [295.8–405.6] |
| stage-3 SSE | 4 × 32 | 127.9 [86.5–204.4] | 386.1 [258.0–552.1] |
| stage-3 SSE | 8 × 32 | 125.2 [107.9–168.7] | 384.3 [336.1–511.5] |
| echo | 1 × 32 | 99.9 [87.8–136.4] | 393.0 [249.7–487.6] |
| echo | 1 × 64 | 37.4 [37.1–37.8] | 137.8 [137.5–138.9] |
| echo | 4 × 32 | 37.4 [37.3–37.8] | 137.2 [137.1–137.9] |
| echo | 8 × 32 | 37.4 [37.2–37.5] | 137.4 [137.3–137.9] |

### Restart boundary

| Workload | Roots × turns | A: copy, µs | B: reference, µs |
| --- | --- | ---: | ---: |
| stage-3 SSE | 1 × 32 | 120.7 [89.2–145.9] | 365.9 [322.6–441.0] |
| stage-3 SSE | 1 × 64 | 117.6 [95.7–133.6] | 348.0 [333.2–385.5] |
| stage-3 SSE | 4 × 32 | 129.3 [86.1–179.6] | 405.2 [256.1–598.0] |
| stage-3 SSE | 8 × 32 | 129.0 [111.2–184.0] | 430.0 [333.5–538.8] |
| echo | 1 × 32 | 109.7 [94.6–124.5] | 428.1 [301.5–604.1] |
| echo | 1 × 64 | 37.3 [37.0–37.8] | 137.7 [136.8–139.1] |
| echo | 4 × 32 | 37.3 [37.2–38.0] | 137.0 [136.8–137.8] |
| echo | 8 × 32 | 37.4 [37.2–37.4] | 137.4 [136.8–137.8] |

### Retained source without an index

The helper hard-links actual source bytes into an isolated retained-only store,
with no catalog, visible parent or derived index, and reads the latest admission
per parent five times through fresh stores. It validates both nested source
shapes. Below are root-parent admissions; pooling short nested logs would hide
these scans. Values are milliseconds, median [min–max] across five home medians.
Each home median covers its roots and five passes. This is source lookup only,
not incremental whole-daemon restart time: context replay may need the same
source independently.

| Workload | Roots × turns | Retained root lookup, ms |
| --- | --- | ---: |
| stage-3 SSE | 1 × 32 | 19.55 [13.40–21.92] |
| stage-3 SSE | 1 × 64 | 34.38 [33.95–52.45] |
| stage-3 SSE | 4 × 32 | 18.81 [12.72–22.58] |
| stage-3 SSE | 8 × 32 | 17.80 [15.24–24.00] |
| echo | 1 × 32 | 13.23 [8.04–13.75] |
| echo | 1 × 64 | 9.22 [8.96–9.74] |
| echo | 4 × 32 | 4.58 [4.52–4.59] |
| echo | 8 × 32 | 4.58 [4.53–4.61] |

## Live resolved authority and CPU

A separate release process holds every one of the 2,120 decoded authorities
from an eight-root stage-3 home. Five paired rounds alternate order. After
resolution, `/usr/bin/heap --noContent` samples live malloc; `proc_pid_rusage`
samples charged memory and process CPU around resolution. Native timebase is
125/3 ns per tick (`timebase.json`), verified against an owned process using
`process_time_ns` (`cpu-crosscheck.json`). Values are median [min–max], n=5.

| Arm | Live malloc bytes | Charged bytes | Batch CPU, ms | Batch wall, ms |
| --- | ---: | ---: | ---: | ---: |
| copy | 11,303,888 [11,303,888–11,303,888] | 17,236,424 [15,909,320–17,301,960] | 161.65 [131.95–189.56] | 206.35 [132.08–353.01] |
| reference | 11,303,888 [11,303,888–11,303,888] | 17,236,424 [16,237,000–17,301,960] | 547.54 [358.11–591.67] | 642.86 [367.57–656.12] |

Live resolved-authority heap is identical; median charged memory is identical.
Reference resolution uses more CPU. These measure **the store boundary**,
not daemon heap/charged-memory acceptance. Serialized frame storage could differ
in a future implementation. Avoided child encoding/writes were not benchmarked;
no whole-admission, late-turn CPU or whole-daemon memory speedup is claimed.

## Verification and exclusions

- Native release fixture probes: both complete matrices; every child authority
  matches its canonical source with the existing model-only transformation.
- `cargo nextest run --release -p engine`: **432 passed, 15 skipped** at the
  baseline with the temporary lookup diagnostic ignored. Existing admission
  crash-cut, frozen-settings reload, presentation-generation and catalog-loss
  invariants pass (`engine-tests.log`).
- The brief's exact `crash_after_admission_creates_the_same_child_once` name is
  absent from this main revision. Do not mistake the passing broader
  `scripted_epoch_delivery_and_crash_cuts_preserve_native_admission` test for
  that named test. `nested_children_remain_addressable_after_the_catalog_is_lost`
  and `settings_roles_and_frozen_child_sets_survive_reload_without_widening` pass.
- Two release lookup diagnostic invocations and the native heap/CPU diagnostic
  pass. The reference prototype exercises real store files and the production
  cut reader and authority codec. Temporary tests were stored in scratch-only `E`
  and removed from the committed diff; those scratch files have since been deleted.
- `mise run check:windows`, through the isolated mise configuration and assigned
  target: passed. The initial diagnostic helper triggered precision-loss lints;
  integer aggregation fixed those. Final lint runs after helper removal too.
- `cargo fmt --all -- --check` and `git diff --check`: passed after removal.
- Initial echo completion detection incorrectly required an assistant reply
  from a tool-only nested child; that smoke run is excluded. A slow inventory
  re-read parent logs repeatedly; its stopped files were retained separately
  in the deleted scratch evidence.
  Interrupting it raced its advance into the first 64-turn case, overlapping
  the replacement. That home is excluded, both scripts ended, and a fresh
  directory rerun passed. The harness now refuses unfinished existing case
  directories. No timing from diagnostic/overlapped construction enters results.

`audit.json` and `audit-final.log` record the final real-file completeness audit.
The final audit covers **29,570 sessions and 19,480 child links**, with no
unfinished calls or failed outcomes. All accepted daemons stopped with their
isolated `A_HOME`. No recorded fixture, cassette, encode golden, wire corpus
or view vector was edited.

Only this report is committed: **0 product Rust lines added or deleted**.
PROTO stays 46 and JSONL stays 2. No candidate binary-size delta is asserted,
and the implementation-only daemon acceptance matrix is not claimed. No full
CI workflow was dispatched. The raw evidence was scratch-only and has since
been deleted; the tables here are the record.

No speculative hardening was added.
