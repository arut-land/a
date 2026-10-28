# Transient receive buffers

Round-3 finding 2 is correct: draining the frame Vec retains roughly 8 MiB
per reader after large frames. The old release branch freed/shrank that
allocation on completion. One sys::TransientBuffer now keeps a native
address reservation and returns its backing after large bursts. Small
bursts use Vec, with initialized scratch separated from unread bytes so
read retries and cancellation do not repeatedly zero the same allocation.

Known lengths select native storage after the read-ahead prefix. Native
accumulation growth/completion never moves or reallocates its reservation;
the initial prefix and unread tail can be copied. Unknown-length SSE lines
can copy a larger heap prefix at promotion. There is one retained metadata
allocation on first promotion and no allocator block churn per native frame.
The primitive uses existing libc/windows-sys bindings. Protocol and llm
add internal sys dependencies; no external dependency was added. Vec cannot
retain its address while returning backing pages; the previous freeing
implementation is measured below as the alternative.

Apple uses MADV_FREE_REUSABLE / MADV_FREE_REUSE; Linux MADV_DONTNEED;
Windows reserve/commit/decommit. Crossovers: Apple 2 MiB, Linux 4 MiB,
Windows 1 MiB. Apple moved up after the 1 MiB native-path row regressed.
Linux uses aligned reservations and progressive huge-page hints, reset
before discard so short reuse does not recharge the old huge allocation.
No THP setting is changed. Compaction tails under pressure are unmeasured;
no mitigation was added, per owner direction.

The shared FrameReader preserves partial input and ready batching. Client
and daemon reads use 64 KiB batches with matching read-ahead reservation;
the daemon retains its cooperative yield budget. Stream::fill and fetch
share cancellation-safe scratch. AsyncIo reads delegate to async-io's
maintained optimistic retry; completion and signal backends keep their
original path. Only rt/src/io.rs was changed in the runtime seam.

SSE scans chunks between CR/LF and shares the primitive for line/event/data
scratch. Typed text appends avoid a redundant UTF-8 scan; raw publication
invalidates that guarantee. Limits, wire/provider fixtures and PROTO are
unchanged. Library tests exercise the real readers and native release.

## Measurements

Release z/fat-LTO, CARGO_BUILD_JOBS=2, one external target. M3 shared host;
21 interleaved samples per framing row, seven per SSE/primitive row, order
reversed in alternate rounds. All values are median [min–max]. Main is
63ac410a, old release branch a3283e2b; candidate includes runtime delegation,
initialized heap scratch, 64 KiB batches and final crossovers. No models
are contacted; MISE_NO_ENV=1 and an allowlisted environment force dummy
loopback provider settings. Readers/encoders are the actual implementation.
All 18 main framing rows and eight SSE rows are faster by median.

| Reader / body bytes / frames | main ms | prior release branch ms | final candidate ms | change vs main |
| --- | ---: | ---: | ---: | ---: |
| server / 0 / 1,000,000 | 358.70 [323.94–397.63] | 353.53 [316.32–416.87] | 313.09 [283.53–365.47] | -12.7% |
| server / 32 / 1,000,000 | 532.61 [497.16–631.66] | 531.45 [496.98–627.46] | 485.45 [456.13–580.65] | -8.9% |
| server / 16,384 / 4,096 | 79.44 [61.80–192.77] | 86.15 [66.94–189.09] | 53.22 [42.27–121.73] | -33.0% |
| server / 65,536 / 1,024 | 85.95 [78.60–197.38] | 87.40 [82.61–195.16] | 58.88 [52.56–122.86] | -31.5% |
| server / 131,072 / 512 | 92.72 [82.67–216.65] | 94.80 [82.49–197.67] | 62.85 [55.97–121.47] | -32.2% |
| server / 262,144 / 256 | 92.78 [85.63–167.00] | 90.17 [86.36–192.83] | 58.96 [56.93–139.91] | -36.5% |
| server / 524,288 / 128 | 96.11 [85.47–184.51] | 91.76 [86.02–217.23] | 62.20 [55.12–164.54] | -35.3% |
| server / 1,048,576 / 64 | 113.07 [87.42–236.29] | 94.14 [88.41–197.85] | 64.20 [56.32–113.92] | -43.2% |
| server / 5,242,880 / 10 | 72.47 [67.94–172.23] | 80.49 [68.41–191.68] | 56.01 [45.90–127.26] | -22.7% |
| client / 0 / 1,000,000 | 365.75 [331.29–436.11] | 360.03 [324.05–444.22] | 308.98 [285.12–399.60] | -15.5% |
| client / 32 / 1,000,000 | 562.60 [516.77–695.14] | 558.31 [514.29–637.15] | 510.15 [457.40–581.39] | -9.3% |
| client / 16,384 / 4,096 | 64.31 [48.78–185.98] | 53.07 [44.24–166.85] | 52.43 [42.70–122.82] | -18.5% |
| client / 65,536 / 1,024 | 66.80 [41.40–191.02] | 70.69 [47.15–189.96] | 61.88 [52.25–113.65] | -7.4% |
| client / 131,072 / 512 | 67.65 [42.72–173.51] | 68.49 [43.77–173.79] | 59.33 [54.96–103.52] | -12.3% |
| client / 262,144 / 256 | 68.15 [39.38–188.27] | 67.62 [40.69–194.17] | 59.93 [55.13–95.66] | -12.1% |
| client / 524,288 / 128 | 67.03 [39.98–195.79] | 68.47 [41.27–211.04] | 60.93 [57.73–105.53] | -9.1% |
| client / 1,048,576 / 64 | 68.00 [47.00–213.37] | 67.22 [42.90–220.38] | 60.72 [55.79–135.53] | -10.7% |
| client / 5,242,880 / 10 | 59.81 [30.63–182.54] | 61.02 [32.25–170.02] | 54.01 [45.16–118.60] | -9.7% |

