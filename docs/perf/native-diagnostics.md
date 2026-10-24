# Native diagnostics

Native delivery supplements the sealed JSON vocabulary and bounded local
writer. Linux uses `std::os::unix::net::UnixDatagram`: one nonblocking send
implements the journald native protocol, with no libsystemd link or runtime
requirement. A systemd binding would add a library requirement for this single
operation without replacing admission, encoding or file fallback. macOS uses
`oslog` 0.2 without its optional logging facade; Windows uses Microsoft's
`tracelogging` 1.2.4. Neither introduces another worker or timer.

`object` 0.40 parses mapped ELF notes and Mach-O load commands with only
`read_core` and the target's format enabled. Compression, archives and other
formats are disabled. Windows reuses `windows-sys` for loaded-module identity
and stack capture. The maintained parsers and native logging adapters avoid
custom Mach-O parsing, os_log argument packing and TraceLogging metadata.

The [hosted paired run](https://github.com/arut-land/a/actions/runs/40036096021)
compares main `4b662d7a` with implementation `fa05741c`, five reference and
five candidate rounds per target in alternating order. Sizes are stripped
shipped binaries; values and deltas below are medians. KiB means 1024 bytes.

| Target | Main KiB | Candidate KiB | Delta KiB | Cold reply delta ms | Attach p99 delta ms | Idle RSS delta KiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Linux x86_64 | 4190.820 | 4194.820 | +4.000 | +0.232 | -0.005 | +40 |
| Linux arm64 | 3603.539 | 3607.789 | +4.250 | -0.732 | +0.026 | +12 |
| macOS arm64 | 3236.031 | 3252.703 | +16.672 | -0.907 | +0.088 | -112 |
| macOS x86_64 | 3896.922 | 3901.453 | +4.531 | -7.144 | -0.022 | -60 |
| Windows x86_64 | 5408.000 | 5415.500 | +7.500 | -0.496 | +0.018 | +28 |
| Windows arm64 | 4754.500 | 4762.000 | +7.500 | +2.546 | -0.008 | +48 |

Idle daemon and line-client CPU medians are zero before and after on every
target. Large-JSON CPU median deltas are:

| Target | CPU delta |
| --- | ---: |
| Linux x86_64 | -0.87% |
| Linux arm64 | +1.53% |
| macOS arm64 | -4.25% |
| macOS x86_64 | -7.77% |
| Windows x86_64 | +9.88% |
| Windows arm64 | +2.58% |

Positive CPU deltas remain visible; these measurements do not establish their
cause. Windows x86_64 CPU samples span 2.38–2.67 s for main and
2.47–2.88 s for the candidate. macOS arm64 attach-p99 samples span
0.39–0.52 ms and 0.45–0.74 ms respectively. The spread limits causal
conclusions and does not rule out an application regression. No active-workload
performance improvement is claimed. Producer admission is unchanged; successful
large-JSON processing does not emit diagnostic facts.

The harness uses isolated `A_HOME`, so native delivery is disabled: these are
application regression measurements, not a benchmark of enabled collector
delivery. Enabled-collector latency and the cause of the positive CPU deltas
remain unmeasured; the transport adds no idle worker, timer or retry queue.

A separate [hosted static-musl probe](https://github.com/arut-land/a/actions/runs/39997996173)
verifies ELF identity without symbol lookup in both static and PIC relocation
modes. The native transport tests cover atomic datagrams, a full collector
queue, missing native delivery, isolated homes and provider lifetimes. Crash
tests cover bounded identity, malformed ELF notes, unreadable PE memory and
the report's privacy and size limits.

[Hosted CI](https://github.com/arut-land/a/actions/runs/40036092401) validates
the combined source on Linux, macOS and both Windows architectures, including
workspace checks, acceptance, release budgets, Apple tests/build, Miri, fuzz
smoke, dependency checks and the no-tokio and Windows seams.
