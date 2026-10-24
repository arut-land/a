# Native diagnostics

Native delivery supplements the sealed JSON vocabulary and bounded local
writer. Linux uses `std::os::unix::net::UnixDatagram`: a nonblocking send
implements the journald native protocol, with no libsystemd link or runtime
requirement. A systemd binding would add a library requirement for this single
operation without replacing admission, encoding or file fallback. macOS uses
`oslog` 0.2 without its optional logging facade; Windows uses Microsoft's
`tracelogging` 1.2.4. Neither introduces another worker or timer.

Linux retains the endpoint pathname and reconnects the existing socket on
connection-refused, missing-endpoint, disconnected, reset, aborted or broken-pipe
errors. Each record allows one reconnect and one resend. A restart gap consumes
no background work: the next record attempts recovery. A full collector drops
the native copy without reconnecting; the readable local copy is independent.

`object` 0.40 parses mapped ELF notes and Mach-O load commands with only
`read_core` and the target's format enabled. Compression, archives and other
formats are disabled. Windows reuses `windows-sys` for loaded-module identity
and stack capture. The maintained parsers and native logging adapters avoid
custom Mach-O parsing, os_log argument packing and TraceLogging metadata.

The [hosted paired run](https://github.com/arut-land/a/actions/runs/40047980069)
compares main `4b662d7a` with reconnect implementation `3e576051`, five reference
and five candidate rounds per target in alternating order. Sizes are stripped
shipped binaries; values and deltas below are medians. KiB means 1024 bytes.

| Target | Main KiB | Candidate KiB | Delta KiB | Cold reply delta ms | Attach p99 delta ms | Idle RSS delta KiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Linux x86_64 | 4190.820 | 4195.320 | +4.500 | +0.068 | +0.005 | +84 |
| Linux arm64 | 3603.539 | 3608.164 | +4.625 | +0.306 | +0.001 | +0 |
| macOS arm64 | 3236.031 | 3252.703 | +16.672 | +0.528 | +0.002 | -96 |
| macOS x86_64 | 3896.922 | 3901.453 | +4.531 | -8.216 | +0.002 | +4 |
| Windows x86_64 | 5408.000 | 5415.500 | +7.500 | -0.034 | +0.020 | +8 |
| Windows arm64 | 4754.500 | 4762.000 | +7.500 | -0.809 | -0.046 | +56 |

Idle daemon and line-client CPU medians are zero before and after on every
target. Large-JSON CPU median deltas are:

| Target | CPU delta |
| --- | ---: |
| Linux x86_64 | -2.53% |
| Linux arm64 | +1.87% |
| macOS arm64 | +7.26% |
| macOS x86_64 | -1.17% |
| Windows x86_64 | +0.00% |
| Windows arm64 | +7.36% |

Positive CPU deltas remain visible; these measurements do not establish their
cause. Linux arm64 CPU samples span 2.214–2.236 s for main and 2.233–2.270 s
for the candidate. macOS arm64 spans 1.552–2.397 s and 1.686–2.947 s;
Windows arm64 spans 2.391–2.641 s and 2.391–2.812 s respectively. The spread
limits causal conclusions and does not rule out an application regression.
No active-workload performance improvement is claimed. Producer admission is
unchanged; successful large-JSON processing does not emit diagnostic facts.

The application harness uses isolated `A_HOME`, so native delivery is disabled.
Enabled-call costs are measured separately below. End-to-end collector latency
and the cause of positive application CPU deltas remain unmeasured; the transport
adds no idle worker, timer or retry queue.

## Calls with a listening collector

The opt-in `sys` release test exercises `Native::write` against real journald,
unified logging and ETW collectors. Each run uses five rounds of 500 distinct
128-byte JSONL records after 32 warmup records. A 10 ms sleep paces records
outside the timed interval. JSON construction, local-file writes, collector
draining and initialization are excluded. Timed calls include bounds/UTF-8
checks, framing or string conversion, enabled checks and the native API.

Collectors confirm readiness before timing: journald and `log stream` must
observe a handshake; ETW must enable the registered provider. Every timed
sequence must subsequently appear exactly once in `journalctl`, `log stream`
or the `tracerpt`-decoded ETL file. Missing/duplicate records fail the probe.
These are elapsed call costs including clock overhead and scheduling, without
subtracting the disabled control. They measure API acceptance, not persistence
latency or collector CPU. Different hosts do not establish an OS ranking.

| Host / collector | Verified records | Call p50 µs | Call p95 µs | Round mean range µs | Disabled p50 µs |
| --- | ---: | ---: | ---: | ---: | ---: |
| Linux x86_64 / journald | 2500/2500 | 12.109 | 15.163 | 11.826–13.545 | 0.080 |
| macOS 15 Intel / os_log + live stream | 2500/2500 | 106.593 | 246.452 | 105.608–214.940 | 0.247 |
| Windows x64 / ETW file session | 7500/7500 | 0.500–3.900 | 2.700–6.500 | 0.799–4.594 | 0.000–0.100 |

Windows ranges retain all three successful runs, including slower observations:
[run 1](https://github.com/arut-land/a/actions/runs/40048332733/job/120735062996),
[run 2](https://github.com/arut-land/a/actions/runs/40048572362/job/120735823271),
[run 3](https://github.com/arut-land/a/actions/runs/40048821964/job/120736617385).
Zero control samples are below clock resolution, not evidence of zero call cost.
[Linux](https://github.com/arut-land/a/actions/runs/40048821964/job/120736617189)
and [macOS](https://github.com/arut-land/a/actions/runs/40050006439/job/120740374320)
use one run each. The artifacts retain raw samples, platform/revision metadata
and collector output. macOS's measured enabled cost is substantially higher on
this host; it remains on the existing bounded storage worker, with producer
admission unchanged.

On an isolated hosted VM with permission to read its journal or start an ETW
session, run `python scripts/perf/native-diagnostics.py --output <fresh-dir>`.
It builds the ignored `listening_collector_cost_per_record` test with
`cargo test -p sys --release --lib --no-run`. `PERF_CARGO` can select a native
Cargo executable where the repository's cache wrapper is unavailable. This
explicit probe bypasses home routing to exercise native delivery, never opens
daemon state, and does not change normal isolated-`A_HOME` behavior.

A separate [hosted static-musl probe](https://github.com/arut-land/a/actions/runs/39997996173)
verifies ELF identity without symbol lookup in both static and PIC relocation
modes. The native transport tests cover atomic datagrams, a full collector
queue, endpoint replacement with and without records during the restart gap,
missing native delivery, isolated homes and provider lifetimes. Crash
tests cover bounded identity, malformed ELF notes, unreadable PE memory and
the report's privacy and size limits.

[Hosted CI](https://github.com/arut-land/a/actions/runs/40048333822) validates
the combined source on Linux, macOS and both Windows architectures, including
workspace checks, acceptance, release budgets, Apple tests/build, Miri, fuzz
smoke, dependency checks and the no-tokio and Windows seams.
