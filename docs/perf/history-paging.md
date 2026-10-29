# Demanded client history

Stage B of [turn residency](turn-residency.md) replaces complete client
transcripts and the daemon's far-attach full-view cache with public controls
and explicitly requested immutable ranges. PROTO 41 carries stable
session/generation/log cuts, text continuations and page replies. The model
request representation and incremental encoding cache remain. Settled encoded
runs now share bounded byte chunks; this preserves their exact request bytes
and prefix reuse while avoiding long-lived multi-MiB cache allocations.

The daemon keeps a log-derived ordinal/fact index without settled transcript
text. A hot index consumes the writer's already-decoded committed entries on
the CPU lane; readers catch up from the log when publication cannot acquire
the index immediately. History requests have admission independent of held
extension queries. Attach certifies controls with `Synced`; it does not
require serializing a whole-session view.

The SDK owns pending authored inputs, control state, live tails and requested
pages. Matching pages share immutable item storage within a process; weak
cache entries do not retain payloads after readers release them. Independent
recall/search/export scans retain their original cut across ordinary new turns
and are invalidated on a generation change. Item indices and source-byte
continuations remain available to their callers. No session or item limit was
introduced: editable recall inputs and indivisible nontext items can exceed a
delivery allowance, and large presentation/search text has continuations.

The TUI and native Swift store replace their requested windows. The browser
uses bounded delivery batches, admitting one oversized committed item when
needed to render canonical Markdown. A partial discovery range is not inserted;
the complete item is requested explicitly, rendered once and released. This
preserves native block semantics, selection and accessibility without a second
browser fragment-stitching renderer. Pages stream into the complete native DOM
and are released by the web server;
find, selection, copy and accessibility retain their ordinary browser behavior.
Browser DOM residency therefore still grows with the displayed transcript.

## Method

Measurements use a shared Apple M3 running arm64 macOS 27.0.1 (26A434).
Both arms use ordinary release builds with size optimization, fat LTO and one
codegen unit, with the same fixture feature and prebuilt standard library. Builds finish before timed samples.
Provider/proxy environment variables are removed after `mise exec`; model
traffic uses only loopback and dummy credentials. Each arm has an isolated
home, stopped with its matching `a daemon stop`. Results report median
[minimum, maximum] from three fresh-process pairs in AB, BA, AB order.

The deep workload runs 32 synchronized closed-loop rounds at 1, 4 and 8
independent roots, with nested task/fork/background/bash/read/skill work,
128 KiB model replies and shared context publications. Each root has one TUI,
one FFI process, one native Swift process and one browser tab. Native Swift
uses the actual AKit `SessionStore` and Rust FFI bridge headlessly; these are
history-ownership measurements, not WindowServer or SwiftUI rendering costs.
The web process is shared across tabs; client-process memory is summed at each
scale. Native `heap` supplies live allocation bytes; process snapshots supply
RSS and `phys_footprint`. These quantities are reported separately.

Warm and restarted-daemon control attach, FFI/Swift first-window readiness,
TUI settled paint and complete browser DOM delivery are separate measurements.
Browser readiness waits for the last oversized item to finish, not merely for
its wrapper to appear. TUI scroll observations count only visible changed
frames; identical repeated fixture rows can require no terminal paint. Separate
real protocol probes time ordinal seeks against the same completed logs: the
baseline reads its resident view, while the candidate requests up to 64 KiB
from the daemon. Twenty seeks per root alternate beginning, one-third,
two-thirds and end positions, across three interleaved fresh-daemon pairs.

The large-reply workload is 100 replies of 5 MiB each, repeating recorded SSE
framing into the actual 100x30 PTY workload. It then attaches FFI, native Swift
and browser consumers sequentially to the same completed cut to avoid retaining
several baseline 500 MiB client copies simultaneously on this shared machine.

## Prototype iteration

