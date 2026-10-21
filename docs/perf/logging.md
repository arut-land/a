# Essential diagnostics (2026-10-21)

The logging API must reject arbitrary text at compile time, keep bounded
memory and disk, and never wait for disk on a session commit. `ext-logging`
uses the existing `json`, `rt`, `sys`, `protocol` and serde dependencies;
it adds no new third-party package to the production graph.

We compared `log` 0.4.34 and `tracing` 0.1.44 with `tracing-subscriber`
0.3.23 (`fmt,json,registry`, defaults off). The standalone probe builds with
size optimization, LTO, one codegen unit and stripping on macOS arm64,
nightly-2026-10-17. Run `bash docs/perf/logging-probe.sh`. The probe counts
bytes in a `Write` sink and formats one million repair records. Tiny and
log emit identical bytes; tracing emits its normal structured JSON envelope
with time, target and span context disabled. These are formatting/dispatch
costs, not disk benchmarks or application startup measurements.

| Probe | Stripped bytes | Delta from tiny | ns/record, median of five |
| --- | ---: | ---: | ---: |
| Direct JSON formatting | 286,208 | 0 | 28 |
| `log`, enabled custom sink | 286,224 | 16 | 73 |
| tracing JSON subscriber | 419,248 | 133,040 | 2,206 |

This machine was shared with other builds: samples varied substantially
(direct 27–47 ns, log 63–102 ns, tracing 1,602–4,074 ns). The earlier single
samples were 18/35/666 ns respectively. Do not use these as latency budgets
or claim their ratios are stable. Binary size and the API constraints are
the useful decision evidence. Both surveyed dependency trees are free of
the crate named exactly `tokio`; the probe checks tracing's tree explicitly.

`log` is effectively free in binary size here. It is a maintained facade,
but does not supply the typed boundary, bounded off-thread writer or size
rotation. Installing it globally would also admit free-form records from
other dependencies. A private adapter would still require the same queue,
writer and rotation, plus another dispatch layer. The JSON subscriber adds
about 130 KiB in this probe and formats an envelope we do not need; it also
needs a separate bounded writer and size-rotation policy. Neither replaces
the necessary implementation. We therefore keep a sealed typed vocabulary
and the small writer directly over the existing runtime and rotation seam.
No general-purpose logging framework or custom JSON codec is introduced.

The producer stores only a fixed-size typed record and notifies its async
consumer; serialization and all filesystem operations run on the logging
runtime's blocking pool. At most 256 queued plus 256 in-flight records are
retained, regardless of input length or writer speed. A deterministic
Faulty-store test proves commit and store recovery finish while the log
writer is stalled, rather than asserting a noisy timing threshold.

`mise run size` before logging: **2,975 KiB** at `9e13330`; after:
**3,039 KiB**, under the 4,096 KiB limit. Main was merged at `9fcf4cd`
(including platform seams and shared flush epochs), so the 64 KiB delta
also includes those changes.

Validation used `CARGO_BUILD_JOBS=3`: `mise run check` passed 926 tests
(28 intentionally skipped), acceptance passed 12, and `test:web` passed 5.
`check:windows`, `machete`, `check:no-tokio` and the `ext-logging`
compile-fail doctest passed. The size task also verified that the shipped
binary lacks the fixtures. Budgets and startup measurements were not run;
the coordinator will measure them on a quiet machine.

The review exposed a blocking wake after the queue's `try_lock`. Admission
now uses `rt::Wakeup`: a coalescing atomic flag and `atomic-waker` 1.1.2,
already linked through `async-io` (no new package). Its maintained atomic
registration state lets a producer publish a wake while the consumer is
suspended inside registration, without waiting for its lock. The existing
multi-waiter `rt::Notify` is retained for flush waiters only. A deterministic
test parks the consumer inside waker replacement and requires the complete
producer admission to finish before releasing it. Final binary size below
includes this direct dependency and all review fixes.

Crash reports reuse serde and serde_json already in the binary, moving the
validated numeric build identity into `sys::crash` so the daemon log and
panic hook share its privacy boundary. Numeric unwind frames replace std's
debug symbolizer and raw panic formatting; report storage is at most
20 × 16 KiB, with a fixed 128-frame capture buffer.
