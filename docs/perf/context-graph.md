# Frozen context cuts

The context graph removes flattened inherited history from admission records.
An immutable content node holds a new local segment and references previous
segments and inherited cuts in the existing daemon-wide content store. Log
admission/spawn facts retain graph roots; a collector follows node references.
The protocol/view envelope still carries effective publication identities, and
execution authority remains private to each session. The initial graph implementation added no dependency; the later persistent compiler dependency is measured in [persistent-compiler.md](persistent-compiler.md).

The owner subsequently approved proposals #1 and #2 together. The current
revision adds forkable prepared encoded prefixes, bounded graph cursors across
inherited scopes and incremental admission lookup. Its operation-scoped handle follow-up removes idle descriptor retention and the measured launch slope. The latest indexed-supervisor follow-up removes all-session pulse scans and reduces wide user CPU. The owner accepts the CPU gate as met: owner-log scans, descriptor-dependent launch cost and supervisor scans are removed; remaining growth is full-context fork_task transmission plus noise. Independent review approved merging. The final section records the measurements, historical verdict and owner decision. The sections
below preserve the earlier measurements and their unmet gates as historical
evidence. Client history paging remains stage B; catalog interning was not
approved.

## Historical initial graph measurement

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
**Historical verdict, superseded by the owner decision below:** The unmet CPU gate is explicit, and CI awaits independent review.

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
**Historical verdict, superseded by the owner decision below:** The required flat end-to-end CPU gate is **unmet**; no owner waiver was received.
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

## Shared prepared prefixes and final graph cursors

The owner approved proposal #2 on this branch, with #1 and #2 accepted together.
Forks now share immutable prepared byte fragments and ID maps, with a private
mutable tail. Compiler identity includes concrete codec and wire quirks,
model/origin preparation, semantic registration scope, tools/offers, options/body
and fitting policy. A changed identity rebuilds authoritatively. Repaired or
uncompiled owner tails cannot claim a settled context prefix. Parent reclaim
releases its lease without invalidating live children. The four provider codecs
retain recorded byte parity; no recorded provider fixtures changed.

Bounded side context spends its requested exchange window across inherited
scopes, preserves system-only model inputs and follows dependencies only in the
requested suffix. Full-context fork_task remains full-context. Settled admission
lookup advances only new links. Production append tests cover both fixes and
provider replay across edits and incremental model-result boundaries.

The final native suite passed 837 engine/LLM/protocol tests, with 28 skipped.
fmt, package check, clippy -D warnings with both supported TLS variants
separately, Windows cross-lint, rustdoc, dependency bans and release build passed.
The FFI suite passed 28 tests after graph/cursor integration. No cancellation
regression recurred. PROTO 40 integrates the graph format with main's MCP changes;
wire/view outputs were generated only by their tests.

This comparison uses a common product/main baseline: 61f02b5d with only 928b4442
reversed before, and 855c100b after. Main 3a32fe61 adds only perf tooling/docs to
the already integrated main 855d6062. Frozen source trees, patches, binary hashes
and runner hashes are in context-final-provenance.json. There are three
interleaved runs per arm and scale, plus three complete after 64 cases. Runtime
provider environments are allowlisted with loopback URLs and dummy credentials.
Only context-final-* cases enter these tables; one interrupted case was restarted
fresh, and all completed cases were preserved. Full consumer, lifecycle and
latency tables are in the phase report beside the raw evidence.

**Historical verdict, superseded by the owner decision below:** **CPU acceptance is not met.** The raw CPU slopes below are positive, especially at wide concurrency. Required outgoing bodies are preserved and grow with full history. Independent native controls below show that send work alone does not explain the residual. Provider receive wall time is never subtracted from daemon CPU. This branch is not ready to merge; further store-file ownership is requested in the phase report.

All figures are median [minimum–maximum], n=3; CPU is ms per root turn, memory MiB. Slopes are OLS over rounds 9–32 (64-run slopes9–64). No provider body receive wall time is subtracted from CPU.

### Construction residency and CPU