Three initial 1x32 interleaved pairs exposed a CPU and full-DOM-delivery
regression. The prototype retained the old demand after a web page was released,
used 64 KiB web batches and reread committed entries while updating its seek
index. The final implementation withdraws released demand, batches forward web
delivery at 1 MiB and reuses decoded committed facts for a hot index. Those
prototype samples are kept separately from final results. No workload was
reduced to address the regression.

The first 100-reply candidate also exposed large allocator blocks backing its
settled encoding cache: native `heap` counted 997.545 MiB versus 507.454 MiB,
although requests still grew by exactly one reply's encoded bytes. RSS was
632.891 versus 648.906 MiB and footprint 657.908 versus 646.861 MiB, so the
block-capacity increase was not a doubling of charged physical memory.
Interleaved allocation-stack probes traced those blocks to `Settled::shared`.
The final encoder streams each logical JSON run into shared byte chunks of
255 KiB, keeping allocation metadata within a 256 KiB class on the measured
host. A run preserves its JSON item boundaries across those chunks. It neither
reencodes the prefix nor changes request bytes; unchanged encoding goldens and
a regression test against the archived implementation verify this. A fast
path avoids adding the full chunk-writer loop to small writes. These earlier
unchunked results are retained separately from final measurements.

The expanded native browser check exposed inserted layout separators and lost
native heading semantics when oversized text was stitched across range replies.
The final web implementation requests a complete oversized committed item and
renders it once through the canonical Markdown renderer, removing the separate
fragment-stitching renderer. Real browser checks verify all 110,000 phrase
matches, full selection, find, native heading elements and accessibility heading
roles. Fresh adjacent rows are inserted with one DOM operation per delivered
range. Earlier partial-Markdown matrix runs are excluded from final comparisons.

A separate 33-turn browser component compared delivery allowances on the same
completed log using three interleaved fresh-home pairs. At 1 MiB full-DOM
delivery was 462.731 [457.237, 467.909] ms; at 8 MiB it was
502.689 [495.950, 508.986] ms. Server footprint after delivery was 7.422 [7.204, 7.469] MiB at 1 MiB and 40.297 [40.219, 40.344] MiB at 8 MiB. The final implementation keeps a 1 MiB normal batch allowance. An earlier
probe reused one runtime home while alternating executables and failed to
receive the expected welcome; those partial timings are excluded. These
component observations are separate from the final complete client matrix.

## Physical memory

Freeing a client page removes its live allocations. It does not imply an
immediate equal decrease in RSS or charged physical footprint. The earlier
[Stage A allocator observations](turn-residency.md) remain relevant; this change
adds no allocator timer, process-wide trimming policy or replacement allocator.
The earlier design experiment used the real FFI reducer with 100 × 5 MiB
items, then replaced/dropped them in three interleaved natural/release pairs.
Live malloc fell from 500.198 to 0.083 MiB. Immediately after natural drop,
RSS was 328.031 [328.031, 328.047] MiB and `phys_footprint` was 323.298
[323.298, 323.313] MiB. At 60 seconds RSS was 5.438 [5.375, 103.500] MiB,
while footprint remained 323.298 [238.251, 323.313] MiB. Immediate and final
explicit `sys::release()` calls produced no corresponding footprint reduction.
System freed the live objects but did not return all charged footprint within
that observation period. These earlier component measurements are included in
the accompanying raw data; they are not a new Stage B or SwiftUI measurement.
Paging avoids creating those full client transcripts in the first place.

Native Linux/glibc and Windows large-drop allocator experiments remain pending;
the Windows static check is not a native allocator measurement.


The browser probe originally polled `textContent`, which concatenated the
transcript repeatedly while it loaded. Those browser measurements are excluded.
The corrected probe polls only item/progress counters and counts text-node
lengths once after delivery, without constructing a full transcript string.
The complete client matrix is rerun with that probe.

## Request preparation CPU

The release component probe seeds 200 tool rounds with 8 KiB results and
measures 100 incremental requests per process. Seven pairs alternate the actual
baseline and candidate encoders, with cached canonical, resident and fresh
construction arms. Warm resident reconstruction panics, proving that appends
do not reload canonical history. No HTTP request is sent. Values are process
user+kernel microseconds per request, median [minimum, maximum].

