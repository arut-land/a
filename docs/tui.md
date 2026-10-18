# The terminal client (`apps/tui`)

At a terminal, `a`, `a resume` and `a attach` open a full-screen client on
the alternate screen, with the mouse. `A_TUI=inline` draws the same view in a
viewport at the bottom of the normal screen instead, printing finished
output into the terminal's own scrollback (no mouse; the terminal keeps its
own scrolling and selection). `A_TUI=plain`, a pipe on stdin or stdout, or
`TERM=dumb` keep the line REPL, which the acceptance tests drive.

## What it shows

Everything comes from the shared reducer, `protocol::view::SessionView`:
the app folds each `DaemonMsg` with `SessionView::fold` and uses the
returned `Update` only to know which item's rows to re-wrap. Nothing is
re-derived; what is local is screen state (scroll anchor, folded cards, the
input, overlays).

- Transcript: your messages, replies streamed from `SessionView::streaming`
  with light markdown (headings, bullets, quotes, fenced code, `**bold**`,
  `` `code` ``), hooks and environment changes as muted lines.
- Tool cards: the call, its state (running, waiting for approval, not
  approved, exit code, failed, interrupted) and the last 10 lines of output.
  A click on a card folds it; Ctrl-O shows every card's whole output.
  Command output has its escapes stripped.
- Approvals: the oldest call `asked()` of this client shows above the
  input with `[ y yes ]` `[ n no ]` buttons, and on its card; `y` or `n`
  with an empty input answers it.
- The recap after an attach, as a banner (Esc or its `×` hides it).
- Status line: link state (connecting, replaying, live, daemon upgrading,
  disconnected), session, workspace, model, tokens.