## SSE component, milliseconds

| Body bytes / events | main ms | final ms | main / final allocations |
| --- | ---: | ---: | ---: |
| 32 / 1,000,000 | 223.85 [202.65–272.54] | 149.91 [133.86–168.54] | 6 [6–6] / 4 [4–4] |
| 65,536 / 512 | 119.94 [113.11–141.11] | 27.78 [25.52–31.21] | 17 [17–17] / 7 [7–7] |
| 131,072 / 256 | 123.82 [116.12–137.84] | 28.02 [25.58–30.50] | 18 [18–18] / 8 [8–8] |
| 262,144 / 128 | 129.35 [116.03–134.04] | 29.08 [25.70–31.55] | 19 [19–19] / 9 [9–9] |
| 524,288 / 64 | 124.72 [114.97–131.11] | 27.71 [25.58–29.58] | 20 [20–20] / 10 [10–10] |
| 1,048,576 / 32 | 125.02 [115.19–131.91] | 28.13 [25.87–36.74] | 21 [21–21] / 11 [11–11] |
| 2,097,152 / 16 | 128.22 [115.49–152.48] | 30.06 [28.64–45.82] | 22 [22–22] / 11 [11–11] |
| 5,242,880 / 10 | 199.07 [181.41–243.10] | 48.14 [43.58–74.03] | 23 [23–23] / 11 [11–11] |

## Final native primitive, ten 5 MiB bursts

Linux x64/ARM64 and Windows x64/ARM64 passed native sys/rt/protocol/client
clippy and invariant tests, then release measurements in run 40703178069.
DONTNEED drops Linux RSS; FREE retains it until pressure. Windows reset
and discard retain private commit; decommit releases it. Earlier policy
runs: 40660702791 and 40662329918; Linux alignment run 40687589452.