| Protocol | Before cached canonical | Before resident | After cached canonical | After resident | After fresh construction + encoding |
| --- | ---: | ---: | ---: | ---: | ---: |
| Chat | 29.960 [29.770, 30.850] | 28.240 [27.860, 28.600] | 32.470 [32.150, 33.500] | 30.690 [30.450, 32.300] | 5288.430 [5192.610, 5471.180] |
| Messages | 44.970 [44.700, 48.490] | 42.760 [42.540, 52.930] | 47.550 [47.230, 53.760] | 45.700 [45.370, 48.340] | 4944.560 [4907.530, 5182.410] |
| Responses | 29.720 [29.530, 31.130] | 28.110 [27.710, 29.560] | 32.370 [32.170, 35.410] | 30.760 [30.380, 33.330] | 5245.090 [5228.550, 5722.930] |
| Gemini | 46.940 [46.230, 49.040] | 44.680 [44.520, 45.550] | 49.800 [48.870, 51.590] | 47.450 [46.640, 48.990] | 5355.950 [5279.650, 5417.130] |

Prefix reuse remains, with roughly 2–3 µs additional preparation per request
at these medians. Fresh encoding also becomes slower; a larger cache/fresh
ratio alone would not establish a CPU improvement. These component results
are separate from the full daemon and client workload timings below.

## Size and source scope

The release benchmark CLI grows from 7,356,192 to 7,887,088 bytes
(+530,896 bytes, 7.22%). These are fixture-enabled measurement artifacts built
with the same ordinary release profile and prebuilt standard library, not a
claim about the shipped non-fixture binary size. Artifact hashes and source
revisions accompany the raw measurements. No dependency was added.

Changed Rust and Swift files have a net addition of 4,430 physical source lines,
including tests, comments and blank lines (excluding documentation and generated
corpus). Paging replaces duplicate transcript ownership but adds range/index and
continuation machinery; it is not a net source-line reduction.

## Deep workload results

Values are median [minimum, maximum], three samples per arm. Each native
client row sums one process per root; the web server is one shared process.
The browser row sums its owned process RSS, including native DOM storage.

| Roots | Process set / metric | Before, MiB | After, MiB |
| ---: | --- | ---: | ---: |
| 1 | Daemon live | 131.101 [131.092, 131.101] | 112.659 [112.653, 112.665] |
| 1 | TUI live | 18.432 [18.432, 18.494] | 0.555 [0.555, 0.555] |
| 1 | FFI live | 20.697 [20.696, 20.697] | 0.617 [0.617, 0.617] |
| 1 | Swift store live | 25.395 [25.394, 25.395] | 0.786 [0.786, 0.787] |
| 1 | Web server live | 18.177 [18.177, 18.177] | 0.360 [0.360, 0.360] |
| 1 | Browser RSS | 638.172 [638.125, 638.484] | 652.203 [641.359, 659.656] |
| 1 | Browser footprint | 271.460 [271.319, 271.725] | 279.928 [269.131, 290.538] |
| 4 | Daemon live | 522.266 [522.236, 522.279] | 448.517 [448.515, 448.518] |
| 4 | TUI live | 73.851 [73.851, 73.851] | 2.222 [2.222, 2.222] |
| 4 | FFI live | 82.785 [82.785, 82.785] | 2.469 [2.469, 2.469] |
| 4 | Swift store live | 101.573 [101.569, 101.578] | 3.156 [3.144, 3.157] |
| 4 | Web server live | 72.053 [72.053, 72.053] | 0.781 [0.781, 0.781] |
| 4 | Browser RSS | 1475.438 [1475.047, 1475.734] | 1543.875 [1536.797, 1544.469] |
| 4 | Browser footprint | 847.667 [847.230, 847.808] | 901.292 [900.120, 907.433] |
| 8 | Daemon live | 1043.688 [1043.663, 1043.699] | 896.178 [896.165, 896.180] |
| 8 | TUI live | 147.637 [147.511, 147.699] | 4.442 [4.442, 4.442] |
| 8 | FFI live | 165.571 [165.569, 165.571] | 4.936 [4.936, 4.936] |
| 8 | Swift store live | 203.142 [203.139, 203.147] | 6.296 [6.295, 6.300] |
| 8 | Web server live | 143.888 [143.888, 143.888] | 1.344 [1.344, 1.344] |
| 8 | Browser RSS | 2419.016 [2134.891, 2514.750] | 2703.594 [2685.172, 2713.531] |
| 8 | Browser footprint | 1590.970 [1588.954, 1592.361] | 1713.282 [1691.767, 1727.376] |

