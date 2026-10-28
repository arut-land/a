# Transient receive buffers

Finding 2 in the round-3 turn-memory survey is reproducible: completed
5 MiB frames leave approximately 8 MiB of receive capacity at each end.
The old release-on-completion branch removes that capacity by freeing
allocations. `sys::TransientBuffer` instead keeps one virtual reservation
and returns physical backing after a completed large burst. Small bursts
keep a Vec and consume read batches through a cursor.

The primitive uses existing libc/windows-sys bindings; there is no new
external dependency. Protocol and llm each add a dependency on the existing
sys crate. Vec cannot retain its address while returning its backing pages;
freeing/shrinking its allocation was measured as the rejected alternative.
Keeping the native calls in sys preserves the workspace's unsafe boundary.

Apple uses MADV_FREE_REUSABLE followed by MADV_FREE_REUSE before access.
Linux uses MADV_DONTNEED; MADV_FREE retained RSS until pressure in the
hosted measurements. Windows decommits while preserving MEM_RESERVE;
MEM_RESET and DiscardVirtualMemory retained private commit in the measured
workload. Native thresholds are internal: Apple 256 KiB, Linux 4 MiB,
Windows 1 MiB. Existing format limits size reservations; these thresholds
are storage selection, not feature caps.

Linux reservations are aligned to 2 MiB. Newly active ranges receive
MADV_HUGEPAGE hints, reset with MADV_NOHUGEPAGE before discard. This lets
large sequential bursts benefit from huge pages while a short subsequent
prefix does not reactivate the old huge allocation. Host THP settings are
unchanged. Compaction latency under pressure remains unmeasured.

Known frame lengths select native storage after the read-ahead prefix.
The prefix and unread tail can be copied; the native accumulation does not
move. SSE has unknown lengths and can copy its heap prefix at promotion.
Each native reservation has one retained metadata allocation on first use.
No claim is made that all payload decoding becomes allocation-free.

## Method

Apple M3, release z/fat-LTO profile, CARGO_BUILD_JOBS=2, one external target.
Seven samples, alternating case order each round, on the shared host.
All final probes use MISE_NO_ENV=1, an allowlisted environment, loopback
endpoints and dummy credentials. No model is contacted. Main is pinned to
7318ef3f; previous branch is a3283e2b. Framing probes extract the real reader
implementations and use the real protocol encoder and native connections.
Numbers are median [minimum–maximum]; times are milliseconds. The framing
and SSE baseline source is unchanged between 7318ef3f and merged main
63ac410a.

## M3 framing

| Reader / workload | main | old release branch | candidate | external rt experiment |
| --- | ---: | ---: | ---: | ---: |
| server / 1M detach | 347.85 [337.97–379.67] | 341.01 [326.94–360.32] | 311.01 [291.18–345.12] | 308.84 [296.32–362.84] |
| server / 1M small hello | 530.83 [493.26–696.66] | 500.98 [494.39–668.88] | 522.25 [491.85–612.95] | 511.75 [483.60–636.42] |
| server / ten 5 MiB | 74.21 [68.72–168.35] | 73.40 [67.88–119.36] | 78.59 [67.51–198.40] | 54.04 [47.43–95.93] |
| client / 1M detach | 347.32 [339.27–483.69] | 350.29 [335.14–451.05] | 329.53 [288.43–492.85] | 315.80 [290.83–423.81] |
| client / 1M small hello | 538.09 [513.86–704.94] | 524.68 [509.84–723.79] | 481.59 [474.52–643.47] | 498.85 [468.81–594.07] |
| client / ten 5 MiB | 47.89 [32.68–58.37] | 48.07 [33.64–51.19] | 71.26 [67.72–78.61] | 47.77 [46.82–52.75] |

The candidate is **not ready for performance acceptance**. Large-frame
client time regresses. An experiment outside the worktree delegates
rt's async-io read_with to the maintained async-io implementation and
removes the ten-large-frame regression. Some medium client samples remain
slower, so that experiment does not establish universal acceptance. The
necessary crates/rt/src/io.rs change is outside
the granted paths and remains owner-pending. The experiment column is not
part of the committed implementation. After ten large frames, live receive
allocation falls from about 8.09 MiB to 0.10–0.11 MiB per reader.

## M3 SSE

The real SSE framer processes generated JSON string data, including
constructor allocation for the boxed candidate framer. This is component
time, not provider latency. Recorded provider fixtures are unchanged.

| Workload | main ms | candidate ms | main / candidate allocations |
| --- | ---: | ---: | ---: |
| 1M small events | 173.46 [166.58–185.33] | 112.87 [111.10–116.94] | 6 / 4 |
| ten 5 MiB events | 169.50 [168.07–177.84] | 36.70 [36.06–38.38] | 23 / 8 |

Ten large events reduce final live allocation from 28.01 MiB to 10.26 MiB,
including the common retained benchmark input. Chunk scanning also removes
per-byte charging/pushing while preserving codec behavior.

## Other candidates

MCP stdio was left unchanged: its 1 MiB message limit falls below the Linux
crossover. A fresh final-primitive M3 check (seven interleaved pairs)
reduced charge but increased ten 900 KB message process CPU from
9.26 [7.23–10.64] ms to 11.95 [9.01–12.54] ms; 128 KiB message CPU
rose from 32.26 [30.19–35.01] ms to 46.84 [40.55–48.74] ms. Shared-host
wall medians were 32.42 versus 30.34 ms, with broad overlapping ranges.
This does not meet the CPU/speed criterion; main already releases completed
MCP Vec allocations.
Responses custom-tool escaping was removed after ten 5 MiB deltas slowed
from 109.30 [108.71–110.55] ms to 180.82 [180.21–184.02] ms.
Transport uses 16 KiB chunks and 64 KiB header/compression buffers, shell
capture is file-backed, edit text reads stream selected lines, and blob
materialization returns immutable paths or streams copies. Their whole
payloads have semantic ownership; replacing them would introduce copying
without a demonstrated retained scratch allocation. Client fetch retains
its existing 64 KiB payload chunks and shares cancellation-safe read scratch.

## Native evidence

Hosted measurements compare DONTNEED/FREE on Linux x64/ARM and
commit/decommit, DiscardVirtualMemory and MEM_RESET on Windows x64/ARM.
Policy runs: 40660702791, 40662329918. Aligned progressive Linux crossover
run: 40687589452. The final native primitive checks and measurements use
Linux run 40695367901 and Windows run 40696465232; results are recorded in the phase-1 report.

Raw JSON, probe source, heap snapshots, rejected patches and exact scripts:
/private/tmp/frame-buffers-probe/. The full phase-1 report is written to the
owner's scratchpad cleanup/briefs/frame-buffers.report.md.

Native guidance: [Apple madvise](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/madvise.2.html),
[XNU constants](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/mman.h),
[Linux madvise](https://man7.org/linux/man-pages/man2/madvise.2.html),
[Linux THP](https://docs.kernel.org/admin-guide/mm/transhuge.html),
[Windows VirtualFree](https://learn.microsoft.com/en-us/windows/win32/api/memoryapi/nf-memoryapi-virtualfree),
[Windows discard](https://learn.microsoft.com/en-us/windows/win32/api/memoryapi/nf-memoryapi-discardvirtualmemory).
