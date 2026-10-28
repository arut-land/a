# Frozen context cuts

The context graph removes flattened inherited history from admission records.
An immutable content node holds a new local segment and references previous
segments and inherited cuts in the existing daemon-wide content store. Log
admission/spawn facts retain graph roots; a collector follows node references.
The protocol/view envelope still carries effective publication identities, and
execution authority remains private to each session. No dependency was added.

**Performance acceptance remains incomplete.** The former turn-64 frame failure
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

At the initial review, the late CPU gate remained incomplete:
`Shared::owner_image` read/folded the owner log before selecting small side
context, and new forks reconstructed/prepared their inherited messages. The
review revision below removes the repeated live owner-log scans. Process
counters still include complete-context encoding and transmission; no saving
from the unimplemented prepared-prefix proposal is claimed.

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

## Independent-review revision

Per-input owner side context now freezes a graph cut and reads its requested
exchange suffix directly. Admission extends only new local log entries;
`Shared::owner_image` and its repeated owner-log fold are removed. Taken inbox
messages delimit thread exchanges, so their bounded history also stops at its
requested suffix. Full-context forks and compaction retain their full context
contract. No history or feature is capped.

Live inputs borrow the exact producer presentation registry until settlement
or cancellation. A typed immutable presentation edge stores its source manifest
and selected registrations for cold replay without the owner log. That edge
does not introduce an inherited history scope: pending assistant tool calls
remain intact. It grants no child execution authority. Child ownership queries
also advance an incremental fact projection instead of rescanning old replies.

Projection tests now construct all cuts through production `context::append`.
Recorded Chat, Responses, Messages and Gemini request bytes are compared with
direct replay at every prefix, across single-entry and multi-entry appends,
model-result boundaries and overlapping cross-segment edits. Additional real
file-store tests cover bounded inbox reads, cold owner presentation recovery,
pending provider calls, incremental child ownership and admission-only graph
retention. The previous test-only context normalizer and layer writer are gone.

This revision changes PROTO 38 → 39. The corpus and view vectors were generated
by their tests; v38 remains immutable. No compatibility path or dependency was
added. The review comparison uses `bf01db76` as its before arm, rather than the
pre-graph baseline in the historical tables above.


The repeated review matrix uses the same workload and release build settings,
three interleaved before/after runs per scale. Only `measured-*` cases enter
these new figures. Values are median [minimum–maximum], n=3; CPU is daemon
user+kernel milliseconds per root turn. The slope is an ordinary least-squares
fit over rounds 9–32, not an isolated codec benchmark. Residency is after
construction and before rendering clients attach.

| Roots ×32 | Arm | First 4 CPU ms/turn | Last 4 CPU ms/turn | CPU slope 9–32 ms/depth | Live malloc MiB | RSS MiB | Physical footprint MiB |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | before | 78.21 [76.55–105.46] | 365.91 [365.53–420.74] | 10.40 [9.92–11.92] | 219.79 [219.62–220.46] | 303.16 [296.97–318.88] | 295.19 [288.99–310.92] |
| 1 | after | 92.62 [70.55–92.91] | 117.41 [116.16–130.77] | 1.57 [0.42–1.74] | 221.35 [220.70–221.59] | 243.98 [243.64–245.06] | 236.00 [235.67–237.08] |
| 4 | before | 83.29 [82.34–97.93] | 405.67 [401.33–506.93] | 11.55 [11.46–13.64] | 866.44 [864.27–868.41] | 933.20 [919.61–935.55] | 859.89 [843.58–860.85] |
| 4 | after | 77.11 [76.10–97.71] | 150.22 [147.01–203.78] | 2.76 [2.64–4.02] | 874.62 [869.81–875.75] | 956.84 [937.97–961.95] | 949.35 [930.45–954.47] |
| 8 | before | 87.70 [87.19–108.27] | 484.45 [466.43–518.15] | 14.54 [10.78–16.54] | 1723.87 [1721.55–1727.84] | 1942.84 [1930.11–1948.94] | 1896.69 [1879.60–1905.22] |
| 8 | after | 83.67 [80.65–84.75] | 187.48 [182.88–242.89] | 3.87 [3.82–6.50] | 1730.15 [1729.05–1737.26] | 1884.59 [1871.70–1887.30] | 1834.00 [1830.64–1841.19] |

Late CPU is substantially lower, but every after-arm slope remains positive.
The required flat end-to-end CPU gate is **unmet**; no owner waiver was received.
Full-context fork request traffic remains 0.323 MiB/turn in the first four turns
and 5.128 MiB/turn in the last four, in both arms (sum of fork-role request
bytes per root turn). The bounded cursor does not cap those requests. A separate
20-second native sampling diagnostic observes graph projection, provider
encoding and durability work; it does not isolate all remaining CPU by role.
Proposal #2 remains unstarted, pending #1 acceptance.

Live heap is slightly higher in this revision, and physical footprint does not
improve uniformly across scales. New cut and child-ownership indexes retain
metadata; this measurement is not an exclusive heap attribution. Reachable
graph storage also increases because per-input side cuts and thread history
now have durable roots. The complete report includes those bytes and client,
detach, attach/fold and cold-recovery tables. The first three after archives were
revalidated with presentation-edge traversal; this corrects attribution only,
without changing runtime samples.

All three 64-turn cases completed an additional live turn and a fresh-daemon
reattach with matching hashes: 65 turns and 520 tool calls each, all done.
Across the 21 matrix/acceptance cases, 2,769 root turns and 22,152 tool calls
completed. The separate sampling diagnostic is excluded from these counts.
All 491 engine/protocol tests passed (15 skipped), along with fmt, package
check, clippy with warnings denied and the Windows workspace cross-lint.
Rustdoc retains its pre-existing private-link warning. The earlier detached
cancellation assertion did not recur, so no unrelated cancellation fix is claimed.

The phase-1 review report and complete evidence are in the scratch directory
above: `context-graph.review.report.md`, `measured-summary.json`,
`measured-traffic.json`, `review-provenance.json`, and `review-completion.json`.
No CI was dispatched. This revision is for re-review; the CPU gate is still open.