| Roots | Operation | Before, ms | After, ms |
| ---: | --- | ---: | ---: |
| 1 | Warm control attach | 54.453 [54.326, 55.047] | 0.232 [0.212, 0.235] |
| 1 | Restarted-daemon control attach | 132.227 [131.924, 132.588] | 77.628 [77.585, 77.938] |
| 1 | TUI settled paint | 344.803 [339.696, 354.443] | 226.517 [221.228, 237.242] |
| 1 | FFI first window | 42.981 [42.854, 54.338] | 1.573 [1.570, 1.657] |
| 1 | Swift first window | 55.321 [54.475, 55.401] | 30.263 [30.247, 30.283] |
| 1 | Complete browser DOM | 146.025 [132.938, 151.608] | 450.878 [445.356, 456.333] |
| 1 | Daemon CPU / root turn | 72.767 [71.504, 73.050] | 72.959 [71.026, 73.271] |
| 1 | Daemon + SDK controller CPU / root turn | 73.624 [72.339, 73.917] | 73.413 [71.465, 73.726] |
| 1 | Changed TUI scroll first frame | 1.524 [1.519, 1.532] | 1.527 [1.526, 1.532] |
| 4 | Warm control attach | 54.848 [54.366, 54.929] | 0.233 [0.228, 0.237] |
| 4 | Restarted-daemon control attach | 128.542 [128.345, 128.646] | 73.539 [73.396, 73.675] |
| 4 | TUI settled paint | 345.472 [337.298, 347.966] | 232.880 [226.157, 233.290] |
| 4 | FFI first window | 42.854 [42.797, 43.116] | 1.604 [1.604, 1.611] |
| 4 | Swift first window | 57.655 [55.732, 58.322] | 30.252 [27.551, 30.253] |
| 4 | Complete browser DOM | 135.824 [133.522, 309.224] | 437.174 [436.925, 451.475] |
| 4 | Daemon CPU / root turn | 78.722 [78.483, 79.225] | 78.716 [77.997, 78.782] |
| 4 | Daemon + SDK controller CPU / root turn | 79.760 [79.520, 80.293] | 79.323 [78.594, 79.351] |
| 4 | Changed TUI scroll first frame | 1.528 [1.528, 1.537] | 1.535 [1.533, 1.540] |
| 8 | Warm control attach | 56.353 [54.924, 56.391] | 0.339 [0.338, 0.340] |
| 8 | Restarted-daemon control attach | 128.640 [128.433, 129.504] | 73.528 [73.399, 73.921] |
| 8 | TUI settled paint | 343.787 [342.062, 347.817] | 235.269 [234.631, 238.225] |
| 8 | FFI first window | 43.011 [42.854, 43.596] | 1.599 [1.596, 1.614] |
| 8 | Swift first window | 59.974 [51.712, 60.276] | 30.242 [27.908, 30.249] |
| 8 | Complete browser DOM | 134.695 [134.272, 152.784] | 429.495 [426.113, 429.647] |
| 8 | Daemon CPU / root turn | 82.041 [81.903, 82.774] | 81.455 [81.362, 81.516] |
| 8 | Daemon + SDK controller CPU / root turn | 83.167 [83.059, 83.904] | 82.115 [82.004, 82.156] |
| 8 | Changed TUI scroll first frame | 1.533 [1.532, 1.533] | 1.530 [1.530, 1.532] |