| Roots ×32 | Arm | Live malloc | RSS | Physical footprint | First4 CPU | Last4 CPU | CPU slope ms/depth |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | before | 222.41 [222.40–222.41] | 243.22 [242.80–246.30] | 235.08 [234.66–238.16] | 69.07 [68.99–71.71] | 116.97 [116.15–128.61] | 2.010 [1.781–2.221] |
| 1 | after | 112.97 [112.97–112.97] | 163.08 [162.81–163.22] | 154.88 [154.63–155.00] | 67.85 [66.30–68.05] | 70.33 [69.96–72.77] | 0.216 [0.203–0.598] |
| 4 | before | 872.47 [870.03–878.22] | 930.22 [923.20–936.55] | 922.53 [915.52–928.86] | 75.69 [75.63–76.31] | 146.79 [146.12–147.11] | 2.656 [2.595–2.705] |
| 4 | after | 449.79 [449.79–449.80] | 617.61 [612.45–626.58] | 609.70 [604.53–618.67] | 74.39 [73.35–80.10] | 98.36 [97.86–103.40] | 1.042 [0.951–1.242] |
| 8 | before | 1733.72 [1731.82–1735.93] | 1890.28 [1884.34–1892.72] | 1849.71 [1837.80–1859.30] | 80.39 [80.28–80.71] | 182.68 [182.19–188.93] | 3.869 [3.735–4.090] |
| 8 | after | 898.75 [898.73–898.77] | 1161.31 [1160.81–1162.84] | 1129.11 [1128.55–1130.95] | 77.95 [77.78–80.68] | 133.50 [131.84–134.84] | 2.022 [1.967–2.152] |

### Native CPU and required outbound body traffic

| Roots | Arm | Last4 user CPU | Last4 kernel CPU | User slope | Kernel slope | Last4 body MiB | Body receive wall ms |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | before | 64.62 [64.02–70.87] | 52.35 [52.13–57.74] | 1.369 [1.266–1.496] | 0.640 [0.515–0.725] | 15.51 [15.51–15.51] | 3.39 [3.17–3.44] |
| 1 | after | 29.28 [28.97–30.01] | 41.05 [40.99–42.75] | 0.063 [0.052–0.221] | 0.164 [0.140–0.376] | 15.51 [15.51–15.51] | 3.84 [3.77–3.88] |
| 4 | before | 72.95 [72.84–73.09] | 73.85 [73.28–74.03] | 1.456 [1.430–1.491] | 1.199 [1.165–1.214] | 15.51 [15.51–15.51] | 11.16 [9.17–11.52] |
| 4 | after | 36.34 [36.20–39.60] | 62.16 [61.51–63.80] | 0.214 [0.184–0.349] | 0.827 [0.767–0.893] | 15.51 [15.51–15.51] | 10.41 [10.34–10.59] |
| 8 | before | 81.34 [80.52–83.22] | 102.17 [100.86–105.71] | 1.725 [1.621–1.807] | 2.144 [2.114–2.282] | 15.51 [15.51–15.51] | 21.17 [18.99–22.41] |
| 8 | after | 44.56 [43.77–45.57] | 88.94 [88.07–89.27] | 0.418 [0.366–0.469] | 1.655 [1.549–1.683] | 15.51 [15.51–15.51] | 21.56 [20.57–22.99] |

The receive wall interval is measured by the loopback provider after headers and summed across concurrent requests. It is neither sender CPU nor elapsed turn latency.

### Three complete 64-turn cases

| Metric (request_bytes in MiB) | First4 | Last4 | Slope 9–64 |
| --- | --- | --- | --- |
| daemon_cpu_ms | 68.23 [66.82–71.51] | 82.54 [82.38–82.86] | 0.342 [0.301–0.353] |
| daemon_user_ms | 28.76 [28.44–30.73] | 32.37 [32.30–32.70] | 0.088 [0.074–0.092] |
| daemon_kernel_ms | 39.47 [38.38–40.78] | 50.08 [49.84–50.50] | 0.254 [0.227–0.261] |
| request_bytes | 1.10 [1.10–1.10] | 31.96 [31.96–31.96] | 0.514 [0.514–0.514] |
| request_read_ms | 0.25 [0.25–0.42] | 8.83 [8.37–9.28] | 0.150 [0.142–0.151] |

