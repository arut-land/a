# Windowed text reads

`read` counts newlines with `memchr_iter(...).count()` once per buffered
chunk, selects the requested byte range from that chunk, and consumes it
whole. Only chunks containing a window boundary need to locate individual
newlines. This removes per-line reader calls for both selected and excluded
lines, without adding a second pass over the file.

The fold retains only selected bytes, bounded by the existing 8 MiB text
limit. Excluded bytes are counted without decoding or retaining them. A
final nonempty line without a newline contributes exactly one to the total;
CRLF bytes and UTF-8 spanning buffers remain unchanged. The file is still
read to EOF to return its exact line count. No limits, dependencies, wire
formats, or runtime/platform seams change.

## Measurement

Measured 2026-10-27 on the shared Apple M3, macOS 27.0.1 arm64,
`rustc 1.101.0-nightly (5d8fec066 2026-10-16)`. Baseline source is
`5b6d12c1`; candidate changes only the text fold and tests. Both use the
same dependencies and release harness profile (`opt-level="z"`, fat LTO,
one codegen unit, stripped symbols). Builds use `CARGO_BUILD_JOBS=2` via
`mise exec --`, with `CARGO_TARGET_DIR` outside the worktree.

This reproduces finding 2 from the round-3 tools-load survey. Its archived
`api-probe/src/main.rs` read workload and `measure-read.py` are used here;
the harness is reduced to the read entry point, pointing its dependencies
at this checkout. It invokes the real `ext_edit::Edit` through
`ext::testing::Rig`, with `ext_shell::Shell` supplying the working directory.
There is no daemon or model provider. Fixtures and rig homes live in external
scratch storage.

Each process warms up with one read, waits for the driver to snapshot its
native process CPU counters, then reads 30 times. Every call requests
`offset=1, limit=1` and asserts the exact returned text and total. The driver
snapshots CPU again while the process waits at a completion gate. CPU/read
is the process CPU delta divided by 30; latency measures each complete rig
call. There are five alternating rounds: A/B, B/A, A/B, B/A, A/B, for each
file. Results below report the median and min–max of the five batch CPU
averages or batch latency medians. Pooled latency percentiles use all 150
calls per variant. This host is shared; these are observations under
contention, not quiet-machine qualifications.

Inputs exactly match the survey generator:

```python
first = b"ordinary line to read.\n"
short = first + first * ((64 << 20) // 22)
long = first + (b"x" * 4095 + b"\n") * 16384
```

The short file is 67,108,866 bytes, 3,050,403 lines; the control is
67,108,886 bytes, 16,385 lines. Both preserve the same first line.
Before changing the fold, an A/A run (both lanes the baseline binary,
five alternating rounds) reproduced the CPU gap: short-line CPU/read
44.58 ms [40.20–47.63], control 12.36 ms [11.29–14.30], across ten batches
each. The final comparison interleaves the saved baseline with the candidate:

| Workload / metric | Baseline | Chunk fold |
| --- | ---: | ---: |
| Short lines: CPU/read, ms | 45.32 [40.35–50.38] | 12.11 [10.13–14.69] |
| Short lines: batch latency p50, ms | 46.63 [39.57–51.96] | 12.14 [10.09–15.31] |
| Short lines: pooled latency p50 / p99, ms | 45.46 / 74.52 | 12.68 / 18.56 |
| 4 KiB lines: CPU/read, ms | 13.06 [11.50–14.03] | 11.20 [10.31–14.25] |
| 4 KiB lines: batch latency p50, ms | 13.06 [11.40–14.54] | 11.02 [10.26–16.08] |
| 4 KiB lines: pooled latency p50 / p99, ms | 12.67 / 17.20 | 11.10 / 21.33 |

Median CPU falls 73.3% for short lines and 14.2% for the control. The
control's latency ranges overlap and its candidate pooled p99 is higher;
these samples do not establish a control-tail improvement. No heap or
cross-platform performance improvement is claimed from this measurement.

## Invariants and verification

`line_windows_consume_each_buffer_once` instruments a real `Cursor` reader
around the production fold. It requires one buffer access for data and one
for EOF even when the chunk contains excluded prefix and suffix lines. It
fails on the original implementation (six accesses instead of two), so the
performance regression has a deterministic test rather than a timing budget.

Real-file rig tests cover exact totals with and without trailing newlines,
empty files and windows, blank lines, CRLF split between buffers, long
selected and excluded lines, UTF-8 crossing buffers, offsets at zero and
past EOF, saturating limits, and invalid UTF-8 outside the window. Existing
tests still enforce selected-text limits and rejection of invalid selected
UTF-8.

Local checks: `cargo check -p ext-edit`, `cargo nextest run -p ext-edit`,
`cargo fmt --all -- --check`,
`cargo clippy -p ext-edit --all-targets --all-features -- -D warnings`, and
`mise run check:windows`. No speculative hardening is included.