Swift readiness is polled every 20 ms. TUI settled paint includes a 200 ms
quiet fence. Restarted-daemon attach leaves the OS filesystem cache warm.
Complete browser hydration is slower despite faster control and native window
attach; it includes reconstructing and transferring every displayed item.
The browser retains the complete DOM in both arms. Spreads on this shared
host limit conclusions about small CPU differences.

## Ordinal seek results

The baseline reads its already-resident view; the candidate performs a real
history RPC. These measure backend range access, separately from UI paint.

| Roots | Samples per arm | Before, ms | After, ms |
| ---: | ---: | ---: | ---: |
| 1 | 60 | 0.000521 [0.000291, 0.002709] | 0.814354 [0.611166, 1.140959] |
| 4 | 240 | 0.000417 [0.000291, 0.004000] | 0.721562 [0.591625, 0.846167] |
| 8 | 480 | 0.000375 [0.000250, 0.004042] | 0.711313 [0.583875, 0.892833] |

## Browser retention diagnostic

The main workload above reports complete-DOM delivery and natural browser
footprint separately from pages released by the web server.
The web server releases its pages, but this does not make the browser DOM
smaller. These costs are retained in the comparison.

A separate component diagnostic reopens the completed logs after the additional
live-renderer round (33 root turns), with no native clients or new model calls.
Three interleaved pairs per scale sample natural browser state, explicitly
collect each tab with CDP, then sample again. This is a diagnostic, not a product
GC policy or a replacement for the natural results. Values are median
[minimum, maximum], MiB summed across tabs/processes as appropriate.

| Roots | Arm | Natural JS used | Collected JS used | Natural footprint | Collected footprint |
| ---: | --- | ---: | ---: | ---: | ---: |
| 1 | before | 0.837 [0.835, 0.838] | 0.799 [0.798, 0.801] | 275.397 [275.147, 279.928] | 263.163 [262.928, 263.944] |
| 1 | after | 2.662 [2.294, 2.824] | 0.979 [0.979, 0.989] | 284.663 [271.756, 288.350] | 271.944 [256.538, 272.303] |
| 4 | before | 3.348 [3.343, 3.349] | 3.200 [3.195, 3.200] | 869.292 [869.011, 869.839] | 817.652 [817.402, 817.730] |
| 4 | after | 13.705 [11.951, 16.094] | 3.972 [3.908, 3.987] | 919.339 [904.308, 926.074] | 851.308 [851.120, 853.495] |
| 8 | before | 6.687 [6.685, 6.689] | 6.392 [6.389, 6.394] | 1652.220 [1652.126, 1652.454] | 1553.407 [1552.126, 1554.985] |
| 8 | after | 28.446 [22.135, 36.867] | 7.904 [7.857, 7.932] | 1735.095 [1732.204, 1800.829] | 1618.954 [1612.751, 1623.267] |

Collection removes much of the candidate's transient JS allocation, but its
collected DOM/node count and footprint are recorded in the raw data.
The diagnostic does not establish
which browser allocation accounts for each remaining physical byte. Natural
browser RSS also includes shared mappings and should not be treated as unique
physical ownership. Native DOM selection/find and all displayed history remain
available; no offscreen content is hidden.

## Native physical footprint

The same deep-workload samples report charged phys_footprint separately from
live malloc bytes. Values are median [minimum, maximum], MiB. Raw RSS snapshots
are also included in the accompanying data.