All three include an additional live client turn: 65 root turns and 520 tool calls, complete outcomes, warm/cold recovery and graph checks.


## Separate native CPU attribution

All values are median [minimum–maximum], n=3, ms per root turn unless stated. Three interleaved plain/instrumented controls per scale use the same final release binary and construction workload; UI attachments afterward are omitted. All outcomes and graph hashes validate. The diagnostic dylibs are scratch harnesses only. CPU inside native TCP send calls and posix_spawn is measured by thread CPU clocks, with back-to-back clock calibration subtracted. Counters cover every outgoing request body byte. This is a measured subset of networking CPU, not a claim that everything outside it is compiler CPU.

| Workload | Plain total CPU slope | Profile total CPU slope | Send CPU slope | CPU outside send slope |
| --- | --- | --- | --- | --- |
| 1x32 | 0.223 [-0.054–0.230] | 0.296 [0.223–0.379] | 0.108 [0.101–0.110] | 0.186 [0.122–0.271] |
| 4x32 | 0.985 [0.955–1.125] | 1.051 [0.880–1.228] | 0.130 [0.120–0.135] | 0.921 [0.760–1.093] |
| 8x32 | 2.055 [1.939–2.209] | 2.061 [2.032–2.364] | 0.133 [0.130–0.138] | 1.928 [1.902–2.227] |
| 1x64 | 0.369 [0.293–0.390] | 0.347 [0.342–0.382] | 0.115 [0.115–0.117] | 0.232 [0.227–0.265] |

The wide residual is real. A second native interposer measured actual posix_spawn calls in three additional interleaved 8-root pairs, recording native open-file counts.

| Metric | First4 | Last4 | Slope 9–32 |
| --- | --- | --- | --- |
| daemon_cpu_ms | 78.922 [78.323–79.096] | 134.316 [133.736–134.769] | 2.116 [1.986–2.206] |
| spawn_ms | 3.350 [3.338–3.417] | 43.852 [43.757–44.239] | 1.490 [1.467–1.539] |
| send_ms | 0.761 [0.724–0.770] | 4.485 [4.441–4.534] | 0.135 [0.134–0.136] |
| other_ms | 74.861 [74.136–74.985] | 85.930 [85.012–86.571] | 0.490 [0.385–0.531] |
| spawn_calls | 2.000 [2.000–2.000] | 2.000 [2.000–2.000] | 0.000 [0.000–0.000] |
| daemon fds (total) | 402.000 [402.000–402.000] | 3986.000 [3986.000–3986.000] | 128.000 [127.983–128.000] |

Matched profile-minus-plain total CPU deltas: first4 -0.405 [-0.413–0.628] ms; last4 1.246 [-2.901–2.357] ms; slope 0.084 [-0.194–0.098] ms/depth. The raw and calibration counters remain available in every turn record. Non-send/non-spawn residual still grows; it is not relabelled as required transmission.

After construction, direct native descriptor inspection found 2,064 log.jsonl plus 2,064 log.idx handles, exactly one pair per durable session. This inspection occurred after all 32 timed rounds, outside the turn window. The process-control snapshots show growth from 79 descriptors before construction to 4,178 after round 32. The sessions are reachable; this is retained resource ownership, not an unreachable allocation leak.

An isolated release probe uses the real public sys::spawn_group implementation, 64 owned child launches per process. Three fresh processes per descriptor/memory setting, interleaving 0/512 MiB resident memory and reversing descriptor order between repetitions, distinguish open-file cost from parent memory. All files are real close-on-exec handles; no product launch logic is copied into the probe. CPU is the parent probe process only; wall time includes waiting for each child.

