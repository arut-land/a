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
- Tool cards: the call as its tool presents it (the `Card` from
  `Tool::present`, else the input, as every client shows it via
  `protocol::view`), its state (running, waiting for approval, not
  approved, the card's note such as `exit 1` or `+3 -1`, failed,
  interrupted) and the last 10 lines of output; an edit's diff is colored.
  A click on a card folds it; Ctrl-O shows every card's whole output.
  Command output has its escapes stripped. Slash commands the daemon runs
  show as cards too.
- Approvals: the oldest call `asked()` of this client shows above the
  input with `[ y yes ]` `[ n no ]` buttons, and on its card; `y` or `n`
  with an empty input answers it.
- The recap after an attach, as a banner (Esc or its `×` hides it).
- Status line: link state (connecting, replaying, live, daemon upgrading,
  disconnected), session, workspace, model, tokens.
- Input: multi-line (Ctrl-J or Alt-Enter for a new line, bracketed paste
  keeps newlines), readline keys, Up/Down through history (seeded from the
  session's own messages).
- `/sessions` (a picker showing each session's activity and residency;
  also what `a resume` opens when this workspace has no session), `/new`,
  `/env refresh`, `/interrupt`, `/detach`, `/help` (or F1). Any other
  `/name` goes to the daemon, which runs the session's slash command or
  says there is none.
- Ctrl-C while a model call, tool call or slash command runs sends
  `ClientMsg::Interrupt`, which stops it and the rest of its turn; otherwise it
  clears the input, or detaches when it is empty. Nothing the client does
  stops the daemon. Ctrl-D on an empty input detaches.
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
                     App::draw ─> ratatui Buffer ─> ratatui Terminal (diff) ─> crossterm ─> stdout
```

The terminal layer is ratatui's core (`ratatui-core`: cell buffer, diffing
`Terminal`, styles) on crossterm (`ratatui-crossterm`), with no widgets and
no layout cache. What stays ours is what a chat transcript needs and no
widget gives: wrapping cached per item, virtualized scrolling from an
anchor, clickable spans, and the input editor.

- One thread owns the app and blocks on an `mpsc` channel. The terminal
  thread blocks in `crossterm::event::read` with no timeout; each
  connection has a reader thread. Nothing polls on a timer. Everything
  queued is folded before one frame.
- `App::draw` writes rows into the frame's `Buffer` (coordinates relative
  to its area) and returns the caret; ratatui writes only the cells that
  changed, inside a synchronized update (`?2026`).
- Full screen: ratatui's full-screen viewport on the alternate buffer, with
  click and wheel reports (SGR) only: crossterm's `EnableMouseCapture` also
  asks for every motion, which would wake the client as the pointer moves.
- Inline (`screen.rs`), as Codex does it: a fixed viewport this client
  places itself, because ratatui's `Viewport::Inline` has a fixed height.
  Its row comes from one cursor position query at start; after that it is
  tracked. It grows by scrolling the screen when it would pass the bottom;
  settled items are printed from its top with `\r\n`, so they scroll into
  the terminal's own scrollback, and the viewport is drawn whole under
  them. After a resize the terminal thread asks where the cursor is (the
  terminal reflowed its lines) and the viewport is placed from that.
  Narrowing gets a new `Terminal`, since ratatui's `resize` clears the
  whole screen when the width shrinks.
- Rows are wrapped lazily and cached per item and width: a frame wraps only
  what it shows, counted from the bottom or from the scroll anchor (an item
  and a row, which stays put as output arrives below). A 6000-item session
  costs about what a short one does to draw, resize or resume. A reply
  streaming in keeps the rows of its finished lines; a delta wraps only the
  line still open. What the bottom rows need of the whole transcript (the
  oldest ask, whether a turn runs, the latest model) is found once per
  change, not per frame.
- Text of characters that are graphemes on their own (most of it) goes
  into the buffer a cell a character; only the rest goes through ratatui's
  grapheme segmentation.
- The terminal is restored on exit, on a panic (a hook), when stdin
  closes, and on SIGTERM or SIGHUP (a signal-hook thread restores it, then
  lets the signal end the process as it would have).
- The first frame is painted before the daemon is reached; connecting (and
  spawning the daemon) happens on the reader thread. An ssh endpoint
  connects first, since ssh may prompt on the terminal.
- crossterm also drives the Windows console, so the client builds there;
  it stays off (the line REPL) until the daemon runs on Windows.

## Choosing a library: measurements

October 2026, release profile (fat LTO, one codegen unit, `build-std`,
stripped), Apple M3, `nightly-2026-10-17`.

A prototype chat client (2000-message transcript, status, input, wheel
scroll, a stream of 1000 deltas, blocking input plus a channel; tui at
opt-level s) on a 120×40 pty, against the same program printing lines:

| | Binary | vs base | Idle RSS | Idle CPU | CPU, 1000 deltas | First frame (in process) |
| --- | --- | --- | --- | --- | --- | --- |
| base (lines) | 256 KiB | | 2.5 MB | 0 | 0.19 s | 1.7 ms |
| ratatui-core + crossterm 0.29 | 457 KiB | +201 KiB | 3.0 MB | 0 | 0.22 s | 1.9 ms |
| ratatui-core + termion 4 | 413 KiB | +157 KiB | 3.0 MB | 0 | 0.21 s | 1.3 ms |
| ratatui-core + termina 0.3 | 428 KiB | +172 KiB | 3.0 MB | 0 | 0.16 s | 0.7 ms |

(CPU is dominated by the prototype re-wrapping its streamed text; the
backends are within noise of each other.)

- crossterm 0.29.0 (April 2025): the de facto backend, used by Codex;
  Windows console and VT; brings mio and signal-hook on Unix.
- termion 4: Unix only, no resize event. Ruled out for Windows.
- termina (helix): about 30 KiB smaller, no mio, Windows via VT; but
  `ratatui-termina` 0.1.0 pins termina 0.3 while termina is at 0.4, and it
  has a fraction of crossterm's users. A swap later is a small change
  (the backend type, the event mapping in `keys.rs`, `term.rs`).
- Frameworks (iocraft, cursive, tui-realm, r3bl_tui, rat-salsa) bring
  layout engines, async runtimes or no inline mode; ratatui-textarea does
  not soft-wrap, which the input needs.

In `a` (`mise run size`, `mise run budgets`, before → after the move):

| | Own renderer (7a38b37) | ratatui-core + crossterm |
| --- | --- | --- |
| Release binary | 3768 KiB | 3898 KiB (+130) |
| Idle client RSS | 7.6 MB | 7.8 MB |
| Idle client CPU (3 s) | 0 | 0 |
| Spawn to first frame (median of 5) | 5.7 ms | 5.0 ms |
| Delta frame p50 / p99, 6000 items | 78 / 143 µs | 210 / 259 µs |
| Scroll frame p50 | 55 µs | 162 µs |
| Bytes per delta frame | 299 | 286 |

(`cargo test --release -p tui -- --ignored --nocapture render_cost`; frame
times exclude the terminal write.) A frame costs more through ratatui
(grapheme segmentation, a buffer reset and a full diff per frame) but stays
far under a millisecond; at opt-level 3 the client's delta frame is 146 µs.
Writing plain text into cells directly, and scanning the transcript once
per change instead of per frame, took the delta frame from 360 to 207 µs
and the scroll frame from 340 to 205 µs (both builds measured in turns on a
loaded machine; the full diff is now about half of what is left), and a
delta at the end of a 28 KB reply from 784 to 89 µs.

## Tests

- Unit tests draw the app through ratatui's `TestBackend` and read the
  buffer's rows (`app/tests.rs`, `view.rs`, `text.rs`, `editor.rs`,
  `keys.rs`, `render.rs`): approvals by key and click, streaming, folding,
  scrolling with an anchor, slash commands, the picker, the recap, the
  inline scrollback, tiny sizes; `keys.rs` checks how crossterm's events
  map to the app's keys.
- `apps/cli/tests/tui.rs` runs `a` on a pseudo-terminal against an echo
  agent daemon in its own `A_HOME`, reads the screen through a small
  terminal model (`tests/common/pty.rs`, which also answers cursor
  position queries), and checks a reply, an approval answered by a click,
  the command's result, detaching, an inline `a resume` that prints the
  replay into the scrollback, and a SIGTERM that leaves the terminal
  cooked.
- `apps/cli/tests/budgets.rs` holds the first frame to 16 ms and an idle
  client to no CPU.

## Limits

- Wrapping counts columns per character while ratatui draws per grapheme,
  so an emoji ZWJ sequence may wrap a little early.
- SIGKILL leaves the terminal in raw mode (`reset` fixes it).
- Ctrl-Z is not handled (raw mode receives it as a key).
- Inline asks the terminal where the cursor is at start and after a
  resize; a terminal that never answers costs half a second (crossterm's
  own query waits 2 s), and the viewport then sits at the bottom. Keys
  typed before the answer are dropped.
- The full screen reports the mouse, so selecting text takes the terminal's
  override (Shift-drag in most, Option-drag in iTerm2 and Terminal.app);
  the inline mode leaves the mouse to the terminal.
