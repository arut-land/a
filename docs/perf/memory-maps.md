# Native memory-map attribution

`process.memory_maps` captures native diagnostics outside timed workload
windows. On macOS it retains the complete `vmmap -resident` stdout, stderr and
exit status before deriving executable-image residency. Failed identification
adds `maps_error`; resident-byte fields remain absent. An unidentified image is
never reported as zero resident bytes.

The region detail is a path, including any embedded spaces. Both that path and
the process executable are resolved before comparison. macOS can name the same
file through `/tmp` and `/private/tmp`, `/var` and `/private/var`, or another
symlink. Normalizing only the process executable loses these matches.

The native columns are rounded region measurements. `executable_mapped_bytes`
is virtual mapping size; `executable_resident_bytes` sums the executable's
resident mappings, and `executable_code_resident_bytes` sums its `__TEXT`
residency. `__TEXT` includes read-only data and unwind information as well as
instructions. These observations do not replace whole-process RSS or physical
footprint and must not be added to shared-cache totals to reconstruct RSS.

## Round-3 reproduction

On the shared Apple M3, macOS 27.0.1, five independent release fixture daemons
under `/private/tmp` reproduced the survey failure before changing the parser:
**0/5** executable identifications. psutil reported `/private/tmp`; vmmap
reported `/tmp`. The release build used the existing rebuilt-std flags,
nightly-2026-10-17 and two build jobs. No product source or build policy changed.

After the fix, five fresh daemons ran the same idle workload: three seconds to
RSS, twelve more seconds for worker retirement, then ten quiet seconds. Each
native capture was parsed by both the original and fixed implementation in
alternating AB/BA order. Original identification remained **0/5**; fixed
identification was **5/5**, with raw diagnostics retained in all five captures.

| Observation | Original | Fixed median [min–max] |
| --- | --- | --- |
| Executable resident KiB | unavailable | 3,696 [3,456–3,696] |
| Executable `__TEXT` resident KiB | unavailable | 3,424 [3,424–3,424] |
| Parser time ms | 0.411 [0.366–0.596], failed identification | 38.553 [35.534–57.088] |

These are recovered attribution measurements, not a product memory or latency
reduction. Canonical path resolution costs diagnostic time outside the timed
windows. The earlier survey's 3,312 KiB `__TEXT` observation measured an older
product revision and is not a before/after memory comparison.

The native harness tests launch an ad-hoc-signed copy of `cat`, exchange a
readiness line over real pipes, and inspect it with vmmap. The executable path
contains spaces and is also accessed through a symlink. A separate test retains
real native output when attribution raises an identification error. Both tests
fail against the original implementation.

Run the tests with the pinned psutil accounting dependency installed:

```sh
PYTHONDONTWRITEBYTECODE=1 mise exec -- python -m unittest discover \
  -s scripts/perf -p 'test_*.py'
```

Raw measurements and the alternating parser comparison for this round live in
the external `scratchpad/perf2/perf-tools` evidence directory; no captures or
executables are committed.