| Open files | Parent MiB | CPU ms/launch | Kernel ms/launch | Wall ms/launch |
| --- | --- | --- | --- | --- |
| 0 | 0 | 0.208 [0.206–0.240] | 0.201 [0.199–0.232] | 4.034 [3.437–4.214] |
| 256 | 0 | 0.885 [0.880–0.946] | 0.877 [0.873–0.936] | 4.997 [4.773–5.874] |
| 1024 | 0 | 2.941 [2.918–2.992] | 2.933 [2.910–2.984] | 16.774 [16.540–16.875] |
| 4096 | 0 | 11.222 [10.990–11.450] | 11.213 [10.981–11.440] | 64.965 [64.124–65.919] |
| 0 | 512 | 0.206 [0.206–0.208] | 0.199 [0.198–0.201] | 3.473 [3.456–3.479] |
| 256 | 512 | 0.868 [0.865–0.875] | 0.860 [0.858–0.868] | 4.984 [4.712–5.067] |
| 1024 | 512 | 2.932 [2.890–3.007] | 2.924 [2.882–2.998] | 16.581 [16.528–16.894] |
| 4096 | 512 | 11.825 [11.345–12.977] | 11.815 [11.335–12.966] | 66.038 [65.321–70.490] |

The process control localizes a substantial part of the remaining depth cost to launch work under descriptor pressure. The isolated probe reproduces that dependency without context encoding or a model. It does not establish that the entire residual has a single cause. Source inspection identifies Index.file and Ends.idx in crates/engine/src/store/jsonl.rs as the retained per-session file owners.

**Historical verdict, superseded by the owner decision below:** **Acceptance gate: not met.** Functional 64-turn completion and all required preservation checks pass, and shared prefix ownership removes repeated inherited compilation. However, total CPU still grows beyond measured transmission. No owner waiver is assumed, and this branch is not ready to merge.

**Owner-pending file allocation:** crates/engine/src/store/jsonl.rs and its real store invariant/restart tests in crates/engine/src/store/tests.rs. The proposed follow-up is operation-scoped log/index file ownership: idle history keeps offsets and source identity, active I/O owns handles, and pending durability epochs retain exactly the handles needed until flush completion. This removes descriptor pressure without limiting sessions, history, tools or recall; it avoids modifying the excluded sys process backend. Preserve append ordering, torn-tail repair, read-only state, delayed cache flushes, concurrent readers, restart and file/source identity. Likelihood/cost: the failure is deterministic in this matrix; directly measured launch growth is about 40 ms/root turn at this scale. Implementation and portable lifetime verification are estimated at 1–2 days, followed by the repeated acceptance matrix. That expected gain is not a measured fix, and further residual attribution may still be needed. No such storage change or speculative hardening was added under the current file allocation.

Raw evidence: context-socket-summary.json, context-process-summary.json, spawn-native-summary.json and their per-case native counters/archives. process-attribution-provenance.json records diagnostic source/binary hashes. The full final-build provenance is context-final-provenance.json.


## Operation-scoped durable handles follow-up

The owner approved store/jsonl.rs, store/tests.rs and a sys FileIdentity accessor for
the measured descriptor/launch cost. Idle history now retains offsets and source
identity; reads/appends own handles only for I/O, and pending durability epochs own the
exact file needed until flush completion. Reopened sources are checked against handle
identity. Sidecars reopen for incremental delta writes. Existing ordering, torn-tail
repair, read-only state, concurrent reads, delayed flushes, restart and source identity
are preserved. No dependency, wire/log change or session/history/tool limit.

All 21 new primary cases pass: three interleaved A/B samples at 1/4/8 roots ×32 plus
three complete 64-turn cases, with real TUI/FFI/web streaming and warm/cold hashes.
Before 3b34dc92, after product code 56bc30bf, common main 3a32fe61, same release
profile/workload as above. A later test-only change uses the production sealed header.
Runtime environments exclude inherited providers/proxies and use loopback/dummy
credentials. Values below are median [min–max], n=3; CPU ms/root turn, slopes OLS rounds
9–32.

