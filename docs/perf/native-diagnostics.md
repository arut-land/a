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

The [hosted paired run](https://github.com/arut-land/a/actions/runs/40005464210)
compares main `781586d5` with implementation `531fcb48`, five reference and
five candidate rounds per target in alternating order. Sizes are stripped
shipped binaries; values and deltas below are medians. KiB means 1024 bytes.

| Target | Main KiB | Candidate KiB | Delta KiB | Cold reply delta ms | Attach p99 delta ms | Idle RSS delta KiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Linux x86_64 | 4160.656 | 4164.531 | +3.875 | +0.240 | -0.012 | +76 |
| Linux arm64 | 3582.914 | 3587.164 | +4.250 | -0.015 | +0.004 | +24 |
| macOS arm64 | 3219.625 | 3236.297 | +16.672 | -0.485 | +0.014 | +80 |
| macOS x86_64 | 3864.828 | 3869.352 | +4.523 | +0.613 | -0.035 | +48 |
| Windows x86_64 | 5340.000 | 5347.000 | +7.000 | -0.514 | -0.147 | +24 |
| Windows arm64 | 4698.500 | 4703.000 | +4.500 | -6.218 | -0.025 | +44 |

Idle daemon and line-client CPU medians are zero before and after on every
target. Large-JSON CPU median deltas are +4.21% Linux x86_64, +2.99% Linux
arm64, +0.17% macOS arm64, -1.30% macOS x86_64, +1.83% Windows x86_64 and
-1.28% Windows arm64. These measurements do not establish a cause for the
Linux CPU increases. The harness uses isolated `A_HOME`, so native delivery
is disabled: these are application regression measurements, not a benchmark
of enabled collector delivery. No enabled-collector latency claim is made.

A separate [hosted static-musl probe](https://github.com/arut-land/a/actions/runs/39997996173)
verifies ELF identity without symbol lookup in both static and PIC relocation
modes. The native transport tests cover atomic datagrams, a full collector
queue, missing native delivery, isolated homes and provider lifetimes. Crash
tests cover bounded identity, malformed ELF notes, unreadable PE memory and
the report's privacy and size limits.
