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
take 8 KiB per call, so their box is amortized over many frames. No
budget (`mise run budgets`: cold start, attach latency, idle RSS) moves
by that; the perf workflow's comparison with main on this branch is the
measurement of record for size, latency and RSS (see the report of the
`link-trait` merge).

## Elsewhere

- The blocking API (`Daemon::connect`, `status`, `stop`) now runs the
  same async hello on its caller's thread through a runtime whose
  blocking work runs inline: no thread is started for it, and nothing
  polls (`crates/client/tests/no_polling.rs` still holds).
- The read buffer is the frame buffer's spare capacity, not an 8 KiB
  array on the stack, which kept the hello's future under 17 KiB.
