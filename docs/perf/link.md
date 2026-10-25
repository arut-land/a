# The client link: `Arc<dyn Link>` over a generic `Daemon<L>`

`client::Daemon` holds `Arc<dyn Link>`, and a link opens a `Box<dyn
Stream>`. The alternative was `Daemon<L: Link>` with an associated stream
type, monomorphized through the `Driver`.

## Why not generic

The link is chosen at run time: `a --host box` picks an `SshLink`, no flag
a `LocalLink`. A generic `Daemon<L>` would make every app (TUI, web, REPL,
FFI) either generic over the link or hold an `Either<LocalLink, SshLink>`,
which is the closed enum the seam removes. So the question measured is
only what erasure costs on the hot path, against the concrete
`rt::Async` calls a generic build would compile to.

## Per-frame cost of erasure

Each `Stream::read` and `Stream::write_all` returns a boxed future: one
allocation per call. A throwaway bench (not kept) wrote 200,000 frames of
96 bytes over a local socket through each, on one `rt::Threads` worker,
release build, Apple M-series, macOS 27, quiet machine; three rounds:

| | round 0 | round 1 | round 2 |
| --- | --- | --- | --- |
| direct `rt::Async::write_all` | 605 ns | 510 ns | 513 ns |
| `Box<dyn Stream>::write_all` | 561 ns | 554 ns | 552 ns |

About 40 ns a frame, under 10 % of a syscall-bound write of the smallest
frame, and less than the `Vec` each frame is already encoded into. Reads
take 8 KiB per call, so their box is amortized over many frames.

## Against main, on hosted runners

The perf workflow (run 40148803457, branch head `f047dd01`, alternating
with main) on each native runner:

| Runner | size | idle RSS | attach p50 | cold reply |
| --- | --- | --- | --- | --- |
| ubuntu-latest | +11.3 KiB (+0.3 %) | +0 KiB | +0.013 ms (+3.0 %) | noise |
| ubuntu-24.04-arm | +11.0 KiB (+0.3 %) | noise | +0.011 ms (+2.6 %) | noise |
| macos-latest | +16.3 KiB (+0.5 %) | +144 KiB (+1.6 %) | noise | noise |
| macos-15-intel | +12.1 KiB (+0.3 %) | noise | noise | noise |
| windows-latest | +11.5 KiB (+0.2 %) | noise | -0.49 ms (-49 %) | -10 ms (-38 %) |
| windows-11-arm | +17.0 KiB (+0.3 %) | noise | noise | noise |

The size is the new SSH bootstrap (scripts, stderr report, batch mode)
and the erased stream; RSS and latency are within noise but for the
single macOS idle reading and two Linux attach readings of about 12 µs,
none of which a generic `Daemon<L>` would remove: its hot path differs
only by the per-call box measured above.

## Elsewhere

- The blocking API (`Daemon::connect`, `status`, `stop`) now runs the
  same async hello on its caller's thread through a runtime whose
  blocking work runs inline: no thread is started for it, and nothing
  polls (`crates/client/tests/no_polling.rs` still holds).
- The read buffer is the frame buffer's spare capacity, not an 8 KiB
  array on the stack, which kept the hello's future under 17 KiB.