| Roots | Process set | Before footprint | After footprint |
| ---: | --- | ---: | ---: |
| 1 | Daemon | 198.813 [194.313, 200.376] | 157.641 [156.579, 159.407] |
| 1 | TUI | 24.360 [23.344, 24.485] | 3.360 [3.344, 3.407] |
| 1 | FFI | 27.844 [27.641, 28.344] | 2.985 [2.922, 3.000] |
| 1 | Swift store | 29.563 [29.501, 30.126] | 3.641 [3.579, 3.657] |
| 1 | Web server | 33.251 [32.297, 33.282] | 7.719 [7.610, 8.016] |
| 4 | Daemon | 755.892 [752.001, 757.376] | 641.845 [640.970, 644.407] |
| 4 | TUI | 95.330 [95.174, 96.455] | 13.611 [13.533, 13.736] |
| 4 | FFI | 110.908 [110.768, 111.049] | 11.908 [11.861, 11.908] |
| 4 | Swift store | 117.580 [117.471, 117.627] | 14.721 [14.643, 14.783] |
| 4 | Web server | 100.329 [100.110, 100.454] | 9.844 [8.735, 10.141] |
| 8 | Daemon | 1280.080 [1257.580, 1354.861] | 1129.627 [1054.502, 1131.689] |
| 8 | TUI | 190.832 [189.535, 191.395] | 27.144 [27.051, 27.160] |
| 8 | FFI | 221.816 [221.785, 221.957] | 23.691 [23.676, 23.848] |
| 8 | Swift store | 235.442 [235.301, 236.395] | 29.129 [29.114, 29.239] |
| 8 | Web server | 190.219 [189.766, 190.688] | 11.797 [11.735, 11.860] |

## 100 replies of 5 MiB

Three interleaved pairs complete all 100 large replies and two auxiliary small
model calls per arm, with 102 successful model results and no model failures.
TUI observations are taken after reply 100. FFI and the actual native Swift
store attach sequentially to that same cut. Values are median [minimum, maximum];
each cell states its sample count. Live heap includes allocator capacity.

| Metric | Before | After |
| --- | ---: | ---: |
| Daemon live heap after reply 100, MiB | 507.454 [507.454, 507.455] (n=3) | 513.355 [513.348, 513.355] (n=3) |
| Daemon live heap after native far attaches, MiB | 1008.727 [1008.695, 1008.757] (n=3) | 513.211 [513.204, 513.211] (n=3) |
| Daemon footprint after native far attaches, MiB | 1338.252 [1337.940, 1339.565] (n=3) | 653.986 [639.392, 666.376] (n=3) |
| Daemon live slope, MiB/reply, replies 10–100 | 5.016 [5.016, 5.016] (n=3) | 5.025 [5.025, 5.025] (n=3) |
| TUI live heap, MiB | 508.207 [505.957, 508.207] (n=3) | 7.854 [5.604, 7.854] (n=3) |
| TUI live slope, MiB/reply, replies 10–100 | 5.051 [5.001, 5.076] (n=3) | 0.070 [-0.000, 0.070] (n=3) |
| FFI live heap, MiB | 500.730 [500.730, 500.730] (n=3) | 0.701 [0.701, 0.701] (n=3) |
| Swift store live heap, MiB | 1002.396 [1002.396, 1002.397] (n=3) | 0.904 [0.904, 0.905] (n=3) |
| Daemon footprint, MiB | 674.470 [644.642, 686.252] (n=3) | 652.954 [639.314, 661.361] (n=3) |
| TUI footprint, MiB | 532.735 [530.439, 533.110] (n=3) | 199.923 [166.110, 229.548] (n=3) |
| FFI footprint, MiB | 826.251 [826.236, 826.298] (n=3) | 2.954 [2.891, 2.985] (n=3) |
| Swift store footprint, MiB | 1019.424 [1019.392, 1019.424] (n=3) | 3.907 [3.876, 3.938] (n=3) |
| Daemon CPU, seconds / 100 replies | 59.463 [59.079, 59.685] (n=3) | 61.416 [61.098, 61.784] (n=3) |
| TUI CPU, seconds / 100 replies | 77.896 [77.363, 78.263] (n=3) | 62.439 [61.839, 62.902] (n=3) |
| Warm control attach, ms | 2498.473 [2495.952, 2645.158] (n=3) | 0.221 [0.212, 0.274] (n=3) |
| Restarted-daemon control attach, ms | 2917.830 [2909.069, 2929.078] (n=3) | 1122.784 [1115.992, 1124.248] (n=3) |
| FFI first window, ms | 677.443 [670.936, 690.863] (n=3) | 11.673 [11.020, 11.775] (n=3) |
| Swift first window, ms | 715.582 [697.277, 718.220] (n=3) | 30.226 [29.172, 31.333] (n=3) |
| Mixed ordinal seek, median of 12 / arm, ms | 0.001 [0.000, 0.001] (n=3) | 7.745 [7.590, 7.769] (n=3) |
| Mixed ordinal seek, maximum of 12 / arm, ms | 0.002 [0.002, 0.003] (n=3) | 8.433 [8.364, 8.655] (n=3) |