| Roots | Arm | Last4 total CPU | User slope | Kernel slope | Total slope |
| --- | --- | --- | --- | --- | --- |
| 1 | before | 71.48 [71.22–71.50] | 0.092 [0.082–0.185] | 0.199 [0.167–0.270] | 0.291 [0.249–0.456] |
| 1 | after | 73.34 [73.01–73.43] | 0.064 [0.044–0.066] | 0.091 [0.041–0.122] | 0.155 [0.107–0.166] |
| 4 | before | 99.41 [98.68–104.64] | 0.231 [0.192–0.322] | 0.842 [0.788–0.907] | 1.073 [0.980–1.229] |
| 4 | after | 86.50 [85.83–86.88] | 0.262 [0.204–0.299] | 0.213 [0.168–0.225] | 0.488 [0.372–0.512] |
| 8 | before | 133.80 [131.74–137.83] | 0.412 [0.404–0.465] | 1.624 [1.590–1.804] | 2.037 [1.994–2.269] |
| 8 | after | 93.98 [93.75–94.63] | 0.418 [0.346–0.421] | 0.220 [0.202–0.258] | 0.638 [0.548–0.679] |


The daemon descriptor invariant reproduces three baseline failures (4/32/128 idle
sessions: 48/104/296 FDs) and three fixed passes (40/40/40 FDs; no log/index handles).
In the deep eight-root matrix, round 32 FDs fall 4178 → 50 in every run; detached 15s
4298 → 42. All 64-turn cases preserve 65 root turns/520 tools including live clients and
cold recovery. Fixed 64-turn last4 CPU 81.53 [81.49–81.67], user slope 0.078
[0.048–0.078], kernel slope 0.135 [0.112–0.142].

Twelve separate interleaved plain/profile controls quantify TCP and actual posix_spawn
CPU. At eight roots, launch first4/last4 is now 0.673 [0.668–0.690]/0.717
[0.715–0.738]ms, versus the earlier 3.350/43.852 ms descriptor-heavy result. TCP slope
0.126 [0.121–0.127]ms/depth leaves a measured 0.433 [0.366–0.666] non-send/non-spawn
residual. Three native stack captures with a matching dSYM identify
Owner::hibernate/next_hibernation/loaded and Live::idle_for scans over daemon-wide
collections (14.87–16.71% of sampled userspace leaf weight; not a CPU-time partition).
These are a remaining hot path, not an exclusive explanation of every residual cycle.
**Historical verdict, superseded by the owner decision below:** **Total CPU acceptance remains unmet; #1+#2 are not ready to merge.** Further
supervisor/index/deadline work needs crates/engine/src/lib.rs ownership and a
counterfactual measurement. No such change was made here.

Live heap remains essentially unchanged (eight-root 898.744 → 898.850 MiB); identities
replace handles without dropping history. Four-root footprint rises 608.80
[606.72–612.11] → 644.00 [643.74–646.81]MiB, mostly vmmap Malloc Large(empty) dirty
regions 129.0 [125.1–130.5] → 162.8 [162.2–163.4]MiB. Eight-root footprint is 1133.67 →
1130.78 MiB. The one-root late CPU cost 71.48 → 73.34 ms from per-operation
reopen/identity checks is reported. No allocator policy or feature cap was added.

622 engine/sys tests pass (16 skipped), plus 64 final store tests; fmt, check,
all-target/all-feature `clippy -D warnings`, Windows cross-lint and release pass. The
unchanged edit rollback test hit a temporary-directory AlreadyExists once; isolation and
the full rerun pass. The detached cancellation regression passes. No CI is dispatched.
Full tables, all spreads/consumer counters, source attribution, owner-pending items and
cleanup record are in context-graph.handles.report.md under the evidence root above.
handles-final-provenance.json and handles-attribution-provenance.json pin
sources/binaries; prior evidence and the public sys::spawn_group probe remain unchanged.

## Indexed supervision follow-up