| Host / policy / mode | ten 5 MiB ms | live KiB | allocations | RSS delta KiB | private delta KiB | faults |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Linux ARM64 / dontneed / hybrid | 6.51 [6.10–6.69] | 4,105.75 [4,105.75–4,105.75] | 11 [11–11] | 4,228.00 [4,228.00–4,228.00] | n/a | 1,073 [1,073–1,073] |
| Linux ARM64 / dontneed / known | 6.83 [6.63–7.10] | 9.75 [9.75–9.75] | 1 [1–1] | 64.00 [64.00–64.00] | n/a | 5,142 [5,142–5,142] |
| Linux ARM64 / dontneed / release | 35.00 [34.79–36.29] | 9.71 [9.71–9.71] | 120 [120–120] | 4,164.00 [4,164.00–4,164.00] | n/a | 13,837 [13,837–13,837] |
| Linux ARM64 / dontneed / vec | 21.38 [21.32–21.44] | 8,201.70 [8,201.70–8,201.70] | 11 [11–11] | 5,252.00 [5,252.00–5,252.00] | n/a | 1,299 [1,299–1,299] |
| Linux X64 / dontneed / hybrid | 6.03 [5.83–6.39] | 4,105.75 [4,105.75–4,105.75] | 11 [11–11] | 4,232.00 [4,232.00–4,232.00] | n/a | 1,074 [1,074–1,074] |
| Linux X64 / dontneed / known | 10.46 [10.25–10.86] | 9.75 [9.75–9.75] | 1 [1–1] | 68.00 [68.00–68.00] | n/a | 5,143 [5,143–5,143] |
| Linux X64 / dontneed / release | 21.17 [20.71–21.73] | 9.71 [9.71–9.71] | 120 [120–120] | 4,168.00 [4,168.00–5,888.00] | n/a | 2,345 [2,138–2,524] |
| Linux X64 / dontneed / vec | 16.99 [16.58–17.33] | 8,201.70 [8,201.70–8,201.70] | 11 [11–11] | 5,256.00 [5,256.00–6,360.00] | n/a | 1,300 [1,065–1,300] |
| Windows ARM64 / decommit / hybrid | 10.48 [10.21–10.81] | 1,037.21 [1,037.21–1,037.21] | 9 [9–9] | 1,096.00 [1,096.00–1,100.00] | 1,068.00 [1,068.00–1,072.00] | 13,340 [13,340–13,341] |
| Windows ARM64 / decommit / known | 9.23 [9.20–9.89] | 13.21 [13.21–13.21] | 1 [1–1] | 32.00 [32.00–32.00] | 0.00 [0.00–0.00] | 12,848 [12,848–12,852] |
| Windows ARM64 / decommit / release | 49.16 [48.55–50.14] | 13.16 [13.16–13.16] | 120 [120–120] | 72.00 [72.00–72.00] | 44.00 [44.00–44.00] | 33,059 [33,059–33,059] |
| Windows ARM64 / decommit / vec | 25.90 [25.84–26.12] | 8,205.16 [8,205.16–8,205.16] | 11 [11–11] | 5,192.00 [5,192.00–5,196.00] | 8,252.00 [8,252.00–8,256.00] | 3,323 [3,323–3,324] |
| Windows X64 / decommit / hybrid | 13.44 [13.10–14.11] | 1,037.21 [1,037.21–1,037.21] | 9 [9–9] | 2,100.00 [2,100.00–2,104.00] | 2,080.00 [2,080.00–2,084.00] | 13,366 [13,366–13,367] |
| Windows X64 / decommit / known | 12.06 [11.82–12.08] | 13.21 [13.21–13.21] | 1 [1–1] | 40.00 [40.00–44.00] | 0.00 [0.00–4.00] | 12,850 [12,850–12,851] |
| Windows X64 / decommit / release | 58.67 [57.93–59.10] | 13.16 [13.16–13.16] | 120 [120–120] | 552.00 [552.00–556.00] | 524.00 [524.00–528.00] | 31,312 [31,300–31,321] |
| Windows X64 / decommit / vec | 36.13 [36.02–43.59] | 8,205.16 [8,205.16–8,205.16] | 11 [11–11] | 6,216.00 [6,188.00–6,244.00] | 9,268.00 [9,264.00–9,272.00] | 3,357 [3,350–3,363] |

Tiny raw guard loops can cost more than Vec alone because Vec omits format
bounds and publication/cancellation semantics. The full report includes
those raw-loop costs; they are not substituted for application framing.

## Other candidates

Owner-approved decisions stand: MCP stdio, Responses escaping, HTTP,
shell capture, file reads and blob materialization remain unchanged.
MCP's candidate lowered charge but raised ten 900 KB message CPU from
9.26 [7.23–10.64] to 11.95 [9.01–12.54] ms. Responses escaping slowed
ten 5 MiB deltas from 109.30 [108.71–110.55] to 180.82 [180.21–184.02] ms.
Transport chunks, file-backed shell captures and streamed text/blob reads
have no equivalent whole-burst retained scratch. Image/collected body
bytes have payload ownership; conversion would add a full copy.

Raw JSON, source, scripts, heap summaries and negative-test logs are in
/private/tmp/frame-buffers-probe/. The full owner report is at the requested
scratchpad/cleanup/briefs/frame-buffers.report.md path.

Native guidance: [Apple madvise](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/madvise.2.html),
[XNU constants](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/mman.h),
[Linux madvise](https://man7.org/linux/man-pages/man2/madvise.2.html),
[Linux THP](https://docs.kernel.org/admin-guide/mm/transhuge.html),
[Windows VirtualFree](https://learn.microsoft.com/en-us/windows/win32/api/memoryapi/nf-memoryapi-virtualfree),
[Windows discard](https://learn.microsoft.com/en-us/windows/win32/api/memoryapi-discardvirtualmemory).