The daemon slope remains approximately one encoded/mutable representation of
each reply. Its fixed allocation level is slightly higher after byte chunking.
The TUI's residual allocation varies with the burst: allocation-stack probes
identify drained inbox capacity rather than committed transcript text. No
mailbox cap, trimming timer or sys page-buffer change was added.

### Browser completion and partial hydration

The probe allows 900 seconds for DOM readiness and 300 seconds for an individual
CDP response. These are measurement timeouts, not product history limits.
Readiness requires all items and their complete canonical content. A wrapper alone
does not count as completed delivery.

| Arm | Completed full DOM | Failure types |
| --- | ---: | --- |
| before | 0/3 | WebSocketTimeoutException: 3 |
| after | 3/3 | none |

| Browser/server metric | Before | After |
| --- | ---: | ---: |
| Completed browser DOM attach, ms | no sample | 527465.248 [527010.522, 536735.796] (n=3) |
| Completed browser footprint, MiB | no sample | 13207.253 [13203.721, 13242.331] (n=3) |
| Completed browser JS heap used, MiB | no sample | 0.845 [0.843, 0.846] (n=3) |
| Web server live heap after completed DOM, MiB | no sample | 0.357 [0.357, 0.357] (n=3) |
| Web server live heap during failed hydration, MiB | 500.358 [500.358, 500.359] (n=3) | no sample |
| Web server footprint during failed hydration, MiB | 505.064 [504.970, 505.079] (n=3) | no sample |
| Browser RSS during failed hydration, MiB | 3331.062 [3153.047, 3801.984] (n=3) | no sample |
| Browser footprint during failed hydration, MiB | 14561.144 [14460.175, 14809.847] (n=3) | no sample |

Partial-hydration observations, if any, are recorded at their failure time and
progress; they are not comparable completed-browser footprints. Completed DOM
timings include all requested content, with no offscreen hiding or truncation.
Normal delivery batches are 1 MiB; a single oversized committed item is admitted
whole to preserve canonical Markdown, selection and accessibility. Server pages
are released after rendering.

## Verification and review scope

The final local suite passed 1,254 Rust nextest tests (41 skipped), touched-crate
check/clippy with warnings denied, formatting, the workspace Windows static
check, 31 real-daemon Swift tests and the native macOS app build. The browser
acceptance probe checks native find, selection text, accessibility content and
unique IDs across demanded delivery. Swift tests/builds use copied packages and
scratch paths under the one external target directory, preserving the worktree.

The separately owned `crates/ext-mcp/tests/oauth.rs` still contains five raw-entry
test readers. They require migration to public heads/ranges or test-side log
inspection before unrestricted CI; production MCP is untouched. This known
test dependency is not included in the touched-crate success claim. Native
Linux/glibc and Windows large-drop allocator measurements also remain pending.
The TUI inbox residual belongs to the separately coordinated mailbox work.
The measured browser DOM latency and footprint costs need independent review
and further profiling; no offscreen hiding, global GC/trim policy or speculative
hardening was added.

[Raw observations, spreads, artifact hashes and exclusions](history-paging.json)
record the measurement provenance. The source baseline is `b7dee58e`; the branch
was created/merged against `ea0ce5ff`. The intervening main change affects MCP
lifecycle/tests, which this fixture workload does not exercise. Scratch harnesses
are identified by path and SHA-256 in the data.