- Input: multi-line (Ctrl-J or Alt-Enter for a new line, bracketed paste
  keeps newlines), readline keys, Up/Down through history (seeded from the
  session's own messages).
- `/sessions` (a picker; also what `a resume` opens when this workspace has
  no session), `/new`, `/env refresh`, `/detach`, `/help` (or F1).
- Ctrl-C clears the input, or detaches when it is empty: the protocol has
  no way to interrupt a turn, and nothing the client does stops the daemon.
  Ctrl-D on an empty input detaches too.
- A daemon that drains for an upgrade is reconnected and replayed from
  `SessionView::seq`; one that does not answer the hello offers a restart.

Colors are the terminal's palette entries (1–7 and bright black) plus bold,
dim, italic and reverse, never RGB or a background, so light and dark themes
both read.

## Design

```
terminal thread ──Input──┐
                          ├─> channel ─> event loop: App::fold / App::input
daemon reader(s) ─Daemon─┘                           │
                                                     ▼
                                     App::draw ─> Buf ─> Screen::render ─> stdout
```

- One thread owns the app and blocks on an `mpsc` channel. The terminal
  thread blocks in `poll(2)` on stdin and a self-pipe the SIGWINCH handler
  writes to (`sys::tty`); each connection has a reader thread. Nothing
  polls on a timer: the only timeout is 30 ms while half an escape sequence
  is pending (a lone Esc). Everything queued is folded before one frame.
- `render::Screen` diffs a cell buffer against the last frame and writes
  only changed cells, inside a synchronized update (`?2026`). The same code
  draws the full screen (absolute moves) and the inline viewport (relative
  moves; `Screen::scrollback` prints settled items above it). That is the
  "inline viewport" both modes share: the app draws a frame of a height it
  chooses, the screen places it.
- Rows are wrapped lazily and cached per item and width: a frame wraps only
  what it shows, counted from the bottom or from the scroll anchor (an item
  and a row, which stays put as output arrives below). A 6000-item session
  costs about what a short one does to draw, resize or resume.
- The terminal is restored on exit, on a panic (a hook), and when stdin
  closes.
- The first frame is painted before the daemon is reached; connecting (and
  spawning the daemon) happens on the reader thread. An ssh endpoint
  connects first, since ssh may prompt on the terminal.

## Choosing a library: measurements

Prototypes of the same app (a 2000-message transcript, a status bar, an
input line, wheel scrolling, a simulated daemon stream, blocking input plus
a channel) were built with the release profile (fat LTO, one codegen unit,
`build-std`, stripped) on an Apple M3, macOS 27, `nightly-2026-10-17`, and
driven on a pseudo-terminal at 120×40. "Base" is the same program printing
lines. Frame costs include writing to the pty.

| | Binary | vs base | RSS (idle) | First frame, empty / 2000 msgs | Delta frame avg (p99) | Scroll frame | Bytes per delta frame |
| --- | --- | --- | --- | --- | --- | --- | --- |
| base (lines) | 182 KiB | | 4144 KiB | 3.6 / 8.9 ms | | | |
| ratatui 0.30 + crossterm 0.29 | 476 KiB | +294 KiB | +832 KiB | 4.0 / 9.2 ms | 209 µs (413) | 300 µs | 400 |
| ratatui 0.30 + termion 4 | 442 KiB | +259 KiB | +752 KiB | 3.2 / 9.2 ms | 169 µs (248) | 220 µs | 391 |
| own renderer + crossterm input | 298 KiB | +116 KiB | +176 KiB | 3.5 / 8.9 ms | 45 µs (105) | 55 µs | 350 |
| own renderer, rustix termios, own input | 215 KiB | +33 KiB | +96 KiB | 3.4 / 8.6 ms | 48 µs (116) | 57 µs | 350 |

Idle CPU was zero for all of them (no change in `ps` CPU time over 10 s):
each blocks on input and a channel. First frames are dominated by process
start and building the transcript; every option paints within the 16 ms
budget.

Maintenance and platforms (October 2026):

- ratatui 0.30.2 (June 2026), active. Its value is widgets and layout; a
  chat transcript needs its own wrapping and virtualized scrolling either
  way, so we would use little of it.
- crossterm 0.29.0 (April 2025), the de facto backend, Windows included
  (console API and VT). It brings mio, signal-hook and derive_more: about
  85 KiB here.
- termion 4.0.6 (November 2025, Redox): Unix only, no Windows at all, and
  no resize event (SIGWINCH is ours to handle).

Decision: our own renderer and input. It is the smallest by 80 to 260 KiB,
about 4× cheaper per frame than ratatui, and the parts it needs are small
(`render.rs`, `keys.rs`, `sys::tty`). On Windows the same renderer works on
any console with VT processing (Windows 10 1809+, Windows Terminal), and
with `ENABLE_VIRTUAL_TERMINAL_INPUT` the console sends the same xterm key
and SGR mouse sequences `keys.rs` parses; `sys::tty` would need a Windows
backend (console modes, `ReadConsoleInputW` for resize events). Until then
(the client does not run on Windows yet) `a` there uses the line REPL.

## The client as built

`apps/tui` is its own crate so it can build for size, like the web UI;
measured on the release build (`cargo build-release -p a`, default
features) with the client stubbed out and not:

| tui opt-level | Binary | Client's share | Delta frame p50 / p99 (6000 items) | Scroll frame | Resize (re-wrap) |
| --- | --- | --- | --- | --- | --- |
| 3 | 3428 KiB | +194 KiB | 50 / 80 µs | 47 µs | 0.10 ms |
| s (chosen) | 3396 KiB | +162 KiB | 89 / 158 µs | 62 µs | 0.16 ms |
| z | 3348 KiB | +114 KiB | 162 / 214 µs | 98 µs | 0.20 ms |

(`cargo test --release -p tui -- --ignored --nocapture render_cost`; frame
times there exclude the terminal write.) `s` keeps a streamed token's frame
under 0.1 ms; `z` would save 48 KiB more at twice the cost per frame.
`mise run size`: 3234 → 3380 KiB of 4096; without the web feature the
client adds the same 162 KiB.

With the release build (`mise run budgets`): spawn to the end of the first
frame 6.6–8.3 ms (median 7.3 ms) with no daemon running; no CPU over 3 s
idle; about 6.8 MB RSS attached, 7.6 MB after 300 turns.

## Tests

- Unit tests render the app into a `Buf` and read its rows (`app/tests.rs`,
  `view.rs`, `text.rs`, `editor.rs`, `keys.rs`, `render.rs`): approvals by
  key and click, streaming, folding, scrolling with an anchor, slash
  commands, the picker, the recap, the inline scrollback, tiny sizes.
- `apps/cli/tests/tui.rs` runs `a` on a pseudo-terminal against an echo
  agent daemon in its own `A_HOME`, reads the screen through a small
  terminal model (`tests/common/pty.rs`), and checks a reply, an approval
  answered by a click, the command's result, detaching, and an inline
  `a resume` that prints the replay into the scrollback.
- `apps/cli/tests/budgets.rs` holds the first frame to 16 ms and an idle
  client to no CPU.

## Limits

- Grapheme clusters are drawn per character: an emoji ZWJ sequence or a
  combining mark may not line up with what the terminal draws.
- SIGTERM or SIGKILL leave the terminal in raw mode (`reset` fixes it); a
  closed terminal needs nothing restored.
- Ctrl-Z is not handled (raw mode receives it as a key).
- The full screen reports the mouse, so selecting text takes the terminal's
  override (Shift-drag in most, Option-drag in iTerm2 and Terminal.app);
  the inline mode leaves the mouse to the terminal.
