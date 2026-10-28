# Residency waves

`scripts/perf/residency.py` measures ownership across complete daemon/session
lifecycles. This is measurement infrastructure, not a product memory fix.
The motivating Infinity survey identified a missing workload, not a reproduced
leak. There was no earlier version of this workload to measure before adding
it. Both interleaved arms below run the same release binary; A2 minus A is a
noise control, never a claimed optimization.

The default matrix varies **stored histories 1/8/32 with one attached session**,
then **attached sessions 1/4/8 at 32 stored histories**. A one-history,
no-attached-client control exercises the same timing boundaries. Duplicate
matrix points run once per arm. Five fresh-home rounds reverse both arm and
population order, giving five samples per arm per point. Every stored session
first completes four 64 KiB echo turns. Both waves add four turns per attached
session; all histories remain intact. These finite workload sizes impose no
product session, history or message limit.

Each sample records the settled catalog, completed wave while clients remain
attached, normal-client detach, the slow follower alone, acknowledged
hibernation, a second wave, and reopening the earlier sessions. Attached means
resident and observed; the used-stage turn has finished, rather than holding
CPU busy artificially. Inputs are sent to all attached sessions before replies
are consumed. The slow follower stops reading after initial synchronization,
then drains through the last exact reply and idle acknowledgement before
closing. A slow or disconnected follower is a failed sample, not successful
work with discarded history. This fixture covers finite reader lag, not an
indefinitely stalled consumer.

The public catalog must have exactly the requested population, idle activity,
and the expected resident/client counts. Hibernation requires every session
to report `hibernated` with zero clients, bounded by a failure deadline.
**The public summary also represents the internal stopping state as
hibernated.** Twelve additional seconds allow blocking workers to retire;
this is a settling interval, not proof that an internal weak owner expired.
The existing real-engine `hibernating_frees_the_session` invariant separately
checks that owner. A stronger per-sample actor-exit acknowledgement needs an
engine/protocol interface outside this branch's ownership. Do not infer a leak
or prove its absence from the public summary alone.

Every initial attach, second-wave attach and final reopen decodes with the
real protocol crate and compares the entire replay, including the synced
sequence, against the independently read durable log. Latency starts just
before Attach and ends at Synced, excluding connection/Hello, log comparison
and process startup. Summaries retain the slowest session per sample, then
report its median and full range over rounds. They are not tail percentiles.
The generated probe's invariant rejects incomplete or prematurely synced
replay as a sample. The Python integration invariant runs two real sessions
through both waves, verifies the slow follower pins only its session, and
checks complete history on reopen.

## Accounting and interpretation

Charge comes from the merged `sys::process_memory` reader: macOS
`phys_footprint`, Linux PSS, Windows private commit. Errors fail the sample;
RSS is diagnostic. An external observer samples native charge every 200 ms
through a separate probe process. Its cumulative sampled peak is a lower
bound, not a native lifetime maximum. Available OS lifetime RSS/footprint
peaks from `process.snapshot` are retained separately. The observer does not
attach to any session. The daemon's process CPU, context switches and thread
counts exclude the probes; observer and catalog requests finish before idle
CPU windows. There are no periodic catalog requests during those windows.
Native memory-map diagnostics after the second hibernation aid attribution.

Catalog growth is expected: immutable headers and last summaries remain for
all stored sessions, and files retain complete histories. Compare the settled
`catalog` across stored populations at fixed attached count. Within each
fresh process, compare used-minus-catalog, first-hibernated-minus-catalog and
second-hibernated-minus-first-hibernated. The JSON summary reports these
paired lifecycle differences and separate A2-minus-A differences by round.
An increased charged footprint can include allocator retention, compressed
memory or native caches. It cannot by itself identify a retained actor,
projection, log cursor or client queue. Native map output likewise does not
provide a portable live-allocation census.

Context-graph owns inherited context representation; turn-residency stage B
owns client transcript paging. This workload uses the echo fixture and
protocol clients, so it neither measures inherited model contexts nor claims
TUI/web/FFI paging savings. Interpret their live-history evidence separately;
no files in either product lane are changed here. No product fix, allocator
trim, history cap, new dependency, log change or wire change is included.

## Reproduction

Use Python 3.11+ and psutil 7.2.2. Build outside the checkout, with two Cargo
jobs on a shared host. All builds and checks should finish before timing.
For example, with an absolute external target and output directory:

```sh
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR="$target" mise exec -- \
  python scripts/perf/residency.py --target "$target" --output "$output" --build-only
PYTHONDONTWRITEBYTECODE=1 CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR="$target" mise exec -- \
  python scripts/perf/residency.py --target "$target" --output "$samples" \
    --binary "$output/a-fixtures" --probe "$output/residency-probe"
```

Set `RESIDENCY_BINARY`, `RESIDENCY_PROBE` and `PYTHONPATH=scripts/perf`, then
run `python -m unittest residency.ResidencyTests` for the real lifecycle
invariant. Generated probe tests, fmt and clippy run during its build.
`cargo nextest run -p engine --lib -E 'test(hibernat)'` exercises the existing
private release invariants independently. All daemon homes are temporary;
the shared `lifecycle.py` helper stops their exact daemon with `A_HOME` and
`a daemon stop`. Its initial line client is closed before measured stages.
The isolated benchmark sets `hibernate_secs=1`, not a proposed product policy.
After measurements, `cargo clean --target-dir "$target"` and remove that
owned target directory.

The manual `perf` workflow input `residency=true` runs only this workload on
Linux x64/ARM64 glibc, macOS ARM64/x64 and Windows x64/ARM64. Each lane builds
natively, retains raw JSONL, summary distributions, failures and binary
identities, and cleans its target. Cross-compilation is not native evidence;
musl is not covered by these six runner labels. `paired_rounds` must be at
least five. A failed sample fails the job and is retained, never silently
removed from a distribution.

## Evidence

Native M3 and hosted results will be recorded after the workload's validation
and full interleaved runs. CI dispatch remains reserved for independent
review and phase 2.