The owner approved lib.rs and residency/wakeup integration for the remaining
all-session work. Product commit `ef986c0d`, compared with `7f95b767`, maintains
identity lookup, loaded/work aggregates and replaceable idle deadlines on
transitions. A supervisor pulse visits changed identities and expired deadlines;
it does not rebuild or scan every known/live session. No polling timer was added.
The canonical ordered registry preserves creation order. Pending loads participate
in shutdown; pressure reclamation and actual upgrade handover retain their global
semantics. Upgrade readiness updates its projection from changed identities.

Full Sessions wire snapshots still require O(output bytes) for subscribers, using
cached summaries rather than rereading Slot/Live state. Only changed rows retain
previous publication values, replacing the old permanently duplicated full list.
Visible-prefix initialization prevents duplicate summary transitions exposed by
that cache. No dependency, wire/log change, recorded fixture edit or feature cap.

All 21 primary cases pass: n=3 interleaved A/B at 1/4/8 roots ×32 plus three
candidate 64-turn constructions, all with live TUI/FFI/web, complete outcomes,
graph hashes and warm/cold view hashes. Common main remains `3a32fe61`; fetched and
merged before implementation and again before delivery. Same release profile and
workload as above; loopback/dummy providers in allowlisted environments. Native
heap inspection is outside timings. No build/test/profile runs overlap accepted
CPU timings. Values are median [min–max], n=3; CPU ms/root turn, OLS slopes 9–32.

| Roots | Arm | Last4 total | Last4 user | Last4 kernel | User slope | Kernel slope | Total slope |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | before | 75.55 [74.20–80.15] | 30.53 [29.89–32.33] | 45.03 [44.30–47.81] | 0.043 [-0.699–0.183] | 0.065 [-0.912–0.247] | 0.108 [-1.611–0.430] |
| 1 | after | 75.48 [72.82–79.73] | 29.44 [28.56–31.26] | 46.05 [44.26–48.46] | 0.093 [-0.150–0.105] | 0.199 [-0.179–0.203] | 0.293 [-0.329–0.308] |
| 4 | before | 90.83 [89.31–94.06] | 38.62 [37.91–40.57] | 52.20 [51.40–53.50] | 0.281 [0.114–0.437] | 0.224 [0.020–0.430] | 0.505 [0.135–0.867] |
| 4 | after | 83.91 [83.48–85.14] | 32.38 [32.35–32.86] | 51.56 [51.10–52.27] | 0.063 [0.029–0.151] | 0.212 [0.145–0.274] | 0.275 [0.174–0.425] |
| 8 | before | 96.17 [95.71–97.23] | 43.85 [42.80–44.33] | 52.90 [52.31–52.90] | 0.355 [0.333–0.402] | 0.165 [0.159–0.233] | 0.514 [0.498–0.635] |
| 8 | after | 87.40 [87.05–87.96] | 34.00 [33.02–34.12] | 53.85 [53.04–54.38] | 0.078 [0.034–0.129] | 0.283 [0.188–0.317] | 0.395 [0.222–0.411] |

All three 64-turn cases preserve 65 root turns and 520 tools including live clients.
Last4 total CPU 83.54 [81.37–128.42] ms, user 31.40 [30.52–50.02], kernel 52.14
[50.85–78.41]; slopes 9–64: total 0.211 [0.152–0.512], user 0.050 [0.020–0.170],
kernel 0.161 [0.132–0.342]. The high sample is retained. At eight roots, live heap
898.83→899.87 MiB, physical footprint 1131.50→1138.60 MiB. The indexes cost about
1.04 MiB across 2,064 durable sessions. The complete report preserves every spread,
client process counter, attach/fold latency and lifecycle residency stage.

The real daemon invariant is RED on the old supervisor (64 visits for one changed
session among 32 idle), GREEN at one visit then zero. Upgrade projection visits 32
initially, then one changed identity, then zero. Another invariant verifies deadline
replacement, attachment, expiry, unload and reload. Existing no-polling, pressure,
shutdown-loading, handover, cancellation, graph/provider parity, restart and blob
lifetime checks pass: 427 engine tests, 14 skipped; fmt, check, all-target/all-feature
clippy `-D warnings`, Windows cross-lint and native release pass. Rustdoc succeeds
with its existing private handover-link warning. Detached 12-second idle CPU is
0.04–0.36 ms across the candidate primary samples. No CI dispatched.

