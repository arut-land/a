# One resident model history

Stage A, measured 2026-10-28 on a shared Apple M3, arm64 macOS 27.0.1
(26A434). Baseline: `7318ef3f`; implementation: `4f5ef559`. Both binaries
use the repository release profile: size optimization, fat LTO, one
codegen unit and rebuilt std. Builds use two jobs and finish before probes
run. [Raw observations and summaries](turn-residency.json) accompany these
results. No new dependency or allocator change was required.

The session retains one active target's encoded prefix and protocol
continuation, plus its canonical mutable suffix and log cursor. Canonical
history is reconstructed on demand rather than retained beside its encoded
copy. Target changes and edits rebuild; no history is capped.

## Daemon live memory

The survey's real 100x30 PTY probe runs ten 5 MiB replies using recorded SSE
framing/deltas repeated to the desired payload size. It talks only to a
loopback HTTP server with dummy credentials, with inherited provider and
proxy settings removed. Each arm uses an isolated home, stopped through
`A_HOME=<home> a daemon stop` and deleted afterwards. Three fresh-process
pairs alternate before/after, after/before, before/after. Every arm has ten
large replies and two small auxiliary results, all successful. This is the
same workload between arms; the original survey used a longer 100-reply run.

Values are median [minimum, maximum], three samples per arm. Live bytes
come from native `heap` All zones counts; RSS and `phys_footprint` come
from the process snapshot tool. Growth is (reply 10 - reply 1)/9.

| Metric | Before | After |
| --- | ---: | ---: |
| Live after reply 1, MiB | 10.773 [10.767, 10.780] | 10.783 [10.769, 10.783] |
| Live after reply 10, MiB | 100.926 [100.926, 100.927] | 55.926 [55.919, 55.927] |
| Live growth per 5 MiB reply, MiB | 10.017 [10.016, 10.018] | 5.016 [5.016, 5.017] |
| CPU for ten streamed replies, seconds | 9.609 [8.674, 11.636] | 9.665 [9.120, 14.222] |
| phys_footprint after reply 10, MiB | 166.626 [161.720, 175.704] | 167.751 [163.876, 176.048] |
| RSS after reply 10, MiB | 178.344 [171.453, 183.406] | 178.078 [172.734, 183.750] |

The live saving is roughly 5 MiB per additional 5 MiB reply, or 45 MiB at
ten replies. The latest outcome still has a fixed roughly 5 MiB kernel
cost, so the whole daemon is not 5 MiB after the first reply. Charged
physical footprint and RSS did not fall correspondingly on this workload.
Streamed CPU is nearly equal at the medians, with wide overlapping ranges;
paired after/before ratios are 1.006, 1.051 and 1.222. These observations do
not establish an aggregate CPU improvement or exclude every regression.

## Request encoding CPU

The external release probe uses the real encoders. It seeds 200 tool
rounds with 8 KiB results, then measures 100 incremental requests per fresh
process. Seven pairs alternate the existing cached canonical path and the
resident path in the same executable. A fresh construction/encoding arm
is interleaved with each pair. Warm resident reconstruction panics, proving
that appends do not reload canonical history. No HTTP request is sent.

CPU is process user+kernel time, microseconds per request, median [min,
max]. The cached control preserves the existing algorithm in the candidate
executable; it is not a separately compiled old-API microbenchmark. The
daemon table above compares actual baseline and candidate binaries.

| Protocol | Cached canonical | Resident | Fresh construction + encoding |
| --- | ---: | ---: | ---: |
| Chat | 43.490 [40.230, 49.720] | 41.180 [37.740, 48.660] | 6233.520 [6021.330, 6340.470] |
| Messages | 65.440 [62.270, 67.820] | 60.160 [55.770, 70.260] | 5846.680 [5781.060, 6111.400] |
| Responses | 45.650 [42.650, 49.620] | 41.640 [40.340, 49.250] | 6317.550 [6221.180, 6491.660] |
| Gemini | 74.500 [73.190, 82.210] | 73.310 [66.610, 79.540] | 7427.600 [6421.790, 7861.390] |

The fragment-cache benefit is preserved: fresh construction/encoding is
roughly 97–152 times the resident median. Paired resident/canonical median
ratios are 0.948, 0.919, 0.921 and 0.966; individual ranges overlap parity.
The original cache evidence is commit
`315ed7d53306074ab727e53a9a25f3bb7c756fa8` (200 x 8 KiB: fresh
1.1–1.2 ms, grown 10–16 us, 52–103 KiB allocated); those historical values
were not previously recorded under docs/perf. The current repository
benchmark is `llm/tests/bench.rs::encode_long_transcript`.

## Size and limits

Release binary size: 6,789,456 -> 6,822,720 bytes (+33,264, +0.49%). SHA-256:

- Before: `8d692d6619e3af1be1bf001bf268511ad0b0ec2d9aed2b0be058f5895ffae3c2`
- After: `9604090c9dc5896205a0d6416eb4241a71dc7f10de6c2689c19470c5bee40d76`

Canonical consumers, edits, fitting and target switches may transiently
reconstruct complete history. Client transcripts and attach projections
remain separate work. Exact large-FFI-drop allocator measurements on native
Linux/glibc and Windows remain pending; the Windows static check is not a
native runtime measurement. No allocator timer or replacement is justified
by the live ownership result alone.
