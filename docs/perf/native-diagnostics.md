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

The [hosted paired run](https://github.com/arut-land/a/actions/runs/40027023965)
compares main `4d978b0b` with implementation `d1afe18b`, five reference and
five candidate rounds per target in alternating order. Sizes are stripped
shipped binaries; values and deltas below are medians. KiB means 1024 bytes.

| Target | Main KiB | Candidate KiB | Delta KiB | Cold reply delta ms | Attach p99 delta ms | Idle RSS delta KiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Linux x86_64 | 4192.070 | 4195.945 | +3.875 | -1.215 | +0.009 | +64 |
| Linux arm64 | 3604.539 | 3608.789 | +4.250 | -0.923 | +0.003 | +8 |
| macOS arm64 | 3236.031 | 3252.703 | +16.672 | -5.023 | +0.300 | +128 |
| macOS x86_64 | 3896.930 | 3901.453 | +4.523 | -0.913 | +0.124 | -12 |
| Windows x86_64 | 5405.000 | 5412.500 | +7.500 | +0.262 | +0.046 | +20 |
| Windows arm64 | 4754.500 | 4762.000 | +7.500 | -16.708 | +0.003 | -4 |

Idle daemon and line-client CPU medians are zero before and after on every
target. Large-JSON CPU median deltas are:

| Target | CPU delta |
| --- | ---: |
| Linux x86_64 | -2.35% |
| Linux arm64 | +2.94% |
| macOS arm64 | +7.91% |
| macOS x86_64 | -3.84% |
| Windows x86_64 | +7.04% |
| Windows arm64 | +2.47% |

Positive CPU deltas remain visible; these measurements do not establish their
cause. macOS arm64's reference CPU samples span 2.18–2.88 s and candidate
samples 2.00–2.87 s; its attach-p99 samples span 0.87–1.28 ms and 0.94–3.37 ms
respectively. Windows x86_64 CPU samples span 3.23–3.48 s and 3.44–3.61 s.
The spread limits causal conclusions and does not rule out an application
regression. No active-workload performance improvement is claimed.

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