Twelve separate interleaved plain/profile controls measure TCP send/sendto and real
posix_spawn thread CPU. At eight roots, last4 sends cost 5.087 [4.544–5.394] ms,
5.261 [5.023–5.352]% of profiled daemon CPU. Accepted TCP bytes are 15.512 MiB/root
turn, 99.984% request body. At depth 64, sends cost 7.420 [7.410–9.686] ms,
8.933 [8.756–9.001]% of CPU, with 31.965 MiB accepted TCP /99.992% body. Full-context
fork traffic is preserved. Spawn last4 0.805 [0.751–0.812] ms at eight roots, slope
0.001 [-0.004–0.010]. The non-send/non-spawn residual slope remains 0.491
[-0.027–1.148] at eight roots, 0.053 [-0.041–0.302] at depth 64. Direct send intervals
exclude other request preparation/readiness work. Plain/profile deltas, clock
calibration and all outliers are retained; provider receive wall time is never
subtracted from CPU.

Three further late-stack diagnostic cases pass 256 root turns each. Native captures
show no legacy loaded/idle scan frames. Remaining userspace leaf paths include
durable reads (12.84–18.64% of sampled userspace leaf weight), snapshots (3.47–4.30%),
provider work (17.29–20.14%) and transport (2.45–3.61%); roughly half remains other
runtime, extension/event, JSON, allocator and outlined work. These are sampled path
groups, not CPU-time partitions or an exclusive explanation of the slope. Parent
stacks identify incremental child-ownership range reads, current context-node
writes, request transport and checkpoints. The child-ownership cursor already
advances; these stacks do not show a settled-prefix reread. Kernel/wait leaves
include file operations/durability and socket sends but are dominated by waits.
**Historical verdict, superseded by the owner decision below:** **Total CPU acceptance remains unmet; this branch is not ready to merge.** Further
narrow native CPU/syscall attribution is owner-pending before another product change
(estimated 0.5–1 day plus any newly approved matrix). No residual was relabelled as
necessary transmission without evidence.

The four-root +35 MiB footprint question was examined separately through six
interleaved native full malloc/free-history cases, without allocator hooks. Live
malloc is 449.8 MiB in both arms. Logged footprint medians 618.77→645.58 MiB and
empty-Large dirty 133.0→159.6 MiB reproduce the prior uninstrumented direction.
Selected empty-region histories identify recovery-image encoding and the second
whole snapshot Vec in JsonlLog::snapshot→Snap::write; selected encoding records
reach 7,284,474 bytes, with frees within snapshot operations. Json clones share Arc
storage. The handle patch changed File/Index lease ownership, not these buffers'
logical lifetimes. Exact allocator-cache placement remains unassigned. Histories
are selected address/size records, not total allocation frequencies or exclusive
resident bytes; logged CPU is not accepted timing. A possible streaming checked
snapshot writer is owner-pending (1–2 days plus durability/restart checks); its
footprint gain is unmeasured. No allocator policy or snapshot change was added.

The edit-test TempDir flake remains with its owner. Full phase report:
`context-graph.supervisor.report.md` in the existing context-graph evidence root;
`super-final-provenance.json` and `supervisor-attribution-provenance.json` pin code,
binaries, scripts, raw tables and diagnostic symbols. Earlier reports and the
native spawn probe remain intact. Phase 1 stopped for independent re-review.

### Owner acceptance after independent review

The owner accepts the CPU gate as met. Owner-log scans, descriptor-dependent
launch cost and supervisor scans have been removed; the owner attributes the
remaining growth to full-context fork_task transmission plus noise. Independent
review approved merging, subject to this documentation correction. This decision
supersedes the earlier unmet-gate and not-ready-to-merge verdicts above. Every
measurement, spread and attribution limitation is retained unchanged. Phase 2
proceeds with main integration and Linux/Windows CI.
