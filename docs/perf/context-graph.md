# Frozen context cuts

The context graph removes flattened inherited history from admission records.
An immutable content node holds a new local segment and references previous
segments and inherited cuts in the existing daemon-wide content store. Log
admission/spawn facts retain graph roots; a collector follows node references.
The protocol/view envelope still carries effective publication identities, and
execution authority remains private to each session. No dependency was added.

This is a **partial performance acceptance**. The former turn-64 frame failure
is fixed, and link-image duplication is removed. Late-turn CPU is substantially
lower but still rises. Proposal #2, forkable prepared encoded prefixes, was not
started because the owner's condition requires #1 to be green first. Client
paging remains stage B; compiled-offer/catalog interning was not approved.

## Method

Apple M3, 16 GiB, native macOS 27.0.1 arm64. Baseline `f3a51b4e` and the graph
patch on that base use the same `cargo build-release -p a --features fixtures`
profile: opt-level z, fat LTO, one codegen unit and rebuilt std, through
`mise exec --` with `CARGO_BUILD_JOBS=2`. Builds use one external target directory.

Three fresh-home samples per arm and scale, interleaving A/B and reversing
scale order between repetitions on the shared host. Each turn runs a loop
agent, fresh/inherited/background children, a grandchild, a real background
shell job, file/skill tools and title side conversations. Five agents stream
128 KiB each, with eight actual tool calls per root turn. A synthetic loopback
Chat SSE provider accepts unsummarized large contexts. Runtime environments
are allowlisted, with loopback URLs and dummy credentials; inherited provider
and proxy settings are absent. This measures engine/client residency, not
model capacity, quality or billing.

After 32 construction turns, all roots attach real PTY TUIs, separate public
Rust FFI Session processes and browser tabs through one web server. An extra
live turn per root exercises streaming consumers. Each case verifies complete
reply/tool counts, all durable outcomes done, FFI/reducer state hashes, browser
item counts, warm/cold reattach hashes and content-node hashes. Default pressure
reclaim and hibernation remain enabled. No feature or history cap was added.

All values below are median [minimum–maximum], n=3. Live malloc comes from
native allocator-zone inspection; physical footprint is macOS charged memory.
CPU is daemon user+kernel milliseconds per root turn, excluding the fixture
provider and shell children. Heap inspection is outside timed windows.

## Results

After construction, before rendering clients attach:

| Roots × 32 turns | Arm | Live malloc MiB | Physical footprint MiB | Mean CPU ms/turn | Last-four CPU ms/turn |
| --- | --- | --- | --- | --- | --- |
| 1 | baseline | 530.66 [526.48–531.63] | 1284.14 [1278.45–1296.24] | 618.28 [612.07–781.84] | 1480.68 [1473.91–1655.41] |
| 1 | graph | 220.60 [218.89–221.40] | 302.14 [300.70–307.69] | 213.62 [203.62–233.66] | 374.82 [362.09–436.01] |
| 4 | baseline | 2117.71 [2112.30–2143.33] | 3485.72 [3379.18–3552.97] | 835.74 [799.54–936.07] | 1981.17 [1969.18–2420.38] |
| 4 | graph | 865.71 [864.19–870.14] | 874.06 [867.60–875.28] | 243.98 [225.09–254.16] | 394.63 [377.32–436.25] |
| 8 | baseline | 4160.09 [4154.69–4194.82] | 5036.47 [4680.66–5355.33] | 909.75 [828.48–955.30] | 2253.94 [2165.55–2418.25] |
| 8 | graph | 1723.07 [1716.62–1728.23] | 1882.52 [1874.47–1912.36] | 262.08 [254.53–309.29] | 444.93 [419.49–491.17] |

For one root's 33-turn completed history, logical context bytes in parent links
fell from **136.31 [136.30–136.31] MiB** to **0.02 [0.02–0.02] MiB**. These are serialized bytes,
not a measured exclusive heap total. Large payloads now live in one reachable
node closure instead of being serialized into every descendant admission.

Three candidate 64-turn constructions, their extra live turns and fresh-daemon
reattaches all completed: 65 root turns and 520 tool calls per case, all done.
The baseline reproduced a stream closure on turn 64; its generated link at
seq 2152 has a 16,932,256-byte stored line. The 16 MiB frame ceiling remains
unchanged. No inherited input was truncated to pass the case.

The late CPU gate remains incomplete. `Shared::owner_image` still reads/folds
the owner log before a per-input side request selects its small context.
A new fork also reconstructs/prepares its inherited messages. Whole-session
process counters do not isolate those costs from necessary wire transmission.
No numerical saving from the unimplemented prepared-prefix proposal is claimed.

## Verification and scope

Invariant tests cover shared immutable nodes, parent-edit isolation, branch
append sharing, bounded inherited-history reads, restart/source-log removal,
transitive blob lifetime, and I/O errors. The admission-only root test failed
without the `link` retention fix and passes with it. Existing edit/compaction,
queue/job, model-only interpretation and four-provider byte parity remain.
PROTO changes 37 → 38; wire corpus/view vectors were generated by their tests,
with no old corpus or provider cassette edits and no compatibility path.

Before integration, 484 engine/protocol tests passed (15 deliberately ignored),
fmt, package clippy with warnings denied and the Windows workspace cross-lint
passed. An unchanged cancellation timing assertion failed once, then passed a
focused rerun and the full rerun; no unrelated ordering fix was added. Rustdoc
has an existing private `handover` link warning. Native Linux/Windows performance
and C ABI/Swift/SwiftUI store amplification are not measured.

The repeated matrix predates main integration at `f343ff37` (MCP instances and
Windows retirement). Post-integration fmt, check, all 484 tests, clippy, Windows cross-lint and release
build passed again. A separate integrated 64-turn smoke completed its extra live
turn and cold reattach with matching hashes (65 turns, 520 tools). These tables
are not a new repeated measurement of that integrated tree.
The unmet CPU gate is explicit, and CI awaits independent review.

Raw provenance, sources, repeated native counters, allocation stacks, renderer
hashes and compressed stopped-home archives are retained outside the worktree:
`/private/tmp/claude-502/-Users-rajaniraiyn-work-a/a536a1f2-a8c3-4ba1-8862-89a02f8c91c0/scratchpad/r3/context-graph`.
Only `accepted-*` samples enter the figures. The full phase-1 report includes
RSS, live consumer CPU/memory, attach/fold spreads, lifecycle states and detailed
allocation attribution. Earlier diagnostic runs are excluded.
